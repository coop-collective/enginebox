// Content/mode oracle. Run outside timed fork/mount intervals; reading every file
// warms caches. Inode identities and timestamps are observations, not equality keys.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import * as fs from 'node:fs';
import { join, resolve } from 'node:path';

const root = resolve(process.argv[2] ?? '.');
const entries = [];
const inodes = new Map();
let logicalBytes = 0;
function walk(relative = '') {
  for (const name of fs.readdirSync(join(root, relative)).sort()) {
    const path = relative ? `${relative}/${name}` : name;
    const full = join(root, path);
    const stat = fs.lstatSync(full);
    const entry = { path, mode: stat.mode & 0o777 };
    if (stat.isDirectory()) {
      entries.push({ ...entry, kind: 'directory' }); walk(path);
    } else if (stat.isSymbolicLink()) {
      entries.push({ ...entry, kind: 'symlink', target: fs.readlinkSync(full) });
    } else if (stat.isFile()) {
      logicalBytes += stat.size;
      entries.push({ ...entry, kind: 'file', size: stat.size, sha256: createHash('sha256').update(fs.readFileSync(full)).digest('hex') });
      const key = `${stat.dev}:${stat.ino}`;
      if (!inodes.has(key)) inodes.set(key, { bytes: stat.size, paths: [] });
      inodes.get(key).paths.push(path);
    } else throw new Error(`unsupported fixture file: ${path}`);
  }
}
walk();
const summary = {
  sha256: createHash('sha256').update(JSON.stringify(entries)).digest('hex'),
  files: entries.filter(e => e.kind === 'file').length,
  directories: entries.filter(e => e.kind === 'directory').length,
  symlinks: entries.filter(e => e.kind === 'symlink').length,
  logicalBytes,
  uniqueInodeBytes: [...inodes.values()].reduce((n, value) => n + value.bytes, 0),
  hardlinkGroups: [...inodes.values()].filter(value => value.paths.length > 1),
};
if (process.env.EXPECTED_MANIFEST_SHA256) assert.equal(summary.sha256, process.env.EXPECTED_MANIFEST_SHA256, 'fixture content/mode/symlink manifest differs');
if (process.argv[3]) fs.writeFileSync(process.argv[3], JSON.stringify({ summary, entries }, null, 2));
console.log(JSON.stringify(summary));
