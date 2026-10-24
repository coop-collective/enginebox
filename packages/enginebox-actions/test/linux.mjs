// Isolated Linux integration fixture. No developer state, credentials or Docker socket enters it.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { resolve } from 'node:path';
const owner = `ebox-preparation-${randomUUID()}`, image = `${owner}:test`, container = `${owner}-test`;
const root = resolve(import.meta.dirname, '../..');
function docker(args, capture = false) {
  const result = spawnSync('docker', args, { stdio: capture ? 'pipe' : 'inherit', encoding: 'utf8', timeout: 600_000 });
  assert(!result.error && result.status === 0, 'Disposable preparation fixture failed');
  return result.stdout;
}
try {
  docker(['build', '--label', `enginebox.preparation.proof=${owner}`, '-t', image, import.meta.dirname]);
  docker(['run', '--rm', '--name', container, '--label', `enginebox.preparation.proof=${owner}`, '--network=none', '--cap-add=SYS_ADMIN', '--security-opt=apparmor=unconfined', '--cpus=2', '--memory=2g', '--pids-limit=256',
    '--mount', `type=bind,src=${root}/enginebox-actions,dst=/packages/enginebox-actions,readonly`,
    '--mount', `type=bind,src=${root}/enginebox-preparation,dst=/packages/enginebox-preparation,readonly`,
    '-e', 'ENGINEBOX_TEST_PREPARATION_BROKER=1', image, 'sh', '-c', 'node --test /packages/enginebox-actions/test/*.test.mjs /packages/enginebox-preparation/test/*.test.mjs']);
} finally {
  // Only exact resources with this invocation's unpredictable ownership label are eligible.
  for (const [kind, name] of [['container', container], ['image', image]]) {
    const result = spawnSync('docker', [kind, 'inspect', name], { encoding: 'utf8', timeout: 30_000 });
    if (result.status === 0) {
      const object = JSON.parse(result.stdout)[0];
      if (object.Config?.Labels?.['enginebox.preparation.proof'] === owner) docker([kind, 'rm', ...(kind === 'container' ? ['-f'] : []), name]);
    }
  }
}
