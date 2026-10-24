// Private diagnostic probe of the actual LayerFS SDK, not an Enginebox backend.
// Each invocation runs one bounded stage. Receipts are append-only; failures stay.
import assert from 'node:assert/strict';
import { appendFileSync, existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import http from 'node:http';

const [stage = 'init'] = process.argv.slice(2);
const data = process.env.PROBE_DATA || '/data';
const project = process.env.PROBE_PROJECT || 'unreal-sample';
const source = process.env.PROBE_SOURCE || `${data}/sample`;
const report = `${data}/${project}.jsonl`;
const statePath = `${data}/${project}-state.json`;
const store = process.env.LAYERFS_STORE_PATH;
assert(store && process.env.LAYERFS_CONTAINER_PREFIX?.startsWith('ebox-lfu-'));
mkdirSync(process.env.LAYERFS_HOME, { recursive: true, mode: 0o700 });
const { LayerFS } = await import('/src/sdk/js/src/index.js');
const lf = new LayerFS(project, { storePath: store });
const json = value => JSON.stringify(value, (_, v) => typeof v === 'bigint' ? v.toString() : v);
function record(value) {
  const row = { at: new Date().toISOString(), stage, ...value };
  appendFileSync(report, json(row) + '\n');
  console.log(json(row));
}
function usage() {
  return Object.fromEntries(['', '-wal', '-shm'].map(suffix => {
    const path = store + suffix;
    const s = existsSync(path) ? statSync(path) : null;
    return [suffix || 'db', { bytes: s?.size || 0, allocated: (s?.blocks || 0) * 512 }];
  }));
}
async function measure(name, fn) {
  const begin = performance.now(), before = usage();
  try {
    const value = await fn();
    record({ name, ok: true, seconds: (performance.now() - begin) / 1000,
      storageBefore: before, storageAfter: usage(), value });
    return value;
  } catch (error) {
    record({ name, ok: false, seconds: (performance.now() - begin) / 1000,
      error: { code: error.code, message: error.message, detail: error.detail }, storageAfter: usage() });
    throw error;
  }
}
const quote = s => `'${s.replaceAll("'", "'\\''")}'`;
async function py(ws, script) {
  const r = await lf.workspace.exec(ws, `timeout 90s python3 -c ${quote(script)}`);
  assert.equal(r.exitCode ?? r.exit_code, 0, r.stderr || json(r));
  return JSON.parse(r.stdout);
}
const hashCode = `import hashlib,json,pathlib
result={}
for p in sorted(pathlib.Path('.').rglob('*')):
 if p.is_file():
  h=hashlib.sha256()
  with p.open('rb') as f:
   for b in iter(lambda:f.read(1048576),b''): h.update(b)
  result[str(p)]={'bytes':p.stat().st_size,'sha256':h.hexdigest()}
print(json.dumps(result))`;
async function verify(ws, expected) {
  const actual = await py(ws, hashCode);
  assert.deepEqual(actual, expected);
  return { files: Object.keys(actual).length,
    bytes: Object.values(actual).reduce((n, x) => n + x.bytes, 0), hashesMatch: true };
}
async function present(ref, path) {
  try { await lf.fs.stat(ref, path); return true; }
  catch (error) { if (error.code === 'NoSuchPath') return false; throw error; }
}
async function boundSandbox(ws) {
  const name = `${process.env.LAYERFS_CONTAINER_PREFIX}${project}-${ws.sandbox}`;
  const body = json({ NanoCpus: 2_000_000_000, Memory: 3 * 1024 ** 3, MemorySwap: 3 * 1024 ** 3 });
  await new Promise((resolve, reject) => {
    const req = http.request({ socketPath: '/var/run/docker.sock',
      path: `/v1.54/containers/${encodeURIComponent(name)}/update`, method: 'POST',
      headers: { 'content-type': 'application/json', 'content-length': Buffer.byteLength(body) } }, res => {
      let out = ''; res.on('data', c => out += c); res.on('end', () =>
        res.statusCode === 200 ? resolve() : reject(Error(`${res.statusCode}: ${out}`)));
    }); req.on('error', reject); req.end(body);
  });
}
const state = existsSync(statePath) ? JSON.parse(readFileSync(statePath)) : {};
function save() { writeFileSync(statePath, json(state)); }
const fixture = JSON.parse(readFileSync(`${source}.json`));
try {
  if (stage === 'init') {
    record({ name: 'identity', upstream: 'v1.0.0 / 2aaf2552ce21154928b9f9c83582e41a637f5910',
      node: process.version, arch: process.arch, image: process.env.LAYERFS_IMAGE,
      files: fixture.files, bytes: fixture.bytes, git: false,
      cacheState: fixture.cache_state, fullEngineLoadTest: false });
    await measure('initialize', () => lf.project.init(source));
  } else if (stage === 'forks') {
    await measure('fork-a', () => lf.branch.fork('agent-a', 'main'));
    await measure('fork-b', () => lf.branch.fork('agent-b', 'main'));
    state.a = await measure('mount-a', () => lf.workspace.mount('agent-a')); save();
  } else if (stage === 'read') {
    await boundSandbox(state.a);
    state.b = await measure('mount-b', () => lf.workspace.mount('agent-b')); save();
    await measure('read-and-hash-a', () => verify(state.a, fixture.entries));
    await measure('read-and-hash-b', () => verify(state.b, fixture.entries));
  } else if (stage === 'edit') {
    const asset = Object.keys(fixture.entries).find(p => p.includes('/Geometry/')) ||
      Object.keys(fixture.entries).find(p => p.includes('Geometry/'));
    assert(asset);
    const script = `import pathlib,hashlib,json,shutil,os
p=pathlib.Path(${JSON.stringify(asset)})
t=p.with_name(p.name+'.new')
shutil.copyfile(p,t)
with t.open('r+b') as f: f.seek(12345); f.write(b'X'*4096)
os.replace(t,p)
pathlib.Path('agent-a.txt').write_text('agent a\\n')
h=hashlib.sha256()
with p.open('rb') as f:
 for b in iter(lambda:f.read(1048576),b''): h.update(b)
print(json.dumps({'path':str(p),'bytes':p.stat().st_size,'sha256':h.hexdigest()}))`;
    state.change = await measure('storage-only-4KiB-edit-via-rename', () => py(state.a, script)); save();
    await measure('commit-a', () => lf.workspace.commit(state.a, 'Storage probe: binary delta'));
    await measure('sibling-isolation', () => verify(state.b, fixture.entries));
    await measure('edit-b', () => py(state.b, "import pathlib,json; pathlib.Path('agent-b.txt').write_text('agent b\\n'); print(json.dumps({'written':True}))"));
    await measure('commit-b', () => lf.workspace.commit(state.b, 'Independent text edit'));
    await measure('unmount-a', () => lf.workspace.unmount(state.a));
    await measure('unmount-b', () => lf.workspace.unmount(state.b));
    delete state.a; delete state.b; save();
  } else if (stage === 'merge') {
    await measure('merge-disjoint-files', () => lf.branch.merge('agent-a', 'agent-b', 'integrated', 'Combine agents'));
    await measure('publish', () => lf.branch.publish('integrated'));
    await measure('fork-reopen', () => lf.branch.fork('reopened', 'main'));
    state.reopen = await measure('mount-reopen', () => lf.workspace.mount('reopened')); save();
    await boundSandbox(state.reopen);
    const expected = structuredClone(fixture.entries);
    const { path, ...entry } = state.change; expected[path] = entry;
    const { createHash } = await import('node:crypto');
    for (const who of ['a','b']) {
      const text = `agent ${who}\n`;
      expected[`agent-${who}.txt`] = { bytes: Buffer.byteLength(text), sha256: createHash('sha256').update(text).digest('hex') };
    }
    await measure('verify-published-bytes', () => verify(state.reopen, expected));
    await measure('unmount-reopen', () => lf.workspace.unmount(state.reopen));
    delete state.reopen; state.expected = expected; save();
    await lf.branch.fork('conflict-a', 'main'); await lf.branch.fork('conflict-b', 'main');
    await lf.fs.write('conflict-a', '/task.json', '{"version":2}');
    await lf.fs.write('conflict-b', '/task.json', '{"version":3}');
    await measure('same-file-conflict-refused', async () => {
      await assert.rejects(lf.branch.merge('conflict-a', 'conflict-b', 'conflicted'), { code: 'Conflict' });
      return { refused: true };
    });
  } else if (stage === 'backup') {
    const script = `import sqlite3,json,os,pathlib,time
src=sqlite3.connect('file:'+${JSON.stringify(store)}+'?mode=ro',uri=True)
dest=${JSON.stringify(data + '/' + project + '-backup.sqlite')}
assert not pathlib.Path(dest).exists()
dst=sqlite3.connect(dest)
src.backup(dst,pages=256)
ok=dst.execute('pragma integrity_check').fetchall()
assert ok==[('ok',)],ok
assert dst.execute('pragma foreign_key_check').fetchall()==[]
dst.close();src.close()
fd=os.open(dest,os.O_RDONLY);os.fsync(fd);os.close(fd)
d=os.open(str(pathlib.Path(dest).parent),os.O_RDONLY);os.fsync(d);os.close(d)
print(json.dumps({'integrity':ok,'bytes':pathlib.Path(dest).stat().st_size,'backup':dest}))`;
    await measure('sqlite-online-backup', async () => {
      const p = spawnSync('python3', ['-c', script], { encoding: 'utf8', timeout: 100000 });
      assert.equal(p.status, 0, p.stderr); return JSON.parse(p.stdout);
    });
    const restored = new LayerFS(project, { storePath: `${data}/${project}-backup.sqlite` });
    await measure('backup-sdk-history-read', async () => {
      const r = await restored.branch.list_commits('integrated', 100);
      const t = await restored.fs.read('main', '/agent-a.txt');
      assert.match(json(t), /agent a/); return { commits: r.items.length, contentRead: true };
    });
    await restored.close();
  } else if (stage === 'restore-read') {
    state.restored = await measure('mount-from-restored-volume', () => lf.workspace.mount('reopened')); save();
    await boundSandbox(state.restored);
    await measure('full-hash-check-from-backup', () => verify(state.restored, state.expected));
    await measure('unmount-restored-workspace', () => lf.workspace.unmount(state.restored));
    delete state.restored; save();
  } else if (stage === 'stale-publication') {
    await lf.branch.fork('stale-a', 'main');
    await lf.branch.fork('stale-b', 'main');
    await lf.branch.fork('unchanged-observer', 'main');
    state.observer = await lf.workspace.mount('unchanged-observer'); save();
    await boundSandbox(state.observer);
    await lf.fs.write('stale-a', '/task.json', '{"version":4}');
    await lf.fs.write('stale-a', '/publish-a.txt', 'a');
    await lf.fs.write('stale-b', '/publish-b.txt', 'b');
    await measure('publish-a-from-baseline', () => lf.branch.publish('stale-a'));
    await measure('unchanged-branch-and-workspace-stay-pinned', async () => {
      const branchSeesA = await present('unchanged-observer', '/publish-a.txt');
      const workspace = await py(state.observer, "import pathlib,json; print(json.dumps({'seesA':pathlib.Path('publish-a.txt').exists(),'task':json.loads(pathlib.Path('task.json').read_text())}))");
      assert.equal(branchSeesA, false); assert.equal(workspace.seesA, false);
      assert.equal(workspace.task.version, 1);
      await lf.branch.fork('new-after-a', 'main');
      const newBranchSeesA = await present('new-after-a', '/publish-a.txt');
      assert.equal(newBranchSeesA, true);
      return { branchSeesA, workspace, newBranchSeesA };
    });
    await measure('publish-b-from-same-old-baseline', async () => {
      const publication = await lf.branch.publish('stale-b');
      const previousPresent = await present('main', '/publish-a.txt');
      const task = await lf.fs.read('main', '/task.json');
      assert.equal(previousPresent, false);
      assert.equal(await present('main', '/publish-b.txt'), true);
      assert.match(json(task), /version\\\":1/);
      return { publication, previousPresent, task };
    });
    await lf.workspace.unmount(state.observer); delete state.observer; save();
    await measure('merge-after-overwrite-does-not-undo-the-overwrite', async () => {
      const merged = await lf.branch.merge('stale-a', 'main', 'reconciled');
      assert.equal(await present('reconciled', '/publish-a.txt'), false);
      assert.equal(await present('reconciled', '/publish-b.txt'), true);
      return { merged, previousPresent: false };
    });
  } else if (stage === 'publication-recovery') {
    await measure('merge-b-with-main-as-it-was-before-b-published', async () => {
      const merged = await lf.branch.merge('stale-b', 'new-after-a', 'checked-before-publication');
      assert.equal(await present('checked-before-publication', '/publish-a.txt'), true);
      assert.equal(await present('checked-before-publication', '/publish-b.txt'), true);
      return { merged, bothPresent: true };
    });
    await measure('recover-from-original-source-branches', async () => {
      const merged = await lf.branch.merge('stale-a', 'stale-b', 'recovered-source-branches');
      assert.equal(await present('recovered-source-branches', '/publish-a.txt'), true);
      assert.equal(await present('recovered-source-branches', '/publish-b.txt'), true);
      const publication = await lf.branch.publish('recovered-source-branches');
      return { merged, publication, bothPresent: true };
    });
  } else if (stage === 'shared-branch') {
    await lf.branch.fork('shared-branch', 'main');
    state.sharedA = await lf.workspace.mount('shared-branch'); save();
    await boundSandbox(state.sharedA);
    state.sharedB = await lf.workspace.mount('shared-branch'); save();
    await measure('same-branch-commit-a', async () => {
      await py(state.sharedA, "import pathlib,json; pathlib.Path('shared-a.txt').write_text('a'); print(json.dumps({'written':True}))");
      return lf.workspace.commit(state.sharedA);
    });
    await measure('same-branch-b-before-commit', async () => {
      const result = await py(state.sharedB, "import pathlib,json; print(json.dumps({'seesA':pathlib.Path('shared-a.txt').exists()}))");
      assert.equal(result.seesA, false); return result;
    });
    await measure('same-branch-commit-b-merges-a', async () => {
      await py(state.sharedB, "import pathlib,json; pathlib.Path('shared-b.txt').write_text('b'); print(json.dumps({'written':True}))");
      const commit = await lf.workspace.commit(state.sharedB);
      const result = await py(state.sharedB, "import pathlib,json; print(json.dumps({'seesA':pathlib.Path('shared-a.txt').exists(),'seesB':pathlib.Path('shared-b.txt').exists()}))");
      assert.equal(result.seesA, true); assert.equal(result.seesB, true);
      return { commit, ...result };
    });
    await lf.workspace.unmount(state.sharedA); await lf.workspace.unmount(state.sharedB);
    delete state.sharedA; delete state.sharedB; save();
  } else throw Error(`Unknown stage: ${stage}`);
} finally { await lf.close(); }
