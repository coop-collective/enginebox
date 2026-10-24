import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm, cp, symlink } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { build, prepare, fingerprint, tree, command, environment } from '../src/pnpm.mjs';

const policy = { schema: 1, recipe: 'pnpm-installed-v1', namespace: 'owner-fixture', repository: '101', trust: 'repository-read', image: 'a'.repeat(64), pnpm: '12.15.0', scripts: 'deny' };
async function fixture(t) {
  const dir = await mkdtemp(join(tmpdir(), 'ebox-preparation-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const root = join(dir, 'private'), source = join(root, 'work/repo/repo'), home = join(dir, 'home');
  await mkdir(source, { recursive: true }); await mkdir(home);
  const env = environment(root, { ...process.env, HOME: home, XDG_CONFIG_HOME: join(home, 'config'), XDG_CACHE_HOME: join(home, 'cache') });
  const pkg = join(source, 'package'); await mkdir(pkg);
  await writeFile(join(pkg, 'package.json'), JSON.stringify({ name: 'ebox-synthetic-dependency', version: '1.0.0', main: 'index.js' }));
  await writeFile(join(pkg, 'index.js'), 'module.exports = 42;\n');
  command(['tar', '-czf', 'dependency.tgz', 'package'], source, env);
  await rm(pkg, { recursive: true });
  await writeFile(join(source, 'package.json'), JSON.stringify({ name: 'fixture', version: '1.0.0', dependencies: { 'ebox-synthetic-dependency': 'file:dependency.tgz' }, scripts: { test: 'node -e "process.exit(17)"' } }));
  command(['pnpm', 'install', '--lockfile-only', '--ignore-scripts', '--offline', '--store-dir', join(root, 'caches/packages')], source, env);
  return { dir, root, source, env, policy, offline: true };
}
test('real offline pnpm: published dependencies survive clean checkout, restore without install, remain private', { skip: process.platform === 'win32' }, async t => {
  const f = await fixture(t);
  const manifest = await build(f);
  const baseline = join(f.dir, 'approved'); await cp(f.root, baseline, { recursive: true, verbatimSymlinks: true });
  const baseDigest = (await tree(join(baseline, 'prepared'))).digest;
  command(['git', 'init', '-q'], f.source, f.env);
  command(['git', 'add', 'package.json', 'pnpm-lock.yaml', 'dependency.tgz'], f.source, f.env);
  command(['git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'fixture'], f.source, f.env);
  await mkdir(join(f.source, 'node_modules'), { recursive: true });
  await writeFile(join(f.source, 'node_modules/stale'), 'must be cleaned');
  command(['git', 'clean', '-ffdx'], f.source, f.env);
  const receipt = await prepare({ ...f, requireHit: true });
  assert.equal(receipt.mode, 'installed-tree'); assert.equal(receipt.key, manifest.key); assert.equal(receipt.downloadedPackages, 0);
  assert.equal(command(['node', '-p', 'require("ebox-synthetic-dependency")'], f.source, f.env).trim(), '42');
  // The package manager still accepts the restored tree at the stable path.
  assert.equal(command(['pnpm', '--offline', 'exec', 'node', '-p', 'require("ebox-synthetic-dependency")'], f.source, f.env).trim(), '42');
  assert.throws(() => command(['pnpm', 'test'], f.source, f.env), /command failed/);
  await writeFile(join(f.source, 'node_modules/job-private.txt'), 'mutation');
  assert.equal((await tree(join(baseline, 'prepared'))).digest, baseDigest);
});
test('dependency, runtime policy, trust and stable paths invalidate installed-tree keys', { skip: process.platform === 'win32' }, async t => {
  const f = await fixture(t), before = await fingerprint(f.source, policy, f.env);
  for (const changed of [{ image: 'b'.repeat(64) }, { repository: '102' }, { trust: 'different' }, { scripts: 'run' }]) {
    assert.notEqual((await fingerprint(f.source, { ...policy, ...changed }, f.env)).key, before.key);
  }
  const relocated = join(f.root, 'work/other/other');
  await cp(f.source, relocated, { recursive: true, verbatimSymlinks: true });
  assert.notEqual((await fingerprint(relocated, policy, f.env)).key, before.key);
  const globalConfig = join(f.env.XDG_CONFIG_HOME, 'pnpm/config.yaml');
  await mkdir(join(f.env.XDG_CONFIG_HOME, 'pnpm'), { recursive: true });
  await writeFile(globalConfig, 'nodeLinker: hoisted\n');
  assert.notEqual((await fingerprint(f.source, policy, f.env)).key, before.key);
  await rm(globalConfig);
  await writeFile(join(f.source, '.npmrc'), 'strict-peer-dependencies=true\n');
  assert.notEqual((await fingerprint(f.source, policy, f.env)).key, before.key);
  await assert.rejects(prepare({ ...f, requireHit: true }), /Required prepared generation/);
  const cold = await prepare(f); assert.equal(cold.mode, 'install');
});
test('corrupt prepared content fails closed and cannot become a hit', { skip: process.platform === 'win32' }, async t => {
  const f = await fixture(t), manifest = await build(f);
  await writeFile(join(f.root, 'prepared', manifest.key, 'node_modules/tampered'), 'bad');
  await assert.rejects(prepare(f), /integrity verification/);
});
test('source symlinks escaping the checkout are refused', async t => {
  const dir = await mkdtemp(join(tmpdir(), 'ebox-prep-links-')); t.after(() => rm(dir, { recursive: true, force: true }));
  const source = join(dir, 'source'); await mkdir(source);
  await writeFile(join(dir, 'private'), 'fixture-secret');
  try { await symlink('../private', join(source, '.npmrc')); } catch (e) { if (process.platform === 'win32' && e.code === 'EPERM') return t.skip('Symlink privilege unavailable'); throw e; }
  await assert.rejects(tree(source, { source: true }), /External/);
});

test('explicit run policy executes lifecycle scripts instead of using an installed-tree hit', { skip: process.platform === 'win32' }, async t => {
  const f = await fixture(t);
  const file = join(f.source, 'package.json'), pkg = JSON.parse(await readFile(file, 'utf8'));
  pkg.scripts.postinstall = `node -e "require('fs').writeFileSync('node_modules/lifecycle-ran', 'yes')"`;
  await writeFile(file, JSON.stringify(pkg));
  await build(f); // deny is explicit: this generation cannot stand in for a script-enabled install.
  const receipt = await prepare({ ...f, policy: { ...policy, scripts: 'run' } });
  assert.equal(receipt.mode, 'install');
  assert.equal(receipt.reason, 'lifecycle-scripts-require-install');
  assert.equal(await readFile(join(f.source, 'node_modules/lifecycle-ran'), 'utf8'), 'yes');
});

test('custom package hooks cannot publish or silently reuse installed dependency trees', { skip: process.platform === 'win32' }, async t => {
  const f = await fixture(t);
  await writeFile(join(f.source, '.pnpmfile.cjs'), 'module.exports = { hooks: { readPackage: pkg => pkg } };\n');
  command(['pnpm', 'install', '--lockfile-only', '--ignore-scripts', '--offline', '--store-dir', join(f.root, 'caches/packages')], f.source, f.env);
  await assert.rejects(build(f), /does not support/);
  await assert.rejects(prepare({ ...f, requireHit: true }), /requires-install/);
  const receipt = await prepare(f);
  assert.equal(receipt.mode, 'install'); assert.equal(receipt.reason, 'workspace-or-package-hook-requires-install');
});
