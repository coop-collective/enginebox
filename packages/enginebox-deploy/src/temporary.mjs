import { mkdir, realpath, lstat, writeFile, rm } from 'node:fs/promises';
import { rmSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { check, json } from './lib.mjs';

async function identity(env) {
  check(typeof env.RUNNER_TEMP === 'string' && /^[1-9][0-9]*$/.test(env.GITHUB_RUN_ID) && /^[1-9][0-9]*$/.test(env.GITHUB_RUN_ATTEMPT), 'workflow-temporary-identity');
  return { root: await realpath(env.RUNNER_TEMP), run: env.GITHUB_RUN_ID, attempt: env.GITHUB_RUN_ATTEMPT };
}
export async function workflowTemporary(operation, env = process.env) {
  check(['create', 'check', 'cleanup'].includes(operation), 'temporary-operation');
  const owner = await identity(env);
  const directory = join(owner.root, `enginebox-deploy-${owner.run}-${owner.attempt}`);
  const marker = join(directory, 'owner.json');
  if (operation === 'create') {
    await mkdir(directory, { mode: 0o700 }); // Never adopt an existing directory.
    await writeFile(marker, JSON.stringify(owner), { mode: 0o600, flag: 'wx' });
  }
  let meta;
  try { meta = await lstat(directory); } catch (error) { if (operation === 'cleanup' && error.code === 'ENOENT') return; throw error; }
  check(meta.isDirectory() && !meta.isSymbolicLink() && meta.uid === process.getuid() && (meta.mode & 0o077) === 0, 'temporary-directory-owner');
  const saved = await json(marker);
  check(JSON.stringify(saved) === JSON.stringify(owner), 'temporary-marker-mismatch');
  if (operation === 'cleanup') await rm(directory, { recursive: true, force: true });
  return directory;
}
export function cleanupOnSignal(directory) {
  const handlers = new Map([['SIGINT', 130], ['SIGTERM', 143]].map(([signal, code]) => {
    const handler = () => { try { rmSync(directory, { recursive: true, force: true }); } finally { process.exit(code); } };
    process.once(signal, handler);
    return [signal, handler];
  }));
  return () => { for (const [signal, handler] of handlers) process.removeListener(signal, handler); };
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  workflowTemporary(process.argv[2]).catch(() => { process.stderr.write('workflow-temporary-cleanup-or-create-failed\n'); process.exitCode = 1; });
}
