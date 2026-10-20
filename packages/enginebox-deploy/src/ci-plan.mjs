import { appendFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { check, version } from './lib.mjs';

export function plan(env) {
  const operation = env.ENGINEBOX_DEPLOY_OPERATION || 'update';
  check(['update', 'rollback', 'status'].includes(operation), 'invalid-deployment-operation');
  let pin = env.ENGINEBOX_DEPLOY_VERSION;
  if (operation === 'status') pin = '';
  else if (operation === 'rollback') version(pin);
  else if (!pin) {
    const base = version(env.ENGINEBOX_DEPLOY_VERSION_BASE || '0.3.1');
    check(!base.includes('-') && /^[1-9][0-9]*$/.test(env.GITHUB_RUN_NUMBER ?? ''), 'invalid-ci-version-base-or-run');
    pin = `${base}-live.${env.GITHUB_RUN_NUMBER}`;
  }
  if (pin) version(pin);
  return { operation, version: pin };
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const result = plan(process.env);
    check(process.env.GITHUB_OUTPUT, 'github-output-required');
    await appendFile(process.env.GITHUB_OUTPUT, `operation=${result.operation}\nversion=${result.version}\n`);
    process.stdout.write(`${JSON.stringify(result)}\n`);
  } catch { process.stderr.write('invalid-deployment-plan\n'); process.exitCode = 1; }
}
