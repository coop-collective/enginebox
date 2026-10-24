// Narrow bridge to the pinned, qualified upstream SDK. Engine Box policy,
// leases, resource admission and receipts stay in Rust. No agent loop here.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { mergeFrozen, inspectFrozen } from './integration.mjs';
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
const quote = text => `'${text.replaceAll("'", "'\\''")}'`;
let answer;
try {
  let result;
  switch (request.operation) {
    case 'inspect': result = await inspectFrozen(client, request.reference, request.inspectPrefix); break;
    case 'integrate': result = await mergeFrozen(client, request.integration); break;
    case 'snapshot': {
      const { DatabaseSync, backup } = await import('node:sqlite');
      const db = new DatabaseSync(process.env.LAYERFS_STORE_PATH, {readOnly:true});
      try { await backup(db, request.output); } finally { db.close(); }
      result = {snapshot:request.output}; break;
    }
    case 'import': result = await client.project.init(request.source); break;
    case 'create': {
      const volumes = request.cache ? [
        { volume: request.cache.store, target: '/pnpm-store', readOnly: true },
        { volume: request.cache.metadata, target: '/pnpm-metadata', readOnly: true },
      ] : [];
      result = await client.sandbox.create(request.sandbox, request.project, request.image, { volumes }); break;
    }
    case 'fork': result = await client.branch.fork(request.branch, request.from ?? 'main'); break;
    case 'mount': result = await client.sandbox.mount(request.sandbox, request.branch); break;
    case 'status': result = await client.sandbox.status(request.sandbox, request.mount); break;
    case 'checkpoint': result = await client.sandbox.commit(request.sandbox, request.mount, request.message); break;
    case 'unmount': result = await client.sandbox.unmount(request.sandbox, request.mount); break;
    case 'close': result = await client.sandbox.close(request.sandbox); break;
    case 'exec': {
      assert(Array.isArray(request.argv) && request.argv.length > 0 && request.argv.every(v => typeof v === 'string' && !v.includes('\0')));
      const command = `cd /project && exec timeout --signal=TERM --kill-after=5s ${request.timeout}s ${request.argv.map(quote).join(' ')}`;
      result = await client.sandbox.exec(request.sandbox, request.mount, command); break;
    }
    default: throw new Error('Unknown storage operation');
  }
  answer = { ok: true, result };
} catch (error) {
  answer = { ok: false, error: { code: error.code ?? 'Unknown', message: error.message, detail: error.detail } };
} finally {
  try { await client.close(); } catch (error) { answer = { ok: false, error: { code: 'Unknown', message: error.message, detail: error.detail } }; }
}
console.log(JSON.stringify(answer, (_, value) => typeof value === 'bigint' ? String(value) : value));
