// Additive adapter for the same pinned SDK; the existing bridge binding stays intact.
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
  if (request.operation === 'project-head') {
    for (let pos = 0; pos < 100000;) {
      const page = await client.project.list_branches(50, pos);
      assert(Array.isArray(page.items), 'Invalid branch page');
      result = page.items.find(row => row.name === (request.branch ?? 'main'));
      if (result || !page.hasMore) break;
      assert(Number.isSafeInteger(page.nextPagePos) && page.nextPagePos > pos, "Invalid branch cursor");
      pos = page.nextPagePos;
    }
    assert(result, 'Branch not found');
  } else if (request.operation === 'project-merge') {
    result = await client.branch.merge(request.branch, 'main', request.candidate, request.message);
  } else if (request.operation === 'project-publish') {
    result = await client.branch.publish(request.branch, request.commit ?? undefined);
  } else throw Error('Unknown publication operation');
  answer = { ok: true, result };
} catch (error) {
  answer = { ok: false, error: { code: error.code ?? 'Unknown', message: error.message } };
} finally {
  try { await client.close(); } catch (error) { answer = { ok: false, error: { code: 'Unknown', message: error.message } }; }
}
console.log(JSON.stringify(answer, (_, value) => typeof value === 'bigint' ? String(value) : value));
