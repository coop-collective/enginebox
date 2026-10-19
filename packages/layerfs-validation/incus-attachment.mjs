// Run as root on a disposable qualification fixture. This deliberately leaves
// failed attachments/data intact; the fixture owner exports evidence and cleans up.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync, existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';

assert.equal(process.env.ENGINEBOX_LAYERFS_LIVE, '1');
assert.equal(process.getuid(), 0);
const env = key => { assert(process.env[key], `Set ${key}`); return process.env[key]; };
const configPath = env('ENGINEBOX_LAYERFS_LIVE_CONFIG');
const reportPath = env('ENGINEBOX_LAYERFS_REPORT');
const ebox = env('ENGINEBOX_LAYERFS_EBOX');
const vm = env('ENGINEBOX_LAYERFS_INCUS_VM');
const project = env('ENGINEBOX_LAYERFS_INCUS_PROJECT');
const marker = env('ENGINEBOX_LAYERFS_MARKER_CONTENT');
assert.equal(readFileSync(env('ENGINEBOX_LAYERFS_MARKER'), 'utf8').trim(), marker);
assert(vm.startsWith('ebox-layerfs-incus-') && marker.startsWith('layerfs-incus-'));
assert(!existsSync(reportPath), 'Never overwrite a qualification report');
const config = JSON.parse(readFileSync(configPath, 'utf8'));
assert(!existsSync(config.root), 'Qualification requires a fresh storage root');
mkdirSync(dirname(reportPath), { recursive: true, mode: 0o700 });
const report = { schema: 1, startedAt: new Date().toISOString(), observations: [], passed: false };
const record = () => writeFileSync(reportPath, JSON.stringify(report, null, 2), { mode: 0o600 });
const run = (program, args, { expectFailure = false, timeout = 120_000, git = false } = {}) => {
  const start = performance.now();
  const result = spawnSync(program, args, { encoding: 'utf8', timeout, maxBuffer: 4 * 1024 * 1024,
    env: git ? { PATH: process.env.PATH, GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null', GIT_TERMINAL_PROMPT: '0' } : process.env });
  report.observations.push({ command: [program, ...args], seconds: (performance.now() - start) / 1000, status: result.status, stdout: result.stdout, stderr: result.stderr });
  record();
  if (result.error) throw result.error;
  if (expectFailure) assert.notEqual(result.status, 0, 'Expected refusal');
  else assert.equal(result.status, 0, `${program} failed: ${result.stderr}`);
  return result.stdout;
};
const storage = (args, options) => run(ebox, ['--host', 'local', 'storage', 'layerfs', '--config', configPath, ...args], options);
const incus = args => run('incus', ['--project', project, ...args]);
const inspect = () => JSON.parse(run('incus', ['query', `/1.0/instances/${vm}?project=${project}`]));
const owned = () => {
  const current = inspect();
  assert.equal(current.config['user.enginebox.experiment'], marker, 'Refuse unrelated VM');
  return current;
};
const guest = command => incus(['exec', vm, '--user=1001', '--group=1001', '--', 'sh', '-eu', '-c', command]);
async function boot() {
  assert.equal(owned().status, 'Stopped');
  incus(['start', vm]);
  let ready = false;
  for (let attempt = 0; attempt < 90; attempt++) {
    const probe = spawnSync('incus', ['--project', project, 'exec', vm, '--', 'true'], { encoding: 'utf8', timeout: 5000 });
    if (probe.status === 0) { ready = true; break; }
    await sleep(1000);
  }
  assert(ready, 'Incus agent did not become ready');
  storage(['incus-verify', 'incus-a']);
}
try {
  const initial = owned();
  assert.equal(initial.status, 'Stopped');
  assert.equal(initial.stateful, false);
  assert.equal(initial.config['migration.stateful'], undefined, 'Fixture must inherit stateful=true');
  assert.equal(initial.expanded_config['migration.stateful'], 'true');
  const source = join(dirname(reportPath), 'source');
  mkdirSync(source, { mode: 0o700 });
  writeFileSync(join(source, 'README.md'), 'Shared committed base; each agent gets its own branch.\n');
  writeFileSync(join(source, 'run.sh'), '#!/bin/sh\nprintf mapped-executable\\n\n', { mode: 0o755 });
  for (const args of [['init', '-q', source], ['-C', source, 'add', '.'], ['-C', source, '-c', 'user.name=Engine Box Qualification', '-c', 'user.email=qualification@example.invalid', 'commit', '-qm', 'Synthetic fixture']]) run('git', args, { git: true });
  const preview = JSON.parse(storage(['preview', '--repo', source]));
  storage(['import', 'incus-fixture', '--repo', source, '--revision', preview.commit, '--approval', preview.approval, '--credential-free']);
  storage(['fork', 'incus-fixture', 'incus-a']);
  storage(['fork', 'incus-fixture', 'incus-b']);
  const attach = () => storage(['incus-attach', 'incus-a', '--project', project, '--instance', vm, '--owner', project, '--uid', '1001', '--gid', '1001']);
  attach();
  assert.equal(owned().config['migration.stateful'], 'false');
  storage(['checkpoint', 'incus-a'], { expectFailure: true });
  await boot();
  assert.equal(guest('stat -c "%u:%g" /project').trim(), '1001:1001');
  guest('cd /project; test -r .git/HEAD; printf private-write > private.txt; mkdir newdir; printf nested > newdir/data; ln private.txt hardlink.txt; mv private.txt renamed.txt; mv renamed.txt private.txt; ln -s newdir/data link.txt; test "$(cat link.txt)" = nested; chmod 600 private.txt; test "$(stat -c %a private.txt)" = 600; ./run.sh; test ! -e /layerfs-store; test ! -S /var/run/docker.sock');
  storage(['exec', 'incus-b', '--', 'sh', '-eu', '-c', 'test ! -e private.txt; test ! -e newdir; test -r README.md']);
  storage(['incus-detach', 'incus-a'], { expectFailure: true });
  storage(['remount', 'incus-a'], { expectFailure: true });
  storage(['close', 'incus-a'], { expectFailure: true });
  run(ebox, ['--host', 'local', '--project', project, '--direct', 'hibernate', vm], { expectFailure: true });
  run(ebox, ['--host', 'local', '--project', project, '--direct', 'snapshot', vm, 'unqualified-memory'], { expectFailure: true });
  const pid = guest('sleep 300 </dev/null >/dev/null 2>&1 & echo $!').trim();
  assert.match(pid, /^\d+$/);
  const identity = guest(`cat /proc/${pid}/stat`).trim().split(' ').slice(0, 22);
  incus(['pause', vm]);
  assert.equal(owned().status, 'Frozen');
  incus(['start', vm]);
  const resumed = guest(`cat /proc/${pid}/stat`).trim().split(' ').slice(0, 22);
  assert.equal(resumed[0], identity[0]);
  assert.equal(resumed[21], identity[21], 'Guest process start time must survive pause');
  guest('test "$(cat /project/private.txt)" = private-write; printf resumed > /project/resumed.txt');
  owned(); incus(['stop', vm, '--timeout=60']);
  storage(['incus-detach', 'incus-a']);
  assert.equal(owned().config['migration.stateful'], undefined);
  assert.equal(owned().expanded_config['migration.stateful'], 'true');
  storage(['checkpoint', 'incus-a', '--message', 'Incus guest writes']);
  storage(['exec', 'incus-a', '--', 'sh', '-eu', '-c', 'test "$(cat private.txt)" = private-write; test "$(cat resumed.txt)" = resumed; test "$(cat newdir/data)" = nested']);
  owned(); incus(['config', 'set', vm, 'migration.stateful=false']);
  attach();
  await boot();
  guest('test "$(cat /project/private.txt)" = private-write; test "$(cat /project/resumed.txt)" = resumed');
  owned(); incus(['stop', vm, '--timeout=60']);
  storage(['incus-detach', 'incus-a']);
  assert.equal(owned().config['migration.stateful'], 'false', 'Preserve explicit local false');
  storage(['checkpoint', 'incus-a']);
  storage(['close', 'incus-a']);
  storage(['close', 'incus-b']);
  report.passed = true;
} catch (error) {
  report.error = String(error.stack ?? error);
  process.exitCode = 1;
} finally {
  report.finishedAt = new Date().toISOString();
  record();
  console.log(JSON.stringify({ passed: report.passed, report: reportPath, error: report.error ?? null }));
}
