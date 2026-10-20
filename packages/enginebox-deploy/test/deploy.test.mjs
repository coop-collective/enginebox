import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, realpath, writeFile, readFile, rm, symlink, mkdir, chmod } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createServer } from 'node:http';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { workflowTemporary } from '../src/temporary.mjs';
import { config, template, version, installerArgs, command, run, selectArtifact, sha256, loadConfig } from '../src/lib.mjs';
import { sshSettings, sshArgs } from '../src/deploy.mjs';
import { syncState, execute } from '../src/remote.mjs';
import { pinnedSources } from '../src/build.mjs';
import { plan } from '../src/ci-plan.mjs';

const root = fileURLToPath(new URL('..', import.meta.url));
const example = JSON.parse(await readFile(join(root, 'environments/example.json')));
const recipe = JSON.parse(await readFile(join(root, 'templates/live-native.json')));
const clone = value => structuredClone(value);
async function temporary(t) {
  const dir = await realpath(await mkdtemp(join(tmpdir(), 'enginebox-deploy-test-')));
  await chmod(dir, 0o700);
  t.after(() => rm(dir, { recursive: true, force: true }));
  return dir;
}
test('config and template policies reject commands, secrets, traversal and unowned template IDs', async () => {
  assert.equal(config(clone(example)).schema_version, 1);
  for (const value of ['/opt/../state', '/opt/a\ncommand', '/opt/$(touch-file)', '/opt//state', 'relative', '/']) {
    const candidate = clone(example); candidate.controller.state_dir = value;
    assert.throws(() => config(candidate));
  }
  const external = clone(example); external.controller.url = 'http://example.com:8765';
  assert.throws(() => config(external));
  assert.throws(() => config({ ...example, credentials: 'not allowed' }));
  const script = clone(recipe); script.config.command = 'arbitrary script';
  assert.throws(() => template(script, example));
  const existing = { ...recipe, id: 'native-dev' };
  assert.throws(() => template(existing, example));
  const allowed = clone(example); allowed.templates.allow_ids = ['native-dev'];
  assert.equal(template(existing, allowed).id, 'native-dev');
  assert.equal((await loadConfig(join(root, 'environments/example.json'), root)).templates.length, 0);
});
test('exact pins and CI plans cannot inject workflow outputs or reuse a mutable previous pointer', () => {
  for (const value of ['latest', 'v0.3.1', '0.3.1+sha', '0.3.1-live.01', '0.3.1\nother=x']) assert.throws(() => version(value));
  assert.deepEqual(plan({ GITHUB_RUN_NUMBER: '42' }), { operation: 'update', version: '0.3.1-live.42' });
  assert.throws(() => plan({ ENGINEBOX_DEPLOY_OPERATION: 'rollback' }));
  assert.equal(plan({ ENGINEBOX_DEPLOY_OPERATION: 'rollback', ENGINEBOX_DEPLOY_VERSION: '0.3.0' }).version, '0.3.0');
  const args = installerArgs(example, 'rollback', '0.3.0');
  assert.equal(args[2], 'update');
  assert.ok(args.includes('0.3.0') && !args.includes('activate') && !args.includes('rollback'));
  assert.ok(args.includes(example.controller.token_file));
});
test('artifact selection requires exact version, platform and both checksums', async t => {
  const directory = await temporary(t); const pin = '0.3.1-live.3';
  const stem = `enginebox-${pin}-${example.target}`;
  const archive = join(directory, `${stem}.tar.gz`);
  await writeFile(archive, 'disposable archive fixture');
  const value = { version: pin, target: example.target, archive_sha256: await sha256(archive), state_schema: 1, read_schema_min: 1, read_schema_max: 1, dashboard: true };
  const file = join(directory, `${stem}.json`); await writeFile(file, JSON.stringify(value));
  assert.equal((await selectArtifact(directory, pin, example.target)).manifest_sha256, await sha256(file));
  await assert.rejects(selectArtifact(directory, '0.3.1-live.4', example.target));
  await writeFile(archive, 'modified'); await assert.rejects(selectArtifact(directory, pin, example.target), /artifact-checksum/);
  await rm(archive); await symlink(file, archive); await assert.rejects(selectArtifact(directory, pin, example.target), /symlink/);
});
test('SSH pins an exact host, disables forwarding and keeps key contents out of argv', () => {
  const env = { ENGINEBOX_DEPLOY_HOST: 'ci.example', ENGINEBOX_DEPLOY_USER: 'ubuntu', ENGINEBOX_DEPLOY_SSH_KEY: '-----BEGIN OPENSSH PRIVATE KEY-----\nfixture\n-----END OPENSSH PRIVATE KEY-----', ENGINEBOX_DEPLOY_KNOWN_HOSTS: 'ci.example ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITestFixture' };
  const settings = sshSettings(env);
  const args = sshArgs(settings, '/tmp/owned-fixture', 'true');
  for (const flag of ['StrictHostKeyChecking=yes', 'ForwardAgent=no', 'ClearAllForwardings=yes', 'IdentitiesOnly=yes']) assert.ok(args.includes(flag));
  assert.ok(!args.join(' ').includes(env.ENGINEBOX_DEPLOY_SSH_KEY));
  assert.throws(() => sshSettings({ ...env, ENGINEBOX_DEPLOY_HOST: '-oProxyCommand=whoami' }));
  assert.throws(() => sshSettings({ ...env, ENGINEBOX_DEPLOY_KNOWN_HOSTS: '*.example ssh-ed25519 AAAA' }));
  assert.throws(() => sshSettings({ ...env, ENGINEBOX_DEPLOY_PORT: '2222' }));
});
test('shell boundary preserves literal arguments and child errors do not print private output', async () => {
  const value = 'a\'b;$(printf injected)';
  const echoed = await run('sh', ['-c', command([process.execPath, '-e', 'process.stdout.write(process.argv[1])', value])]);
  assert.equal(echoed, value);
  await assert.rejects(run(process.execPath, ['-e', 'console.error("private-token-fixture");process.exit(1)']), error => !error.message.includes('private-token-fixture'));
});
test('template synchronization waits for every registered node and exposes terminal failure', () => {
  const expected = [{ id: 'live-native', revision: 2, manifest_digest: 'a'.repeat(64) }];
  const catalog = status => ({ templates: [{ ...expected[0], sync: { nodes: [{ status: 'ready' }, { status }] } }] });
  assert.equal(syncState(catalog('pending'), expected), false);
  assert.equal(syncState(catalog('syncing'), expected), false);
  assert.equal(syncState(catalog('ready'), expected), true);
  assert.throws(() => syncState(catalog('failed'), expected), /template-sync-failed/);
  assert.throws(() => syncState(catalog('ready'), [{ ...expected[0], revision: 3 }]), /revision-changed/);
  assert.throws(() => syncState({ templates: [{ ...expected[0], sync: { nodes: [] } }] }, expected), /no-registered-nodes/);
});
test('isolated version patch preserves LF and CRLF while changing only workspace versions', async () => {
  const workspace = resolve(root, '../..');
  const cargoSource = await readFile(join(workspace, 'Cargo.toml'), 'utf8');
  const lockSource = await readFile(join(workspace, 'Cargo.lock'), 'utf8');
  for (const newline of ['\n', '\r\n']) {
    const cargo = cargoSource.replace(/\r?\n/g, newline);
    const lock = lockSource.replace(/\r?\n/g, newline);
    const patched = pinnedSources(cargo, lock, '0.3.1-live.7');
    assert.ok(patched.cargo.includes(`[workspace.package]${newline}version = "0.3.1-live.7"`));
    const versions = [...patched.lock.matchAll(/name = "enginebox-[^"]+"\r?\nversion = "([^"]+)"/g)];
    assert.ok(versions.length >= 5 && versions.every(match => match[1] === '0.3.1-live.7'));
    const normalizeLock = text => text.replace(/(name = "enginebox-[^"]+"\r?\nversion = ")[^"]+/g, '$1VERSION');
    const normalizeCargo = text => text.replace(/(\[workspace.package\]\r?\nversion = ")[^"]+/, '$1VERSION');
    assert.equal(normalizeLock(patched.lock), normalizeLock(lock));
    assert.equal(normalizeCargo(patched.cargo), normalizeCargo(cargo));
    for (const text of [patched.lock, patched.cargo]) assert.ok([...text.matchAll(/\r?\n/g)].every(match => match[0] === newline));
  }
});
test('Windows deployment entrypoints reject before reading config or creating credential files', async () => {
  const deployUrl = new URL('../src/deploy.mjs', import.meta.url).href;
  const temporaryUrl = new URL('../src/temporary.mjs', import.meta.url).href;
  // Exercise the same fail-closed path on every CI platform without changing the parent process.
  const script = `Object.defineProperty(process,'platform',{value:'win32'});Object.defineProperty(process,'getuid',{value:undefined});
    const {main}=await import(${JSON.stringify(deployUrl)});
    const {workflowTemporary,cleanupOnSignal}=await import(${JSON.stringify(temporaryUrl)});
    const outcomes=[];for(const action of [()=>main([]),()=>workflowTemporary('create',{}),()=>cleanupOnSignal('unused')]){
      try{await action();outcomes.push('unexpected-success');}catch(error){outcomes.push(error.message);}
    }process.stdout.write(JSON.stringify(outcomes));`;
  const output = await run(process.execPath, ['--input-type=module', '-e', script]);
  assert.deepEqual(JSON.parse(output), Array(3).fill('deployment-client-requires-unix-permissions'));
});
test('remote fixture updates, verifies template sync, preserves private paths, and rolls back by exact pin', { skip: process.platform !== 'linux' || process.arch !== 'x64' }, async t => {
  const dir = await temporary(t); const prefix = join(dir, 'controller'); const state = join(dir, 'legacy-state');
  await mkdir(prefix, { mode: 0o700 }); await mkdir(state, { mode: 0o700 }); await mkdir(join(prefix, 'etc'), { mode: 0o700 });
  const currentFile = join(dir, 'current'); await writeFile(currentFile, '0.3.0');
  const tokenFile = join(prefix, 'etc/api-token'); await writeFile(tokenFile, 'fixture-token-that-is-private-and-long', { mode: 0o600 });
  const memory = join(state, 'memory'); await writeFile(memory, 'private fixture memory', { mode: 0o600 });
  let registered = null;
  const server = createServer(async (request, response) => {
    assert.equal(request.headers.authorization, 'Bearer fixture-token-that-is-private-and-long');
    const current = await readFile(currentFile, 'utf8'); let data;
    if (request.url === '/__enginebox/status') data = { current };
    else if (request.url === '/readyz') data = { version: current, mode: 'active' };
    else if (request.method === 'POST') {
      let body = ''; for await (const chunk of request) body += chunk;
      registered = { ...JSON.parse(body).manifest, manifest_digest: 'a'.repeat(64) }; data = { template: registered };
    } else data = { templates: [{ ...registered, sync: { nodes: [{ status: 'ready' }] } }] };
    response.setHeader('content-type', 'application/json'); response.end(JSON.stringify(data));
  });
  await new Promise(resolveReady => server.listen(0, '127.0.0.1', resolveReady));
  t.after(() => new Promise(resolveClosed => { server.close(resolveClosed); server.closeAllConnections(); }));
  const deployment = clone(example);
  deployment.controller = { prefix, installer: join(prefix, 'releases/0.3.0/bin/enginebox-install'), supervisor_config: join(prefix, 'etc/supervisor.json'), state_dir: state, token_file: tokenFile, url: `http://127.0.0.1:${server.address().port}` };
  await writeFile(deployment.controller.supervisor_config, JSON.stringify({ prefix, state_dir: state, token_file: tokenFile, listen: `127.0.0.1:${server.address().port}` }), { mode: 0o600 });
  const protocol = { component: 'ebox', protocol: 1, os: 'linux', layerfs: { workspace_schema: 2, attachment_schema: 3, exclusive_guest_writer: true }, capabilities: { reserved_workspace_guard: 1, external_checkout_lifecycle_guard: 1 } };
  for (const pin of ['0.3.0', '0.3.1-live.1']) {
    const binary = join(prefix, 'releases', pin, 'bin'); await mkdir(binary, { recursive: true, mode: 0o700 });
    const stem = `enginebox-${pin}-${example.target}`; const archive = join(dir, `${stem}.tar.gz`); await writeFile(archive, pin);
    const manifest = { version: pin, target: example.target, archive_sha256: await sha256(archive), state_schema: 1, read_schema_min: 1, read_schema_max: 1, dashboard: true };
    await writeFile(join(dir, `${stem}.json`), JSON.stringify(manifest)); await writeFile(join(binary, '../manifest.json'), JSON.stringify(manifest));
    await writeFile(join(binary, 'ebox'), `#!${process.execPath}\nconsole.log(${JSON.stringify(JSON.stringify(protocol))})\n`, { mode: 0o700 });
    await writeFile(join(binary, 'enginebox-controller'), `#!${process.execPath}\nconsole.log('enginebox-controller ${pin}')\n`, { mode: 0o700 });
  }
  await writeFile(deployment.controller.installer, `#!${process.execPath}\nconst fs=require('node:fs');const a=process.argv.slice(2);if(a[2]!=='update')process.exit(1);fs.writeFileSync(${JSON.stringify(currentFile)},a[a.indexOf('--version')+1]);\n`, { mode: 0o700 });
  const manifestHash = await sha256(join(dir, `enginebox-0.3.1-live.1-${example.target}.json`));
  await assert.rejects(execute({ operation: 'update', version: '0.3.1-live.1', config: deployment, templates: [recipe], manifest_sha256: '0'.repeat(64) }, dir), /transferred-manifest/);
  const updated = await execute({ operation: 'update', version: '0.3.1-live.1', config: deployment, templates: [recipe], manifest_sha256: manifestHash }, dir);
  assert.equal(updated.version, '0.3.1-live.1'); assert.equal(updated.templates_ready, 1);
  const rolled = await execute({ operation: 'rollback', version: '0.3.0', config: deployment, templates: [], manifest_sha256: null }, dir);
  assert.equal(rolled.version, '0.3.0');
  assert.equal((await execute({ operation: 'status', version: null, config: deployment, templates: [], manifest_sha256: null }, dir)).ready, true);
  assert.equal(await readFile(memory, 'utf8'), 'private fixture memory');
  assert.equal(await readFile(tokenFile, 'utf8'), 'fixture-token-that-is-private-and-long');
});

test('health and rollback configuration do not need template recipes to exist', async t => {
  const dir = await temporary(t); const value = clone(example); value.templates.files = ['templates/missing.json'];
  const selected = join(dir, 'deployment.json'); await writeFile(selected, JSON.stringify(value));
  await assert.rejects(loadConfig(selected, dir));
  assert.deepEqual((await loadConfig(selected, dir, false)).templates, []);
});
test('workflow cleanup refuses unowned directories and signal cleanup removes credentials', { skip: process.platform === 'win32' }, async t => {
  const dir = await temporary(t); const env = { RUNNER_TEMP: dir, GITHUB_RUN_ID: '123', GITHUB_RUN_ATTEMPT: '1' };
  const owned = await workflowTemporary('create', env);
  await writeFile(join(owned, 'identity'), 'fixture-key', { mode: 0o600 });
  await assert.rejects(workflowTemporary('create', env));
  await writeFile(join(owned, 'owner.json'), '{}');
  await assert.rejects(workflowTemporary('cleanup', env), /marker-mismatch/);
  await writeFile(join(owned, 'owner.json'), JSON.stringify({ root: dir, run: '123', attempt: '1' }));
  await workflowTemporary('cleanup', env);
  await assert.rejects(readFile(join(owned, 'identity')));
  const signalDir = join(dir, 'signal'); await mkdir(signalDir, { mode: 0o700 });
  await writeFile(join(signalDir, 'identity'), 'fixture-key', { mode: 0o600 });
  const moduleUrl = new URL('../src/temporary.mjs', import.meta.url).href;
  const child = spawn(process.execPath, ['--input-type=module', '-e', `import {cleanupOnSignal} from ${JSON.stringify(moduleUrl)};cleanupOnSignal(${JSON.stringify(signalDir)});console.log('ready');setInterval(()=>{},1000);`]);
  await once(child.stdout, 'data'); const closed = once(child, 'close'); child.kill('SIGTERM');
  assert.equal((await closed)[0], 143);
  await assert.rejects(readFile(join(signalDir, 'identity')));
});
