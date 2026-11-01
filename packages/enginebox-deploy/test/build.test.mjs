import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, realpath, writeFile, readFile, rm, symlink, chmod } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { command, run, selectArtifact, sha256, version } from '../src/lib.mjs';
import { pinnedSources } from '../src/build.mjs';

const root = fileURLToPath(new URL('..', import.meta.url));
const target = 'x86_64-unknown-linux-gnu';
async function temporary(t) {
  const dir = await realpath(await mkdtemp(join(tmpdir(), 'enginebox-build-test-')));
  await chmod(dir, 0o700);
  t.after(() => rm(dir, { recursive: true, force: true }));
  return dir;
}
test('exact versions only', () => {
  assert.equal(version('0.3.1-live.3'), '0.3.1-live.3');
  for (const bad of ['0.3', 'v0.3.1', '0.3.1\n', '0.3.1 && echo']) assert.throws(() => version(bad), /exact-version-required/);
});
test('artifact selection requires exact version, platform and both checksums', { skip: process.platform === 'win32' }, async t => {
  const directory = await temporary(t); const pin = '0.3.1-live.3';
  const stem = `enginebox-${pin}-${target}`;
  const archive = join(directory, `${stem}.tar.gz`);
  await writeFile(archive, 'disposable archive fixture');
  const value = { version: pin, target, archive_sha256: await sha256(archive), state_schema: 1, read_schema_min: 1, read_schema_max: 1, dashboard: true };
  const file = join(directory, `${stem}.json`); await writeFile(file, JSON.stringify(value));
  assert.equal((await selectArtifact(directory, pin, target)).manifest_sha256, await sha256(file));
  await assert.rejects(selectArtifact(directory, '0.3.1-live.4', target));
  await writeFile(archive, 'modified'); await assert.rejects(selectArtifact(directory, pin, target), /artifact-checksum/);
  await rm(archive); await symlink(file, archive); await assert.rejects(selectArtifact(directory, pin, target), /symlink/);
});
test('shell boundary preserves literal arguments and child errors do not print private output', async () => {
  const value = 'a\'b;$(printf injected)';
  const echoed = await run('sh', ['-c', command([process.execPath, '-e', 'process.stdout.write(process.argv[1])', value])]);
  assert.equal(echoed, value);
  await assert.rejects(run(process.execPath, ['-e', 'console.error("private-token-fixture");process.exit(1)']), error => !error.message.includes('private-token-fixture'));
});
test('isolated version patch preserves LF and CRLF while changing only workspace versions', async () => {
  const workspace = resolve(root, '../..');
  const cargoSource = await readFile(join(workspace, 'Cargo.toml'), 'utf8');
  const lockSource = await readFile(join(workspace, 'Cargo.lock'), 'utf8');
  for (const newline of ['\n', '\r\n']) {
    const cargo = cargoSource.replace(/\r?\n/g, newline);
    const lock = lockSource.replace(/\r?\n/g, newline);
    const patched = pinnedSources(cargo, lock, '0.3.1-live.7');
    assert.ok(patched.cargo.includes(`[workspace.package]${newline}version = "0.3.1-live.7"`));
    const versions = [...patched.lock.matchAll(/name = "enginebox-[^"]+"\r?\nversion = "([^"]+)"/g)];
    assert.ok(versions.length >= 5 && versions.every(match => match[1] === '0.3.1-live.7'));
    const normalizeLock = text => text.replace(/(name = "enginebox-[^"]+"\r?\nversion = ")[^"]+/g, '$1VERSION');
    const normalizeCargo = text => text.replace(/(\[workspace.package\]\r?\nversion = ")[^"]+/, '$1VERSION');
    assert.equal(normalizeLock(patched.lock), normalizeLock(lock));
    assert.equal(normalizeCargo(patched.cargo), normalizeCargo(cargo));
    for (const text of [patched.lock, patched.cargo]) assert.ok([...text.matchAll(/\r?\n/g)].every(match => match[0] === newline));
  }
});
