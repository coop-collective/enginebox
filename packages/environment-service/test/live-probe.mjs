// Opt-in qualification of the disposable fixture provisioned by an operator.
// Does not create/delete environments or change a project source revision.
import { readFile, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import net from 'node:net';
import tls from 'node:tls';
import dgram from 'node:dgram';
import { request } from 'node:http';
import assert from 'node:assert/strict';
const [configFile, expectedVersion, output] = process.argv.slice(2);
assert(configFile && expectedVersion && output, 'Pass PRIVATE_CONFIG EXPECTED_VERSION OUTPUT');
const config = JSON.parse(await readFile(configFile, 'utf8'));
const root = dirname(configFile);
const token = (await readFile(config.tokenFile, 'utf8')).trim();
const api = (path, body) => fetch(`http://127.0.0.1:${config.listenPort}${path}`, { method: body ? 'POST' : 'GET', headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' }, ...(body ? { body: JSON.stringify(body) } : {}) });
const http = (hostname, options = {}) => new Promise((resolve, reject) => {
  const req = request('http://127.0.0.1:18880/state', { ...options, headers: { host: hostname, ...options.headers }, timeout: 5000 }, response => {
    let text = ''; response.on('data', chunk => { text += chunk; if (text.length > 16384) req.destroy(new Error('response-bound')); });
    response.on('end', () => resolve({ status: response.statusCode, json: async () => JSON.parse(text) }));
  }); req.on('error', reject); req.on('timeout', () => req.destroy(new Error('http-timeout'))); req.end();
});
const first = await http('main-dev.coopcollective.dev'); assert.equal(first.status, 200);
const state = await first.json(); assert.equal(state.version, expectedVersion);
const unknown = await http('unregistered.preview.invalid'); assert.equal(unknown.status, 404);
assert.equal((await http('private.preview.invalid')).status, 401);
const password = (await readFile(join(root, 'gateway-password'), 'utf8')).trim();
const authenticated = await http('private.preview.invalid', { headers: { authorization: `Basic ${Buffer.from(`operator:${password}`).toString('base64')}` } });
assert.equal(authenticated.status, 200); assert.equal((await authenticated.json()).authorizationForwarded, false);
const tcp = (port, secure = false) => new Promise((resolve, reject) => {
  const socket = secure ? tls.connect({ host: '127.0.0.1', port, servername: 'tls.preview.invalid', ca: undefined, rejectUnauthorized: true }) : net.connect(port, '127.0.0.1');
  socket.setTimeout(5000, () => { socket.destroy(); reject(new Error('tcp-timeout')); });
  socket.on('error', reject); socket.on(secure ? 'secureConnect' : 'connect', () => socket.write('ping'));
  socket.once('data', data => { socket.end(); resolve(data.toString()); });
});
assert.equal(await tcp(19110), `${expectedVersion}:ping`);
const certificate = await readFile(join(config.stateDir, 'main/data/tls.crt'));
const tlsResult = await new Promise((resolve, reject) => {
  const socket = tls.connect({ host: '127.0.0.1', port: 19112, servername: 'tls.preview.invalid', ca: certificate });
  socket.setTimeout(5000, () => { socket.destroy(); reject(new Error('tls-timeout')); }); socket.on('error', reject);
  socket.on('secureConnect', () => socket.write('ping')); socket.once('data', data => { socket.end(); resolve(data.toString()); });
});
assert.equal(tlsResult, `${expectedVersion}:ping`);
for (const message of ['client-one', 'client-two']) {
  const reply = await new Promise((resolve, reject) => {
    const socket = dgram.createSocket('udp4');
    const timer = setTimeout(() => { socket.close(); reject(new Error('udp-timeout')); }, 5000);
    socket.once('error', reject); socket.once('message', data => { clearTimeout(timer); socket.close(); resolve(data.toString()); });
    socket.send(message, 19111, '127.0.0.1');
  });
  assert.equal(reply, `${expectedVersion}:${message}`);
}
await new Promise((resolve, reject) => {
  const socket = net.connect(18880, '127.0.0.1'); let data = Buffer.alloc(0);
  socket.setTimeout(5000, () => { socket.destroy(); reject(new Error('websocket-timeout')); }); socket.on('error', reject);
  socket.on('connect', () => socket.write('GET /refresh HTTP/1.1\r\nHost: main-dev.coopcollective.dev\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n'));
  socket.on('data', chunk => {
    data = Buffer.concat([data, chunk]); const at = data.indexOf('\r\n\r\n'); if (at < 0 || data.length < at + 6) return;
    assert.match(data.subarray(0, at).toString(), /^HTTP\/1\.1 101/);
    const length = data[at + 5] & 127; if (data.length < at + 6 + length) return;
    assert.equal(JSON.parse(data.subarray(at + 6, at + 6 + length).toString()).version, expectedVersion);
    socket.destroy(); resolve();
  });
});
assert.equal((await fetch(`http://127.0.0.1:${config.listenPort}/v1/environments`)).status, 401);
assert.equal((await api('/v1/environments/routes', { id: 'invalid', environment: 'main', service: 'web', hostname: 'attacker.invalid', access: 'public' })).status, 400);
const saved = await api('/v1/environments/routes', { id: 'live-probe', environment: 'main', service: 'web', hostname: 'alternate.preview.invalid', access: 'public' }); assert.equal(saved.status, 200);
async function awaitStatus(host, status) {
  for (let n = 0; n < 50; n++) { if ((await http(host)).status === status) return; await new Promise(r => setTimeout(r, 100)); }
  throw new Error('gateway-route-reload-timeout');
}
await awaitStatus('alternate.preview.invalid', 200);
assert.equal((await api('/v1/environments/routes', { remove: 'live-probe' })).status, 200);
await awaitStatus('alternate.preview.invalid', 404);
const report = { checkedAt: new Date().toISOString(), version: expectedVersion, revision: state.revision, dataIdentity: state.identity, dataCount: state.count, checks: ['http', 'unknown-host-rejected', 'gateway-authentication', 'auth-header-removed', 'tcp', 'udp-client-sessions', 'tls-sni-passthrough', 'websocket-upgrade-and-frame', 'control-api-authentication', 'domain-allowlist', 'route-hot-add', 'route-hot-remove'] };
await writeFile(output, JSON.stringify(report, null, 2), { mode: 0o600 }); console.log(JSON.stringify(report));
