import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, mkdir, link, symlink, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { measure } from '../src/usage.mjs';

test('regular-file bytes deduplicate hardlinks and never follow symlinks', async () => {
  const root = await mkdtemp(join(tmpdir(), 'enginebox-usage-'));
  try {
    await mkdir(join(root, 'cache'));
    await writeFile(join(root, 'cache', 'data'), 'fixture');
    await link(join(root, 'cache', 'data'), join(root, 'cache', 'hardlink'));
    await writeFile(join(root, 'outside'), 'not attributed');
    if (process.platform !== 'win32') {
      await symlink('../outside', join(root, 'cache', 'escape'));
      await symlink('.', join(root, 'cache', 'cycle'));
    }
    const [result] = await measure([join(root, 'cache')]);
    assert.equal(result.status, 'complete');
    assert.equal(result.apparent_bytes, 7);
    assert.equal(result.files, 1);
    // Linux/macOS expose allocated blocks; Windows is never a runtime host.
    if (process.platform !== 'win32') assert.equal(result.allocated_bytes, (await stat(join(root, 'cache', 'data'))).blocks * 512);
    assert(!JSON.stringify(result).includes('outside'));
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('empty, unavailable, and truncated scans are distinguishable', async () => {
  const root = await mkdtemp(join(tmpdir(), 'enginebox-usage-'));
  try {
    assert.deepEqual((await measure([root]))[0], { status: 'complete', apparent_bytes: 0, allocated_bytes: 0, files: 0 });
    assert.deepEqual((await measure([join(root, 'missing')]))[0], { status: 'unavailable', apparent_bytes: null, allocated_bytes: null, files: null });
    await writeFile(join(root, 'a'), 'one');
    await writeFile(join(root, 'b'), 'two');
    const [partial] = await measure([root], { maxEntries: 2 });
    assert.equal(partial.status, 'partial');
    assert.equal(partial.apparent_bytes, 3);
    assert.equal(partial.files, 1);
    assert.equal((await measure([root], { maxMs: 0 }))[0].status, 'unavailable');
  } finally { await rm(root, { recursive: true, force: true }); }
});
