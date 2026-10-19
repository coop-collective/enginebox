import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import { createHash } from 'node:crypto';
const mode = process.argv[2];
if (mode === 'metadata') {
  const result = {};
  for (const name of ['src/index.ts', 'dist/src/index.js', 'dist/tsconfig.tsbuildinfo', 'mtime-sentinel.txt']) {
    const s = fs.statSync(name, { bigint: true });
    result[name] = { size: String(s.size), mtimeNs: String(s.mtimeNs), sha256: createHash('sha256').update(fs.readFileSync(name)).digest('hex') };
  }
  console.log(JSON.stringify(result));
} else if (mode === 'mtime' || mode === 'mtime-set') {
  if (mode === 'mtime-set') fs.utimesSync('mtime-sentinel.txt', 1893456000, 1893456000);
  const s = fs.statSync('mtime-sentinel.txt', { bigint: true });
  console.log(JSON.stringify({ mtimeNs: String(s.mtimeNs), sha256: createHash('sha256').update(fs.readFileSync('mtime-sentinel.txt')).digest('hex'), requestedMtimeNs: '1893456000000000000' }));
} else if (mode === 'operations') {
  const out = {};
  fs.writeFileSync('atomic.new', 'renamed bytes\n');
  fs.renameSync('atomic.new', 'atomic.txt');
  assert.equal(fs.readFileSync('atomic.txt', 'utf8'), 'renamed bytes\n'); out.rename = true;
  fs.symlinkSync('atomic.txt', 'relative-link');
  assert.equal(fs.readFileSync('relative-link', 'utf8'), 'renamed bytes\n'); out.symlink = true;
  fs.linkSync('atomic.txt', 'hard-link');
  assert.equal(fs.statSync('atomic.txt').ino, fs.statSync('hard-link').ino); out.hardlink = true;
  try { fs.linkSync('/pnpm-store/marker.txt', 'external-hard-link'); out.crossVolumeHardlink = 'unexpected-success'; }
  catch (e) { out.crossVolumeHardlink = e.code; }
  assert.equal(out.crossVolumeHardlink, 'EXDEV');
  const fd = fs.openSync('atomic.txt', 'r+');
  try { fs.fsyncSync(fd); out.fsync = 'syscall-success-only-no-durability-claim'; } catch (e) { out.fsync = e.code; }
  fs.closeSync(fd);
  try { fs.readFileSync('/layerfs-store/global/store.sqlite'); out.rawStoreRead = 'unexpected-success'; }
  catch (e) { out.rawStoreRead = e.code; }
  assert.equal(out.rawStoreRead, 'EACCES');
  out.watchEvents = await new Promise(resolve => {
    const events = []; let watcher;
    try {
      watcher = fs.watch('.', (event, file) => { if (file === 'watch-probe.txt') events.push(event); });
      fs.writeFileSync('watch-probe.txt', 'event\n');
      setTimeout(() => { watcher.close(); resolve(events); }, 300);
    } catch (e) { watcher?.close(); resolve({ error: e.code }); }
  });
  out.watchQualification = 'observation only; events are not a required pass condition';
  console.log(JSON.stringify(out));
} else throw new Error('metadata|operations required');
