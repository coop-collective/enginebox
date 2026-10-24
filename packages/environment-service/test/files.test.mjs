import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, writeFile, rm, cp, stat, symlink, utimes, realpath } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { createHash } from 'node:crypto';
import { syncFiles } from '../src/files.mjs';
import { validateConfig } from '../src/config.mjs';
import { Environments } from '../src/runtime.mjs';

async function source(root, files) {
  const directory = await mkdtemp(join(root, 'source-')); const entries = [];
  for (const [path, value] of Object.entries(files)) {
    await mkdir(dirname(join(directory, path)), { recursive: true }); await writeFile(join(directory, path), value);
    entries.push({ path, kind: 'file', size: Buffer.byteLength(value), sha256: createHash('sha256').update(value).digest('hex'), executable: false });
  }
  const revision = createHash('sha256').update(JSON.stringify(entries)).digest('hex');
  return { root, revision, sourceRoot: await realpath(directory), manifest: { entries } };
}
test('verified source updates preserve the directory, unchanged files, dependencies and runtime data', async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-watch-'));
  try {
    const first = await source(root, { 'worker.js': 'version 1', 'unchanged.txt': 'same', 'deleted.txt': 'old' });
    const target = join(root, 'workspace'); await cp(first.sourceRoot, target, { recursive: true });
    await mkdir(join(target, 'node_modules')); await writeFile(join(target, 'node_modules', 'installed'), 'keep');
    await utimes(join(target, 'unchanged.txt'), new Date(1000), new Date(1000));
    const inode = (await stat(target)).ino;
    const next = await source(root, { 'worker.js': 'version 2', 'unchanged.txt': 'same', 'nested/new.js': 'new' });
    const result = await syncFiles(next, target, first.manifest.entries, ['.wrangler']);
    assert.deepEqual(result, { written: 2, removed: 1 });
    assert.equal((await stat(target)).ino, inode);
    assert.equal(await readFile(join(target, 'worker.js'), 'utf8'), 'version 2');
    assert.equal(await readFile(join(target, 'nested/new.js'), 'utf8'), 'new');
    assert.equal(await readFile(join(target, 'node_modules/installed'), 'utf8'), 'keep');
    assert.equal((await stat(join(target, 'unchanged.txt'))).mtimeMs, 1000);
    await assert.rejects(stat(join(target, 'deleted.txt')), { code: 'ENOENT' });
  } finally { await rm(root, { recursive: true, force: true }); }
});
test('invalid bytes, cache overlap and unmanaged collisions fail before changing live files', async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-watch-reject-'));
  try {
    const first = await source(root, { 'worker.js': 'good' }); const target = join(root, 'workspace');
    await cp(first.sourceRoot, target, { recursive: true });
    const bad = await source(root, { 'worker.js': 'new', 'extra.js': 'expected' });
    await writeFile(join(bad.sourceRoot, 'extra.js'), 'tampered');
    await assert.rejects(syncFiles(bad, target, first.manifest.entries), /source-(file|digest)-mismatch/);
    await assert.rejects(syncFiles(await source(root, { '.wrangler/config': 'no' }), target, first.manifest.entries, ['.wrangler']), /overlaps-writable/);
    await writeFile(join(target, 'runtime.json'), 'keep');
    await assert.rejects(syncFiles(await source(root, { 'worker.js': 'new', 'runtime.json': 'overwrite' }), target, first.manifest.entries), /unmanaged/);
    assert.equal(await readFile(join(target, 'worker.js'), 'utf8'), 'good');
    assert.equal(await readFile(join(target, 'runtime.json'), 'utf8'), 'keep');
  } finally { await rm(root, { recursive: true, force: true }); }
});
test('file/directory transitions and interrupted additions resume without deleting unmanaged content', async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-watch-transition-'));
  try {
    const first = await source(root, { 'a': 'old' }); const target = join(root, 'workspace'); await cp(first.sourceRoot, target, { recursive: true });
    const nested = await source(root, { 'a/b': 'new' }); let pending;
    await syncFiles(nested, target, first.manifest.entries, [], async entries => { pending = entries; });
    assert.equal(await readFile(join(target, 'a/b'), 'utf8'), 'new');
    // Simulate interruption after writes, before the completed receipt persisted.
    await syncFiles(nested, target, pending);
    await writeFile(join(target, 'a/unmanaged'), 'keep');
    await assert.rejects(syncFiles(first, target, nested.manifest.entries), /unmanaged/);
    assert.equal(await readFile(join(target, 'a/b'), 'utf8'), 'new');
    await rm(join(target, 'a/unmanaged'));
    await syncFiles(first, target, nested.manifest.entries, [], async entries => { pending = entries; });
    await syncFiles(first, target, pending);
    assert.equal(await readFile(join(target, 'a'), 'utf8'), 'old');
  } finally { await rm(root, { recursive: true, force: true }); }
});
test('destination parent symlinks cannot redirect a source update', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-watch-link-'));
  try {
    const target = join(root, 'workspace'); const outside = join(root, 'outside'); await mkdir(target); await mkdir(outside);
    await writeFile(join(outside, 'secret'), 'keep'); await symlink(outside, join(target, 'link'));
    const next = await source(root, { 'link/secret': 'replace' });
    await assert.rejects(syncFiles(next, target, []), /parent-not-directory/);
    // An owned removed symlink can become a real directory, without following it.
    await syncFiles(next, target, [{ path: 'link', kind: 'symlink', target: '../outside' }]);
    assert.equal(await readFile(join(outside, 'secret'), 'utf8'), 'keep');
    assert.equal(await readFile(join(target, 'link/secret'), 'utf8'), 'replace');
  } finally { await rm(root, { recursive: true, force: true }); }
});

// The Linux supervisor requires Unix private-directory permissions. Keep the
// portable file-update tests above running on Windows as well.
test('a dev process survives source changes, source failures and controller recovery', { skip: process.platform === 'win32' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-watch-process-'));
  try {
    const env = { id: 'main', updateMode: 'files', writableDirs: ['.wrangler'], source: { kind: 'layerfs', project: 'test', command: ['/bin/export'] }, image: `node@sha256:${'a'.repeat(64)}`, command: ['dev-server'], prepare: ['install-dependencies'], ports: [{ name: 'web', protocol: 'http', container: 8787, host: 18787 }], health: { port: 'web', path: '/' }, cpu: 1, memoryMiB: 256, timeoutSeconds: 5 };
    const config = { schema: 1, stateDir: root, tokenFile: join(root, 'token'), listenPort: 18790, baseDomains: ['dev.example.com'], httpEntrypoint: 'web', httpTls: false, basicAuthUsers: [], transportEntrypoints: [], pollSeconds: 2, environments: [env] };
    validateConfig(config);
    let selected = await source(root, { 'worker.js': 'one' }); let container; let starts = 0; let prepares = 0; let fail = false;
    const setup = () => {
      const manager = new Environments(config);
      manager.source = async () => { if (fail) throw new Error('command-failed'); return { ...selected, root: join(root, 'main') }; };
      manager.prepare = async (_env, snapshot) => {
        prepares++; const prepared = join(snapshot.root, snapshot.revision); await cp(snapshot.sourceRoot, prepared, { recursive: true });
        await mkdir(join(prepared, 'node_modules')); await writeFile(join(prepared, 'node_modules/installed'), 'keep'); return prepared;
      };
      manager.start = async () => { starts++; container = { State: { Running: true }, Config: { Labels: { 'enginebox.environment.recipe': createHash('sha256').update(JSON.stringify(env)).digest('hex') } } }; };
      manager.inspect = async () => container;
      manager.ready = async () => {}; manager.healthy = async () => !!container;
      return manager;
    };
    let manager = setup(); await manager.init(); await manager.tick();
    assert.equal(starts, 1); assert.equal(prepares, 1); assert.equal(manager.states.main.status, 'ready');
    await manager.setRoute({ id: 'main', environment: 'main', service: 'web', hostname: 'app.dev.example.com', access: 'public' });
    selected = await source(root, { 'worker.js': 'two', 'new.js': 'new' });
    await manager.tick(); assert.equal(starts, 1); assert.equal(prepares, 1);
    assert.equal(await readFile(join(root, 'main/workspace/worker.js'), 'utf8'), 'two');
    fail = true; await manager.tick(); assert.equal(manager.states.main.healthy, true); assert.equal(starts, 1);
    manager = setup(); await manager.init();
    assert.equal(manager.states.main.healthy, true);
    assert((JSON.parse(await readFile(manager.dynamicFile))).http.routers.main);
    fail = false; await manager.tick();
    assert.equal(manager.states.main.status, 'ready'); assert.equal(manager.states.main.revision, selected.revision);
    assert.equal(starts, 1); assert.equal(prepares, 1);
    manager = setup(); manager.healthy = async () => false; await manager.init();
    assert.equal(manager.states.main.healthy, false);
    assert.equal(Object.keys((JSON.parse(await readFile(manager.dynamicFile))).http.routers).length, 0);
  } finally { await rm(root, { recursive: true, force: true }); }
});
