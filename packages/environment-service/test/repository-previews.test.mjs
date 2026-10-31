import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtemp, mkdir, writeFile, readFile, rm, realpath, chmod } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, dirname, resolve } from 'node:path';
import { Environments } from '../src/runtime.mjs';
import { validateConfig } from '../src/config.mjs';
import { digest } from '../src/project-template.mjs';
import { authHeader, exportBranch, parseArgs } from '../src/repository-export.mjs';

const template = () => ({ schema_version: 1, kind: 'dev', id: 'web', revision: 1, image: `node@sha256:${'a'.repeat(64)}`, prepare: ['install'], command: ['node', 'app.mjs'], installInputs: ['package.json'], ports: [{ name: 'web', protocol: 'http', port: 3000, expose: true }], health: { port: 'web', path: '/health' }, timeoutSeconds: 5 });
const files = (version = 'one') => ({ '.enginebox/config.json': JSON.stringify({ schema_version: 2, dev: { template: './templates/web.json' }, agents: { profiles: { default: { default: true } } } }), '.enginebox/templates/web.json': JSON.stringify(template()), 'package.json': '{}', 'app.mjs': version });
const commit = '1'.repeat(40);

async function snapshot(root, contents, repository, branch) {
  const dir = await mkdtemp(join(root, 'source-')); const entries = [];
  for (const [path, value] of Object.entries(contents)) {
    await mkdir(dirname(join(dir, path)), { recursive: true }); await writeFile(join(dir, path), value);
    entries.push({ path, kind: 'file', size: Buffer.byteLength(value), sha256: digest(value), executable: false });
  }
  const revision = digest(entries);
  return { revision, sourceRoot: await realpath(dir), manifest: { schema: 1, authority: 'repository-branch', repository, branch, commit, revision, treeHash: revision, entries } };
}
function config(root) {
  return { schema: 1, stateDir: root, tokenFile: join(root, 'token'), listenPort: 18790, baseDomains: ['dev.example.com'], httpEntrypoint: 'web', httpTls: false, basicAuthUsers: [], transportEntrypoints: [], pollSeconds: 2, environments: [], bootstrap: { hosts: [{ id: 'node', command: [resolve('/export')], repositoryCommand: [resolve('/repository-export'), '--cache', '/cache'] }], templates: [], portRange: [21000, 21100], domain: 'dev.example.com', access: 'public', maxCpu: 2, maxMemoryMiB: 2048, maxProjects: 4 } };
}
async function harness(root) {
  let contents = files(); const exports = []; const containers = new Map(); const images = new Set();
  const run = async (program, args) => {
    if (program === resolve('/repository-export')) {
      exports.push(args);
      const [repository, branch] = args.slice(-2);
      return JSON.stringify(await snapshot(root, contents, repository, branch));
    }
    if (args[0] === 'image') { if (!images.has(args[2])) throw new Error('command-failed'); return '[]'; }
    if (args[0] === 'pull') { images.add(args[1]); return ''; }
    if (args[0] === 'ps') { const name = args[3].slice('name=^/'.length, -1); return containers.has(name) ? name : ''; }
    if (args[0] === 'inspect') return JSON.stringify([containers.get(args[1])]);
    if (args[0] === 'rm') { containers.delete(args[2]); return ''; }
    if (args[0] === 'stop') { containers.get(args.at(-1)).State.Running = false; return ''; }
    if (args[0] === 'run') {
      const name = args[args.indexOf('--name') + 1];
      const labels = Object.fromEntries(args.flatMap((v, i) => v === '--label' ? [args[i + 1].split('=')] : []));
      if (name.endsWith('-prepare')) {
        const mount = args.find(v => v.startsWith('type=bind,src=') && v.includes(',dst=/app'));
        const directory = mount.slice('type=bind,src='.length).split(',dst=')[0];
        await mkdir(join(directory, 'node_modules')); return '';
      }
      assert(!args.some(v => /token|authorization/i.test(v)), 'no repository credential reaches the container');
      containers.set(name, { State: { Running: true }, Config: { Labels: labels } }); return name;
    }
    throw new Error(`Unexpected fake Docker command: ${args[0]}`);
  };
  const create = async () => {
    const manager = new Environments(validateConfig(config(root)), run);
    manager.healthy = async env => !!(await manager.inspect(env))?.State.Running;
    manager.ready = async env => { if (!await manager.healthy(env)) throw new Error('service-exited'); };
    await manager.init(); return manager;
  };
  return { create, exports, containers, setFiles: value => { contents = value; } };
}
const request = (branch = 'autopilot/feat_a-v2') => ({ host: 'node', repository: 'ideator-apps/site', branch, task: 'feat-a', ttlSeconds: 300 });

test('a repository branch preview exports that branch, routes a draft, follows new commits and revokes cleanly', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-repo-preview-'));
  try {
    const h = await harness(root); let m = await h.create();
    const a = await m.previews.register(request());
    assert.match(a.id, /^r-[a-f0-9]{20}$/); assert.equal(a.repository, 'ideator-apps/site'); assert.equal(a.branch, 'autopilot/feat_a-v2');
    // The same branch asked again is the same draft and lease.
    assert.equal((await m.previews.register({ ...request(), task: 'feat-a-retry' })).id, a.id);
    await m.tick();
    assert.deepEqual(h.exports[0], ['--cache', '/cache', 'ideator-apps/site', 'autopilot/feat_a-v2']);
    let ready = m.previews.summary(m.previews.records[a.id]);
    assert.equal(ready.status, 'ready'); assert.equal(ready.healthy, true); assert.equal(ready.commit, commit);
    assert.match(ready.urls[0], /^http:\/\/r-[a-f0-9]{20}-web\.dev\.example\.com$/);
    assert(!JSON.stringify(m.catalog()).includes(a.lease), 'the general catalog never shows a lease');
    h.setFiles(files('two')); await m.tick();
    const env = m.config.environments.find(e => e.id === ready.environment);
    assert.equal(await readFile(join(root, env.id, 'workspace', 'app.mjs'), 'utf8'), 'two');
    // A forged source naming another branch is refused.
    await assert.rejects(m.source({ ...env, source: { ...env.source, branch: 'main' } }), /preview-lease-expired/);
    m = await h.create(); ready = m.previews.summary(m.previews.records[a.id]); assert.equal(ready.healthy, true);
    const revoked = await m.previews.register({ id: a.id, lease: a.lease, expectedRevision: a.revision, revoke: true });
    assert.equal(revoked.status, 'revoked'); assert.deepEqual(revoked.urls, []);
    assert(!h.containers.has(m.container(env)));
    assert.notEqual((await m.previews.register(request())).id, a.id);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('repository preview requests are validated and need the host repository exporter', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-repo-preview-v-'));
  try {
    const h = await harness(root); const m = await h.create();
    for (const bad of [{ repository: 'no-slash' }, { repository: 'a/b', branch: '../main' }, { branch: 'x..y' }, { extra: true }, { ttlSeconds: 5 }])
      await assert.rejects(m.previews.register({ ...request(), ...bad }));
    m.previews.policy.hosts[0] = { id: "node", command: [resolve("/export")] };
    await assert.rejects(m.previews.register(request()), /repository-preview-not-configured/);
    assert.throws(() => validateConfig({ ...config(root), bootstrap: { ...config(root).bootstrap, hosts: [{ id: 'node', command: [resolve('/export')], repositoryCommand: ['relative'] }] } }), /invalid-repository-export-command/);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('the exporter fetches the branch head, inventories its tree and reuses the export by commit', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-repo-export-'));
  const sh = (cwd, ...args) => execFileSync('git', args, { cwd, env: { ...process.env, GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null', GIT_AUTHOR_NAME: 't', GIT_AUTHOR_EMAIL: 't@example.com', GIT_COMMITTER_NAME: 't', GIT_COMMITTER_EMAIL: 't@example.com' } }).toString().trim();
  try {
    const work = join(root, 'work'); await mkdir(join(work, 'bin'), { recursive: true });
    sh(root, 'init', '-q', '-b', 'main', 'work');
    await writeFile(join(work, 'app.mjs'), 'one'); await writeFile(join(work, 'bin/run'), '#!/bin/sh\n'); await chmod(join(work, 'bin/run'), 0o755);
    execFileSync('ln', ['-s', 'app.mjs', join(work, 'link.mjs')]);
    sh(work, 'add', '.'); sh(work, 'commit', '-q', '-m', 'one'); sh(work, 'checkout', '-q', '-b', 'autopilot/feat_a-v2');
    await writeFile(join(work, 'app.mjs'), 'two'); sh(work, 'commit', '-q', '-am', 'two');
    const head = sh(work, 'rev-parse', 'HEAD');
    sh(root, 'clone', '-q', '--bare', 'work', 'remote.git');
    const remote = `file://${join(root, 'remote.git')}`;
    let minted = 0;
    const options = { controller: 'http://127.0.0.1:8765', tokenFile: '/unused', cache: join(root, 'cache'), repository: 'ideator-apps/site', branch: 'autopilot/feat_a-v2' };
    const mintToken = async () => { minted++; return { token: 'tok', remote, auth: 'bearer' }; };
    const out = await exportBranch(options, { mintToken });
    assert.equal(out.manifest.commit, head); assert.equal(out.manifest.authority, 'repository-branch');
    assert.equal(out.manifest.revision, out.revision); assert.equal(out.manifest.treeHash, digest(JSON.stringify(out.manifest.entries)));
    const byPath = Object.fromEntries(out.manifest.entries.map(e => [e.path, e]));
    assert.equal(byPath['app.mjs'].sha256, digest('two')); assert.equal(byPath['bin/run'].executable, true);
    assert.deepEqual(byPath['link.mjs'], { path: 'link.mjs', kind: 'symlink', target: 'app.mjs' });
    assert.equal(await readFile(join(out.sourceRoot, 'app.mjs'), 'utf8'), 'two');
    const again = await exportBranch(options, { mintToken });
    assert.equal(again.sourceRoot, out.sourceRoot); assert.equal(minted, 2);
    assert.throws(() => parseArgs(['ideator-apps/site', 'main']), /invalid-repository-export-config/);
    assert.throws(() => parseArgs(['--controller', 'http://example.com', '--token-file', '/t', '--cache', '/c', 'ideator-apps/site', 'main']), /invalid-repository-export-controller/);
    assert.equal(parseArgs(['--controller', 'http://127.0.0.1:8765', '--token-file', '/t', '--cache', '/c', 'ideator-apps/site', 'main']).branch, 'main');
    assert.equal(authHeader('t', 'github_token'), `Authorization: Basic ${Buffer.from('x-access-token:t').toString('base64')}`);
    assert.equal(authHeader('t', 'bearer'), 'Authorization: Bearer t');
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('a project shares one pnpm store across its repositories, and a repository on npm is warned', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-repo-preview-pm-'));
  try {
    const h = await harness(root); const m = await h.create();
    h.setFiles({ ...files(), 'package-lock.json': '{}' });
    const npm = await m.previews.register(request());
    await m.tick();
    const warned = m.previews.summary(m.previews.records[npm.id]);
    assert.equal(warned.status, 'ready'); assert.match(warned.warnings[0], /^package-manager-npm: environments are tuned for pnpm/);
    h.setFiles({ ...files(), 'pnpm-lock.yaml': '' });
    const pnpm = await m.previews.register({ ...request(), repository: 'ideator-apps/other' });
    await m.tick();
    assert.deepEqual(m.previews.summary(m.previews.records[pnpm.id]).warnings, []);
    const stores = new Set(); const caches = new Set();
    for (const env of m.config.environments) {
      const { cache, store } = await m.packageCache(env); caches.add(cache); stores.add(store);
    }
    assert.equal(stores.size, 1, 'one store for the project'); assert.equal(caches.size, 2, 'a cache per repository');
  } finally { await rm(root, { recursive: true, force: true }); }
});
