import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const canonical = value => value && typeof value === 'object'
  ? Array.isArray(value) ? `[${value.map(canonical).join(',')}]` : `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonical(value[key])}`).join(',')}}`
  : JSON.stringify(value);
const timestamp = value => { const result = Date.parse(value); assert(Number.isFinite(result), `Missing or invalid timestamp: ${value}`); return result; };
const duration = (start, end) => { const result = timestamp(end) - timestamp(start); assert(result >= 0, 'Timestamp order is invalid'); return result; };
const percentile = (values, fraction) => [...values].sort((a, b) => a - b)[Math.ceil(values.length * fraction) - 1];

export function analyze(samples) {
  assert(samples.length >= 2, 'At least two completed jobs are required');
  let identity;
  const seen = new Set();
  const records = samples.map(({ measurement: m, job, queuedAt, queueSource }) => {
    assert(m.schema === 1 && m.identity && m.stages, 'Invalid measurement');
    for (const field of ['repositoryId', 'sourceCommit', 'sourceTree', 'lockSha256', 'node', 'pnpm', 'platform', 'arch', 'runnerImage', 'resources', 'installPolicy']) assert(typeof m.identity[field] === 'string' && m.identity[field], `Missing identity: ${field}`);
    identity ??= canonical(m.identity);
    assert.equal(canonical(m.identity), identity, 'Source, lockfile, toolchain, image, resources or install policy differs');
    assert(job.id && !seen.has(job.id), 'Missing or repeated GitHub job'); seen.add(job.id);
    assert.equal(job.head_sha, m.identity.sourceCommit, 'GitHub job and measured checkout differ');
    assert.equal(job.status, 'completed', 'Job has not completed');
    assert.equal(job.conclusion, 'success', 'Failed jobs cannot establish a speedup');
    assert(['cold', 'warm', 'prepared'].includes(m.cache?.mode), 'Unknown cache mode');
    assert(typeof m.cache.key === 'string' && m.cache.key, 'Missing cache identity');
    assert.equal(m.cache.hit, m.cache.mode !== 'cold', 'Cache outcome does not match comparison mode');
    const times = {};
    for (const stage of ['dependencies', 'integration']) {
      const value = m.stages[stage];
      assert(value && value.exitCode === 0 && !value.signal, `Missing or unsuccessful ${stage}`);
      assert(Number.isFinite(value.elapsedMs) && value.elapsedMs >= 0, `Invalid ${stage} duration`);
      assert(Array.isArray(value.command) && value.command.length > 0, `Missing ${stage} command`);
      duration(job.started_at, value.startedAt);
      assert(timestamp(value.completedAt) < timestamp(job.completed_at) + 1000, `${stage} ends after the job`);
      times[`${stage}Ms`] = value.elapsedMs;
    }
    duration(m.stages.dependencies.completedAt, m.stages.integration.startedAt);
    duration(job.started_at, m.firstStepAt); duration(m.firstStepAt, m.stages.dependencies.startedAt);
    const queue = queuedAt ?? job.created_at;
    assert(!queuedAt || queueSource, 'An explicit queue timestamp needs its provenance');
    duration(queue, job.started_at);
    // GitHub API timestamps have second precision; the guest first-step marker has milliseconds.
    return { jobId: job.id, url: job.html_url, mode: m.cache.mode, cache: m.cache,
      queueSource: queuedAt ? queueSource : 'GitHub job.created_at',
      queueToFirstStepMs: duration(queue, m.firstStepAt),
      queueToRunnerMs: duration(queue, job.started_at),
      executionMs: duration(job.started_at, job.completed_at),
      wholeJobMs: duration(queue, job.completed_at), ...times };
  });
  const integration = canonical(samples[0].measurement.stages.integration.command);
  assert(samples.every(sample => canonical(sample.measurement.stages.integration.command) === integration), 'Integration commands differ');
  const installs = samples.filter(sample => sample.measurement.cache.mode !== 'prepared');
  const install = canonical(installs[0]?.measurement.stages.dependencies.command);
  assert(installs.every(sample => canonical(sample.measurement.stages.dependencies.command) === install), 'Cold and warm installation commands differ');
  const groups = Object.fromEntries([...new Set(records.map(row => row.mode))].map(mode => {
    const rows = records.filter(row => row.mode === mode);
    return [mode, { count: rows.length, ...Object.fromEntries(['queueToFirstStepMs', 'queueToRunnerMs', 'dependenciesMs', 'integrationMs', 'executionMs', 'wholeJobMs'].map(metric => [metric, { p50: percentile(rows.map(row => row[metric]), 0.5), p95: percentile(rows.map(row => row[metric]), 0.95) }])) }];
  }));
  assert(groups.cold && (groups.warm || groups.prepared), 'Need a cold control and a warm/prepared job');
  return { schema: 1, identity: JSON.parse(identity), groups, records,
    limits: ['Job timestamps include GitHub API second-level precision.', 'Workflow measurements are diagnostics, not trusted CI approval evidence.', 'Small samples do not establish production p95; idle reserve cost is not measured.', 'Downloaded package counts are not network byte totals.'] };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const config = resolve(process.argv[2]);
  const samples = JSON.parse(await readFile(config, 'utf8'));
  const load = async path => JSON.parse(await readFile(resolve(dirname(config), path), 'utf8'));
  const inputs = await Promise.all(samples.map(async sample => ({ ...sample, job: await load(sample.job), measurement: await load(sample.measurement) })));
  console.log(JSON.stringify(analyze(inputs), null, 2));
}
