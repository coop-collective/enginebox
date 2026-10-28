import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, rm, chmod, symlink } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { validateConfig } from '../src/config.mjs';
import { assertAdmission } from '../src/admission.mjs';
import { Environments } from '../src/runtime.mjs';
import { admissionFixture } from './admission-fixture.mjs';

const config = root => ({ schema: 1, stateDir: root, tokenFile: join(root, 'api-token'), listenPort: 18790, baseDomains: ['dev.example.com'], httpEntrypoint: 'web', httpTls: false, basicAuthUsers: [], transportEntrypoints: [], pollSeconds: 2, environments: [], bootstrap: { hosts: [{ id: 'node', command: ['/export'], workspaceCommand: ['/workspace-export'] }], templates: [], portRange: [21000, 21100], domain: 'dev.example.com', access: 'public', maxCpu: 2, maxMemoryMiB: 2048, maxProjects: 4 } });

test('admission accepts only strict operator config and HTTPS or literal loopback HTTP', () => {
  const c = config('/tmp/unused'); validateConfig(c);
  for (const controller of ['http://127.0.0.1:8770', 'http://[::1]:8770', 'https://controller.example']) validateConfig({ ...c, admission: { controller, tokenFile: '/private/token', hostId: 'node' } });
  assert.throws(() => validateConfig({ ...c, admission: {} }));
  for (const value of [null, { controller: 'http://example.com' }, { controller: 'http://localhost' }, { controller: 'file:///private' }, { controller: 'https://user:secret@example.com' }, { controller: 'https://example.com/path' }, { controller: 'https://example.com?query=1' }, { controller: 'https://example.com#fragment' }, { tokenFile: 'relative' }, { hostId: '../node' }, { extra: true }]) {
    assert.throws(() => validateConfig({ ...c, admission: value === null ? null : { controller: 'https://controller.example', tokenFile: '/private/token', hostId: 'node', ...value } }));
  }
});

test('a fence arriving during static preparation does not stop the old writer, withdraw its route or suppress a later retry', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-admission-static-')); const authority = await admissionFixture(root);
  try {
    const c = config(root); c.admission = authority.config;
    const env = { id: 'main', source: { kind: 'git', url: 'https://github.com/example/demo.git', branch: 'main' }, image: `node@sha256:${'a'.repeat(64)}`, command: ['node', 'app.mjs'], cpu: 1, memoryMiB: 128, timeoutSeconds: 5, health: { port: 'web', path: '/health' }, ports: [{ name: 'web', protocol: 'http', container: 3000, host: 18081 }] };
    c.environments.push(env);
    const calls = []; const manager = new Environments(validateConfig(c), async (...args) => { calls.push(args); return ''; });
    await manager.init();
    const revision = 'a'.repeat(40);
    manager.states.main = { revision, healthy: true, status: 'ready' };
    await manager.setRoute({ id: 'web', environment: 'main', service: 'web', hostname: 'main.dev.example.com', access: 'public' });
    const routes = structuredClone(manager.routes);
    manager.source = async () => ({ revision: 'b'.repeat(40) });
    manager.prepare = async () => authority.set({ allowed: false });
    manager.inspect = async () => ({ State: { Running: true }, Config: { Labels: { 'enginebox.environment.revision': revision } } });
    manager.healthy = async () => true;
    await manager.reconcile(env);
    assert.equal(manager.states.main.status, 'admission-blocked'); assert.equal(manager.states.main.healthy, true);
    assert.equal(manager.states.main.revision, revision); assert.equal(manager.states.main.failedRevision, undefined);
    assert.deepEqual(manager.routes, routes); assert.deepEqual(calls, []);
  } finally { await authority.close(); await rm(root, { recursive: true, force: true }); }
});

test('owner admission is uncached, bounded and fails closed on denial, malformed data, redirects and unavailable authority', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-admission-http-')); const authority = await admissionFixture(root);
  try {
    const c = { admission: authority.config };
    await assertAdmission(c);
    authority.set({ allowed: false }); await assert.rejects(assertAdmission(c), /node-admission-denied/);
    for (const mode of [{ status: 401, raw: authority.token }, { status: 503 }, { status: 302, location: '/credential-leak' }, { type: 'text/html', raw: '{}' }, { raw: 'invalid' }, { raw: 'x'.repeat(4097) }, { body: { host_id: 'other', allowed: true } }, { body: { host_id: 'node', allowed: 'true' } }, { body: { host_id: 'node', allowed: true, unexpected: true } }, { body: [] }, { hang: true }]) {
      authority.set(mode);
      await assert.rejects(assertAdmission(c), error => error.message === 'node-admission-unavailable' && !error.message.includes(authority.token));
    }
    assert(authority.requests.every(path => path === '/v1/hosts/node/admission'));
    authority.set({ allowed: true }); await assertAdmission(c);
    await chmod(authority.config.tokenFile, 0o644); await assert.rejects(assertAdmission(c), /node-admission-unavailable/);
    await chmod(authority.config.tokenFile, 0o600);
    await symlink(authority.config.tokenFile, join(root, 'linked-token'));
    await assert.rejects(assertAdmission({ admission: { ...authority.config, tokenFile: join(root, 'linked-token') } }), /node-admission-unavailable/);
    await authority.close(); await assert.rejects(assertAdmission(c), /node-admission-unavailable/);
    await assertAdmission({});
  } finally { await authority.close(); await rm(root, { recursive: true, force: true }); }
});

test('fencing blocks prepare, start, Docker exec and new or retried project/preview registration before mutation', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-admission-actions-')); const authority = await admissionFixture(root);
  try {
    const calls = []; const manager = new Environments(validateConfig({ ...config(root), admission: authority.config }), async (...args) => { calls.push(args); return ''; });
    await manager.init(); authority.set({ allowed: false });
    const env = { id: 'demo', source: { kind: 'git' } };
    for (const action of [() => manager.start(env, 'a'.repeat(64)), () => manager.prepare(env, {}), () => manager.source(env), () => manager.docker(['exec', 'existing', 'new-command']), () => manager.projects.register({ host: 'node', project: 'demo' }), () => manager.previews.register({ host: 'node', project: 'demo', workspace: 'draft', task: 'task', ttlSeconds: 60 })]) await assert.rejects(action(), /node-admission-denied/);
    assert.deepEqual(calls, []); assert.deepEqual(manager.projects.records, {}); assert.deepEqual(manager.previews.records, {});
    authority.set({ allowed: true });
    const project = await manager.projects.register({ host: 'node', project: 'demo' });
    const preview = await manager.previews.register({ host: 'node', project: 'demo', workspace: 'draft', task: 'task', ttlSeconds: 60 });
    const before = structuredClone(manager.previews.records[preview.id]);
    authority.set({ allowed: false });
    await assert.rejects(manager.projects.register({ host: 'node', project: 'demo', retry: true }), /node-admission-denied/);
    await assert.rejects(manager.previews.register({ id: preview.id, lease: preview.lease, expectedRevision: preview.revision, ttlSeconds: 60, retry: true }), /node-admission-denied/);
    assert.deepEqual(manager.previews.records[preview.id], before);
    assert.equal(manager.projects.records[project.id].retry, undefined);
    assert(!JSON.stringify(manager.catalog()).includes(authority.token));
    const revoked = await manager.previews.register({ id: preview.id, lease: preview.lease, expectedRevision: preview.revision, revoke: true });
    assert.equal(revoked.status, 'revoked');
    await manager.previews.close();
  } finally { await authority.close(); await rm(root, { recursive: true, force: true }); }
});
