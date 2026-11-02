// Standalone research instrumentation. Never stores command output or environment values.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawn, execFileSync } from 'node:child_process';
import { readFile, writeFile } from 'node:fs/promises';

const [operation, output, ...args] = process.argv.slice(2);
const command = (...argv) => execFileSync(argv[0], argv.slice(1), { encoding: 'utf8' }).trim();
const save = data => writeFile(output, JSON.stringify(data, null, 2) + '\n', { mode: 0o600 });
assert(output, 'Usage: measure.mjs identity|run|cache OUTPUT [arguments]');

if (operation === 'identity') {
  assert.equal(command('git', 'status', '--porcelain', '--untracked-files=normal'), '', 'Benchmark checkout must be clean; write artifacts outside the checkout');
  const required = name => { assert(process.env[name], `${name} is required`); return process.env[name]; };
  const mode = required('BENCHMARK_CACHE_MODE');
  assert(['cold', 'warm', 'prepared'].includes(mode), 'Unknown cache mode');
  const firstStepAt = required('BENCHMARK_FIRST_STEP_AT');
  assert(Number.isFinite(Date.parse(firstStepAt)), 'Invalid first-step timestamp');
  const data = {
    schema: 1,
    identity: {
      repositoryId: required('GITHUB_REPOSITORY_ID'),
      sourceCommit: command('git', 'rev-parse', 'HEAD'),
      sourceTree: command('git', 'rev-parse', 'HEAD^{tree}'),
      lockSha256: createHash('sha256').update(await readFile('pnpm-lock.yaml')).digest('hex'),
      node: process.version, pnpm: command('pnpm', '--version'),
      platform: process.platform, arch: process.arch,
      runnerImage: required('BENCHMARK_IMAGE'), resources: required('BENCHMARK_RESOURCES'),
      installPolicy: required('BENCHMARK_INSTALL_POLICY'),
    },
    cache: { mode, kind: required('BENCHMARK_CACHE_KIND'), key: null, hit: null, downloadedPackages: null },
    firstStepAt, stages: {},
  };
  if (process.env.BENCHMARK_SEED_MANIFEST) {
    const bytes = await readFile(process.env.BENCHMARK_SEED_MANIFEST);
    const seed = JSON.parse(bytes);
    assert.equal(seed.schema, 1, 'Unsupported seed manifest');
    assert.equal(seed.kind, 'image-local-pnpm-store', 'Wrong seed kind');
    for (const field of ['repositoryId', 'lockSha256', 'node', 'pnpm', 'platform', 'arch']) assert.equal(seed[field], data.identity[field], `Seed ${field} differs from job`);
    assert.equal(seed.packageSha256, createHash('sha256').update(await readFile('package.json')).digest('hex'), 'Seed package manifest differs from job');
    if (seed.workspaceSha256) assert.equal(seed.workspaceSha256, createHash('sha256').update(await readFile('pnpm-workspace.yaml')).digest('hex'), 'Seed workspace policy differs from job');
    assert.equal(seed.builderScripts, 'deny', 'Seed lifecycle policy is unsupported');
    assert.equal(seed.offlineInstallVerified, true, 'Seed did not pass offline installation');
    assert.equal(seed.portableOfflineVerified, true, 'Seed did not pass installation in a fresh HOME and path');
    data.identity.seedManifestSha256 = createHash('sha256').update(bytes).digest('hex');
    data.seed = seed;
  }
  await save(data);
} else if (operation === 'cache') {
  const [key, hit] = args;
  assert(key && ['true', 'false'].includes(hit), 'cache requires a key and true|false hit');
  const data = JSON.parse(await readFile(output, 'utf8'));
  Object.assign(data.cache, { key, hit: hit === 'true' });
  await save(data);
} else if (operation === 'run') {
  const [stage, separator, executable, ...argv] = args;
  assert(['dependencies', 'integration'].includes(stage) && separator === '--' && executable, 'run requires dependencies|integration -- COMMAND [ARGS]');
  const data = JSON.parse(await readFile(output, 'utf8'));
  assert(!data.stages[stage], `Stage already recorded: ${stage}`);
  const startedAt = new Date().toISOString(), start = performance.now();
  let downloadedPackages = 0, progressSeen = false;
  const inspect = text => {
    try {
      const entry = JSON.parse(text);
      if (entry.name === 'pnpm:progress') {
        progressSeen = true;
        if (entry.status === 'fetched') downloadedPackages++;
      }
    } catch { /* Ordinary output is not a package event. */ }
  };
  const child = spawn(executable, argv, { stdio: ['inherit', 'pipe', 'pipe'], shell: false });
  const tails = [];
  for (const [stream, target] of [[child.stdout, process.stdout], [child.stderr, process.stderr]]) {
    let line = '';
    stream.setEncoding('utf8');
    stream.on('data', text => {
      target.write(text);
      if (stage !== 'dependencies') return;
      for (const chunk of text.split(/(\n)/)) {
        if (chunk === '\n') { inspect(line); line = ''; }
        else line = (line + chunk).slice(-1024 * 1024);
      }
    });
    tails.push(() => inspect(line));
  }
  let failedToStart = false;
  child.on('error', () => { failedToStart = true; });
  const [code, signal] = await new Promise(resolve => child.on('close', (code, signal) => resolve([code, signal])));
  tails.forEach(inspectTail => inspectTail());
  data.stages[stage] = { command: [executable, ...argv], startedAt, completedAt: new Date().toISOString(), elapsedMs: performance.now() - start, exitCode: failedToStart ? 127 : code, signal };
  if (stage === 'dependencies') data.cache.downloadedPackages = progressSeen ? downloadedPackages : null;
  await save(data);
  process.exitCode = failedToStart ? 127 : code ?? 1;
} else throw new Error(`Unknown operation: ${operation}`);
