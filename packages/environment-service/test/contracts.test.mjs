import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, mkdir, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { validateConfig, validateRoutes, gatewayConfig } from '../src/config.mjs';
import { atomic, Environments, command } from '../src/runtime.mjs';

export function fixture(stateDir = '/tmp/enginebox-contract') {
  return { schema: 1, stateDir, tokenFile: '/tmp/token', listenPort: 18790, baseDomains: ['dev.example.com'], httpEntrypoint: 'web', httpTls: false, basicAuthUsers: ['owner:$2y$05$example'], transportEntrypoints: [{ name: 'tcp-one', protocol: 'tcp', port: 19110, public: false }, { name: 'udp-one', protocol: 'udp', port: 19111, public: false }, { name: 'tls-one', protocol: 'tcp', port: 19112, public: true }], pollSeconds: 2,
    environments: [{ id: 'main', source: { kind: 'git', url: 'https://github.com/example/demo.git', branch: 'main' }, image: `node@sha256:${'a'.repeat(64)}`, command: ['node', 'app.mjs'], cpu: 1, memoryMiB: 128, timeoutSeconds: 5, health: { port: 'web', path: '/health' }, ports: [{ name: 'web', protocol: 'http', container: 3000, host: 18081 }, { name: 'stream', protocol: 'tcp', container: 3001, host: 18082 }, { name: 'packets', protocol: 'udp', container: 3002, host: 18083 }, { name: 'tls', protocol: 'tls', container: 3003, host: 18084 }] }] };
}
const routes = [
  { id: 'web', environment: 'main', service: 'web', hostname: 'app.dev.example.com', access: 'authenticated' },
  { id: 'tcp', environment: 'main', service: 'stream', entrypoint: 'tcp-one', access: 'private' },
  { id: 'udp', environment: 'main', service: 'packets', entrypoint: 'udp-one', access: 'private' },
  { id: 'tls', environment: 'main', service: 'tls', hostname: 'tls.dev.example.com', entrypoint: 'tls-one', access: 'public' },
];
test('routes compile to distinct HTTP/WS, TCP, UDP and TLS passthrough services', () => {
  const config = validateConfig(fixture());
  const result = gatewayConfig(routes, config, new Set(['main']));
  assert.deepEqual(result.http.routers.web.middlewares, ['operator']);
  assert.equal(result.http.middlewares.operator.basicAuth.removeHeader, true);
  assert.equal(result.tcp.routers.tcp.rule, 'HostSNI(`*`)');
  assert.equal(result.tcp.routers.tls.tls.passthrough, true);
  assert.deepEqual(result.udp.services.udp.loadBalancer.servers, [{ address: '127.0.0.1:18083' }]);
  assert.equal(Object.keys(gatewayConfig(routes, config, new Set()).http.routers).length, 0);
});
test('domains, listener ownership, access defaults and target identity cannot be bypassed', () => {
  const config = fixture();
  for (const mutation of [{ hostname: 'app.dev.example.com.attacker.com' }, { hostname: 'other.com' }, { hostname: 'a`.dev.example.com' }, { access: 'private' }, { access: undefined }, { service: 'missing' }, { target: 'http://169.254.169.254' }])
    assert.throws(() => validateRoutes([{ ...routes[0], ...mutation }], config));
  assert.throws(() => validateRoutes([routes[0], { ...routes[0], id: 'duplicate' }], config), /route-conflict/);
  assert.throws(() => validateRoutes([{ ...routes[1], hostname: 'app.dev.example.com' }], config), /no-hostname/);
  assert.throws(() => validateRoutes([{ ...routes[1], access: 'public' }], config), /exposure-mismatch/);
  assert.throws(() => validateRoutes([{ ...routes[1], entrypoint: 'udp-one' }], config), /unprovisioned/);
  assert.throws(() => validateRoutes([routes[0]], { ...config, basicAuthUsers: [] }), /auth-not-configured/);
});
test('pinned images, bounded resource claims and unique loopback ports are mandatory', () => {
  for (const mutation of [{ image: 'node:latest' }, { cpu: 0 }, { memoryMiB: 999999 }, { command: ['node\0bad'] }, { source: { kind: 'git', url: 'https://token@github.com/org/repo', branch: 'main' } }]) {
    const config = fixture(); Object.assign(config.environments[0], mutation); assert.throws(() => validateConfig(config));
  }
  const config = fixture(); config.environments[0].ports[1].host = 18081;
  assert.throws(() => validateConfig(config), /duplicate-port/);
});
test('file watching requires verified storage and isolated nonoverlapping writable directories', () => {
  const config = fixture(); const env = config.environments[0];
  env.updateMode = 'files';
  assert.throws(() => validateConfig(config), /files-update-requires-layerfs/);
  env.source = { kind: 'layerfs', project: 'app', command: ['/opt/export-app'] };
  for (const dirs of [['../escape'], ['.git'], ['cache', 'cache/nested'], ['cache', 'cache'], ['cache:rw'], ['cache,nodev'], ['/absolute']]) {
    env.writableDirs = dirs; assert.throws(() => validateConfig(config));
  }
  env.writableDirs = ['.wrangler', 'node_modules/.vite']; validateConfig(config);
  env.updateMode = 'restart'; assert.throws(() => validateConfig(config), /require-files-mode/);
  env.updateMode = 'unknown'; assert.throws(() => validateConfig(config), /invalid-update-mode/);
});
test('dev processes get a stable read-only source, bounded writable caches and a usable private home', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-watch-mounts-'));
  try {
    const config = fixture(root); const env = config.environments[0]; env.updateMode = 'files'; env.writableDirs = ['.wrangler', 'node_modules/.mf'];
    const calls = []; const manager = new Environments(config, async (_program, args) => { calls.push(args); return ''; });
    await manager.start(env, 'a'.repeat(64));
    const argv = calls.find(args => args[0] === 'run');
    assert(argv.includes(`type=bind,src=${join(root, 'main/workspace')},dst=/app,readonly`));
    assert(argv.includes(`type=bind,src=${join(root, 'main/data')},dst=/data`));
    assert(argv.includes(`/home/app:rw,nosuid,nodev,size=16m,mode=700,uid=${process.getuid()},gid=${process.getgid()}`));
    for (const path of env.writableDirs) assert(argv.includes(`/app/${path}:rw,nosuid,nodev,size=256m,mode=700,uid=${process.getuid()},gid=${process.getgid()}`));
    assert(argv.includes('127.0.0.1:18081:3000/tcp'));
    assert(argv.some(value => value.startsWith('enginebox.environment.recipe=')));
  } finally { await rm(root, { recursive: true, force: true }); }
});
test('source follows only main, checks out exact commits, and preserves prior code and data', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-environment-test-'));
  try {
    const repo = join(root, 'origin'); await mkdir(repo);
    await command('git', ['init', '-b', 'main', repo]);
    const git = args => command('git', ['-C', repo, '-c', 'user.name=Test', '-c', 'user.email=test@example.invalid', ...args]);
    await atomic(join(repo, 'code.json'), { version: 1 }); await git(['add', '.']); await git(['commit', '-m', 'first']);
    const config = fixture(join(root, 'state')); config.environments[0].source.url = repo;
    const manager = new Environments(config); await manager.init(); const env = config.environments[0];
    const first = await manager.source(env); await manager.prepare(env, first);
    await mkdir(join(first.root, 'data')); await atomic(join(first.root, 'data', 'database.json'), { count: 42 });
    await git(['checkout', '-b', 'agent']); await atomic(join(repo, 'code.json'), { version: 2 }); await git(['add', '.']); await git(['commit', '-m', 'agent work']);
    assert.equal((await manager.source(env)).revision, first.revision);
    await git(['checkout', 'main']); await git(['merge', '--ff-only', 'agent']);
    const second = await manager.source(env); assert.notEqual(second.revision, first.revision); await manager.prepare(env, second);
    assert.equal(JSON.parse(await readFile(join(first.root, first.revision, 'code.json'))).version, 1);
    assert.equal(JSON.parse(await readFile(join(second.root, second.revision, 'code.json'))).version, 2);
    assert.equal(JSON.parse(await readFile(join(first.root, 'data', 'database.json'))).count, 42);
  } finally { await rm(root, { recursive: true, force: true }); }
});
test('failed readiness restores the previous process and suppresses automatic retries', async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-environment-failure-'));
  try {
    const manager = new Environments(fixture(root));
    manager.states.main = { revision: 'a'.repeat(40), healthy: true, status: 'ready' };
    const env = manager.config.environments[0]; const started = [];
    manager.source = async () => ({ revision: 'b'.repeat(40) }); manager.prepare = async () => {};
    manager.inspect = async () => ({ State: { Running: true }, Config: { Labels: { 'enginebox.environment.revision': started.at(-1) ?? 'a'.repeat(40) } } });
    manager.save = manager.render = manager.docker = async () => {};
    manager.start = async (_env, revision) => { started.push(revision); };
    manager.ready = async () => { if (started.at(-1) === 'b'.repeat(40)) throw new Error('service-exited'); };
    manager.healthy = async () => true;
    await manager.reconcile(env);
    assert.deepEqual(started, ['b'.repeat(40), 'a'.repeat(40)]);
    assert.equal(manager.states.main.revision, 'a'.repeat(40));
    assert.equal(manager.states.main.status, 'update-failed'); assert.equal(manager.states.main.healthy, true);
    await manager.reconcile(env); assert.equal(started.length, 2);
    assert.equal(manager.states.main.status, 'update-failed');
    assert.equal(manager.states.main.failedRevision, 'b'.repeat(40));
    assert.equal(manager.states.main.error, 'service-exited');
  } finally { await rm(root, { recursive: true, force: true }); }
});
test('a recovered source read clears transient failure for the healthy active revision', async () => {
  const manager = new Environments(fixture()); const env = manager.config.environments[0];
  const revision = 'a'.repeat(64); let available = false;
  manager.states.main = { revision, desiredRevision: revision, healthy: true, status: 'ready' };
  manager.source = async () => { if (!available) throw new Error('command-failed'); return { revision }; };
  manager.inspect = async () => ({ State: { Running: true }, Config: { Labels: { 'enginebox.environment.revision': revision } } });
  manager.ready = async () => {}; manager.healthy = async () => true;
  manager.start = manager.prepare = async () => assert.fail('Healthy active process must remain running');
  await manager.reconcile(env);
  assert.equal(manager.states.main.status, 'update-failed');
  assert.equal(manager.states.main.healthy, true);
  available = true;
  await manager.reconcile(env);
  assert.equal(manager.states.main.status, 'ready');
  assert.equal(manager.states.main.healthy, true);
  assert.equal(manager.states.main.failedRevision, undefined);
  assert.equal(manager.states.main.error, undefined);
});
test('container actions refuse an unrelated process even if it occupies the configured name', async () => {
  const manager = new Environments(fixture()); const env = manager.config.environments[0];
  const calls = []; manager.docker = async args => { calls.push(args); return args[0] === 'ps' ? manager.container(env) : JSON.stringify([{ Config: { Labels: {} } }]); };
  await assert.rejects(manager.remove(env), /unowned-container/); assert.equal(calls.some(a => a[0] === 'rm'), false);
});
