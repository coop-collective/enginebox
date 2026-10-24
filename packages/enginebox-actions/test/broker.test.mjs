import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm, stat, symlink } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { spawn, spawnSync } from 'node:child_process';
import { activatePreparedTree } from '../src/broker.mjs';
const enabled = process.platform === 'linux' && process.getuid() === 0 && process.env.ENGINEBOX_TEST_PREPARATION_BROKER === '1';
test('root handoff protects approved bytes, refuses escaping/symlink targets and moves without copying', { skip: !enabled }, async t => {
  const dir = await mkdtemp(join(tmpdir(), 'ebox-broker-')), root = join(dir, 'project'), control = join(dir, 'control');
  const view = join(control, 'private/source'), key = 'a'.repeat(64), source = join(root, 'work/repo/repo');
  await mkdir(join(root, 'prepared', key, 'node_modules'), { recursive: true });
  await mkdir(source, { recursive: true }); await mkdir(view, { recursive: true, mode: 0o700 });
  await mkdir(join(control, 'requests'));
  const recipe = { scripts: 'deny' }, inputs = { workspace: source, policy: recipe };
  await writeFile(join(control, 'policy.json'), JSON.stringify({ root, repository_id: 101, recipe }));
  await writeFile(join(control, 'verified.json'), JSON.stringify({ verified: true, repository_id: 101 }));
  await writeFile(join(root, 'prepared', key, 'manifest.json'), JSON.stringify({ key, inputs }));
  await writeFile(join(root, 'prepared', key, 'node_modules/fixture'), 'approved');
  function command(args) { const r = spawnSync(args[0], args.slice(1), { encoding: 'utf8' }); assert.equal(r.status, 0, r.stderr); }
  t.after(async () => {
    spawnSync('umount', [join(root, 'prepared')]); spawnSync('umount', [view]);
    await rm(dir, { recursive: true, force: true });
  });
  command(['mount', '--bind', root, view]); command(['mount', '--make-private', view]);
  command(['mount', '--bind', join(root, 'prepared'), join(root, 'prepared')]);
  command(['mount', '-o', 'remount,bind,ro,nosuid,nodev', join(root, 'prepared')]);
  await assert.rejects(writeFile(join(root, 'prepared', key, 'node_modules/fixture'), 'tampered'), { code: 'EROFS' });
  async function request(value, success) {
    const child = spawn('python3', [resolve(import.meta.dirname, '../src/broker.py'), control, '5']);
    const done = new Promise((accept, reject) => { child.once('error', reject); child.once('exit', code => code === 0 ? accept() : reject(new Error(`Broker exit ${code}`))); });
    t.after(() => { if (child.exitCode === null) child.kill(); });
    if (success) await activatePreparedTree(value, control);
    else await assert.rejects(activatePreparedTree(value, control), /handoff failed/);
    await done;
    await rm(join(control, 'requests/restore.json')); await rm(join(control, 'restored.json'));
  }
  await request({ key, inputs: { ...inputs, workspace: dir }, source: dir }, false);
  const link = join(root, 'work/repo/link'); await symlink(source, link);
  await request({ key, inputs: { ...inputs, workspace: link }, source: link }, false);
  await request({ key, inputs: { ...inputs, policy: { scripts: 'run' } }, source }, false);
  const before = await stat(join(root, 'prepared', key, 'node_modules/fixture'));
  await request({ key, inputs, source }, true);
  const after = await stat(join(source, 'node_modules/fixture'));
  assert.equal(after.ino, before.ino); assert.equal(after.dev, before.dev);
  assert.equal(await readFile(join(source, 'node_modules/fixture'), 'utf8'), 'approved');
  await writeFile(join(source, 'node_modules/fixture'), 'private job write');
});
