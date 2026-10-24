import assert from 'node:assert/strict';
import { readFile, appendFile, realpath, stat, writeFile, rename } from 'node:fs/promises';
import { join, relative, isAbsolute } from 'node:path';
import { setTimeout } from 'node:timers/promises';
import { pathToFileURL } from 'node:url';

export function identity(env, policy) {
  assert(env.RUNNER_NAME === policy.runner && env.GITHUB_REPOSITORY === policy.repository && env.GITHUB_REPOSITORY_ID === String(policy.repository_id), 'Assigned repository/runner differs from prepared profile');
  assert(policy.workflows.includes(env.GITHUB_WORKFLOW_REF), 'Assigned workflow is outside the prepared profile');
  assert(/^[1-9][0-9]{0,15}$/.test(env.GITHUB_RUN_ID) && /^[1-9][0-9]{0,8}$/.test(env.GITHUB_RUN_ATTEMPT), 'Missing actual run identity');
  return { schema: 1, repository_id: policy.repository_id, repository: policy.repository, runner: policy.runner, run_id: Number(env.GITHUB_RUN_ID), run_attempt: Number(env.GITHUB_RUN_ATTEMPT), workflow: env.GITHUB_WORKFLOW_REF };
}
export async function hook({ policy, env = process.env, verifyDirectory = '/run/enginebox-ci', mountinfo = '/proc/self/mountinfo', timeoutMs = 120_000 }) {
  const assigned = identity(env, policy);
  const root = await realpath(policy.root), work = await realpath(env.GITHUB_WORKSPACE);
  const rel = relative(join(root, 'work'), work);
  assert(rel && rel !== '..' && !rel.startsWith('../') && !isAbsolute(rel), 'Workspace is not on the prepared work root');
  // The host verifies the exact LayerFS attachment receipt. The guest additionally refuses an ordinary directory fallback.
  const mounts = await readFile(mountinfo, 'utf8');
  assert(mounts.split('\n').some(line => line.split(' ')[4] === root), 'Required LayerFS mount is missing');
  assert((await stat(root)).dev === (await stat(work)).dev, 'Workspace filesystem differs from LayerFS mount');
  await writeFile(join(verifyDirectory, 'requests/identity.tmp'), JSON.stringify(assigned), { flag: 'wx', mode: 0o600 });
  await rename(join(verifyDirectory, 'requests/identity.tmp'), join(verifyDirectory, 'requests/identity.json'));
  const deadline = Date.now() + timeoutMs;
  let verified;
  while (Date.now() < deadline) {
    try { verified = JSON.parse(await readFile(join(verifyDirectory, 'verified.json'), 'utf8')); break; }
    catch (e) { if (e.code !== 'ENOENT') throw e; }
    await setTimeout(250);
  }
  assert(verified?.verified === true && verified.run_id === assigned.run_id && verified.run_attempt === assigned.run_attempt && verified.repository_id === assigned.repository_id && Number.isSafeInteger(verified.job_id), 'Host could not verify the assigned job');
  const config = { ENGINEBOX_PREPARATION_READY: '1', PNPM_CONFIG_STORE_DIR: join(root, 'caches/packages'), PNPM_CONFIG_CACHE_DIR: join(root, 'caches/metadata'), PNPM_CONFIG_FROZEN_STORE: 'false', npm_config_cache: join(root, 'caches/npm') };
  await appendFile(env.GITHUB_ENV, Object.entries(config).map(([k, v]) => `${k}=${v}\n`).join(''));
  return { schema: 1, prepared: true, ...assigned, job_id: verified.job_id };
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    const policy = JSON.parse(await readFile('/run/enginebox-ci/policy.json', 'utf8'));
    const receipt = await hook({ policy });
    console.log(`Enginebox prepared LayerFS workspace for job ${receipt.job_id}.`);
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}
