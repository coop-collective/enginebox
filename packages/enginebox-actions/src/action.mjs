import { activatePreparedTree } from './broker.mjs';
import assert from 'node:assert/strict';
import { appendFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { prepare, json, inside } from '../../enginebox-preparation/src/pnpm.mjs';
try {
  const env = process.env;
  assert(env.ENGINEBOX_PREPARATION_READY === '1', 'Enginebox runner preparation is required');
  const policy = await json('/run/enginebox-ci/policy.json');
  assert(policy.protected_preparation === true, 'Root-protected preparation is required');
  assert(env.ENGINEBOX_ACTION_SCRIPTS === policy.recipe.scripts, 'Action install-scripts differs from runner policy');
  for (const key of ['ENGINEBOX_ACTION_REQUIRE_HIT', 'ENGINEBOX_ACTION_OFFLINE']) assert(['true', 'false'].includes(env[key]), `Invalid ${key}`);
  assert(env.GITHUB_REPOSITORY_ID === String(policy.repository_id), 'Repository does not match preparation');
  const source = await inside(env.GITHUB_WORKSPACE, resolve(env.GITHUB_WORKSPACE, env.ENGINEBOX_ACTION_PATH || '.'));
  const receipt = await prepare({ source, root: policy.root, policy: policy.recipe, activate: activatePreparedTree, offline: env.ENGINEBOX_ACTION_OFFLINE === 'true', requireHit: env.ENGINEBOX_ACTION_REQUIRE_HIT === 'true' });
  await appendFile(env.GITHUB_OUTPUT, `mode=${receipt.mode}\nkey=${receipt.key}\n`);
  if (env.GITHUB_STEP_SUMMARY) await appendFile(env.GITHUB_STEP_SUMMARY, `Enginebox dependencies: **${receipt.mode}** (${receipt.reason}); ${Math.round(receipt.elapsedMs)} ms.\n`);
  console.log(JSON.stringify(receipt));
} catch (error) { console.error(error.message); process.exitCode = 1; }
