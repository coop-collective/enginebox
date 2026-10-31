import { appendFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { check, version } from './lib.mjs';

// Where a pipeline run goes. A push builds once, deploys that artifact to staging, gates it, then promotes the same
// artifact to production unless ENGINEBOX_PROD_HOLD is "true". A manual run may target one environment: an update
// straight to production skips staging (an emergency path, chosen explicitly), and rollback/status act on one.
const TARGETS = ['pipeline', 'staging', 'production'];

export function plan(env) {
  const operation = env.ENGINEBOX_DEPLOY_OPERATION || 'update';
  check(['update', 'rollback', 'status'].includes(operation), 'invalid-deployment-operation');
  const target = env.ENGINEBOX_DEPLOY_TARGET || (operation === 'update' ? 'pipeline' : 'production');
  check(TARGETS.includes(target), 'invalid-deployment-target');
  check(operation === 'update' || target !== 'pipeline', 'rollback-and-status-name-one-environment');
  let pin = env.ENGINEBOX_DEPLOY_VERSION;
  if (operation === 'status') pin = '';
  else if (operation === 'rollback') version(pin);
  else if (!pin) {
    const base = version(env.ENGINEBOX_DEPLOY_VERSION_BASE || '0.3.1');
    check(!base.includes('-') && /^[1-9][0-9]*$/.test(env.GITHUB_RUN_NUMBER ?? ''), 'invalid-ci-version-base-or-run');
    pin = `${base}-live.${env.GITHUB_RUN_NUMBER}`;
  }
  if (pin) version(pin);
  const hold = env.ENGINEBOX_PROD_HOLD === 'true';
  const staging = target === 'pipeline' || target === 'staging';
  // Production follows a passing staging gate in the pipeline; held, the artifact waits in staging.
  const production = target === 'production' || (target === 'pipeline' && !hold);
  return { operation, version: pin, target, staging, production, held: target === 'pipeline' && hold };
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const result = plan(process.env);
    check(process.env.GITHUB_OUTPUT, 'github-output-required');
    await appendFile(process.env.GITHUB_OUTPUT, `${Object.entries(result).map(([k, v]) => `${k}=${v}`).join('\n')}\n`);
    process.stdout.write(`${JSON.stringify(result)}\n`);
  } catch { process.stderr.write('invalid-deployment-plan\n'); process.exitCode = 1; }
}
