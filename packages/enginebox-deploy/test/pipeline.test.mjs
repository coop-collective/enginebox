import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { config, smokeConfig, watchConfig, watchVerdict, startFailure, nodeInstallerArgs, WATCH_DEFAULTS } from '../src/lib.mjs';
import { plan } from '../src/ci-plan.mjs';

const root = fileURLToPath(new URL('..', import.meta.url));
const environment = async name => JSON.parse(await readFile(`${root}/environments/${name}.json`, 'utf8'));

test('a push goes through staging and promotes the same artifact unless production is held', () => {
  assert.deepEqual(plan({ GITHUB_RUN_NUMBER: '7' }), { operation: 'update', version: '0.3.1-live.7', target: 'pipeline', staging: true, production: true, held: false });
  assert.deepEqual(plan({ GITHUB_RUN_NUMBER: '7', ENGINEBOX_PROD_HOLD: 'true' }), { operation: 'update', version: '0.3.1-live.7', target: 'pipeline', staging: true, production: false, held: true });
  // Only the exact string holds production.
  assert.equal(plan({ GITHUB_RUN_NUMBER: '7', ENGINEBOX_PROD_HOLD: 'yes' }).production, true);
});

test('a manual run names one environment; the emergency path to production skips staging only when asked', () => {
  assert.deepEqual(plan({ GITHUB_RUN_NUMBER: '8', ENGINEBOX_DEPLOY_TARGET: 'staging' }).production, false);
  const direct = plan({ GITHUB_RUN_NUMBER: '8', ENGINEBOX_DEPLOY_TARGET: 'production' });
  assert.equal(direct.staging, false); assert.equal(direct.production, true);
  // The hold stops the pipeline's promotion, not an explicit production run.
  assert.equal(plan({ GITHUB_RUN_NUMBER: '8', ENGINEBOX_DEPLOY_TARGET: 'production', ENGINEBOX_PROD_HOLD: 'true' }).production, true);
  const rollback = plan({ ENGINEBOX_DEPLOY_OPERATION: 'rollback', ENGINEBOX_DEPLOY_VERSION: '0.3.1-live.6' });
  assert.equal(rollback.target, 'production'); assert.equal(rollback.staging, false);
  assert.equal(plan({ ENGINEBOX_DEPLOY_OPERATION: 'status', ENGINEBOX_DEPLOY_TARGET: 'staging' }).staging, true);
  assert.throws(() => plan({ ENGINEBOX_DEPLOY_OPERATION: 'rollback', ENGINEBOX_DEPLOY_VERSION: '0.3.0', ENGINEBOX_DEPLOY_TARGET: 'pipeline' }), /one-environment/);
  assert.throws(() => plan({ GITHUB_RUN_NUMBER: '8', ENGINEBOX_DEPLOY_TARGET: 'prod' }), /invalid-deployment-target/);
});

test('staging is isolated from production: its own prefix, state, token, listener, and no node helper', async () => {
  const prod = config(await environment('production'));
  const staging = config(await environment('staging'));
  assert.ok(prod.node, 'production owns the host node helper');
  assert.equal(staging.node, undefined, 'staging never touches the shared node helper');
  for (const key of ['prefix', 'installer', 'supervisor_config', 'state_dir', 'token_file', 'url']) assert.notEqual(staging.controller[key], prod.controller[key], key);
  for (const key of ['prefix', 'supervisor_config', 'state_dir', 'token_file']) assert.ok(!staging.controller[key].startsWith(prod.controller.prefix), key);
  assert.ok(staging.smoke && staging.watch === undefined, 'staging is gated by smoke, production by the watch');
  assert.ok(prod.watch);
});

test('smoke and watch settings are bounded, and a run needs both a pool and a provider', () => {
  smokeConfig({}); smokeConfig({ pool: 'staging', provider: 'codex', prompt: 'Reply with exactly: OK', expect: 'OK', timeout_seconds: 300 });
  assert.throws(() => smokeConfig({ pool: 'staging' }), /pool-and-provider/);
  assert.throws(() => smokeConfig({ provider: 'grok', pool: 'p' }), /smoke-provider/);
  assert.throws(() => smokeConfig({ command: 'rm -rf /' }));
  watchConfig({ seconds: 600, interval_seconds: 30 });
  assert.throws(() => watchConfig({ seconds: 99999 }), /watch-seconds/);
  assert.throws(() => watchConfig({ max_start_failure_ratio: 0 }), /watch-ratio/);
});

test('the watch rolls back on lost readiness or on start failures above both a count and a share', () => {
  const ok = { consecutive_unready: 0, new_runs: 0, start_failures: 0 };
  assert.equal(watchVerdict(ok).regressed, false);
  assert.deepEqual(watchVerdict({ ...ok, consecutive_unready: WATCH_DEFAULTS.max_unready_checks }), { regressed: true, reason: 'controller-lost-readiness' });
  assert.equal(watchVerdict({ ...ok, consecutive_unready: WATCH_DEFAULTS.max_unready_checks - 1 }).regressed, false);
  // Three failures of three new runs: a regression. Three of twenty: ordinary noise, no rollback.
  assert.equal(watchVerdict({ ...ok, new_runs: 3, start_failures: 3 }).reason, 'run-start-failures');
  assert.equal(watchVerdict({ ...ok, new_runs: 20, start_failures: 3 }).regressed, false);
  assert.equal(watchVerdict({ ...ok, new_runs: 2, start_failures: 2 }).regressed, false);
  assert.equal(watchVerdict({ ...ok, new_runs: 4, start_failures: 2 }, { max_start_failures: 2 }).regressed, true);
});

test('a start failure is a failed run whose agent never exited; an agent exiting nonzero is not', () => {
  assert.equal(startFailure({ status: 'failed', error: 'workspace_unavailable' }), true);
  assert.equal(startFailure({ status: 'failed', result: { exit_code: 1 } }), false);
  assert.equal(startFailure({ status: 'succeeded' }), false);
  assert.equal(startFailure(null), false);
});

test('the node helper rollout stages, activates and republishes with the release\'s own installer', () => {
  const d = { node: { prefix: '/opt/enginebox-node', sudo: '/usr/bin/sudo' } };
  const artifact = { manifest: '/t/m.json', manifest_sha256: 'a'.repeat(64), archive: '/t/a.tar.gz' };
  assert.deepEqual(nodeInstallerArgs(d, 'stage', '0.3.1-live.9', artifact), ['--prefix', '/opt/enginebox-node', 'stage', '--version', '0.3.1-live.9', '--manifest', '/t/m.json', '--manifest-sha256', 'a'.repeat(64), '--archive', '/t/a.tar.gz']);
  assert.deepEqual(nodeInstallerArgs(d, 'activate', '0.3.1-live.9'), ['--prefix', '/opt/enginebox-node', 'activate', '--version', '0.3.1-live.9']);
  assert.deepEqual(nodeInstallerArgs(d, 'expose-host', '0.3.1-live.9'), ['--prefix', '/opt/enginebox-node', 'expose-host', '--apply']);
  assert.throws(() => nodeInstallerArgs(d, 'delete', '0.3.1-live.9'));
});
