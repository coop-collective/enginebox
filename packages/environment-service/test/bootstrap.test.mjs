import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm, realpath, symlink } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, dirname, resolve } from 'node:path';
import { Environments } from '../src/runtime.mjs';
import { validateConfig } from '../src/config.mjs';
import { digest, resolveProject } from '../src/project-template.mjs';
import { serve } from '../src/main.mjs';
import { createServer } from 'node:net';
import { admissionFixture } from './admission-fixture.mjs';

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
  return { schema: 1, stateDir: root, tokenFile: join(root, 'token'), listenPort: 18790, baseDomains: ['dev.example.com'], httpEntrypoint: 'web', httpTls: false, basicAuthUsers: [], transportEntrypoints: [], pollSeconds: 2, environments: [], bootstrap: { hosts: [{ id: 'node', command: [resolve('/export')] }], templates: [], portRange: [21000, 21100], domain: 'dev.example.com', access: 'public', maxCpu: 2, maxMemoryMiB: 2048, maxProjects: 4 } };
}

async function harness(root) {
  let selected = await snapshot(root, files()); const calls = []; const containers = new Map(); const images = new Set();
  let setupFailure = false; let startFailure = false; let fetchFailure = false; let afterPrepare = () => {};
  const run = async (program, args) => {
    if (program === resolve('/export')) {
      if (fetchFailure) throw new Error('export-unavailable');
      assert.equal(args.length, 1);
      return JSON.stringify({ ...selected, manifest: { ...selected.manifest, project: args[0] } });
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
        await mkdir(join(directory, 'node_modules')); await writeFile(join(directory, 'node_modules/installed'), 'installed'); afterPrepare(); return '';
      }
      const dataMount = args.find(v => v.includes(',dst=/data'));
      assert(![...containers.values()].some(c => c.State.Running && c.dataMount === dataMount), 'old and new runtimes must never write shared data concurrently');
      containers.set(name, { State: { Running: !startFailure }, Config: { Labels: labels }, dataMount });
      startFailure = false; return name;
    }
    throw new Error(`Unexpected fake Docker command: ${args[0]}`);
  };
  const create = async admission => {
    const manager = new Environments(validateConfig({ ...config(root), ...(admission ? { admission } : {}) }), run);
    manager.healthy = async env => !!(await manager.inspect(env))?.State.Running;
    manager.ready = async env => { if (!await manager.healthy(env)) throw new Error('service-exited'); };
    await manager.init(); return manager;
  };
  return { create, calls, containers, setSource: async (contents) => { selected = await snapshot(root, contents); }, failSetup: value => { setupFailure = value; }, failStart: () => { startFailure = true; }, failFetch: value => { fetchFailure = value; }, afterPrepare: callback => { afterPrepare = callback; } };
}

test('managed admission adopts legacy state and fences hot updates and interrupted replacement without changing running containers or routes', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-project-admission-')); const authority = await admissionFixture(root);
  try {
    const h = await harness(root); let m = await h.create();
    await m.projects.register({ host: 'node', project: 'demo' }); await m.tick();
    const before = m.projects.catalog()[0]; const active = m.config.environments[0];
    const containers = [...h.containers]; const routes = structuredClone(m.routes);
    const gateway = await readFile(m.dynamicFile, 'utf8');
    // Match a pre-preview production state: no preview registry, admission or
    // workspace exporter. Adding managed enrollment must not alter its recipe.
    await rm(join(root, 'previews.json'), { force: true });
    authority.set({ allowed: false });
    const operations = () => h.calls.filter(a => ['run', 'stop', 'rm', 'pull', 'exec'].includes(a[0]));
    const initialOperations = operations().length;
    m = await h.create(authority.config);
    assert.equal(m.projects.catalog()[0].healthy, true);
    await h.setSource(files('two')); await m.tick();
    assert.equal(m.projects.catalog()[0].status, 'admission-blocked');
    assert.equal(m.projects.catalog()[0].healthy, true);
    assert.equal(await readFile(join(root, active.id, 'workspace/app.mjs'), 'utf8'), 'one');
    assert.deepEqual([...h.containers], containers); assert.deepEqual(m.routes, routes);
    assert.equal(await readFile(m.dynamicFile, 'utf8'), gateway); assert.equal(operations().length, initialOperations);
    assert(!JSON.stringify(m.catalog()).includes(authority.token)); assert(!JSON.stringify(h.calls).includes(authority.token));
    authority.set({ allowed: true }); await m.tick();
    assert.equal(await readFile(join(root, active.id, 'workspace/app.mjs'), 'utf8'), 'two');
    assert.deepEqual([...h.containers], containers); assert.equal(operations().length, initialOperations);
    await h.setSource(files('three', template(), '{"changed":true}'));
    h.afterPrepare(() => authority.set({ allowed: false }));
    await m.tick();
    const p = m.projects.records[before.id];
    assert.equal(p.status, 'admission-blocked'); assert(p.pending);
    assert.equal(m.projects.catalog()[0].healthy, true); assert.deepEqual([...h.containers], containers); assert.deepEqual(m.routes, routes);
    assert.equal(await readFile(join(root, active.id, 'workspace/app.mjs'), 'utf8'), 'two');
    // Recovery must not stop a pending process or restart the old writer while
    // its machine is fenced, even with an interrupted handoff receipt.
    p.phase = 'switching'; await m.projects.save();
    const beforeRecovery = operations().length;
    m = await h.create(authority.config); await m.tick();
    assert.equal(operations().length, beforeRecovery); assert.deepEqual([...h.containers], containers);
    assert.deepEqual(m.routes, routes); assert.equal(await readFile(m.dynamicFile, 'utf8'), gateway);
    assert.equal(m.projects.records[before.id].phase, 'switching');
    h.afterPrepare(() => {}); authority.set({ allowed: true }); await m.tick();
    assert.equal(m.projects.catalog()[0].status, 'ready');
    assert.notEqual(m.projects.catalog()[0].environment, before.environment);
    assert.equal([...h.containers.values()].filter(c => c.State.Running).length, 1);
  } finally { await authority.close(); await rm(root, { recursive: true, force: true }); }
});

test('project declarations resolve local or registered templates and bind install inputs, never host paths', async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-project-schema-'));
  try {
    const s = await snapshot(root, files()); const resolved = await resolveProject(s);
    assert.equal(resolved.template.id, 'web'); assert.equal(resolved.inputs['package.json'], digest('{}'));
    assert.equal(await resolveProject(await snapshot(root, { 'app.mjs': 'plain project' })), null);
    const catalogFiles = files(); catalogFiles['.enginebox/config.json'] = JSON.stringify({ schema_version: 1, dev: { template: 'web@1' } });
    assert.equal((await resolveProject(await snapshot(root, catalogFiles), [template()])).template.id, 'web');
    await assert.rejects(resolveProject(await snapshot(root, catalogFiles)), /not-registered/);
    for (const change of [{ image: 'node:latest' }, { kind: 'ci' }, { host: '/etc' }, { env: { HOME: '/root' } }, { cwd: '../escape' }, { ports: [{ name: 'web', protocol: 'http', port: 3000, host: 1 }] }, { installInputs: ['missing.lock'] }]) {
      await assert.rejects(resolveProject(await snapshot(root, files('one', { ...template(), ...change }))));
    }
    const escaping = files(); escaping['.enginebox/config.json'] = JSON.stringify({ schema_version: 1, dev: { template: './templates/../../escape.json' } });
    await assert.rejects(resolveProject(await snapshot(root, escaping)), /invalid-template-path/);
    await writeFile(join(s.sourceRoot, '.enginebox/templates/web.json'), 'tampered');
    await assert.rejects(resolveProject(s), /file-mismatch|integrity-mismatch/);
    if (process.platform !== 'win32') {
      const link = await snapshot(root, files()); const path = join(link.sourceRoot, '.enginebox/templates/web.json');
      await rm(path); await symlink(join(s.sourceRoot, '.enginebox/templates/web.json'), path);
      await assert.rejects(resolveProject(link), /symlink-refused/);
    }
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('repo bootstrap is idempotent; source-only promotion and controller recovery reuse the running process and dependencies', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-project-live-'));
  try {
    const h = await harness(root); let m = await h.create();
    const p = await m.projects.register({ host: 'node', project: 'demo' });
    assert.equal((await m.projects.register({ host: 'node', project: 'demo' })).id, p.id);
    await m.tick(); const ready = m.projects.catalog()[0]; assert.equal(ready.status, 'ready'); assert.equal(ready.healthy, true); assert.equal(ready.urls.length, 1);
    const env = m.config.environments[0]; const data = join(root, p.id, 'data', 'state'); await writeFile(data, 'retained');
    const runCount = () => h.calls.filter(c => c[0] === 'run').length;
    const calls = runCount();
    await h.setSource(files('two')); await m.tick();
    assert.equal(await readFile(join(root, env.id, 'workspace', 'app.mjs'), 'utf8'), 'two'); assert.equal(runCount(), calls);
    assert.equal(await readFile(join(root, env.id, 'workspace', 'node_modules/installed'), 'utf8'), 'installed');
    h.failFetch(true); await m.tick(); assert.equal(m.projects.catalog()[0].healthy, true);
    m = await h.create(); assert.equal(m.projects.catalog()[0].healthy, true);
    h.failFetch(false); await m.tick(); assert.equal(runCount(), calls); assert.equal(await readFile(data, 'utf8'), 'retained');
    assert.equal(h.calls.filter(c => c[0] === 'pull').length, 1);
    const startup = h.calls.find(c => c.includes('--detach'));
    assert(startup.some(v => v.startsWith('ENGINEBOX_PUBLIC_HOST=')));
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('dependency updates prepare separately, preserve data, roll back failed readiness and suppress repeat failures', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-project-deps-'));
  try {
    const h = await harness(root); let m = await h.create(); await m.projects.register({ host: 'node', project: 'demo' }); await m.tick();
    const before = m.projects.catalog()[0]; const old = before.environment;
    await m.setRoute({ id: 'custom-alias', environment: old, service: 'web', hostname: 'custom.dev.example.com', access: 'public' });
    await writeFile(join(root, before.id, 'data', 'state'), 'keep');
    await h.setSource(files('two', template(), '{"changed":true}')); h.failSetup(true); await m.tick();
    assert.equal(m.projects.catalog()[0].healthy, true); assert.equal(m.projects.catalog()[0].error, 'setup-failed');
    assert.equal(m.projects.catalog()[0].environment, old); assert.equal(await readFile(join(root, old, 'workspace/app.mjs'), 'utf8'), 'one');
    const count = h.calls.length; await m.tick(); assert.equal(h.calls.length, count);
    h.failSetup(false); h.failStart(); await m.projects.register({ host: 'node', project: 'demo', retry: true }); await m.tick();
    assert.equal(m.projects.catalog()[0].healthy, true); assert.equal(m.projects.catalog()[0].environment, old); assert.equal(m.projects.catalog()[0].error, 'service-exited');
    m = await h.create(); await m.projects.register({ host: 'node', project: 'demo', retry: true }); await m.tick();
    assert.equal(m.projects.catalog()[0].status, 'ready'); assert.notEqual(m.projects.catalog()[0].environment, old);
    assert.equal(await readFile(join(root, before.id, 'data/state'), 'utf8'), 'keep');
    assert.equal(h.calls.filter(c => c[0] === 'pull').length, 1);
    assert.equal(m.routes[0].environment, m.projects.catalog()[0].environment);
    assert.equal(m.routes.find(r => r.id === 'custom-alias').environment, m.projects.catalog()[0].environment);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('resource policy, missing configuration and unavailable templates are visible without starting a process', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-project-policy-'));
  try {
    const h = await harness(root); const m = await h.create();
    await assert.rejects(m.projects.register({ host: 'unknown', project: 'demo' }), /unknown-project-host/);
    await assert.rejects(m.projects.register({ host: 'node', project: '../demo' }), /invalid-project-request/);
    await m.projects.register({ host: 'node', project: 'demo' });
    await h.setSource({ 'app.mjs': 'no config' }); await m.tick(); assert.equal(m.projects.catalog()[0].status, 'not-configured');
    await h.setSource(files('one', { ...template(), cpu: 4 })); await m.tick(); assert.equal(m.projects.catalog()[0].error, 'project-resource-policy-exceeded');
    assert.equal(h.calls.filter(c => c[0] === 'run').length, 0);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('different projects reuse a cached runtime image while retaining separate data, ports and setup', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-project-shared-image-'));
  try {
    const h = await harness(root); const m = await h.create();
    await m.projects.register({ host: 'node', project: 'demo' });
    await m.projects.register({ host: 'node', project: 'second' });
    await m.tick();
    const projects = m.projects.catalog(); assert.equal(projects.length, 2); assert(projects.every(p => p.status === 'ready'));
    assert.notEqual(projects[0].urls[0], projects[1].urls[0]);
    assert.equal(new Set(m.config.environments.flatMap(e => e.ports.map(p => p.host))).size, 2);
    assert.equal(new Set([...h.containers.values()].map(c => c.dataMount)).size, 2);
    assert.equal(h.calls.filter(c => c[0] === 'pull').length, 1);
    assert.equal(h.calls.filter(c => c[0] === 'run' && !c.includes('--detach')).length, 2);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('interrupted handoff stops the replacement before recovering the original data writer and route', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-project-crash-'));
  try {
    const h = await harness(root); let m = await h.create();
    await m.projects.register({ host: 'node', project: 'demo' }); await m.tick();
    const before = m.projects.catalog()[0];
    await h.setSource(files('two', template(), '{"updated":true}'));
    // Model a process crash after replacement readiness but before route/owner commit.
    m.projects.routes = async () => { throw new Error('crash'); };
    m.projects.rollback = async () => { throw new Error('crash'); };
    await m.tick(); assert.equal(m.projects.catalog()[0].error, 'project-recovery-required');
    m = await h.create();
    const restored = m.projects.catalog()[0];
    assert.equal(restored.environment, before.environment); assert.equal(restored.healthy, true);
    assert.deepEqual(restored.urls, before.urls);
    assert.equal([...h.containers.values()].filter(c => c.State.Running).length, 1);
    assert.equal(m.projects.records[before.id].pending, undefined);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('project registration API is authenticated, durable and excludes host-command overrides', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-project-api-')); let service;
  try {
    const listener = createServer(); await new Promise(r => listener.listen(0, '127.0.0.1', r));
    const port = listener.address().port; await new Promise(r => listener.close(r));
    const c = config(root); c.listenPort = port; await writeFile(c.tokenFile, 't'.repeat(32));
    service = await serve(c, async () => { throw new Error('export-unavailable'); });
    const url = `http://127.0.0.1:${port}/v1/environments/projects`;
    assert.equal((await fetch(url)).status, 401);
    const request = body => fetch(url, { method: 'POST', headers: { authorization: `Bearer ${'t'.repeat(32)}`, 'content-type': 'application/json' }, body: JSON.stringify(body) });
    assert.equal((await request({ host: 'node', project: 'demo', command: ['/bin/sh'] })).status, 400);
    const first = await request({ host: 'node', project: 'demo' }); assert.equal(first.status, 202);
    const receipt = await first.json();
    assert.equal((await (await request({ host: 'node', project: 'demo' })).json()).id, receipt.id);
    const stored = JSON.parse(await readFile(join(root, 'projects.json'), 'utf8')); assert.equal(stored[receipt.id].project, 'demo');
    assert(!JSON.stringify(receipt).includes('/export')); assert(!JSON.stringify(receipt).includes('token'));
  } finally { await service?.close(); await rm(root, { recursive: true, force: true }); }
});


test('workspace metrics measure verified copying, preparation reuse and readiness without exposing commands', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-project-metrics-'));
  try {
    const h = await harness(root); let m = await h.create();
    await m.projects.register({ host: 'node', project: 'demo' }); await m.tick();
    const first = m.projects.catalog()[0]; const env = m.config.environments[0];
    const measured = first.metrics;
    assert.equal(measured.preparation.status, 'succeeded');
    assert.equal(measured.preparation.revision, first.revision);
    assert.equal(measured.preparation.source_bytes, Object.values(files()).reduce((sum, value) => sum + Buffer.byteLength(value), 0));
    assert.equal(measured.preparation_reuse.status, 'reused');
    for (const stage of ['preparation', 'preparation_reuse', 'startup', 'source_update']) {
      assert(Number.isFinite(measured[stage].elapsed_ms) && measured[stage].elapsed_ms >= 0);
      assert(Number.isFinite(Date.parse(measured[stage].observed_at)));
      assert.equal(measured[stage].schema, 1);
    }
    assert(measured.preparation.source_copy_ms >= 0 && measured.preparation.prepare_ms >= 0);
    assert.equal(measured.startup.status, 'succeeded');
    const copy = structuredClone(measured.preparation); const startup = structuredClone(measured.startup);
    await h.setSource(files('changed source')); await m.tick();
    const updated = m.projects.catalog()[0];
    assert.deepEqual(updated.metrics.preparation, copy, 'live source updates must not invent another dependency install');
    assert.deepEqual(updated.metrics.startup, startup, 'a surviving dev process has no new startup sample');
    assert.equal(updated.metrics.source_update.revision, updated.revision);
    assert.equal(updated.metrics.source_update.written_files, 1);
    assert.equal(updated.metrics.source_update.source_bytes, Object.values(files('changed source')).reduce((sum, value) => sum + Buffer.byteLength(value), 0));
    assert.equal(m.catalog().environments.find(e => e.id === env.id).source.project, 'demo');
    assert(!JSON.stringify(m.catalog()).includes('/export'));
    assert(!JSON.stringify(measured).includes('app.mjs'));
    m = await h.create();
    assert.deepEqual(m.projects.catalog()[0].metrics, updated.metrics, 'measured samples survive controller recovery');
    // Failure samples remain failures and cannot masquerade as a cache hit or a successful command.
    await h.setSource(files('next', template(), '{"dependenciesChanged":true}'));
    h.failSetup(true); await m.tick();
    const project = m.projects.records[first.id];
    const failed = m.states[project.pending.env.id].metrics.preparation;
    assert.equal(failed.status, 'failed'); assert(failed.prepare_ms >= 0);
    assert.notEqual(failed.revision, first.revision);
  } finally { await rm(root, { recursive: true, force: true }); }
});
