import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { hook, identity } from '../src/hook.mjs';
const workflow = 'fixture/repo/.github/workflows/ci.yml@refs/heads/main';
const policy = { repository: 'fixture/repo', repository_id: 101, runner: 'ebox-ci-test', workflows: [workflow] };
const env = { RUNNER_NAME: policy.runner, GITHUB_REPOSITORY: policy.repository, GITHUB_REPOSITORY_ID: '101', GITHUB_WORKFLOW_REF: workflow, GITHUB_RUN_ID: '55', GITHUB_RUN_ATTEMPT: '2' };
test('preparation binds repository and workflow, independently of the triggering demand', () => {
  assert.equal(identity(env, policy).run_id, 55);
  assert.throws(() => identity({ ...env, GITHUB_REPOSITORY_ID: '102' }, policy), /Assigned/);
  assert.throws(() => identity({ ...env, GITHUB_WORKFLOW_REF: workflow.replace('/main', '/unreviewed') }, policy), /workflow/);
});
test('hook requires mount and host acknowledgement before exporting package paths', { skip: process.platform === 'win32' }, async t => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-hook-')); t.after(() => rm(root, { recursive: true, force: true }));
  const realRoot = await (await import('node:fs/promises')).realpath(root);
  const work = join(realRoot, 'work/repo/repo'), control = join(root, 'control'), mountinfo = join(root, 'mountinfo'), envfile = join(root, 'env');
  await mkdir(work, { recursive: true }); await mkdir(join(control, 'requests'), { recursive: true });
  await writeFile(envfile, ''); await writeFile(mountinfo, '');
  const args = { policy: { ...policy, root: realRoot }, env: { ...env, GITHUB_WORKSPACE: work, GITHUB_ENV: envfile }, verifyDirectory: control, mountinfo, timeoutMs: 10 };
  await assert.rejects(hook(args), /mount is missing/); assert.equal(await readFile(envfile, 'utf8'), '');
  await writeFile(mountinfo, `1 0 0:1 / ${realRoot} rw - fixture fixture rw\n`);
  await assert.rejects(hook(args), /could not verify/); assert.equal(await readFile(envfile, 'utf8'), '');
  await rm(join(control, 'requests/identity.json'));
  await writeFile(join(control, 'verified.json'), JSON.stringify({ verified: true, run_id: 55, run_attempt: 2, repository_id: 101, job_id: 999 }));
  assert.equal((await hook(args)).job_id, 999);
  const exported = await readFile(envfile, 'utf8'); assert.match(exported, /ENGINEBOX_PREPARATION_READY=1/); assert.match(exported, /caches\/packages/); assert.doesNotMatch(exported, /GITHUB_WORKSPACE=/);
});
