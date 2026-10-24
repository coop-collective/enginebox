import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { acceptedSource, copyAccepted } from '../src/accepted.mjs';

test('accepted LayerFS exports bind controller head, inventory and copied bytes; candidates and tampering cannot activate', async () => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-accepted-'));
  const generation = 'a'.repeat(64); let accepted = generation;
  const entries = [{ executable: false, kind: 'file', path: 'app.mjs', sha256: createHash('sha256').update('good').digest('hex'), size: 4 }];
  const treeHash = createHash('sha256').update(JSON.stringify(entries)).digest('hex');
  const metadata = { treeHash, integration: 'batch-one', layerfsRoot: 'b'.repeat(64), layerfsRef: 'ref:one' };
  const server = createServer((req, res) => {
    assert.equal(req.headers.authorization, `Bearer ${'t'.repeat(32)}`);
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify(req.url.endsWith('/demo') ? { project: 'demo', head: accepted } : { id: accepted, manifest: { project: 'demo', metadata } }));
  });
  await new Promise(r => server.listen(0, '127.0.0.1', r));
  try {
    const directory = join(root, 'exports', 'batch-one'); await mkdir(join(directory, 'source'), { recursive: true });
    await writeFile(join(root, 'token'), 't'.repeat(32)); await writeFile(join(directory, 'source', 'app.mjs'), 'good');
    await writeFile(join(directory, 'source.json'), JSON.stringify({ schema: 1, project: 'demo', generation, treeHash, layerfs: { root: metadata.layerfsRoot, ref: metadata.layerfsRef }, entries }));
    const env = { id: 'main', source: { kind: 'accepted', project: 'demo', controller: `http://127.0.0.1:${server.address().port}`, tokenFile: join(root, 'token'), exportsDir: join(root, 'exports') } };
    const source = await acceptedSource(env, join(root, 'state'));
    await copyAccepted(source, join(root, 'checkout'));
    assert.equal(await readFile(join(root, 'checkout', 'app.mjs'), 'utf8'), 'good');
    accepted = 'c'.repeat(64); await assert.rejects(acceptedSource(env, root), /provenance-mismatch/);
    accepted = generation; await writeFile(join(directory, 'source', 'app.mjs'), 'evil');
    await assert.rejects(copyAccepted(source, join(root, 'tampered')), /digest-mismatch/);
    source.manifest.entries[0].path = '../escape'; await assert.rejects(copyAccepted(source, join(root, 'escape-test')), /unsafe-source-path/);
  } finally { await new Promise(r => server.close(r)); await rm(root, { recursive: true, force: true }); }
});
