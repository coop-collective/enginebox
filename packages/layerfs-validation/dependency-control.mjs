import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import { createHash } from 'node:crypto';
import { DatabaseSync } from 'node:sqlite';
const root = process.env.EXPERIMENT_ROOT;
assert(root?.startsWith('/opt/layerfs-validation/'));
const { LayerFS } = await import(`${process.env.UPSTREAM_ROOT}/sdk/js/src/index.js`);
const client = new LayerFS('engineboxfixture', { storePath: '/layerfs-store/global/store.sqlite' });
const name = process.env.CONTROL_NAME;
assert(/^[a-z0-9-]+$/.test(name));
const receipt = `${root}/${name}/receipts.jsonl`;
const quote = v => `'${v.replaceAll("'", "'\\''")}'`;
const record = value => { fs.appendFileSync(receipt, JSON.stringify(value) + '\n'); console.log(JSON.stringify(value)); };
const expected = createHash('sha256').update(fs.readFileSync(`${root}/fixture/node_modules/typescript/lib/typescript.js`)).digest('hex');
async function step(id, operation) {
  const start = performance.now();
  try { const result = await operation(); record({ id, outcome: 'PASS', ms: performance.now() - start, result }); return result; }
  catch (e) { record({ id, outcome: 'FAIL', ms: performance.now() - start, message: e.message }); throw e; }
}
const verify = `node -e ${quote(`const fs=require('fs'),c=require('crypto'),a=require('assert/strict');a.equal(require('typescript/package.json').version,'5.9.3');a.equal(c.createHash('sha256').update(fs.readFileSync('node_modules/typescript/lib/typescript.js')).digest('hex'),'${expected}');console.log('typescript version and content verified')`)}`;
let mounted;
async function command(id, body, expectSuccess = true) {
  return step(id, async () => {
    const result = await client.sandbox.exec(name, mounted, `timeout 60 bash -c ${quote(`set -euo pipefail; export PNPM_CONFIG_CACHE_DIR=/tmp/pnpm-cache; ${body}`)}`);
    if (expectSuccess) assert.equal(result.exitCode, 0, JSON.stringify(result));
    else {
      assert.notEqual(result.exitCode, 0, 'ordinary readonly store unexpectedly succeeded');
      assert.match(result.stderr, /ERR_PNPM_NO_OFFLINE_TARBALL/, 'negative control failed for a different reason');
    }
    return result;
  });
}
try {
  if (process.env.CONTROL_FINISH_MOUNT) {
    // Explicit operator continuation after inspecting a retained Dirty receipt;
    // no automatic replay of an uncertain operation.
    mounted = process.env.CONTROL_FINISH_MOUNT;
    assert(/^\/workspaces\/\d+$/.test(mounted));
    await step('explicit-finish-commit', () => client.sandbox.commit(name, mounted, 'persist observed pnpm mount-path metadata'));
  } else {
  await step('create', () => client.sandbox.create(name, 'engineboxfixture', process.env.LAYERFS_IMAGE, { volumes: [{ volume: process.env.PNPM_VOLUME, target: '/pnpm-store', readOnly: true }, { volume: process.env.CACHE_VOLUME, target: '/seed-cache', readOnly: true }] }));
  await step('fork', () => client.branch.fork(name, 'main'));
  mounted = await step('mount', () => client.sandbox.mount(name, name));
  await command('prepare-private-metadata-cache', 'cp -R /seed-cache /tmp/pnpm-cache; chmod -R u+w /tmp/pnpm-cache');
  await command('imported-dependency-content', verify);
  await command('ordinary-readonly-store-negative-control', 'rm -rf node_modules; pnpm install --offline --frozen-lockfile --ignore-scripts --package-import-method=copy --store-dir /pnpm-store --reporter append-only', false);
  await command('frozen-readonly-store-offline-reinstall', `pnpm install --frozen-store --offline --frozen-lockfile --ignore-scripts --package-import-method=copy --store-dir /pnpm-store --reporter append-only; ${verify}`);
  await step('commit', () => client.sandbox.commit(name, mounted, 'explicit frozen readonly store reinstall'));
  await step('unmount', () => client.sandbox.unmount(name, mounted));
  mounted = await step('remount', () => client.sandbox.mount(name, name));
  await command('remounted-dependency-content', verify);
  await command('remounted-pnpm-exec-with-offline-frozen-store', 'PNPM_CONFIG_OFFLINE=true PNPM_CONFIG_FROZEN_STORE=true PNPM_CONFIG_STORE_DIR=/pnpm-store pnpm exec tsc --version');
  await step('commit-pnpm-remount-metadata', () => client.sandbox.commit(name, mounted, 'persist pnpm mount-path metadata'));
  }
  await step('unmount-final', () => client.sandbox.unmount(name, mounted));
  await step('close', () => client.sandbox.close(name));
} finally { await client.close(); }
const db = new DatabaseSync('/layerfs-store/global/store.sqlite', { readOnly: true });
const integrity = db.prepare('PRAGMA integrity_check').all();
assert.equal(integrity.length, 1); assert.equal(Object.values(integrity[0])[0], 'ok');
record({ id: 'sqlite-integrity-check', outcome: 'PASS', result: integrity }); db.close();
