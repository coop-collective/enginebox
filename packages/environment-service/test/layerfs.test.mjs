import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createHash } from 'node:crypto';
import { layerfsSource } from '../src/layerfs.mjs';
import { copyAccepted } from '../src/accepted.mjs';

test('local LayerFS previews require the configured project and checkpoint, then verify copied bytes', async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-layerfs-dev-'));
  const canonical = await (await import('node:fs/promises')).realpath(root);
  const hash = value => createHash('sha256').update(value).digest('hex');
  const entries = [{ kind: 'file', path: 'app.mjs', size: 5, sha256: hash('hello'), executable: false }];
  const value = { revision: 'a'.repeat(64), sourceRoot: canonical, manifest: { schema: 1, authority: 'local-layerfs', project: 'demo', workspace: 'dev-one', revision: 'a'.repeat(64), entries, treeHash: hash(JSON.stringify(entries)) } };
  const env = { id: 'preview', source: { kind: 'layerfs', project: 'demo', workspace: 'dev-one', command: ['/operator/export', 'dev-one'] } };
  try {
    await writeFile(join(root, 'app.mjs'), 'hello');
    const run = async (program, args) => { assert.equal(program, '/operator/export'); assert.deepEqual(args, ['dev-one']); return JSON.stringify(value); };
    const source = await layerfsSource(env, root, run);
    await copyAccepted(source, join(root, 'checked'));
    value.manifest.workspace = null;
    await assert.rejects(layerfsSource(env, root, run), /provenance/);
    value.manifest.workspace = 'dev-one'; value.manifest.project = 'foreign';
    await assert.rejects(layerfsSource(env, root, run), /provenance/);
    value.manifest.project = 'demo'; value.manifest.entries[0].sha256 = 'b'.repeat(64);
    await assert.rejects(layerfsSource(env, root, run), /inventory/);
    value.manifest.entries[0].sha256 = hash('hello'); await writeFile(join(root, 'app.mjs'), 'other');
    await assert.rejects(copyAccepted(await layerfsSource(env, root, run), join(root, 'bad-copy')), /digest/);
  } finally { await rm(root, { recursive: true, force: true }); }
});
