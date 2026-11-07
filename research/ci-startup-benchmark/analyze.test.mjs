import assert from 'node:assert/strict';
import { test } from 'node:test';
import { analyze } from './analyze.mjs';

function sample(id, mode) {
  return {
    job: { id, head_sha: 'a'.repeat(40), status: 'completed', conclusion: 'success', created_at: '2026-11-02T00:00:00Z', started_at: '2026-11-02T00:00:02Z', completed_at: '2026-11-02T00:00:20Z' },
    measurement: { schema: 1,
      identity: { repositoryId: '1', sourceCommit: 'a'.repeat(40), sourceTree: 'b'.repeat(40), lockSha256: 'c'.repeat(64), node: 'v24.22.0', pnpm: '12.15.0', platform: 'linux', arch: 'x64', runnerImage: 'image-digest', resources: '2CPU/4GiB', installPolicy: 'ignore-scripts' },
      cache: { mode, kind: 'pnpm-store', key: 'exact-key', hit: mode !== 'cold', downloadedPackages: mode === 'cold' ? 12 : 0 },
      firstStepAt: '2026-11-02T00:00:03.125Z',
      stages: {
        dependencies: { command: ['pnpm', 'install', '--frozen-lockfile'], startedAt: '2026-11-02T00:00:04Z', completedAt: '2026-11-02T00:00:10Z', elapsedMs: mode === 'cold' ? 6000 : 1000, exitCode: 0 },
        integration: { command: ['pnpm', 'test'], startedAt: '2026-11-02T00:00:11Z', completedAt: '2026-11-02T00:00:20.123Z', elapsedMs: 9123, exitCode: 0 },
      },
    },
  };
}

test('separates queue, dependency, integration and job durations with API timestamp precision', () => {
  const result = analyze([sample(1, 'cold'), sample(2, 'warm')]);
  assert.equal(result.groups.cold.queueToFirstStepMs.p50, 3125);
  assert.equal(result.groups.cold.dependenciesMs.p50, 6000);
  assert.equal(result.groups.warm.dependenciesMs.p50, 1000);
  assert.equal(result.groups.warm.integrationMs.p50, 9123);
  assert.equal(result.groups.warm.executionMs.p50, 18000);
  assert.equal(result.groups.warm.wholeJobMs.p50, 20000);
});

for (const field of ['sourceCommit', 'sourceTree', 'lockSha256', 'node', 'pnpm', 'runnerImage', 'resources', 'installPolicy']) {
  test(`rejects unmatched ${field}`, () => {
    const warm = sample(2, 'warm'); warm.measurement.identity[field] = 'different';
    assert.throws(() => analyze([sample(1, 'cold'), warm]), /differs|differ/);
  });
}

test('rejects failed integration even when job reports success', () => {
  const warm = sample(2, 'warm'); warm.measurement.stages.integration.exitCode = 1;
  assert.throws(() => analyze([sample(1, 'cold'), warm]), /unsuccessful integration/);
});
test('rejects a cache miss labeled warm', () => {
  const warm = sample(2, 'warm'); warm.measurement.cache.hit = false;
  assert.throws(() => analyze([sample(1, 'cold'), warm]), /Cache outcome/);
});
test('requires actual job queue time and explicit provenance for overrides', () => {
  const warm = sample(2, 'warm'); delete warm.job.created_at;
  assert.throws(() => analyze([sample(1, 'cold'), warm]), /timestamp/);
  warm.queuedAt = '2026-11-02T00:00:00Z';
  assert.throws(() => analyze([sample(1, 'cold'), warm]), /provenance/);
  warm.queueSource = 'workflow_job queued webhook';
  assert.equal(analyze([sample(1, 'cold'), warm]).records[1].queueSource, warm.queueSource);
});
test('rejects different integration commands and duplicated job samples', () => {
  const warm = sample(2, 'warm'); warm.measurement.stages.integration.command = ['true'];
  assert.throws(() => analyze([sample(1, 'cold'), warm]), /commands differ/);
  assert.throws(() => analyze([sample(1, 'cold'), sample(1, 'warm')]), /repeated/);
});
test('requires identical cold/warm install flags', () => {
  const warm = sample(2, 'warm'); warm.measurement.stages.dependencies.command.push('--ignore-scripts');
  assert.throws(() => analyze([sample(1, 'cold'), warm]), /installation commands differ/);
});
