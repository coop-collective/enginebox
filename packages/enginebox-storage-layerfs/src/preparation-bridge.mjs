// Narrow bridge to the pinned, qualified upstream SDK. Engine Box policy,
// leases, resource admission and receipts stay in Rust. No agent loop here.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const request = JSON.parse(readFileSync(0, 'utf8'));
process.env.LAYERFS_HOME = request.home;
process.env.LAYERFS_VOLUME = request.volume;
process.env.LAYERFS_DAEMON = '/opt/enginebox-layerfs/daemon';
process.env.LAYERFS_NATIVE = `${request.sdk}/native/layerfs.node`;
process.env.LAYERFS_CONTAINER_PREFIX = request.prefix;
process.env.LAYERFS_STORE_PATH = '/layerfs-store/global/store.sqlite';
process.env.LAYERFS_CONSTRUCTION_WORKERS = '1';
process.env.UV_THREADPOOL_SIZE = '4';
const { LayerFS } = await import(`${request.sdk}/src/index.js`);
const client = new LayerFS(request.project, { storePath: process.env.LAYERFS_STORE_PATH });
let answer;
try {
  let result;
  switch (request.operation) {
    case 'preparation-inspect': {
      assert(/^[a-f0-9]{64}$/.test(request.key));
      const listing = await client.fs.list('main', '/', 10);
      assert(!listing.hasMore && Array.isArray(listing.items));
      assert(listing.items.every(item => ['caches','prepared'].includes((item.path ?? item.name).replace(/^\//, '').replace(/\/$/, ''))), 'Preparation contains job/source files');
      const read = await client.fs.read('main', `/prepared/${request.key}/manifest.json`, 1, 4096);
      assert(typeof read.text === 'string' && Buffer.byteLength(read.text) <= 256 * 1024 && !read.truncated, 'Invalid bounded preparation manifest');
      const manifest = JSON.parse(read.text);
      assert.equal(manifest.key, request.key);
      assert.equal(manifest.inputs.policy.scripts, 'deny');
      assert.equal(manifest.inputs.recipe, 'pnpm-installed-v1');
      result = {key:request.key}; break;
    }
    default: throw new Error('Unknown preparation operation');
  }
  answer = { ok: true, result };
} catch (error) {
  answer = { ok: false, error: { code: error.code ?? 'Unknown', message: error.message, detail: error.detail } };
} finally {
  try { await client.close(); } catch (error) { answer = { ok: false, error: { code: 'Unknown', message: error.message, detail: error.detail } }; }
}
console.log(JSON.stringify(answer, (_, value) => typeof value === 'bigint' ? String(value) : value));
