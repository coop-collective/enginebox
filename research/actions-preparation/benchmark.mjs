// Diagnostic comparison; no product dependency and no GitHub/Incus performance claim.
import assert from 'node:assert/strict';
import { mkdir, writeFile, readFile, rm, rename, copyFile } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { activatePreparedTree } from '../../packages/enginebox-actions/src/broker.mjs';
import { join } from 'node:path';
import { build, prepare, command, environment } from '../../packages/enginebox-preparation/src/pnpm.mjs';
const root = '/project', source = join(root, 'work/fixture/fixture');
assert(process.platform === 'linux' && process.env.ENGINEBOX_DISPOSABLE_PROOF === '1');
const env = environment(root, { ...process.env, HOME: '/tmp/benchmark-home', XDG_CONFIG_HOME: '/tmp/benchmark-home/config' });
const policy = { schema: 1, recipe: 'pnpm-installed-v1', namespace: 'benchmark', repository: 'fixture', trust: 'repository-read', image: 'a'.repeat(64), pnpm: '12.15.0', scripts: 'deny' };
const options = { source, root, policy, env, offline: true };
if (process.argv[2] === 'rebuild') {
  const result = await build(options);
  await writeFile(join(root, 'benchmark.json'), JSON.stringify(result));
  console.log(JSON.stringify({ rebuilt: true, key: result.key, bytes: result.modules.bytes }));
} else if (process.argv[2] === 'seed') {
  await mkdir(source, { recursive: true });
  const dependencies = { typescript: '5.9.3', lodash: '4.17.21', react: '19.3.1', 'react-dom': '19.3.1', '@types/node': '24.10.0' };
  if (process.env.BENCHMARK_FIXTURE === 'frontend') Object.assign(dependencies, { vite: '8.3.4', vitest: '5.0.5', '@vitejs/plugin-react': '6.1.2', tailwindcss: '4.3.3', '@playwright/test': '1.64.1' });
  await writeFile(join(source, 'package.json'), JSON.stringify({ name: 'prepared-benchmark', version: '1.0.0', type: 'module', dependencies }));
  if (dependencies.vitest) await writeFile(join(source, 'fixture.test.ts'), "import { test, expect } from 'vitest'; import { sum } from 'lodash-es'; test('result',()=>expect(20+22).toBe(42));\n".replace(" import { sum } from 'lodash-es';", ''));

  if (process.env.BENCHMARK_FIXTURE === 'frontend') await copyFile(new URL('./fixture/pnpm-lock.yaml', import.meta.url), join(source, 'pnpm-lock.yaml'));
  else command(['pnpm', 'install', '--lockfile-only', '--ignore-scripts'], source, env);
  const result = await build({ ...options, offline: false });
  await writeFile(join(root, 'benchmark.json'), JSON.stringify(result));
  console.log(JSON.stringify({ seeded: true, bytes: result.modules.bytes, key: result.key }));
} else {
  const manifest = JSON.parse(await readFile(join(root, 'benchmark.json'), 'utf8'));
  const installed = join(source, 'node_modules'), saved = join(root, 'prepared', manifest.key, 'node_modules');
  const conventional = [], prepared = [];
  const protectedMode = process.argv[2] === 'protected';
  const control = '/run/benchmark-control';
  if (protectedMode) {
    await mkdir(join(control, 'private/source'), { recursive: true, mode: 0o700 });
    await mkdir(join(control, 'requests'));
    await writeFile(join(control, 'policy.json'), JSON.stringify({ root, repository_id: 101, recipe: policy }));
    await writeFile(join(control, 'verified.json'), JSON.stringify({ verified: true, repository_id: 101 }));
    command(['mount', '--bind', root, join(control, 'private/source')], root);
    command(['mount', '--make-private', join(control, 'private/source')], root);
    command(['mount', '--bind', join(root, 'prepared'), join(root, 'prepared')], root);
    command(['mount', '-o', 'remount,bind,ro,nosuid,nodev', join(root, 'prepared')], root);
    // Even the directory owner cannot change a read-only approved generation.
    assert.throws(() => command(['sh', '-c', 'touch /project/prepared/forbidden'], root));
  }
  const verify = ['node', '-e', "require('assert/strict').equal(require('lodash').sum([20,22]),42);require('assert/strict').equal(require('typescript/package.json').version,'5.9.3'); require('react-dom/server').renderToString(require('react').createElement('p',null,'correct'))"];
  function check() {
    command(verify, source, env);
    if (process.env.BENCHMARK_FIXTURE === 'frontend') command(['pnpm', 'exec', 'vitest', 'run'], source, env);
  }
  async function normal() {
    const start = performance.now();
    command(['pnpm', 'install', '--offline', '--frozen-lockfile', '--ignore-scripts', '--store-dir', join(root, 'caches/packages'), '--reporter', 'silent'], source, env);
    conventional.push(performance.now() - start);
    check(); await rm(installed, { recursive: true });
  }
  async function accelerated() {
    const broker = protectedMode ? spawn('python3', ['/repo/packages/enginebox-actions/src/broker.py', control, '30'], { stdio: 'inherit' }) : null;
    const done = broker && new Promise((resolve,reject) => { broker.once('error', reject); broker.once('exit', code => code === 0 ? resolve() : reject(new Error('Broker failed'))); });
    const start = performance.now();
    const result = await prepare({ ...options, requireHit: true, activate: protectedMode ? request => activatePreparedTree(request, control) : undefined });
    prepared.push(performance.now() - start);
    assert.equal(result.mode, 'installed-tree'); assert.equal(result.downloadedPackages, 0);
    check();
    if (broker) {
      await done;
      for (const name of ['requests/restore.json', 'restored.json']) await rm(join(control, name));
    }
    // Reset fixture outside measurement. Production uses a separate private LayerFS fork per job.
    await rm(join(installed, '.vite'), { recursive: true, force: true });
    await rename(protectedMode ? join(control, 'private/source/work/fixture/fixture/node_modules') : installed, protectedMode ? join(control, 'private/source/prepared', manifest.key, 'node_modules') : saved);
  }
  for (let i = 0; i < 21; i++) {
    if (i % 2 === 0) { await normal(); await accelerated(); }
    else { await accelerated(); await normal(); }
  }
  const p = (values, q) => [...values].sort((a,b) => a-b)[Math.ceil(values.length*q)-1];
  const report = { node: process.version, pnpm: policy.pnpm, platform: process.platform, arch: process.arch, dependencyBytes: manifest.modules.bytes, runs: conventional.length, conventionalMs: { p50: p(conventional,.5), p95: p(conventional,.95), samples: conventional }, preparedMs: { p50: p(prepared,.5), p95: p(prepared,.95), samples: prepared }, speedup: p(conventional,.5)/p(prepared,.5), downloadedPackages: 0, correctness: true, includes: ['runtime/config/source fingerprint', protectedMode ? 'read-only generation verification and root handoff' : 'full dependency content scan', 'tree handoff', 'receipt'], excludes: ['VM provision', 'LayerFS fork/mount', 'checkout', 'generation transfer', 'build/test'] };
  const minimum = Number(process.env.BENCHMARK_MIN_SPEEDUP || '1.2');
  assert(Number.isFinite(minimum) && minimum >= 1.0 && minimum <= 10);
  report.benefitQualified = report.speedup >= minimum && report.preparedMs.p95 < report.conventionalMs.p95;
  report.minimumSpeedup = minimum;
  await writeFile(join(root, `result-${process.argv[2] || 'baseline'}.json`), JSON.stringify(report, null, 2));
  console.log(JSON.stringify(report));
  assert(report.benefitQualified, 'Dependency benefit gate failed; do not qualify this profile as accelerated');
}
