import { readFile, lstat } from 'node:fs/promises';
import { join, resolve, dirname } from 'node:path';
import { pathToFileURL } from 'node:url';
import { check, fields, config, template, version, json, regular, selectArtifact, manifest, installerArgs, run } from './lib.mjs';

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
  check(['update', 'rollback', 'status'].includes(request.operation), 'invalid-deployment-operation');
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
  return { ok: true, operation: request.operation, version: pin, previous_version: previous.current, templates_ready: expected.length, node_rollout: false, private_state_preserved: true };
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
