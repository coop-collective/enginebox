// Runs only in an isolated, read-only helper with explicitly selected volumes.
// File metadata observations are not unique physical/CoW storage accounting.
import { lstat, opendir, realpath } from 'node:fs/promises';
import { resolve, sep } from 'node:path';

export async function measure(roots, { maxEntries = 100_000, maxMs = 5_000 } = {}) {
  const deadline = performance.now() + maxMs;
  let entries = 0;
  const results = [];
  for (const root of roots) {
    const result = { status: 'complete', apparent_bytes: 0, allocated_bytes: 0, files: 0 };
    const seen = new Set();
    try {
      const base = await realpath(root);
      const walk = async (path) => {
        if (++entries > maxEntries || performance.now() >= deadline) throw new Error('limit');
        const stat = await lstat(path);
        if (stat.isSymbolicLink()) return;
        // Refuse escapes if a concurrently modified directory changes to a link.
        const actual = await realpath(path);
        if (actual !== base && !actual.startsWith(base + sep)) throw new Error('escape');
        if (stat.isDirectory()) {
          const dir = await opendir(path);
          for await (const entry of dir) await walk(resolve(path, entry.name));
        } else if (stat.isFile()) {
          const key = `${stat.dev}:${stat.ino}`;
          if (seen.has(key)) return;
          seen.add(key);
          result.apparent_bytes += stat.size;
          result.allocated_bytes += stat.blocks * 512;
          result.files++;
        }
      };
      await walk(base);
    } catch {
      result.status = result.files ? 'partial' : 'unavailable';
      if (!result.files) result.apparent_bytes = result.allocated_bytes = result.files = null;
    }
    results.push(result);
  }
  return results;
}

// stdin mode keeps the embedded code immutable and paths out of shell text.
if (process.argv[1] === undefined) {
  const chunks = [];
  for await (const chunk of process.stdin) chunks.push(chunk);
  const roots = JSON.parse(Buffer.concat(chunks).toString());
  console.log(JSON.stringify(await measure(roots)));
}
