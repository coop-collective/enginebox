import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtemp, writeFile, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

test('records stderr pnpm progress and preserves command failures without storing logs', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'enginebox-measure-test-'));
  const path = join(directory, 'measurement.json');
  try {
    await writeFile(path, JSON.stringify({ cache: {}, stages: {} }));
    const script = 'console.error(JSON.stringify({name:"pnpm:progress",status:"fetched"}));console.error("private-output");process.exit(7)';
    const child = spawnSync(process.execPath, [fileURLToPath(new URL('./measure.mjs', import.meta.url)), 'run', path, 'dependencies', '--', process.execPath, '-e', script]);
    assert.equal(child.status, 7);
    const result = JSON.parse(await readFile(path, 'utf8'));
    assert.equal(result.stages.dependencies.exitCode, 7);
    assert.equal(result.cache.downloadedPackages, 1);
    assert(result.stages.dependencies.elapsedMs > 0);
    assert.equal(result.stages.dependencies.stdout, undefined);
    assert.equal(result.stages.dependencies.stderr, undefined);
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test('missing executable records a failure, not success', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'enginebox-measure-test-'));
  const path = join(directory, 'measurement.json');
  try {
    await writeFile(path, JSON.stringify({ cache: {}, stages: {} }));
    const child = spawnSync(process.execPath, [fileURLToPath(new URL('./measure.mjs', import.meta.url)), 'run', path, 'integration', '--', join(directory, 'missing')]);
    assert.equal(child.status, 127);
    assert.equal(JSON.parse(await readFile(path, 'utf8')).stages.integration.exitCode, 127);
  } finally { await rm(directory, { recursive: true, force: true }); }
});
