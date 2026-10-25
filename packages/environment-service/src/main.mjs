import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { timingSafeEqual, createHash } from 'node:crypto';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { validateConfig, check, fields, id } from './config.mjs';
import { Environments } from './runtime.mjs';

export async function serve(config, run) {
  validateConfig(config);
  const token = (await readFile(config.tokenFile, 'utf8')).trim();
  check(token.length >= 32, 'private-api-token-required');
  const hash = value => createHash('sha256').update(value).digest();
  const manager = new Environments(config, run);
  let ready = false; let mutations = Promise.resolve();
  const server = createServer(async (request, response) => {
    const reply = (status, body) => { response.writeHead(status, { 'content-type': 'application/json', 'cache-control': 'no-store', 'x-content-type-options': 'nosniff' }); response.end(JSON.stringify(body)); };
    try {
      if (!timingSafeEqual(hash(request.headers.authorization ?? ''), hash(`Bearer ${token}`))) return reply(401, { error: 'unauthorized' });
      if (!ready) return reply(503, { error: 'initializing' });
      if (request.url === '/v1/environments' && request.method === 'GET') return reply(200, manager.catalog());
      if (request.url === '/v1/environments/previews' && request.method === 'GET') return reply(200, { previews: manager.previews.catalog() });
      if (request.url?.startsWith('/v1/environments/previews/') && request.method === 'GET') {
        const key = request.url.slice('/v1/environments/previews/'.length);
        const preview = id(key) && manager.previews.records[key];
        return preview ? reply(200, manager.previews.summary(preview)) : reply(404, { error: 'unknown-preview' });
      }
      if (request.url === '/v1/environments/previews' && request.method === 'POST') {
        let body = ''; for await (const chunk of request) { body += chunk; check(Buffer.byteLength(body) <= 8192, 'body-too-large'); }
        const value = JSON.parse(body);
        return reply(value.id ? 200 : 202, await manager.previews.register(value));
      }
      if (request.url === '/v1/environments/projects' && request.method === 'GET') return reply(200, { projects: manager.projects.catalog() });
      if (request.url === '/v1/environments/projects' && request.method === 'POST') {
        let body = ''; for await (const chunk of request) { body += chunk; check(Buffer.byteLength(body) <= 8192, 'body-too-large'); }
        return reply(202, await manager.projects.register(JSON.parse(body)));
      }
      if (request.url === '/v1/environments/routes' && request.method === 'POST') {
        let body = ''; for await (const chunk of request) { body += chunk; check(Buffer.byteLength(body) <= 8192, 'body-too-large'); }
        const value = JSON.parse(body);
        const next = mutations.then(async () => {
          if (value.remove !== undefined) { fields(value, ['remove']); check(id(value.remove), 'invalid-route-id'); await manager.deleteRoute(value.remove); }
          else await manager.setRoute(value);
        });
        mutations = next.catch(() => {}); await next;
        return reply(200, manager.catalog());
      }
      return reply(404, { error: 'unknown-operation' });
    } catch (error) { reply(400, { error: /^[a-z0-9-]+$/.test(error.message) ? error.message : 'request-failed' }); }
  });
  server.requestTimeout = 10000; server.headersTimeout = 5000;
  await new Promise((resolveListen, reject) => { server.once('error', reject); server.listen(config.listenPort, '127.0.0.1', resolveListen); });
  try { await manager.init(); ready = true; } catch (error) { server.close(); throw error; }
  let stopped = false; let timer;
  const tick = async () => { try { await manager.tick(); } catch { console.error('environment-reconcile-failed'); } finally { if (!stopped) timer = setTimeout(tick, config.pollSeconds * 1000); } };
  void tick();
  // Containers remain supervised by Docker while the control service restarts.
  return { manager, server, close: async () => { stopped = true; clearTimeout(timer); const leasesClosed = manager.previews.close(); await new Promise(r => server.close(r)); while (manager.busy) await new Promise(r => setTimeout(r, 50)); await leasesClosed; } };
}
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const service = await serve(JSON.parse(await readFile(process.argv[2], 'utf8')));
  for (const signal of ['SIGTERM', 'SIGINT']) process.once(signal, () => { void service.close(); });
}
