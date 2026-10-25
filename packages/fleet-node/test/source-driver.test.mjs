import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, chmod, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { createHash } from 'node:crypto';
import { lifecycle } from '../src/source-driver.mjs';

for (const outcome of ['published', 'validation-failed', 'source-changed']) {
  test(`source acceptance: ${outcome}`, async t => {
    const root = await mkdtemp(join(tmpdir(), 'ebox-source-'));
    t.after(() => rm(root, { recursive: true, force: true }));
    const attempt = 'attempt-one', id = createHash('sha256').update(attempt).digest('hex').slice(0, 32);
    const config = { project: 'demo', sourceProject: 'game', host: 'local', workspace: 'vm', incusProject: 'demo', owner: 'demo', root: join(root, 'state'), ebox: join(root, 'storage'), layerfs: join(root, 'layerfs.json'), nativeConfig: join(root, 'native.json'), githubToken: join(root, 'github.json'), buildConnection: join(root, 'build.json'), incus: '/usr/bin/true', repository: 'example/game', branch: 'main' };
    const source = join(root, 'source'), promoted = join(root, 'promoted'), calls = join(root, 'calls');
    await mkdir(source); await writeFile(join(source, 'game.txt'), 'candidate');
    await mkdir(join(config.root, id), { recursive: true });
    await writeFile(join(config.root, 'current.json'), JSON.stringify({ attempt, workspace: 'draft', closed: true, published: false }));
    await writeFile(join(config.root, id, 'receipt.json'), JSON.stringify({ id, attempt, phase: 'prepared', workspace: 'draft', expectedMain: 'a'.repeat(64), remoteCommit: 'b'.repeat(40) }));
    await writeFile(config.nativeConfig, JSON.stringify({ nativeAssistant: { endpoint: 'http://native', token: 'native' } }));
    await writeFile(config.githubToken, JSON.stringify({ token: 'github' }));
    await writeFile(config.buildConnection, JSON.stringify({ url: 'http://build', token: 'build' }));
    await writeFile(config.ebox, `#!${process.execPath}\nconst fs=require('node:fs');const args=process.argv.slice(2),op=args[6];fs.appendFileSync(${JSON.stringify(calls)},op+'\\n');let r;
if(op==='dev-export') r={revision:'snapshot',sourceRoot:${JSON.stringify(source)},manifest:{treeHash:args.includes('--workspace')?'candidate':fs.existsSync(${JSON.stringify(promoted)})?${JSON.stringify(outcome === 'source-changed' ? 'different' : 'candidate')}:'old'}};
else if(op==='github-settings')r={main:{revision:'${'a'.repeat(64)}'}};
else if(op==='github-promote'){fs.writeFileSync(${JSON.stringify(promoted)},'yes');r={sync:{attempt:{status:'delivered',delivery_commit:'${'c'.repeat(40)}'}}};}
else throw Error('Unexpected storage command');process.stdout.write(JSON.stringify(r));`);
    await chmod(config.ebox, 0o755);
    let publications = 0;
    const fetcher = async (url, options) => {
      if (url.startsWith('https://api.github.com/')) return Response.json({ object: { sha: 'b'.repeat(40) } });
      if (url.endsWith('/publish')) { publications++; assert.equal(JSON.parse(options.body).commit, 'c'.repeat(40)); return Response.json({ status: 'published', commit: 'c'.repeat(40), url: 'https://game.example.com' }); }
      assert.equal(options.headers['x-source-sha256'], createHash('sha256').update(options.body).digest('hex'));
      return Response.json({ status: outcome === 'validation-failed' ? 'failed' : 'validated', ok: outcome !== 'validation-failed', sourceDigest: options.headers['x-source-sha256'] });
    };
    const result = await lifecycle(config, { action: 'complete', project: 'demo', attempt }, fetcher);
    assert.equal(result.ok, outcome === 'published');
    assert.equal(publications, outcome === 'published' ? 1 : 0);
    const operations = await readFile(calls, 'utf8');
    assert.equal(operations.includes('github-promote'), outcome !== 'validation-failed');
    if (outcome === 'validation-failed') assert.equal(result.uncertain, false);
    if (outcome === 'source-changed') assert.equal(result.uncertain, true);
  });
}
