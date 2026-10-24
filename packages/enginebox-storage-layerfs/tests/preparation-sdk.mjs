// Opt-in product integration proof. Run only in a disposable Linux container with
// a fresh /state and /layerfs-store volume and the exact pinned native SDK.
import assert from 'node:assert/strict';
import { mkdir, writeFile, copyFile, chmod, readFile } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import { DatabaseSync, backup } from 'node:sqlite';
const sdk = process.env.ENGINEBOX_TEST_SDK;
assert(process.platform === 'linux' && sdk && process.env.ENGINEBOX_DISPOSABLE_PROOF === '1');
const { LayerFS } = await import(`${sdk}/src/index.js`);
const project = 'preparation-fixture', key = 'a'.repeat(64);
await mkdir('/state/source/prepared/' + key, { recursive: true });
await mkdir('/state/source/caches', { recursive: true });
await writeFile(`/state/source/prepared/${key}/manifest.json`, JSON.stringify({ key, inputs: { recipe: 'pnpm-installed-v1', policy: { scripts: 'deny' } } }));
process.env.LAYERFS_HOME = '/state/original-home';
const original = new LayerFS(project, { storePath: '/state/original.sqlite' });
await original.project.init('/state/source');
await copyFile(`/state/original-home/${project}/sealed.sqlite`, '/state/original.sqlite');
const before = await original.fs.read('main', `/prepared/${key}/manifest.json`);
assert.equal(JSON.parse(before.text).key, key);
await original.close();
await mkdir('/layerfs-store/global', { recursive: true });
const db = new DatabaseSync('/state/original.sqlite', { readOnly: true });
await backup(db, '/layerfs-store/global/store.sqlite'); db.close();
const request = { operation: 'preparation-inspect', sdk, project, key, home: '/state/restored-home', volume: 'fixture-unused', prefix: 'fixture-unused', image: 'fixture-unused' };
function inspect() {
  const output = spawnSync(process.execPath, ['/product/preparation-bridge.mjs'], { input: JSON.stringify(request), encoding: 'utf8', timeout: 30_000 });
  assert.equal(output.status, 0, output.stderr); return JSON.parse(output.stdout);
}
assert.equal(inspect().ok, false, 'Missing native discovery must not qualify a restore');
await mkdir(`/state/restored-home/${project}`, { recursive: true, mode: 0o700 });
await copyFile(`/state/original-home/${project}/project.record`, `/state/restored-home/${project}/project.record`);
await chmod(`/state/restored-home/${project}/project.record`, 0o600);
assert.equal(inspect().ok, true, 'Restored Store must be readable through the production bridge');
process.env.LAYERFS_HOME = '/state/restored-home';
const restored = new LayerFS(project, { storePath: '/layerfs-store/global/store.sqlite' });
await restored.branch.fork('job-one', 'main'); await restored.branch.fork('job-two', 'main');
await restored.fs.write('job-one', '/caches/job-only.txt', 'private first job');
await assert.rejects(restored.fs.read('job-two', '/caches/job-only.txt'));
await assert.rejects(restored.fs.read('main', '/caches/job-only.txt'));
assert.equal(JSON.parse((await restored.fs.read('job-two', `/prepared/${key}/manifest.json`)).text).key, key);
await restored.close();
console.log(JSON.stringify({ passed: true, nativeStoreReopened: true, productionPreparationBridge: true, missingRecordRefused: true, independentBranches: true, incusAttachment: false, liveGitHubJob: false }));
