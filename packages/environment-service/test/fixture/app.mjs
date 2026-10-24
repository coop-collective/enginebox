import http from 'node:http';
import net from 'node:net';
import tls from 'node:tls';
import dgram from 'node:dgram';
import { createHash, randomUUID } from 'node:crypto';
import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';
const { version, fail = false } = JSON.parse(readFileSync(new URL('./version.json', import.meta.url)));
if (fail) process.exit(12);
const directory = process.env.ENGINEBOX_DATA_DIR;
const file = join(directory, 'application.json');
if (!existsSync(file)) writeFileSync(file, JSON.stringify({ identity: randomUUID(), count: 0 }));
const revision = process.env.ENGINEBOX_REVISION;
const app = http.createServer((req, res) => {
  if (req.url === '/state' || req.url === '/health') {
    const data = JSON.parse(readFileSync(file));
    if (req.method === 'POST') { data.count++; writeFileSync(file, JSON.stringify(data)); }
    res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({ version, revision, ...data, authorizationForwarded: !!req.headers.authorization }));
  } else {
    res.setHeader('content-type', 'text/html');
    res.end(`<!doctype html><html><head><title>Engine Box live main</title><meta name="viewport" content="width=device-width"></head><body style="font:20px system-ui;max-width:760px;margin:12vh auto;padding:24px;background:#101318;color:#e9f0f6"><h1>Engine Box · main</h1><p id="version">Running ${version}</p><p>Accepted code updates this persistent development service.</p><p>HTTP, WebSocket, TCP, UDP and TLS routing are exercised by its qualification test.</p><small>Revision ${revision}</small><script>const current=${JSON.stringify(revision)};function connect(){const ws=new WebSocket((location.protocol==='https:'?'wss://':'ws://')+location.host+'/refresh');ws.onmessage=e=>{if(JSON.parse(e.data).revision!==current)location.reload()};ws.onclose=()=>setTimeout(connect,1000)}connect()</script></body></html>`);
  }
});
app.on('upgrade', (req, socket) => {
  const key = req.headers['sec-websocket-key']; if (!key) return socket.destroy();
  const accept = createHash('sha1').update(`${key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest('base64');
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
  const payload = Buffer.from(JSON.stringify({ version, revision }));
  socket.write(Buffer.concat([Buffer.from([0x81, payload.length]), payload]));
  socket.on('error', () => {}); socket.on('data', data => { if ((data[0] & 15) === 8) socket.end(Buffer.from([0x88, 0])); });
});
app.listen(3000, '0.0.0.0');
net.createServer(socket => { socket.on('error', () => {}); socket.on('data', data => socket.write(`${version}:${data}`)); }).listen(3001, '0.0.0.0');
const packets = dgram.createSocket('udp4'); packets.on('message', (data, from) => packets.send(`${version}:${data}`, from.port, from.address)); packets.bind(3002, '0.0.0.0');
tls.createServer({ key: readFileSync(join(directory, 'tls.key')), cert: readFileSync(join(directory, 'tls.crt')) }, socket => { socket.on('error', () => {}); socket.on('data', data => socket.write(`${version}:${data}`)); }).listen(3003, '0.0.0.0');
process.on('SIGTERM', () => { app.close(); setTimeout(() => process.exit(0), 200); });
