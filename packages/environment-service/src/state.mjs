import { open, readFile, rename } from 'node:fs/promises';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';

export async function atomic(file, value) {
  const temporary = `${file}.${randomUUID()}.tmp`;
  const handle = await open(temporary, 'wx', 0o600);
  try { await handle.writeFile(JSON.stringify(value, null, 2)); await handle.sync(); } finally { await handle.close(); }
  await rename(temporary, file);
  // Windows does not support opening/flushing a directory this way.
  if (process.platform !== 'win32') {
    const parent = await open(join(file, '..'), 'r');
    try { await parent.sync(); } finally { await parent.close(); }
  }
}
export async function load(file, fallback) {
  try { return JSON.parse(await readFile(file, 'utf8')); } catch (error) { if (error.code === 'ENOENT') return fallback; throw error; }
}
