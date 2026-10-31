import { readFile, lstat } from 'node:fs/promises';
import { join, resolve, dirname } from 'node:path';
import { pathToFileURL } from 'node:url';
import { check, fields, config, template, version, json, regular, selectArtifact, manifest, installerArgs, nodeInstallerArgs, run, WATCH_DEFAULTS, watchVerdict, startFailure } from './lib.mjs';

async function privateInput(file, directory = false) {
  if (directory) {
    for (let cursor = file; ; cursor = dirname(cursor)) {
      check(!(await lstat(cursor)).isSymbolicLink(), 'symlink-input-refused');
      if (cursor === dirname(cursor)) break;
    }
  }
  const meta = directory ? await lstat(file) : await regular(file);
  check(!meta.isSymbolicLink() && (directory ? meta.isDirectory() : meta.isFile()) && meta.uid === process.getuid() && (meta.mode & 0o077) === 0, 'server-input-must-be-private-and-owned');
}
async function api(base, token, endpoint, body) {
  const response = await fetch(`${base}${endpoint}`, {
    method: body === undefined ? 'GET' : 'POST', redirect: 'error', signal: AbortSignal.timeout(15000),
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  check(response.ok, `controller-request-rejected-${response.status}`);
  const text = await response.text();
  check(text.length <= 3 * 1024 * 1024, 'controller-response-bound');
  try { return JSON.parse(text); } catch { throw new Error('controller-json'); }
}
export function syncState(catalog, expected) {
  check(Array.isArray(catalog.templates), 'template-catalog-shape');
  let pending = false;
  for (const wanted of expected) {
    const current = catalog.templates.find(t => t.id === wanted.id);
    check(current && current.revision === wanted.revision && current.manifest_digest === wanted.manifest_digest, 'template-revision-changed-during-sync');
    const nodes = current.sync?.nodes;
    check(Array.isArray(nodes) && nodes.length > 0, 'template-has-no-registered-nodes');
    check(nodes.every(n => ['ready', 'pending', 'syncing', 'failed'].includes(n.status)), 'unknown-template-sync-state');
    check(!nodes.some(n => n.status === 'failed'), 'template-sync-failed-inspect-controller');
    pending ||= nodes.some(n => n.status !== 'ready');
  }
  return !pending;
}
function protocol(report) {
  check(report.component === 'ebox' && report.protocol === 1 && report.os === 'linux' && report.layerfs?.workspace_schema === 2 && report.layerfs?.attachment_schema === 3 && report.layerfs?.exclusive_guest_writer === true && report.capabilities?.reserved_workspace_guard === 1 && report.capabilities?.external_checkout_lifecycle_guard === 1, 'candidate-node-protocol-incompatible');
}
export async function execute(request, directory) {
  fields(request, ['operation', 'version', 'config', 'templates', 'manifest_sha256']);
  check(request.operation === 'update' ? /^[a-f0-9]{64}$/.test(request.manifest_sha256) : request.manifest_sha256 === null, 'request-manifest-pin');
  check(['update', 'rollback', 'status', 'smoke', 'watch'].includes(request.operation), 'invalid-deployment-operation');
  const deployment = config(request.config); const pin = request.operation === 'status' ? null : version(request.version); const c = deployment.controller;
  check(Array.isArray(request.templates) && request.templates.length <= 32, 'template-list');
  request.templates.forEach(v => template(v, deployment));
  check(new Set(request.templates.map(t => t.id)).size === request.templates.length, 'duplicate-template-id');
  check(request.operation === 'update' || request.templates.length === 0, 'nonupdate-cannot-change-templates');
  check(process.platform === 'linux' && process.arch === 'x64', 'remote-platform');
  await privateInput(c.prefix, true); await privateInput(c.state_dir, true);
  await privateInput(c.token_file); await privateInput(c.supervisor_config);
  await regular(c.installer, 512 * 1024 ** 2);
  const settings = await json(c.supervisor_config);
  check(settings.prefix === c.prefix && (settings.state_dir ?? `${c.prefix}/state`) === c.state_dir && settings.token_file === c.token_file && `http://${settings.listen}` === c.url, 'supervisor-path-binding-mismatch');
  const token = (await readFile(c.token_file, 'utf8')).trim();
  check(/^[!-~]{32,}$/.test(token), 'invalid-server-token');
  const previous = await api(c.url, token, '/__enginebox/status');
  version(previous.current);
  if (request.operation === 'status') {
    const ready = await api(c.url, token, '/readyz');
    check(ready.version === previous.current && ready.mode === 'active', 'controller-not-ready');
    return { ok: true, operation: 'status', version: previous.current, ready: true, private_state_preserved: true };
  }
  if (request.operation === 'smoke') return smoke(deployment, c.url, token, pin, previous);
  if (request.operation === 'watch') return watch(deployment, c.url, token, pin, previous);
  const release = join(c.prefix, 'releases', pin);
  if (request.operation === 'update') {
    const selected = await selectArtifact(directory, pin, deployment.target);
    check(selected.manifest_sha256 === request.manifest_sha256, 'transferred-manifest-checksum');
    let exists = false;
    try { await lstat(release); exists = true; } catch (error) { if (error.code !== 'ENOENT') throw error; }
    if (exists) {
      const installed = manifest(await json(join(release, 'manifest.json')), pin, deployment.target);
      check(installed.archive_sha256 === selected.value.archive_sha256, 'immutable-version-already-used-by-another-artifact');
    } else {
      await run(c.installer, installerArgs(deployment, 'stage', pin, selected), { timeout: 600000 });
    }
  }
  manifest(await json(join(release, 'manifest.json')), pin, deployment.target);
  // An older controller with the same generic state schema may ignore external
  // checkout markers. Require the matching complete release's behavioral gate.
  const report = JSON.parse(await run(join(release, 'bin/ebox'), ['--host', 'local', 'node-protocol']));
  protocol(report);
  const builtVersion = (await run(join(release, 'bin/enginebox-controller'), ['--version'])).trim();
  check(builtVersion === `enginebox-controller ${pin}`, 'candidate-binary-version-mismatch');
  if (previous.current !== pin) await run(c.installer, installerArgs(deployment, request.operation, pin), { timeout: 1200000 });
  const ready = await api(c.url, token, '/readyz');
  check(ready.version === pin && ready.mode === 'active', 'deployed-controller-not-ready');
  const expected = [];
  for (const recipe of request.templates) {
    const result = await api(c.url, token, '/v1/templates', { manifest: recipe });
    check(result.template?.id === recipe.id && result.template.revision === recipe.revision && /^[a-f0-9]{64}$/.test(result.template.manifest_digest), 'template-registration-response');
    expected.push(result.template);
  }
  const deadline = Date.now() + deployment.templates.timeout_seconds * 1000;
  while (expected.length && !syncState(await api(c.url, token, '/v1/templates'), expected)) {
    check(Date.now() < deadline, 'template-sync-timeout-inspect-controller');
    await new Promise(resolveWait => setTimeout(resolveWait, 10000));
  }
  let nodeRollout = false;
  if (deployment.node) {
    // Same release, same verified artifact: stage it in the node installation (once), make it current, republish the
    // host helper. Workspaces then refresh their guest helper from the version this controller expects. A rollback
    // re-activates the retained node release of the same version, so controller and helper never drift apart.
    const sudo = deployment.node.sudo;
    const installer = join(release, 'bin/enginebox-install');
    let staged = true;
    try { await run(sudo, ['-n', 'test', '-d', join(deployment.node.prefix, 'releases', pin)]); } catch { staged = false; }
    if (!staged) {
      check(request.operation === 'update', 'rollback-node-release-not-retained');
      const selected = await selectArtifact(directory, pin, deployment.target);
      await run(sudo, ['-n', installer, ...nodeInstallerArgs(deployment, 'stage', pin, selected)], { timeout: 600000 });
    }
    const current = JSON.parse(await run(sudo, ['-n', 'cat', join(deployment.node.prefix, 'current.json')]));
    if (current.current !== pin) await run(sudo, ['-n', installer, ...nodeInstallerArgs(deployment, 'activate', pin)], { timeout: 600000 });
    await run(sudo, ['-n', installer, ...nodeInstallerArgs(deployment, 'expose-host', pin)], { timeout: 120000 });
    nodeRollout = true;
  }
  return { ok: true, operation: request.operation, version: pin, previous_version: previous.current, templates_ready: expected.length, node_rollout: nodeRollout, private_state_preserved: true };
}

const sleep = ms => new Promise(r => setTimeout(r, ms));
const listRuns = async (base, token) => {
  const value = await api(base, token, '/v1/agent-runs');
  check(Array.isArray(value.runs), 'agent-runs-shape');
  return value.runs;
};

/** The staging gate: the exact version is active and serving reads; with a pool and provider, a trivial run succeeds. */
async function smoke(deployment, base, token, pin, status) {
  check(status.current === pin, 'smoke-version-not-active');
  const ready = await api(base, token, '/readyz');
  check(ready.version === pin && ready.mode === 'active', 'smoke-controller-not-ready');
  for (const endpoint of ['/v1/pools', '/v1/agent-runs', '/v1/templates']) await api(base, token, endpoint);
  const s = deployment.smoke ?? {};
  if (!s.pool) return { ok: true, operation: 'smoke', version: pin, run: 'skipped' };
  const started = await api(base, token, `/v1/pools/${s.pool}/agent-runs`, { provider: s.provider, prompt: s.prompt ?? 'Reply with exactly: OK', timeout_seconds: s.timeout_seconds ?? 300 });
  const id = started.run?.id ?? started.id;
  check(typeof id === 'string' && /^[a-f0-9]{32}$/.test(id), 'smoke-run-not-started');
  const deadline = Date.now() + ((s.timeout_seconds ?? 300) + 120) * 1000;
  for (;;) {
    const read = await api(base, token, `/v1/pools/${s.pool}/agent-runs/${id}`);
    const run = read.run ?? read;
    if (['succeeded', 'failed', 'cancelled'].includes(run.status)) {
      check(run.status === 'succeeded', 'smoke-run-failed');
      const output = String(run.result?.output ?? '');
      check(output.includes(s.expect ?? 'OK'), 'smoke-run-unexpected-output');
      return { ok: true, operation: 'smoke', version: pin, run: { status: run.status, start_path: run.start_path ?? null, start_latency_ms: run.start_latency_ms ?? null } };
    }
    check(Date.now() < deadline, 'smoke-run-timeout');
    await sleep(5000);
  }
}

/**
 * The post-promotion watch: readiness and new runs' start failures since the cutover. A regression exits nonzero
 * with its reason so the workflow rolls back to the previous version.
 */
async function watch(deployment, base, token, pin, status) {
  check(status.current === pin, 'watch-version-not-active');
  const w = { ...WATCH_DEFAULTS, ...(deployment.watch ?? {}) };
  const baseline = new Set((await listRuns(base, token)).map(r => r.id));
  const seen = new Map();
  let consecutive = 0; let checks = 0;
  const deadline = Date.now() + w.seconds * 1000;
  do {
    checks++;
    let readyNow = false;
    try { const ready = await api(base, token, '/readyz'); readyNow = ready.version === pin && ready.mode === 'active'; } catch { readyNow = false; }
    consecutive = readyNow ? 0 : consecutive + 1;
    if (readyNow) for (const r of await listRuns(base, token)) if (!baseline.has(r.id)) seen.set(r.id, r);
    const observed = { consecutive_unready: consecutive, new_runs: seen.size, start_failures: [...seen.values()].filter(startFailure).length };
    const verdict = watchVerdict(observed, w);
    if (verdict.regressed) throw new Error(`watch-regressed-${verdict.reason}`);
    if (Date.now() >= deadline) return { ok: true, operation: 'watch', version: pin, checks, ...observed };
    await sleep(w.interval_seconds * 1000);
  } while (true);
}
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    const file = resolve(process.argv[2]);
    const result = await execute(await json(file), resolve(file, '..'));
    process.stdout.write(`${JSON.stringify(result)}\n`);
  } catch (error) {
    // Print only authored codes, never provider/configuration/HTTP error bodies.
    const code = /^[a-z0-9-]+$/.test(error.message) ? error.message : 'deployment-failed-inspect-private-server-logs';
    process.stdout.write(`${JSON.stringify({ ok: false, error: code })}\n`);
    process.exitCode = 1;
  }
}
