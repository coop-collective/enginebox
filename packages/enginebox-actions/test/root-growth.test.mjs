import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { delimiter, join, resolve } from 'node:path';
import { existsSync } from 'node:fs';

const script = resolve(import.meta.dirname, '../../../examples/ci/grow-linux-root.sh');
const quote = value => `'${value.replaceAll("'", "'\\''")}'`;
// The isolated preparation container mounts only packages, not repository examples.
test('CI root expansion guards geometry and preserves grow-only failures', { skip: process.platform === 'win32' || !existsSync(script) }, async t => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-root-growth-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const mock = join(root, 'mock.cjs'), calls = join(root, 'calls');
  await writeFile(mock, `
const fs = require('node:fs');
const [command, ...args] = process.argv.slice(2), mode = process.env.GROWTH_CASE;
if (command === 'id') console.log('0');
else if (command === 'cat') console.log(mode === 'os' ? 'ID=debian\\nVERSION_ID="12"' : 'ID=ubuntu\\nVERSION_ID="24.04"');
else if (command === 'findmnt') console.log(mode === 'root' ? '/dev/nvme0n1p2 ext4' : '/dev/sda2 ext4');
else if (command === 'lsblk') console.log(args.includes('TYPE') ? 'disk' : args.includes('NAME') ? (mode === 'layout' ? 'sda\\nsda1\\nsda2\\nsda3' : 'sda\\nsda1\\nsda2') : 'vfat');
else if (command === 'blockdev') console.log(args.at(-1) === '/dev/sda' ? '64424509440' : '32212254720');
else {
  fs.appendFileSync(process.env.GROWTH_CALLS, command + ' ' + args.join(' ') + '\\n');
  if (command === 'growpart' && mode === 'error') { console.error('FAILED: partition table unreadable'); process.exit(2); }
  if (command === 'growpart' && mode === 'unchanged') { console.log('NOCHANGE: partition 2 already fills disk'); process.exit(1); }
}
`);
  for (const command of ['id', 'cat', 'findmnt', 'lsblk', 'blockdev', 'growpart', 'resize2fs']) {
    await writeFile(join(root, command), `#!/bin/sh\nexec ${quote(process.execPath)} ${quote(mock)} ${quote(command)} "$@"\n`, { mode: 0o700 });
  }
  for (const mode of ['os', 'root', 'layout', 'error', 'unchanged', 'grow']) {
    await writeFile(calls, '');
    const result = spawnSync('sh', [script], { env: { ...process.env, PATH: `${root}${delimiter}${process.env.PATH}`, GROWTH_CASE: mode, GROWTH_CALLS: calls }, encoding: 'utf8', timeout: 10_000 });
    const operations = await readFile(calls, 'utf8');
    if (['os', 'root', 'layout'].includes(mode)) {
      assert.equal(result.status, 1, result.stderr);
      assert.equal(operations, '', `must refuse ${mode} before a mutation`);
    } else if (mode === 'error') {
      assert.equal(result.status, 1);
      assert.match(result.stderr, /partition table unreadable/);
      assert.equal(operations, 'growpart /dev/sda 2\n');
    } else {
      assert.equal(result.status, 0, result.stderr);
      assert.equal(operations, 'growpart /dev/sda 2\nresize2fs /dev/sda2\n');
    }
  }
});
