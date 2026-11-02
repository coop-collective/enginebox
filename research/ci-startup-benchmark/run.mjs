// Explicitly invoked live research harness. Never retries a workflow submission.
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdir, open, readFile, readdir, lstat, writeFile, rename, appendFile } from 'node:fs/promises';
import { isAbsolute, join, resolve, relative } from 'node:path';
import { pathToFileURL } from 'node:url';
import { analyze } from './analyze.mjs';

const exec = promisify(execFile);
export function configuration(input) {
  const config = {
    repository: 'coop-collective/cloudflare-template', workflow: 'compatibility.yml',
    ref: 'perf/ci-startup-benchmark', expectedSha: '24a4fe9ded7ca31ffc2af996802246146b105d46',
    runnerGroupId: 4, runnerLabel: 'enginebox-template', repeats: 5,
    pollSeconds: 10, readinessTimeoutSeconds: 300, runTimeoutSeconds: 3600,
    ...input,
  };
  for (const key of Object.keys(input)) assert(Object.hasOwn({ repository: 1, workflow: 1, ref: 1, expectedSha: 1, runnerGroupId: 1, runnerLabel: 1, repeats: 1, pollSeconds: 1, readinessTimeoutSeconds: 1, runTimeoutSeconds: 1, output: 1 }, key), `Unknown configuration field: ${key}`);
  assert(/^[\w.-]+\/[\w.-]+$/.test(config.repository), 'Invalid repository');
  assert(/^[\w.-]+\.ya?ml$/.test(config.workflow), 'Invalid workflow filename');
  assert(typeof config.ref === 'string' && /^[\w./-]+$/.test(config.ref) && !config.ref.includes('..'), 'Invalid workflow ref');
  assert(/^[a-f0-9]{40}$/.test(config.expectedSha), 'Pin the complete expected commit');
  assert(Number.isSafeInteger(config.runnerGroupId) && config.runnerGroupId > 0, 'Invalid runner group');
  assert(/^[\w.-]+$/.test(config.runnerLabel), 'Invalid runner label');
  for (const [key, min, max] of [['repeats', 5, 20], ['pollSeconds', 1, 10], ['readinessTimeoutSeconds', 1, 300], ['runTimeoutSeconds', 1, 3600]]) assert(Number.isInteger(config[key]) && config[key] >= min && config[key] <= max, `Invalid ${key}`);
  assert(typeof config.output === 'string' && isAbsolute(config.output), 'Choose a new private absolute output directory');
  return config;
}

async function durable(path, value) {
  const file = await open(path, 'wx', 0o600);
  try { await file.writeFile(JSON.stringify(value, null, 2) + '\n'); await file.sync(); }
  finally { await file.close(); }
}
async function realGh(args, { timeout = 60000 } = {}) {
  const env = { ...process.env, GH_PROMPT_DISABLED: '1', GH_PAGER: 'cat', NO_COLOR: '1' };
  delete env.GH_DEBUG;
  try {
    const result = await exec('gh', args, { env, timeout, maxBuffer: 64 * 1024 * 1024, encoding: 'utf8' });
    return result.stdout;
  } catch { throw new Error('GitHub CLI command failed or timed out; inspect private run journal'); }
}
export function submittedRun(output, repository) {
  const urls = [...output.matchAll(/https:\/\/github\.com\/([^\s/]+\/[^\s/]+)\/actions\/runs\/(\d+)\b/g)]
    .filter(match => match[1] === repository).map(match => match[2]);
  if (/^\s*[1-9]\d*\s*$/.test(output)) urls.push(output.trim());
  try {
    const value = JSON.parse(output);
    if (value && Number.isSafeInteger(value.workflow_run_id) && value.workflow_run_id > 0) urls.push(String(value.workflow_run_id));
  } catch { /* gh normally returns a human-readable run URL. */ }
  const ids = [...new Set(urls)];
  assert(ids.length === 1 && Number.isSafeInteger(Number(ids[0])) && Number(ids[0]) > 0, 'Submission outcome ambiguous: no unique returned run ID; do not resubmit');
  return Number(ids[0]);
}

async function measurements(directory) {
  const found = []; let entries = 0;
  async function visit(path) {
    for (const name of await readdir(path)) {
      assert(++entries <= 20000, 'Artifact traversal limit exceeded');
      const file = join(path, name), info = await lstat(file);
      assert(!info.isSymbolicLink(), 'Symlink in downloaded artifact');
      if (info.isDirectory()) await visit(file);
      else if (name === 'measurement.json') { assert(info.size <= 1024 * 1024, 'Oversized measurement'); found.push(file); }
    }
  }
  await visit(directory);
  assert.equal(found.length, 1, 'Expected exactly one measurement.json in this run artifact');
  return found[0];
}

export async function runBenchmark(input, dependencies = {}) {
  const config = configuration(input), gh = dependencies.gh ?? realGh;
  const now = dependencies.now ?? Date.now;
  const sleep = dependencies.sleep ?? (ms => new Promise(resolve => setTimeout(resolve, ms)));
  const progress = dependencies.progress ?? (message => console.log(message));
  const root = config.output;
  await mkdir(root, { mode: 0o700 }); // Existing runs are never resumed/replayed implicitly.
  await durable(join(root, 'config.json'), config);
  const journal = await open(join(root, 'journal.jsonl'), 'wx', 0o600);
  const event = async (type, fields = {}) => { await journal.writeFile(JSON.stringify({ at: new Date(now()).toISOString(), type, ...fields }) + '\n'); await journal.sync(); };
  const json = async args => JSON.parse(await gh(args, { timeout: 10000 }));
  const samples = [], comparison = [];
  let current;
  try {
    for (let pair = 1; pair <= config.repeats; pair++) for (const mode of ['cold', 'warm']) {
      const sample = String(pair), directory = join(root, `${mode}-${sample}`);
      current = { mode, sample };
      await mkdir(directory, { mode: 0o700 });
      const ref = await json(['api', `repos/${config.repository}/commits/${encodeURIComponent(config.ref)}`]);
      await durable(join(directory, 'ref.json'), ref);
      assert.equal(ref.sha, config.expectedSha, 'Remote ref changed; stop before dispatch');
      const readyDeadline = now() + config.readinessTimeoutSeconds * 1000;
      let ready;
      while (!ready) {
        const group = await json(['api', `orgs/${config.repository.split('/')[0]}/actions/runner-groups/${config.runnerGroupId}/runners?per_page=100`]);
        assert(Array.isArray(group.runners) && group.total_count <= 100, 'Unexpected runner group response or pagination');
        const eligible = group.runners.filter(runner => runner.status === 'online' && runner.busy === false && Number.isSafeInteger(runner.id) && runner.name?.startsWith('ebox-ci-') && runner.labels?.some(label => label.name === config.runnerLabel));
        await event('readiness-observation', { ...current, runners: group.runners.map(runner => ({ id: runner.id, name: runner.name, status: runner.status, busy: runner.busy })) });
        if (eligible.length) { ready = eligible[0]; await durable(join(directory, 'ready.json'), { observedAt: new Date(now()).toISOString(), runnerGroupId: config.runnerGroupId, runner: ready }); break; }
        assert(now() < readyDeadline, 'No online idle runner before readiness deadline; no workflow submitted');
        await sleep(Math.min(config.pollSeconds * 1000, readyDeadline - now()));
      }
      // Recheck after waiting: never deliberately submit a moved ref.
      const finalRef = await json(['api', `repos/${config.repository}/commits/${encodeURIComponent(config.ref)}`]);
      assert.equal(finalRef.sha, config.expectedSha, 'Remote ref changed while awaiting readiness');
      const intent = { ...current, expectedSha: config.expectedSha, repository: config.repository, workflow: config.workflow, ref: config.ref, readyRunnerId: ready.id };
      await durable(join(directory, 'intent.json'), { at: new Date(now()).toISOString(), ...intent });
      await event('dispatch-intent', intent);
      let response, runId;
      try {
        response = await gh(['workflow', 'run', config.workflow, '--repo', config.repository, '--ref', config.ref, '-f', 'benchmark=true', '-f', `cache_mode=${mode}`, '-f', `sample=${sample}`, '-f', 'attribution=false']);
        await writeFile(join(directory, 'dispatch-response.txt'), response, { flag: 'wx', mode: 0o600 });
        runId = submittedRun(response, config.repository);
      } catch {
        await event('submission-ambiguous', current);
        throw new Error('Submission outcome ambiguous. Intent retained; do not automatically retry or infer a run by time.');
      }
      current.runId = runId;
      await durable(join(directory, 'submission.json'), { runId, url: `https://github.com/${config.repository}/actions/runs/${runId}` });
      await event('submitted', current);
      progress(`${mode} ${sample}/${config.repeats}: https://github.com/${config.repository}/actions/runs/${runId}`);
      const deadline = now() + config.runTimeoutSeconds * 1000;
      let run;
      while (true) {
        run = await json(['api', `repos/${config.repository}/actions/runs/${runId}`]);
        await appendFile(join(directory, 'run-polls.jsonl'), JSON.stringify({ observedAt: new Date(now()).toISOString(), run }) + '\n', { mode: 0o600 });
        assert.equal(run.id, runId, 'Returned run identity differs');
        assert.equal(run.head_sha, config.expectedSha, 'Dispatched run used a different source');
        assert.equal(run.event, 'workflow_dispatch', 'Unexpected run event');
        assert.equal(run.run_attempt, 1, 'Rerun attempts are not interchangeable benchmark samples');
        assert.equal(run.path, `.github/workflows/${config.workflow}`, 'Returned run workflow differs');
        await event('run-observation', { ...current, status: run.status, conclusion: run.conclusion });
        if (run.status === 'completed') break;
        assert(now() < deadline, 'Own run exceeded polling deadline; retained without cancellation or replay');
        await sleep(Math.min(config.pollSeconds * 1000, deadline - now()));
      }
      await durable(join(directory, 'run.json'), run);
      try {
        const logs = await gh(['run', 'view', String(runId), '--repo', config.repository, '--log'], { timeout: 120000 });
        await writeFile(join(directory, 'run.log'), logs, { flag: 'wx', mode: 0o600 });
      } catch { await event('log-download-failed', current); }
      const jobs = await json(['api', `repos/${config.repository}/actions/runs/${runId}/jobs?per_page=100`]);
      await durable(join(directory, 'jobs.json'), jobs);
      assert(Array.isArray(jobs.jobs) && jobs.total_count <= 100, 'Unexpected jobs response or pagination');
      const matches = jobs.jobs.filter(job => job.name === `startup-cache-${mode}-${sample}`);
      assert.equal(matches.length, 1, 'Expected exactly one numeric benchmark job');
      assert(Number.isSafeInteger(matches[0].id) && matches[0].id > 0, 'Missing numeric benchmark job');
      const job = await json(['api', `repos/${config.repository}/actions/jobs/${matches[0].id}`]);
      await durable(join(directory, 'job.json'), job);
      assert.equal(job.run_id, runId, 'Benchmark job belongs to a different run');
      assert.equal(job.run_attempt, 1, 'Unexpected job attempt');
      assert.equal(job.head_sha, config.expectedSha, 'Numeric job source differs');
      assert.equal(job.runner_group_id, config.runnerGroupId, 'Job ran in another runner group');
      // Preserve a failed sample's artifacts, if any, before stopping the batch.
      const artifacts = join(directory, 'artifacts'); await mkdir(artifacts, { mode: 0o700 });
      try {
        await gh(['run', 'download', String(runId), '--repo', config.repository, '--pattern', `startup-${mode}-${sample}-${runId}`, '--dir', artifacts], { timeout: 180000 });
      } catch {
        await event('artifact-download-failed', current);
        throw new Error('Own run artifact download failed; sample retained and batch stopped');
      }
      assert.equal(run.conclusion, 'success', 'Workflow failed; stop before another sample');
      const measurementPath = await measurements(artifacts);
      const measurement = JSON.parse(await readFile(measurementPath, 'utf8'));
      assert.equal(measurement.cache.mode, mode, 'Artifact cache mode differs from submitted mode');
      assert.equal(measurement.cache.kind, 'pnpm-store', 'Artifact used another cache mechanism');
      assert.equal(measurement.cache.hit, mode === 'warm', 'Artifact cache outcome differs from submitted mode');
      assert(typeof measurement.cache.key === 'string' && measurement.cache.key, 'Artifact cache identity missing');
      assert.equal(measurement.identity.sourceCommit, config.expectedSha, 'Artifact source differs');
      assert.equal(measurement.identity.node, 'v24.14.0', 'Node differs from the benchmark toolchain');
      assert.equal(measurement.identity.pnpm, '12.15.0', 'pnpm differs from the benchmark toolchain');
      assert.equal(job.status, 'completed'); assert.equal(job.conclusion, 'success');
      assert(Number.isFinite(Date.parse(job.created_at)), 'Numeric job has no valid created_at queue timestamp');
      for (const stage of ['dependencies', 'integration']) assert.equal(measurement.stages?.[stage]?.exitCode, 0, `Unsuccessful ${stage}`);
      assert.deepEqual(measurement.stages.dependencies.command, ['pnpm', 'install', '--frozen-lockfile', '--store-dir', '/opt/enginebox-benchmark/seed/store', '--package-import-method', 'copy', '--reporter', 'ndjson'], 'Dependency command differs from the benchmark contract');
      assert.deepEqual(measurement.stages.integration.command, ['pnpm', 'run', 'benchmark:integration'], 'Integration command differs from the benchmark contract');
      samples.push({ job, measurement });
      comparison.push({ job: relative(root, join(directory, 'job.json')), measurement: relative(root, measurementPath) });
      // Cold/warm pair and every later sample must satisfy exact comparability.
      if (samples.length >= 2) analyze(samples);
      await durable(join(directory, 'recorded.json'), { runId, jobId: job.id, diagnosticOnly: true });
      const nextComparison = join(root, 'comparison.next.json');
      await durable(nextComparison, comparison);
      await rename(nextComparison, join(root, 'comparison.json'));
      await event('sample-complete', { ...current, jobId: job.id });
    }
    const report = analyze(samples);
    await durable(join(root, 'result.json'), report);
    await event('complete', { samples: samples.length });
    progress(`Completed ${samples.length} matched samples; private report: ${join(root, 'result.json')}`);
    return report;
  } catch (error) {
    await event('stopped', { ...current, reason: error.message });
    throw error;
  } finally { await journal.close(); }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    assert(process.argv.length === 3, 'Usage: node run.mjs /absolute/private/config.json');
    await runBenchmark(JSON.parse(await readFile(process.argv[2], 'utf8')));
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}
