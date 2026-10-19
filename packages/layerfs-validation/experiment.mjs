import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import http from 'node:http';
import { spawnSync } from 'node:child_process';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
const root = process.env.EXPERIMENT_ROOT;
assert(root?.startsWith('/opt/layerfs-validation/'));
const { LayerFS } = await import(`${process.env.UPSTREAM_ROOT}/sdk/js/src/index.js`);
const { LayerFSOperator } = await import(`${process.env.UPSTREAM_ROOT}/sdk/js/operator/index.js`);
const receipts = join(root, 'receipts.jsonl');
const fixtureManifest = JSON.parse(fs.readFileSync(join(root, 'fixture-manifest.json'))).summary;
const records = [];
const json = value => JSON.stringify(value, (_, v) => typeof v === 'bigint' ? String(v) : v);
function record(row) { records.push(row); fs.appendFileSync(receipts, json(row) + '\n'); console.log(json(row)); }
async function measured(id, operation, { critical = true } = {}) {
  const began = process.hrtime.bigint();
  const timer = setTimeout(() => { record({ id, outcome: 'TIMEOUT', hardTimeoutMs: 120000 }); process.exit(124); }, 120000);
  try {
    const result = await operation();
    const ns = process.hrtime.bigint() - began;
    const outcome = result?.exitCode && result.exitCode !== 0 ? 'FAIL' : 'PASS';
    record({ id, outcome, ns: String(ns), result });
    if (critical && outcome === 'FAIL') { const error = new Error(`${id}: command failed`); error.recorded = true; throw error; }
    return result;
  } catch (e) {
    if (!e.recorded) record({ id, outcome: 'FAIL', ns: String(process.hrtime.bigint() - began), error: { code: e.code, message: e.message, detail: e.detail } });
    if (critical) throw e;
  } finally { clearTimeout(timer); }
}
function local(program, args) {
  const r = spawnSync(program, args, { encoding: 'utf8', timeout: 120000, maxBuffer: 4 * 1024 * 1024 });
  assert.equal(r.status, 0, `${program}: ${r.stderr}`); return r.stdout;
}
function allocated(path) { return { physicalBytes: Number(local('du', ['-s', '-B1', path]).split(/\s/)[0]), apparentBytes: Number(local('du', ['-sb', path]).split(/\s/)[0]) }; }
function docker(method, path, body) {
  return new Promise((done, fail) => {
    const req = http.request({ socketPath: '/var/run/docker.sock', method, path: `/v1.54${path}`, headers: { 'Content-Type': 'application/json' }, timeout: 10000 }, res => {
      const chunks = []; res.on('data', c => chunks.push(c)); res.on('end', () => { const b = Buffer.concat(chunks); if (res.statusCode >= 300) fail(new Error(`${path}: ${res.statusCode} ${b}`)); else done(b); });
    });
    req.on('timeout', () => req.destroy(new Error('Docker observer timeout'))); req.on('error', fail); req.end(body ? json(body) : undefined);
  });
}
async function observerExec(id, command) {
  const created = JSON.parse(await docker('POST', `/containers/${id}/exec`, { AttachStdout: true, AttachStderr: true, Cmd: ['sh', '-c', command], User: '0' }));
  const framed = await docker('POST', `/exec/${created.Id}/start`, { Detach: false, Tty: false });
  let text = ''; for (let at = 0; at + 8 <= framed.length;) { const n = framed.readUInt32BE(at + 4); text += framed.subarray(at + 8, at + 8 + n); at += 8 + n; }
  const inspected = JSON.parse(await docker('GET', `/exec/${created.Id}/json`));
  assert.equal(inspected.ExitCode, 0, `observer command failed: ${text}`);
  return text;
}
const client = new LayerFS('engineboxfixture', { storePath: '/layerfs-store/global/store.sqlite' });
const operator = new LayerFSOperator(client);
const sandboxes = [];
async function storage(id) {
  const overlays = [];
  for (const name of sandboxes) {
    const container = fs.readFileSync(join(process.env.LAYERFS_HOME, '_sandboxes', name, 'container'), 'utf8').trim();
    overlays.push({ name, container, output: await observerExec(container, 'du -s -B1 /layerfs-local/overlay; du -sb /layerfs-local/overlay; find /layerfs-local/overlay -type f -printf "%p %s %b\\n"') });
  }
  record({ id, outcome: 'OBSERVATION', store: allocated('/layerfs-store'), files: fs.readdirSync('/layerfs-store/global').map(name => { const s = fs.statSync(`/layerfs-store/global/${name}`); return { name, size: s.size, physicalBytes: s.blocks * 512 }; }), overlays, stats: await operator.store.stats() });
}
const quote = value => `'${value.replaceAll("'", "'\\''")}'`;
const run = (name, path, command, id, critical = true) => measured(id, () => client.sandbox.exec(name, path, `timeout 100 bash -c ${quote(`set -euo pipefail; ${command}`)}`), { critical });
async function logs(name) {
  const container = fs.readFileSync(join(process.env.LAYERFS_HOME, '_sandboxes', name, 'container'), 'utf8').trim();
  fs.writeFileSync(join(root, `daemon-${name}.log`), await docker('GET', `/containers/${container}/logs?stdout=1&stderr=1&tail=2000`));
}
record({ id: 'identity', sourcePin: '968a5d0be0ea3e9456b06f9148b65523138e9490', persistence: 'Disposable/WAL/synchronous=OFF', cache: 'uncontrolled warm; no host cache drops; diagnostic observations only', sampleCount: 1, image: process.env.LAYERFS_IMAGE, node: process.version, source: allocated(join(root, 'fixture')), powerLossTest: 'NOT_RUN', forcedDaemonCrash: 'NOT_RUN' });
try {
  record({ id: 'fixture-manifest', outcome: 'OBSERVATION', admissionEligible: false, oracle: fixtureManifest, verificationCacheEffect: 'reads every file before LayerFS initialization; uncontrolled warm cache' });
  await measured('baseline-copy-a', () => local('cp', ['--reflink=never', '-a', join(root, 'fixture'), join(root, 'copy-a')]));
  await measured('baseline-copy-b', () => local('cp', ['--reflink=never', '-a', join(root, 'fixture'), join(root, 'copy-b')]));
  await measured('baseline-reflink-capability', () => {
    const r = spawnSync('cp', ['--reflink=always', '-a', join(root, 'fixture'), join(root, 'copy-reflink')], { encoding: 'utf8', timeout: 120000 });
    return { outcome: r.status === 0 ? 'SUPPORTED' : 'NOT_SUPPORTED', exitCode: r.status, stderr: r.stderr, storage: r.status === 0 ? allocated(join(root, 'copy-reflink')) : null };
  }, { critical: false });
  record({ id: 'baseline-storage', copyA: allocated(join(root, 'copy-a')), copyB: allocated(join(root, 'copy-b')) });
  for (const name of ['copy-a', 'copy-b']) await measured(`verify-${name}-manifest`, () => {
    const result = JSON.parse(local('node', [join(root, name, 'validation-manifest.mjs'), join(root, name)]));
    assert.equal(result.sha256, fixtureManifest.sha256); return result;
  });
  await measured('project-init', () => client.project.init(join(root, 'fixture')));
  await measured('sandbox-a-create', () => client.sandbox.create('agent-a', 'engineboxfixture', process.env.LAYERFS_IMAGE, { volumes: [{ volume: process.env.PNPM_VOLUME, target: '/pnpm-store', readOnly: true }] })); sandboxes.push('agent-a');
  await storage('before-forks');
  await measured('fork-a', () => client.branch.fork('agent-a', 'main'));
  await measured('fork-b', () => client.branch.fork('agent-b', 'main'));
  await storage('after-forks');
  await measured('sandbox-b-create', () => client.sandbox.create('agent-b', 'engineboxfixture', process.env.LAYERFS_IMAGE)); sandboxes.push('agent-b');
  let a = await measured('mount-a', () => client.sandbox.mount('agent-a', 'agent-a'));
  const b = await measured('mount-b', () => client.sandbox.mount('agent-b', 'agent-b'));
  await run('agent-a', a, `EXPECTED_MANIFEST_SHA256=${fixtureManifest.sha256} node validation-manifest.mjs .`, 'verify-mounted-a-manifest');
  await run('agent-b', b, `EXPECTED_MANIFEST_SHA256=${fixtureManifest.sha256} node validation-manifest.mjs .`, 'verify-mounted-b-manifest');
  await run('agent-a', a, 'git status --porcelain=v1; node -p \'require("typescript/package.json").version\'; sha256sum -c payload.sha256', 'first-use-git-node-a');
  await run('agent-b', b, 'git status --porcelain=v1; node -p \'require("typescript/package.json").version\'; sha256sum -c payload.sha256', 'first-use-git-node-b');
  await run('agent-a', a, 'pnpm exec tsc --version', 'imported-pnpm-shim', false);
  await run('agent-a', a, 'node validation-probe.mjs operations', 'filesystem-operations');
  await run('agent-a', a, 'printf "agent A\\n" > agent-a.txt; printf "// metadata-only compiler output\\n" >> src/index.ts; node node_modules/typescript/bin/tsc -b --verbose; node validation-probe.mjs metadata; git add agent-a.txt src/index.ts; git commit -qm agent-a', 'edit-build-git-a');
  await run('agent-b', b, 'test ! -e agent-a.txt; printf "agent B\\n" > agent-b.txt; git add agent-b.txt; git commit -qm agent-b', 'isolation-edit-git-b');
  await storage('live-overlays');
  await measured('concurrent-commits', () => Promise.all([client.sandbox.commit('agent-a', a, 'agent A changes'), client.sandbox.commit('agent-b', b, 'agent B changes')]));
  await storage('after-commits');
  await measured('unmount-a', () => client.sandbox.unmount('agent-a', a));
  a = await measured('remount-a', () => client.sandbox.mount('agent-a', 'agent-a'));
  await run('agent-a', a, 'node validation-probe.mjs metadata; node node_modules/typescript/bin/tsc -b --verbose; node -p \'require("typescript/package.json").version\'; cat agent-a.txt; test ! -e agent-b.txt', 'remount-dependencies-mtimes-build');
  const checkText = async (c, branch, file, expected) => { const value = await c.fs.read(branch, file); assert.equal(value.content ?? value.text, expected, json(value)); return value; };
  await measured('committed-edit-a-retained', () => checkText(client, 'agent-a', '/agent-a.txt', 'agent A\n'));
  await measured('committed-edit-b-retained', () => checkText(client, 'agent-b', '/agent-b.txt', 'agent B\n'));
  await run('agent-a', a, 'node validation-probe.mjs mtime-set', 'mtime-only-set');
  await measured('mtime-only-commit', () => client.sandbox.commit('agent-a', a, 'mtime only observation'));
  await measured('mtime-only-unmount', () => client.sandbox.unmount('agent-a', a));
  a = await measured('mtime-only-remount', () => client.sandbox.mount('agent-a', 'agent-a'));
  await run('agent-a', a, 'node validation-probe.mjs mtime', 'mtime-only-after-remount');
  // Full-tree merge also includes .git metadata; conflicts are evidence, not
  // grounds to discard failures or skip independent dependency checks.
  const merged = await measured('merge-branches', () => client.branch.merge('agent-a', 'agent-b', 'combined', 'combine independent agent edits'), { critical: false });
  if (merged) {
    const combined = await measured('mount-combined', () => client.sandbox.mount('agent-b', 'combined'));
    await run('agent-b', combined, 'cat agent-a.txt agent-b.txt; sha256sum -c payload.sha256; git log -1 --format=%s', 'both-edits-retained');
    await measured('unmount-combined', () => client.sandbox.unmount('agent-b', combined));
  }
  await run('agent-a', a, 'rm -rf node_modules; pnpm install --offline --frozen-lockfile --ignore-scripts --package-import-method=copy --store-dir /pnpm-store --reporter append-only; node -p \'require("typescript/package.json").version\'', 'pnpm-offline-reinstall-readonly-store', false);
  await measured('commit-after-reinstall', () => client.sandbox.commit('agent-a', a, 'dependency reinstall observation'));
  await storage('after-reinstall');
  await measured('unmount-final-a', () => client.sandbox.unmount('agent-a', a));
  await measured('unmount-final-b', () => client.sandbox.unmount('agent-b', b));
  await logs('agent-a'); await logs('agent-b');
  await measured('close-sandbox-a', () => client.sandbox.close('agent-a')); sandboxes.splice(sandboxes.indexOf('agent-a'), 1);
  await measured('close-sandbox-b', () => client.sandbox.close('agent-b')); sandboxes.splice(sandboxes.indexOf('agent-b'), 1);
  await client.close();
  const reopened = new LayerFS('engineboxfixture', { storePath: '/layerfs-store/global/store.sqlite' });
  await measured('reopen-store-read-a', () => checkText(reopened, 'agent-a', '/agent-a.txt', 'agent A\n'));
  await measured('reopen-store-read-b', () => checkText(reopened, 'agent-b', '/agent-b.txt', 'agent B\n'));
  await reopened.close();
  const db = new DatabaseSync('/layerfs-store/global/store.sqlite', { readOnly: true });
  const integrity = db.prepare('PRAGMA integrity_check').all();
  assert.equal(integrity.length, 1); assert.equal(Object.values(integrity[0])[0], 'ok');
  record({ id: 'sqlite-integrity-check', outcome: 'PASS', result: integrity, store: allocated('/layerfs-store') }); db.close();
} catch (error) {
  record({ id: 'campaign-error', outcome: 'FAIL', message: error.message, stack: error.stack });
  process.exitCode = 1;
} finally {
  for (const name of sandboxes) {
    try {
      await logs(name);
    } catch (e) { record({ id: 'diagnostic-log-error', name, message: e.message }); }
  }
  fs.writeFileSync(join(root, 'summary.json'), json(records));
  await client.close().catch(e => record({ id: 'client-close-error', message: e.message }));
}
