// Explicit local-only qualification. Never upload the supplied source.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { constants, existsSync, mkdirSync, readdirSync, lstatSync, statSync, statfsSync, copyFileSync, readFileSync, writeFileSync, openSync, closeSync, appendFileSync } from 'node:fs';
import { resolve, join, relative } from 'node:path';
import { createReadStream } from 'node:fs';
import { once } from 'node:events';

const [sourceArg, outputArg, binArg] = process.argv.slice(2);
assert(sourceArg && outputArg && binArg, 'usage: node lore.mjs SOURCE FRESH_OUTPUT LORE_BIN_DIRECTORY');
const source = resolve(sourceArg), output = resolve(outputArg), bin = resolve(binArg);
assert(!existsSync(output), 'Refuse existing output');
assert(!output.startsWith(source + '/'), 'Output must be outside source');
mkdirSync(output, { mode: 0o700, recursive: true });
const receipts = [], report = { schema: 1, boundary: 'local loopback; no Cloudflare Artifacts or R2 transfer', receipts };
const save = () => writeFileSync(join(output, 'report.json'), JSON.stringify(report, null, 2), { mode: 0o600 });
const env = { PATH: process.env.PATH, LANG: 'en_US.UTF-8', LORE_MAX_THREAD: '4', LORE_GLOBAL_PATH: join(output, 'lore-global'), LORE_SERVICE_SOCKET: `ebox-benchmark-${process.pid}.sock` };
async function run(label, executable, args, cwd = output) {
  const start = performance.now();
  const fd = openSync(join(output, `${label}.log`), 'wx', 0o600);
  const child = spawn(executable, args, { cwd, env, stdio: ['ignore', fd, fd] });
  const timer = setTimeout(() => child.kill('SIGKILL'), 900_000);
  let code, signal;
  try { [code, signal] = await once(child, 'exit'); } finally { clearTimeout(timer); closeSync(fd); }
  receipts.push({ label, seconds: (performance.now() - start) / 1000, code, signal }); save();
  assert.equal(code, 0, `${label} failed; inspect private log`);
}
const cli = (label, args, cwd) => run(label, join(bin, 'lore'), ['--non-interactive', '--no-pager', '--sync-data', ...args], cwd);
function files(root) {
  assert(lstatSync(root).isDirectory() && !lstatSync(root).isSymbolicLink(), 'Need a real directory');
  const result = [];
  function walk(path) {
    for (const entry of readdirSync(path, { withFileTypes: true })) {
      assert(!entry.isSymbolicLink(), 'Symlinks require separate qualification');
      const p = join(path, entry.name);
      if (entry.isDirectory()) walk(p);
      else { assert(entry.isFile(), 'Special file refused'); result.push(p); }
    }
  }
  walk(root); return result.sort();
}
async function manifest(root, prefixes) {
  const hashes = new Map(); let bytes = 0;
  for (const prefix of prefixes) for (const path of files(join(root, prefix))) {
    const h = createHash('sha256');
    for await (const chunk of createReadStream(path)) h.update(chunk);
    hashes.set(relative(root, path), h.digest('hex')); bytes += statSync(path).size;
  }
  return { hashes, bytes, files: hashes.size };
}
function verify(expected, actual) { assert.equal(actual.bytes, expected.bytes); assert.deepEqual(actual.hashes, expected.hashes); }
function disk(root) { return files(root).reduce((n, p) => n + lstatSync(p).size, 0); }
const prefixes = ['Assets', 'Packages', 'ProjectSettings'];
const repo = join(output, 'source-copy'); mkdirSync(repo);
const baseline = await manifest(source, prefixes);
assert(baseline.bytes < 20 * 1024 ** 3, 'Fixture exceeds 20 GiB budget');
const space = statfsSync(output);
assert(space.bavail * space.bsize >= baseline.bytes * 6 + 4 * 1024 ** 3, 'Need space for private copies, stores, and a 4 GiB reserve');
report.fixture = { files: baseline.files, bytes: baseline.bytes };
for (const prefix of prefixes) for (const path of files(join(source, prefix))) {
  const destination = join(repo, relative(source, path));
  mkdirSync(resolve(destination, '..'), { recursive: true });
  copyFileSync(path, destination, constants.COPYFILE_FICLONE);
}
verify(baseline, await manifest(repo, prefixes));
const config = join(output, 'server-config'); mkdirSync(config);
const store = join(output, 'server-store'); mkdirSync(store);
const port = 43187, http = 43189;
writeFileSync(join(config, 'local.toml'), `[server.quic]\nhost="127.0.0.1"\nport=${port}\nnum_listeners=2\n[server.grpc]\nhost="127.0.0.1"\nport=${port}\n[server.http]\nhost="127.0.0.1"\nport=${http}\n[server.quic_internal]\nenabled=false\n[server.grpc_internal]\nenabled=false\n[immutable_store.local]\npath=${JSON.stringify(store)}\nflush_delay_seconds=0\n[mutable_store.local]\npath=${JSON.stringify(store)}\nflush_delay_seconds=0\n[topology]\nprovider="none"\n`);
// Refuse an existing listener before starting an owned process.
const net = await import('node:net');
for (const p of [port, http]) {
  const probe = net.createServer(); probe.listen(p, '127.0.0.1'); await once(probe, 'listening'); await new Promise(r => probe.close(r));
}
const serverLog = openSync(join(output, 'server.log'), 'wx', 0o600);
const server = spawn(join(bin, 'loreserver'), ['--config', config], { env, stdio: ['ignore', serverLog, serverLog] });
const serverExited = once(server, 'exit');
const remote = `lore://127.0.0.1:${port}/fixture`;
try {
  let healthy = false;
  for (let attempt = 0; attempt < 60; attempt++) {
    assert(server.exitCode === null, 'Server exited before readiness');
    try { healthy = (await fetch(`http://127.0.0.1:${http}/health_check`, { signal: AbortSignal.timeout(500) })).ok; } catch {}
    if (healthy) break;
    await new Promise(r => setTimeout(r, 250));
  }
  assert(healthy, 'Owned server did not become healthy');
  await cli('create', ['repository', 'create', remote, '--identity', 'benchmark@example.invalid'], repo);
  const clientConfig = join(repo, '.lore/config.toml');
  writeFileSync(clientConfig, readFileSync(clientConfig, 'utf8').replace(/max_capacity = \d+/, 'max_capacity = 21474836480').replace(/max_size = \d+/, 'max_size = 21474836480'));
  await cli('stage-initial', ['stage', '--scan', ...prefixes], repo);
  await cli('commit-initial', ['commit', 'Private benchmark baseline copy', '--stats'], repo);
  report.localStoreInitialBytes = disk(join(repo, '.lore'));
  await cli('push-initial-loopback', ['push', '--stats'], repo);
  report.serverInitialBytes = disk(store);
  const shared = join(output, 'shared-store');
  await cli('shared-store-create', ['shared-store', 'create', `lore://127.0.0.1:${port}`, '--path', shared, '--make-default', 'false']);
  const sharedConfig = files(shared).find(p => p.endsWith('shared_store.toml'));
  assert(sharedConfig, 'Shared store configuration missing');
  writeFileSync(sharedConfig, readFileSync(sharedConfig, 'utf8').replace(/max_capacity = \d+/, 'max_capacity = 21474836480').replace(/max_size = \d+/, 'max_size = 21474836480'));
  const a = join(output, 'clone-a'), b = join(output, 'clone-b');
  await cli('clone-empty-cache', ['clone', remote, a, '--use-shared-store', '--shared-store-path', shared, '--cache', '--stats']);
  verify(baseline, await manifest(a, prefixes));
  report.sharedStoreAfterFirstCloneBytes = disk(shared);
  await cli('clone-shared-cache', ['clone', remote, b, '--use-shared-store', '--shared-store-path', shared, '--cache', '--stats']);
  verify(baseline, await manifest(b, prefixes));
  report.sharedStoreAfterSecondCloneBytes = disk(shared);
  // Modify only the private copy: a tiny text delta and 4 KiB in one large asset.
  const asset = files(join(repo, 'Assets')).filter(p => p.endsWith('.tif') && statSync(p).size > 1024 ** 2)[0];
  assert(asset, 'Need a large TIFF for the incremental workload');
  const { writeSync } = await import('node:fs');
  const fd = openSync(asset, 'r+'); writeSync(fd, Buffer.alloc(4096, 90), 0, 4096, 1024 ** 2); closeSync(fd);
  appendFileSync(join(repo, 'ProjectSettings/ProjectVersion.txt'), '\n# isolated storage benchmark\n');
  await cli('stage-delta', ['stage', relative(repo, asset), 'ProjectSettings/ProjectVersion.txt'], repo);
  await cli('commit-delta', ['commit', 'Isolated small asset change', '--stats'], repo);
  report.localStoreAfterDeltaBytes = disk(join(repo, '.lore'));
  await cli('push-delta-loopback', ['push', '--stats'], repo);
  report.serverAfterDeltaBytes = disk(store);
  await cli('sync-delta-loopback', ['sync', '--stats'], a);
  const changed = await manifest(repo, prefixes); verify(changed, await manifest(a, prefixes));
  verify(baseline, await manifest(source, prefixes));
  report.sourceUnchanged = true; report.ok = true;
} catch (error) { report.ok = false; report.error = String(error); throw error; }
finally {
  server.kill('SIGTERM');
  const timer = setTimeout(() => server.kill('SIGKILL'), 30_000);
  await serverExited; clearTimeout(timer); closeSync(serverLog); save();
}
console.log(JSON.stringify({ ok: report.ok, fixture: report.fixture, receipts }));
