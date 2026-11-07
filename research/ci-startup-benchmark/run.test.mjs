import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtemp, mkdir, readFile, writeFile, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { configuration, runBenchmark, submittedRun } from './run.mjs';

function measurement(config, mode) {
  return { schema: 1,
    identity: { repositoryId: '1', sourceCommit: config.expectedSha, sourceTree: 'b'.repeat(40), lockSha256: 'c'.repeat(64), node: 'v24.14.0', pnpm: '12.15.0', platform: 'linux', arch: 'x64', runnerImage: 'image-digest', resources: '4CPU/4GiB', installPolicy: 'frozen-lockfile,copy,allow-esbuild-workerd' },
    cache: { mode, kind: 'pnpm-store', key: 'seed-key', hit: mode === 'warm', downloadedPackages: null },
    firstStepAt: '2026-11-02T00:00:03Z',
    stages: {
      dependencies: { command: ['pnpm', 'install', '--frozen-lockfile', '--store-dir', '/opt/enginebox-benchmark/seed/store', '--package-import-method', 'copy', '--reporter', 'ndjson'], startedAt: '2026-11-02T00:00:04Z', completedAt: '2026-11-02T00:00:10Z', elapsedMs: mode === 'cold' ? 6000 : 1000, exitCode: 0 },
      integration: { command: ['pnpm', 'run', 'benchmark:integration'], startedAt: '2026-11-02T00:00:11Z', completedAt: '2026-11-02T00:00:19Z', elapsedMs: 8000, exitCode: 0 },
    },
  };
}

async function fixture(t, scenario = {}) {
  const directory = await mkdtemp(join(tmpdir(), 'enginebox-benchmark-run-'));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const config = configuration({ output: join(directory, 'output'), readinessTimeoutSeconds: 1, runTimeoutSeconds: 1, pollSeconds: 1 });
  const calls = [], submissions = [], polls = new Map();
  let clock = Date.parse('2026-11-02T00:00:00Z');
  const gh = async args => {
    calls.push(args);
    if (args[0] === 'workflow') {
      const mode = args.find(value => value.startsWith('cache_mode=')).split('=')[1];
      const sample = args.find(value => value.startsWith('sample=')).split('=')[1];
      const intent = JSON.parse(await readFile(join(config.output, `${mode}-${sample}`, 'intent.json')));
      assert.equal(intent.mode, mode, 'Intent must exist before any dispatch');
      submissions.push({ mode, sample, id: 1000 + submissions.length });
      if (scenario.submitError) throw new Error('simulated lost response');
      if (scenario.ambiguous) return 'Created workflow_dispatch event, no run ID returned';
      return `https://github.com/${config.repository}/actions/runs/${submissions.at(-1).id}\n`;
    }
    if (args[0] === 'api') {
      const path = args[1];
      if (path.includes('/commits/')) return JSON.stringify({ sha: scenario.wrongRef ? 'd'.repeat(40) : config.expectedSha });
      if (path.includes('/runner-groups/')) return JSON.stringify({ total_count: 1, runners: [{ id: 42, name: 'ebox-ci-fixture', status: scenario.notReady ? 'offline' : 'online', busy: false, labels: [{ name: config.runnerLabel }] }] });
      const submission = submissions.at(-1);
      if (path.endsWith('/jobs?per_page=100')) return JSON.stringify({ total_count: 1, jobs: [{ id: submission.id + 5000, name: `startup-cache-${submission.mode}-${submission.sample}` }] });
      if (path.includes('/actions/jobs/')) return JSON.stringify({ id: submission.id + 5000, run_id: submission.id, run_attempt: 1, runner_group_id: config.runnerGroupId, head_sha: config.expectedSha, status: 'completed', conclusion: scenario.failed ? 'failure' : 'success', created_at: '2026-11-02T00:00:00Z', started_at: '2026-11-02T00:00:02Z', completed_at: '2026-11-02T00:00:20Z' });
      assert.equal(path, `repos/${config.repository}/actions/runs/${submission.id}`, 'Only the returned own run may be polled');
      polls.set(submission.id, (polls.get(submission.id) ?? 0) + 1);
      const status = scenario.neverCompletes || polls.get(submission.id) === 1 ? 'in_progress' : 'completed';
      return JSON.stringify({ id: submission.id, head_sha: scenario.wrongRunSha ? 'd'.repeat(40) : config.expectedSha, event: 'workflow_dispatch', run_attempt: 1, path: `.github/workflows/${config.workflow}`, status, conclusion: status === 'completed' ? scenario.failed ? 'failure' : 'success' : null });
    }
    if (args[1] === 'view') return 'private fixture log\n';
    if (args[1] === 'download') {
      const submission = submissions.at(-1), directory = args.at(-1);
      assert(args.includes(`startup-${submission.mode}-${submission.sample}-${submission.id}`));
      await mkdir(join(directory, 'nested'));
      const data = measurement(config, submission.mode);
      if (scenario.mismatch && submission.mode === 'warm') data.identity.lockSha256 = 'different';
      if (scenario.wrongCommand) data.stages.integration.command = ['true'];
      await writeFile(join(directory, 'nested/measurement.json'), JSON.stringify(data));
      return '';
    }
    throw new Error(`Unexpected fake gh protocol: ${args.join(' ')}`);
  };
  return { config, calls, submissions, run: () => runBenchmark(config, { gh, now: () => clock, sleep: async ms => { clock += ms; }, progress: () => {} }) };
}

test('five alternating pairs gate every dispatch, use returned IDs, and preserve comparison evidence', async t => {
  const f = await fixture(t); const result = await f.run();
  assert.deepEqual(f.submissions.map(value => value.mode), Array.from({ length: 10 }, (_, index) => index % 2 ? 'warm' : 'cold'));
  assert.equal(result.groups.cold.count, 5); assert.equal(result.groups.warm.count, 5);
  assert.equal(f.calls.filter(args => args[1]?.includes('/runner-groups/')).length, 10);
  assert.equal(JSON.parse(await readFile(join(f.config.output, 'comparison.json'))).length, 10);
  assert.equal((await stat(f.config.output)).mode & 0o077, 0);
  await assert.rejects(f.run(), /EEXIST/); assert.equal(f.submissions.length, 10);
});
for (const scenario of ['ambiguous', 'submitError']) test(`${scenario} submission is journaled exactly once and never inferred or retried`, async t => {
  const f = await fixture(t, { [scenario]: true });
  await assert.rejects(f.run(), /Submission outcome ambiguous/);
  assert.equal(f.submissions.length, 1);
  assert((await readFile(join(f.config.output, 'journal.jsonl'), 'utf8')).includes('submission-ambiguous'));
  assert(!f.calls.some(args => args[1]?.includes('/actions/runs/')));
});
test('readiness timeout stops without dispatch', async t => {
  const f = await fixture(t, { notReady: true }); await assert.rejects(f.run(), /readiness deadline/); assert.equal(f.submissions.length, 0);
});
test('changed ref stops without dispatch', async t => {
  const f = await fixture(t, { wrongRef: true }); await assert.rejects(f.run(), /Remote ref changed/); assert.equal(f.submissions.length, 0);
});
test('run timeout retains its ID without cancellation or replay', async t => {
  const f = await fixture(t, { neverCompletes: true }); await assert.rejects(f.run(), /polling deadline/);
  assert.equal(f.submissions.length, 1); assert(!f.calls.some(args => args.includes('cancel')));
});
test('failed own run saves logs/artifacts and stops before the next sample', async t => {
  const f = await fixture(t, { failed: true }); await assert.rejects(f.run(), /Workflow failed/);
  assert.equal(f.submissions.length, 1);
  assert.equal(await readFile(join(f.config.output, 'cold-1/run.log'), 'utf8'), 'private fixture log\n');
  await readFile(join(f.config.output, 'cold-1/artifacts/nested/measurement.json'));
});
test('mismatched cache identity stops after preserving the second raw sample', async t => {
  const f = await fixture(t, { mismatch: true }); await assert.rejects(f.run(), /differs/);
  assert.equal(f.submissions.length, 2); await readFile(join(f.config.output, 'warm-1/job.json'));
});
test('source mismatch and changed integration command stop immediately', async t => {
  for (const scenario of ['wrongRunSha', 'wrongCommand']) {
    const f = await fixture(t, { [scenario]: true }); await assert.rejects(f.run(), /different source|command differs/); assert.equal(f.submissions.length, 1);
  }
});
test('dispatch parser rejects guessed, conflicting, and wrong-repository IDs', () => {
  assert.equal(submittedRun('12345\n', 'o/r'), 12345);
  assert.throws(() => submittedRun('created at 12:30, ID unknown', 'o/r'), /ambiguous/);
  assert.throws(() => submittedRun('https://github.com/other/repo/actions/runs/123', 'o/r'), /ambiguous/);
  assert.throws(() => submittedRun('https://github.com/o/r/actions/runs/123 https://github.com/o/r/actions/runs/456', 'o/r'), /ambiguous/);
});
