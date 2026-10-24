import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { writeFile, rename, readFile, lstat } from 'node:fs/promises';
import { join } from 'node:path';
import { setTimeout } from 'node:timers/promises';

export async function activatePreparedTree(request, control = '/run/enginebox-ci') {
  const id = randomUUID();
  await writeFile(join(control, 'requests/restore.tmp'), JSON.stringify({ ...request, request: id }), { flag: 'wx', mode: 0o600 });
  await rename(join(control, 'requests/restore.tmp'), join(control, 'requests/restore.json'));
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    try {
      const path = join(control, 'restored.json'), info = await lstat(path);
      assert(info.isFile() && !info.isSymbolicLink() && info.uid === 0 && (info.mode & 0o022) === 0 && info.size < 4096, 'Invalid root handoff receipt');
      const reply = JSON.parse(await readFile(path, 'utf8'));
      assert(reply.ok === true && reply.request === id && reply.key === request.key && reply.validation === 'protected-generation', 'Approved dependency handoff failed');
      return;
    } catch (e) { if (e.code !== 'ENOENT') throw e; }
    await setTimeout(10);
  }
  throw new Error('Approved dependency handoff timed out');
}
