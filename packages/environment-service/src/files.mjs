// Apply verified source to a stable, read-only container bind mount. Only the
// controller writes this tree; writable runtime directories are separate tmpfs
// mounts, and persistent application data is mounted at /data.
import { lstat, realpath, readdir, mkdir, readlink, rename, unlink, rmdir, rm } from 'node:fs/promises';
import { createReadStream } from 'node:fs';
import { createHash, randomUUID } from 'node:crypto';
import { join, dirname } from 'node:path';
import { copyAccepted } from './accepted.mjs';
import { check, sourcePath } from './config.mjs';

const overlap = (a, b) => a === b || a.startsWith(`${b}/`) || b.startsWith(`${a}/`);
async function metadata(path) { try { return await lstat(path); } catch (error) { if (error.code === 'ENOENT') return null; throw error; } }
async function hash(path) { const h = createHash('sha256'); for await (const part of createReadStream(path)) h.update(part); return h.digest('hex'); }
function inventory(entries) {
  check(Array.isArray(entries) && entries.length <= 100000, 'invalid-managed-inventory');
  const result = new Map();
  for (const entry of entries) {
    check(sourcePath(entry.path) && ['file', 'symlink'].includes(entry.kind) && !result.has(entry.path), 'invalid-managed-path');
    result.set(entry.path, entry);
  }
  return result;
}

export async function writableDirectories(target, paths) {
  for (const path of paths) {
    let current = target;
    for (const part of path.split('/')) {
      current = join(current, part);
      const meta = await metadata(current);
      check(!meta || meta.isDirectory(), 'writable-directory-parent-invalid');
      if (!meta) await mkdir(current, { mode: 0o700 });
    }
  }
}

export async function syncFiles(source, target, previousEntries, writableDirs = [], beforeApply = async () => {}) {
  check(source.manifest && source.manifest.entries, 'files-update-requires-verified-source');
  check((await lstat(target)).isDirectory(), 'invalid-live-source-root');
  target = await realpath(target);
  const next = inventory(source.manifest.entries); const previous = inventory(previousEntries);
  const directories = paths => new Set([...paths].flatMap(path => { const parts = path.split('/'); return parts.slice(0, -1).map((_, i) => parts.slice(0, i + 1).join('/')); }));
  const nextDirs = directories(next.keys()); const previousDirs = directories(previous.keys());
  for (const path of next.keys()) check(!writableDirs.some(dir => overlap(path, dir)), 'source-overlaps-writable-directory');
  const stage = join(dirname(target), `.source-${randomUUID()}`);
  try {
    // Verify every byte and link before touching the running process's files.
    await copyAccepted(source, stage);
    const removed = new Set([...previous.keys()].filter(path => !next.has(path)));
    const changed = [];
    async function parents(path, removing = false) {
      let current = target; let relative = '';
      for (const part of path.split('/').slice(0, -1)) {
        current = join(current, part); relative = relative ? `${relative}/${part}` : part;
        const meta = await metadata(current);
        if (!meta) return false;
        if (!meta.isDirectory()) {
          check(removed.has(relative) || (removing && next.has(relative)), 'live-source-parent-not-directory');
          return true; // Never follow an old/new leaf through a parent symlink.
        }
      }
      return false;
    }
    let inspected = 0;
    async function ownedDirectory(path) {
      for (const entry of await readdir(join(target, path), { withFileTypes: true })) {
        check(++inspected <= 100000, 'live-directory-entry-limit');
        const relative = `${path}/${entry.name}`;
        if (entry.isDirectory()) await ownedDirectory(relative);
        else check(removed.has(relative), 'source-would-remove-unmanaged-file');
      }
    }
    const absent = new Set();
    for (const path of removed) {
      if (await parents(path, true)) { absent.add(path); continue; }
      const meta = await metadata(join(target, path));
      if (meta?.isDirectory() && nextDirs.has(path)) { absent.add(path); continue; }
      if (meta?.isDirectory() && previousDirs.has(path)) {
        await ownedDirectory(path); absent.add(path); continue;
      }
      check(!meta || meta.isFile() || meta.isSymbolicLink(), 'managed-file-became-directory');
    }
    for (const [path, entry] of next) {
      const blocked = await parents(path);
      let meta;
      try { if (!blocked) meta = await metadata(join(target, path)); }
      catch (error) { if (error.code !== 'ENOTDIR') throw error; }
      if (meta?.isDirectory()) await ownedDirectory(path);
      else check(!meta || previous.has(path), 'source-would-overwrite-unmanaged-file');
      const same = entry.kind === 'file'
        ? meta?.isFile() && meta.size === entry.size && (process.platform === 'win32' || !!(meta.mode & 0o111) === entry.executable) && await hash(join(target, path)) === entry.sha256
        : meta?.isSymbolicLink() && await readlink(join(target, path)) === entry.target;
      if (!same) changed.push(path);
    }
    // A write-ahead inventory lets the next poll safely resume a partial update
    // after a controller crash, including additions that reached disk.
    const pending = [...new Map([...previous, ...next]).values()];
    check(pending.length <= 100000, 'managed-update-inventory-limit');
    await beforeApply(pending);
    for (const path of [...removed].sort((a, b) => b.length - a.length)) {
      if (absent.has(path)) continue;
      try { await unlink(join(target, path)); } catch (error) { if (error.code !== 'ENOENT' && error.code !== 'ENOTDIR') throw error; }
      let parent = dirname(path);
      while (parent !== '.') {
        try { await rmdir(join(target, parent)); } catch (error) { if (!['ENOENT', 'ENOTEMPTY', 'EEXIST'].includes(error.code)) throw error; break; }
        parent = dirname(parent);
      }
    }
    // Add dependencies before changing files that may import them. Each rename
    // is atomic; batching/debouncing and recompilation belong to the dev server.
    changed.sort((a, b) => Number(previous.has(a)) - Number(previous.has(b)) || a.localeCompare(b));
    for (const path of changed) {
      const output = join(target, path);
      if ((await metadata(output))?.isDirectory()) await rmdir(output);
      await mkdir(dirname(output), { recursive: true, mode: 0o700 });
      await rename(join(stage, path), output);
    }
    return { written: changed.length, removed: removed.size };
  } finally { await rm(stage, { recursive: true, force: true }); }
}
