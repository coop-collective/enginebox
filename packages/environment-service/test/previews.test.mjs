import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm, realpath, symlink } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, dirname, resolve } from 'node:path';
import { Environments } from '../src/runtime.mjs';
import { WorkspacePreviews } from '../src/previews.mjs';
import { validateConfig } from '../src/config.mjs';
import { digest, resolveProject } from '../src/project-template.mjs';
import { serve } from '../src/main.mjs';
import { createServer } from 'node:net';

const template = () => ({ schema_version: 1, kind: 'dev', id: 'web', revision: 1, image: `node@sha256:${'a'.repeat(64)}`, prepare: ['install'], command: ['node', 'app.mjs'], installInputs: ['package.json'], ports: [{ name: 'web', protocol: 'http', port: 3000, expose: true }], health: { port: 'web', path: '/health' }, timeoutSeconds: 5 });
const files = (version = 'one', t = template(), pkg = '{}') => ({ '.enginebox/config.json': JSON.stringify({ schema_version: 1, dev: { template: './templates/web.json' } }), '.enginebox/templates/web.json': JSON.stringify(t), 'package.json': pkg, 'app.mjs': version });
async function snapshot(root, contents, project = 'demo') {
  const dir = await mkdtemp(join(root, 'source-')); const entries = [];
  for (const [path, value] of Object.entries(contents)) {
    await mkdir(dirname(join(dir, path)), { recursive: true }); await writeFile(join(dir, path), value);
    entries.push({ path, kind: 'file', size: Buffer.byteLength(value), sha256: digest(value), executable: false });
  }
  const revision = digest(entries);
  return { revision, sourceRoot: await realpath(dir), manifest: { schema: 1, authority: 'local-layerfs', project, revision, treeHash: digest(entries), entries } };
}
function config(root) {
  return { schema: 1, stateDir: root, tokenFile: join(root, 'token'), listenPort: 18790, baseDomains: ['dev.example.com'], httpEntrypoint: 'web', httpTls: false, basicAuthUsers: [], transportEntrypoints: [], pollSeconds: 2, environments: [], bootstrap: { hosts: [{ id: 'node', command: [resolve('/export')], workspaceCommand: [resolve('/workspace-export')] }], templates: [], portRange: [21000, 21100], domain: 'dev.example.com', access: 'public', maxCpu: 2, maxMemoryMiB: 2048, maxProjects: 4 } };
}

async function harness(root) {
  let selected = await snapshot(root, files()); const calls = []; const containers = new Map(); const images = new Set();
  let setupFailure = false; let startFailure = false; let fetchFailure = false;
  const run = async (program, args) => {
    if ([resolve('/export'), resolve('/workspace-export')].includes(program)) {
      if (fetchFailure) throw new Error('export-unavailable');
      return JSON.stringify({ ...selected, manifest: { ...selected.manifest, project: args[0], ...(program === resolve('/workspace-export') ? { authority: 'local-layerfs-workspace', workspace: args[1], consistency: 'exclusive-command-boundary', sandbox: 'owned-sandbox', branch: args[1] } : {}) } });
    }
    calls.push(args);
    if (args[0] === 'image') { if (!images.has(args[2])) throw new Error('command-failed'); return '[]'; }
    if (args[0] === 'pull') { images.add(args[1]); return ''; }
    if (args[0] === 'ps') { const name = args[3].slice('name=^/'.length, -1); return containers.has(name) ? name : ''; }
    if (args[0] === 'inspect') return JSON.stringify([containers.get(args[1])]);
    if (args[0] === 'rm') { containers.delete(args[2]); return ''; }
    if (args[0] === 'stop') { containers.get(args.at(-1)).State.Running = false; return ''; }
    if (args[0] === 'run') {
      const name = args[args.indexOf('--name') + 1];
      const labels = Object.fromEntries(args.flatMap((v, i) => v === '--label' ? [args[i + 1].split('=')] : []));
      const mount = args.find(v => v.startsWith('type=bind,src=') && v.includes(',dst=/app'));
      const directory = mount.slice('type=bind,src='.length).split(',dst=')[0];
      if (name.endsWith('-prepare')) {
        assert(!args.some(v => v.includes(',dst=/data')), 'setup must not write live persistent data');
        if (setupFailure) throw new Error('setup-failed');
        await mkdir(join(directory, 'node_modules')); await writeFile(join(directory, 'node_modules/installed'), 'installed'); return '';
      }
      const dataMount = args.find(v => v.includes(',dst=/data'));
      assert(![...containers.values()].some(c => c.State.Running && c.dataMount === dataMount), 'old and new runtimes must never write shared data concurrently');
      containers.set(name, { State: { Running: !startFailure }, Config: { Labels: labels }, dataMount });
      startFailure = false; return name;
    }
    throw new Error(`Unexpected fake Docker command: ${args[0]}`);
  };
  const create = async () => {
    const manager = new Environments(validateConfig(config(root)), run);
    manager.healthy = async env => !!(await manager.inspect(env))?.State.Running;
    manager.ready = async env => { if (!await manager.healthy(env)) throw new Error('service-exited'); };
    await manager.init(); return manager;
  };
  return { create, calls, containers, setSource: async (contents) => { selected = await snapshot(root, contents); }, failSetup: value => { setupFailure = value; }, failStart: () => { startFailure = true; }, failFetch: value => { fetchFailure = value; } };
}

const request = (workspace = 'draft-a') => ({ host: 'node', project: 'demo', workspace, task: `task-${workspace}`, ttlSeconds: 300 });

test('leased drafts reuse the file watcher, preserve main, fence renewals, and remove only owned routes/processes', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-preview-'));
  try {
    const h = await harness(root); let m = await h.create();
    await m.projects.register({ host: 'node', project: 'demo' }); await m.tick();
    const main = m.projects.catalog()[0];
    const a = await m.previews.register(request()); const b = await m.previews.register(request('draft-b'));
    assert.equal((await m.previews.register(request())).id, a.id);
    await m.tick();
    assert(!JSON.stringify(m.catalog()).includes(a.lease), 'general dashboard catalog must not expose leases');
    const ready = m.previews.summary(m.previews.records[a.id]); assert.equal(ready.status, 'ready'); assert.equal(ready.healthy, true);
    assert.notEqual(ready.environment, main.environment); assert.notEqual(ready.urls[0], main.urls[0]);
    const process = m.container(m.config.environments.find(e => e.id === ready.environment));
    const old = h.containers.get(process); const runs = h.calls.filter(c => c[0] === 'run').length;
    const changed = files('two'); delete changed['app.mjs']; changed['renamed.mjs'] = 'renamed';
    await h.setSource(changed); await m.tick();
    assert.equal(h.calls.filter(c => c[0] === 'run').length, runs); assert.equal(h.containers.get(process), old);
    const workspace = join(root, ready.environment, 'workspace');
    assert.equal(await readFile(join(workspace, 'renamed.mjs'), 'utf8'), 'renamed');
    await assert.rejects(readFile(join(workspace, 'app.mjs')), /ENOENT/);
    assert.notEqual(m.previews.summary(m.previews.records[a.id]).sourceRevision, ready.sourceRevision);
    h.failFetch(true); await m.tick(); assert.equal(m.previews.summary(m.previews.records[a.id]).healthy, true);
    m = await h.create(); assert.equal(m.previews.summary(m.previews.records[a.id]).healthy, true);
    const renew = { id: a.id, lease: a.lease, expectedRevision: a.revision, ttlSeconds: 300 };
    const results = await Promise.allSettled([m.previews.register(renew), m.previews.register(renew)]);
    assert.equal(results.filter(r => r.status === 'fulfilled').length, 1);
    assert.equal(results.find(r => r.status === 'rejected').reason.message, 'stale-preview-lease');
    await assert.rejects(m.previews.register({ ...renew, lease: b.lease }), /stale-preview-lease/);
    h.containers.set(`${process}-prepare`, { State: { Running: true }, Config: { Labels: { 'enginebox.environment.owner': m.owner, 'enginebox.environment.id': ready.environment } } });
    const revoked = await m.previews.register({ id: a.id, lease: a.lease, expectedRevision: 2, revoke: true });
    assert.equal(revoked.status, 'revoked'); assert.equal(revoked.healthy, false); assert.deepEqual(revoked.urls, []);
    assert(!h.containers.has(`${process}-prepare`));
    assert(!h.containers.has(process)); assert(h.containers.has(m.container(m.config.environments.find(e => e.id === main.environment))));
    assert.equal(m.previews.summary(m.previews.records[b.id]).healthy, true);
    await assert.rejects(m.previews.register({ ...renew, expectedRevision: 3 }), /preview-lease-expired/);
    const fresh = await m.previews.register(request()); assert.notEqual(fresh.id, a.id); assert.notEqual(fresh.lease, a.lease);
    m.previews.records[b.id].expiresAt = new Date(0).toISOString(); await m.tick();
    assert.equal(m.previews.summary(m.previews.records[b.id]).status, 'expired');
    assert(m.routes.some(r => r.environment === main.environment));
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('projection validates project/workspace/lease provenance and failed dependency preparation preserves the last draft', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-preview-proof-'));
  try {
    const h = await harness(root); const m = await h.create(); const p = await m.previews.register(request()); await m.tick();
    const before = m.previews.summary(m.previews.records[p.id]);
    const env = m.config.environments.find(e => e.id === before.environment);
    for (const changes of [{ project: 'foreign' }, { workspace: 'foreign' }, { lease: '0'.repeat(64) }]) {
      await assert.rejects(m.source({ ...env, source: { ...env.source, ...changes } }), /preview-lease-expired/);
    }
    const original = m.run;
    for (const changes of [{ project: 'foreign' }, { workspace: 'foreign' }, { authority: 'local-layerfs' }, { consistency: 'raw-copy' }, { sandbox: 'foreign' }, { branch: 'foreign' }]) {
      m.run = async (program, args, options) => {
        const result = await original(program, args, options);
        if (program !== resolve('/workspace-export')) return result;
        const value = JSON.parse(result); Object.assign(value.manifest, changes); return JSON.stringify(value);
      };
      await assert.rejects(m.source(env), /provenance-mismatch|consistency-proof-required|projection-identity-changed/);
    }
    m.run = original;
    await h.setSource(files('two', template(), '{"new":"dependency"}')); h.failSetup(true); await m.tick();
    const failed = m.previews.summary(m.previews.records[p.id]);
    assert.equal(failed.error, 'setup-failed'); assert.equal(failed.healthy, true); assert.equal(failed.environment, before.environment);
    assert.equal(await readFile(join(root, before.environment, 'workspace/app.mjs'), 'utf8'), 'one');
    await assert.rejects(m.previews.register({ id: p.id, lease: p.lease, expectedRevision: 1, revoke: true, retry: true }), /invalid-preview-request/);
    await assert.rejects(m.previews.register({ id: p.id, lease: p.lease, expectedRevision: 1, ttlSeconds: 300, retry: 'yes' }), /invalid-preview-request/);
    h.failSetup(false);
    await m.tick(); assert.equal(m.previews.summary(m.previews.records[p.id]).status, 'failed', 'same failed revision remains suppressed until explicit retry');
    const retry = { id: p.id, lease: p.lease, expectedRevision: 1, ttlSeconds: 300, retry: true };
    const renewed = await m.previews.register(retry); assert.equal(renewed.revision, 2); assert.equal(renewed.environment, before.environment); assert.equal(renewed.healthy, true);
    await assert.rejects(m.previews.register(retry), /stale-preview-lease/);
    await m.tick(); const recovered = m.previews.summary(m.previews.records[p.id]);
    assert.equal(recovered.id, p.id); assert.equal(recovered.lease, p.lease); assert.equal(recovered.status, 'ready'); assert.equal(recovered.healthy, true);
    assert.notEqual(recovered.environment, before.environment);
    assert.equal(await readFile(join(root, recovered.environment, 'workspace/app.mjs'), 'utf8'), 'two');
    await m.previews.register({ id: p.id, lease: p.lease, expectedRevision: 2, revoke: true });
    assert.equal(h.containers.size, 0);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('preview HTTP API authenticates every path and rejects caller-selected command/path overrides', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-preview-api-'));
  let service;
  try {
    const listener = createServer(); await new Promise(r => listener.listen(0, '127.0.0.1', r));
    const port = listener.address().port; await new Promise(r => listener.close(r));
    const c = config(root); c.listenPort = port;
    const token = 'a'.repeat(64); await writeFile(c.tokenFile, token);
    service = await serve(c, async () => { throw new Error('workspace-writer-busy'); });
    const origin = `http://127.0.0.1:${port}/v1/environments/previews`;
    assert.equal((await fetch(origin)).status, 401);
    const headers = { authorization: `Bearer ${token}`, 'content-type': 'application/json' };
    let response = await fetch(origin, { method: 'POST', headers, body: JSON.stringify({ ...request(), command: ['/bad'] }) }); assert.equal(response.status, 400);
    response = await fetch(origin, { method: 'POST', headers, body: JSON.stringify(request()) }); assert.equal(response.status, 202);
    const preview = await response.json();
    assert.equal((await fetch(`${origin}/${preview.id}`)).status, 401);
    const read = await (await fetch(`${origin}/${preview.id}`, { headers })).json(); assert.equal(read.lease, preview.lease); assert.equal(read.project, 'demo');
    assert.equal((await fetch(`${origin}/missing`, { headers })).status, 404);
    const revoked = await fetch(origin, { method: 'POST', headers, body: JSON.stringify({ id: preview.id, lease: preview.lease, expectedRevision: 1, revoke: true }) });
    assert.equal(revoked.status, 200); assert.equal((await revoked.json()).status, 'revoked');
  } finally { if (service) await service.close(); await rm(root, { recursive: true, force: true }); }
});

test('expiration withdraws routes while an export is in flight and prevents stale activation', { skip: process.platform === 'win32', timeout: 20000 }, async t => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-preview-expiry-'));
  let m; let release; let runningTick;
  t.signal.addEventListener('abort', () => release?.(), { once: true });
  try {
    const h = await harness(root); m = await h.create();
    const p = await m.previews.register(request()); await m.tick();
    assert.equal(m.routes.length, 1);
    const original = m.run; let entered;
    const blocked = new Promise(r => { release = r; }); const started = new Promise(r => { entered = r; });
    m.run = async (program, args, options) => { if (program === resolve('/workspace-export')) { entered(); await blocked; } return original(program, args, options); };
    runningTick = m.tick(); await started;
    const record = m.previews.records[p.id]; record.expiresAt = new Date(Date.now() + 30).toISOString(); m.previews.arm(record);
    const deadline = Date.now() + 10000;
    while (m.routes.length && Date.now() < deadline) await new Promise(r => setTimeout(r, 10));
    assert.equal(m.routes.length, 0); assert.equal(m.previews.summary(record).healthy, false);
    release(); await runningTick; await m.previews.operations;
    assert.equal(h.containers.size, 0); assert.equal(m.routes.length, 0);
  } finally {
    // Assertions and test cancellation must release the intentional blocked read
    // before close() waits for the preview operation queue.
    release?.(); await runningTick?.catch(() => {});
    await m?.previews.close(); await rm(root, { recursive: true, force: true });
  }
});

test('enabling a draft exporter preserves an existing accepted-main host binding', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-preview-main-'));
  try {
    const h = await harness(root); const m = await h.create();
    delete m.config.bootstrap.hosts[0].workspaceCommand;
    await m.projects.register({ host: 'node', project: 'demo' }); await m.tick();
    const before = m.projects.catalog()[0]; assert.equal(before.healthy, true);
    m.config.bootstrap.hosts[0].workspaceCommand = [resolve('/workspace-export')]; await m.tick();
    const after = m.projects.catalog()[0]; assert.equal(after.status, 'ready'); assert.equal(after.environment, before.environment);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('explicit ingress ports reach status URLs and the framework public URL', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-preview-port-'));
  let m;
  try {
    const h = await harness(root); m = await h.create(); m.config.bootstrap.urlPort = 18443;
    const p = await m.previews.register(request()); await m.tick();
    const ready = m.previews.summary(m.previews.records[p.id]);
    assert.equal(new URL(ready.urls[0]).port, '18443');
    const start = h.calls.find(c => c.includes('--detach'));
    assert(start.includes(`ENGINEBOX_PUBLIC_URL=${ready.urls[0]}`));
    for (const invalid of [0, -1, 65536, 1.5, '8443']) assert.throws(() => validateConfig({ ...m.config, bootstrap: { ...m.config.bootstrap, urlPort: invalid } }), /invalid-public-url-port/);
  } finally { await m?.previews.close(); await rm(root, { recursive: true, force: true }); }
});


test('an early lease timer re-arms until the current wall-clock deadline', async t => {
  const previews = new WorkspacePreviews({ config: { stateDir: resolve('/unused-preview-timer-test') } });
  const p = { id: 'timer-proof', expiresAt: new Date(Date.now() + 60000).toISOString() };
  const callbacks = []; let withdrawn = 0; let cleaned = 0;
  previews.withdraw = async () => { withdrawn++; }; previews.cleanup = async () => { cleaned++; };
  // Invoke the timer before wall-clock expiry, then after expiry. No real wait,
  // host filesystem access or Docker process is involved in this clock test.
  const scheduled = t.mock.method(globalThis, 'setTimeout', callback => { callbacks.push(callback); return { unref() {} }; });
  const cleared = t.mock.method(globalThis, 'clearTimeout', () => {});
  try {
    previews.arm(p); assert.equal(callbacks.length, 1);
    callbacks.shift()(); assert.equal(callbacks.length, 1); assert.equal(withdrawn, 0);
    p.expiresAt = new Date(0).toISOString(); callbacks.shift()();
  } finally { scheduled.mock.restore(); cleared.mock.restore(); }
  await Promise.all([...previews.expirations]); await previews.close();
  assert.equal(withdrawn, 1); assert.equal(cleaned, 1);
});
