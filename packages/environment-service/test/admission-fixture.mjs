import { createServer } from 'node:http';
import { writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import assert from 'node:assert/strict';

export async function admissionFixture(root) {
  const token = 'f'.repeat(64); const tokenFile = join(root, 'owner-token');
  await writeFile(tokenFile, token, { mode: 0o600 });
  let mode = { allowed: true }; const requests = [];
  const server = createServer((request, response) => {
    requests.push(request.url);
    assert.equal(request.headers.authorization, `Bearer ${token}`);
    if (mode.hang) return;
    response.writeHead(mode.status ?? 200, { 'content-type': mode.type ?? 'application/json', ...(mode.location ? { location: mode.location } : {}) });
    response.end(mode.raw ?? JSON.stringify(mode.body ?? { host_id: 'node', allowed: mode.allowed }));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  return { config: { controller: `http://127.0.0.1:${server.address().port}`, tokenFile, hostId: 'node' }, token, requests,
    set(value) { mode = value; }, close: () => new Promise(resolve => { server.closeAllConnections(); server.close(resolve); }) };
}
