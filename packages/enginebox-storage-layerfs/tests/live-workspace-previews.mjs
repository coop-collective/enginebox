// Opt-in Linux qualification. Uses only a fresh, operator-selected private root;
// leaves owned fixtures for evidence and explicit cleanup. Never uses a live Store.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { readFile, writeFile, mkdir, stat } from 'node:fs/promises';
import { join, isAbsolute } from 'node:path';
const [binary, configFile, evidence] = process.argv.slice(2);
assert(process.platform === 'linux' && process.env.ENGINEBOX_DISPOSABLE_PROOF === '1');
assert([binary, configFile, evidence].every(p => p && isAbsolute(p)));
const config = JSON.parse(await readFile(configFile));
await assert.rejects(stat(config.root), { code: 'ENOENT' });
await assert.rejects(stat(evidence), { code: 'ENOENT' });
const fixture = `${config.root}-seed`; await mkdir(fixture, { recursive: false });
await writeFile(join(fixture, 'one.txt'), 'base'); await writeFile(join(fixture, 'two.txt'), 'base');
for (const args of [['init', '-b', 'main'], ['add', '.'], ['-c', 'user.name=Proof', '-c', 'user.email=proof@example.invalid', 'commit', '-m', 'fixture']]) {
  const result = spawnSync('git', args, { cwd: fixture, encoding: 'utf8' }); assert.equal(result.status, 0, result.stderr);
}
const receipts = [];
const argv = args => ['storage', 'layerfs', '--config', configFile, ...args];
function call(args, rejected = false) {
  const result = spawnSync(binary, argv(args), { encoding: 'utf8', timeout: 180000 });
  receipts.push({ args, status: result.status, stderr: result.stderr });
  if (rejected) { assert.notEqual(result.status, 0); return result.stderr; }
  assert.equal(result.status, 0, result.stderr); return JSON.parse(result.stdout);
}
try {
  const approval = call(['preview', '--repo', fixture]);
  call(['import', 'preview-proof', '--repo', fixture, '--revision', approval.commit, '--approval', approval.approval, '--credential-free']);
  call(['fork', 'preview-proof', 'draft-a']); call(['fork', 'preview-proof', 'draft-b']);
  const main = call(['main', 'preview-proof']);
  call(['exec', 'draft-a', '--', 'sh', '-c', 'printf draft-a > one.txt; printf draft-a > two.txt']);
  const first = call(['workspace-export', 'preview-proof', 'draft-a']);
  assert.equal(first.manifest.consistency, 'exclusive-command-boundary');
  assert.equal(await readFile(join(first.sourceRoot, 'one.txt'), 'utf8'), 'draft-a');
  assert.equal(call(['workspace-export', 'preview-proof', 'draft-a']).revision, first.revision);
  const other = call(['workspace-export', 'preview-proof', 'draft-b']);
  assert.equal(await readFile(join(other.sourceRoot, 'one.txt'), 'utf8'), 'base');
  call(['workspace-export', 'foreign-project', 'draft-a'], true);
  call(['dev-export', 'preview-proof', '--workspace', 'draft-a'], true);
  const writer = spawn(binary, argv(['exec', 'draft-a', '--', 'sh', '-c', 'printf next > one.txt; sleep 2; printf next > two.txt']), { stdio: ['ignore', 'pipe', 'pipe'] });
  const done = new Promise((resolve, reject) => { let stderr = ''; writer.stderr.on('data', part => { stderr += part; }); writer.on('error', reject); writer.on('exit', code => code === 0 ? resolve() : reject(new Error(stderr))); });
  const deadline = Date.now() + 10000;
  while (JSON.parse(await readFile(join(config.root, 'workspaces/draft-a.json'))).status !== 'exec') { assert(Date.now() < deadline); await new Promise(r => setTimeout(r, 10)); }
  assert.match(call(['workspace-export', 'preview-proof', 'draft-a'], true), /workspace-writer-busy/);
  await done;
  const next = call(['workspace-export', 'preview-proof', 'draft-a']);
  assert.notEqual(next.revision, first.revision);
  assert.equal(await readFile(join(next.sourceRoot, 'one.txt'), 'utf8'), 'next');
  assert.equal(await readFile(join(next.sourceRoot, 'two.txt'), 'utf8'), 'next');
  call(['exec', 'draft-a', '--', 'sh', '-c', 'mv one.txt renamed.txt; rm two.txt']);
  const renamed = call(['workspace-export', 'preview-proof', 'draft-a']);
  assert.deepEqual(renamed.manifest.entries.map(e => e.path), ['renamed.txt']);
  call(['exec', 'draft-a', '--', 'sh', '-c', 'printf synthetic > .env']);
  call(['workspace-export', 'preview-proof', 'draft-a'], true);
  assert.equal(await readFile(join(renamed.sourceRoot, 'renamed.txt'), 'utf8'), 'next');
  call(['exec', 'draft-a', '--', 'rm', '.env']);
  assert.deepEqual(call(['main', 'preview-proof']), main);
  const record = JSON.parse(await readFile(join(config.root, 'workspaces/draft-a.json')));
  assert.equal(record.status, 'ready'); assert.equal(record.last_checkpoint, null);
  await writeFile(evidence, JSON.stringify({ passed: true, platform: process.platform, arch: process.arch, node: process.version,
    liveLayerfs: true, commandBoundary: true, concurrentWriterRefused: true, completeBatch: true, independentDrafts: true,
    crossProjectRefused: true, closedExporterUnchanged: true, deletionRename: true, failedExportPreservesPrior: true,
    acceptedMainUnchanged: true, workspaceRemainsOpen: true, first: first.revision, next: next.revision, receipts }, null, 2));
  console.log(JSON.stringify({ passed: true, evidence, root: config.root }));
} catch (error) {
  await writeFile(evidence, JSON.stringify({ passed: false, error: error.message, receipts }, null, 2)); throw error;
}
