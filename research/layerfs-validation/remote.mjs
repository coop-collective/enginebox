// Validation transport only: provisioning/admission and cleanup belong to the
// operator's existing fleet tools. Never adopts, creates or deletes a host VM.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { appendFileSync, chmodSync, lstatSync, mkdirSync, readFileSync } from 'node:fs';
import { isAbsolute, resolve } from 'node:path';

const [configFile, action, ...args] = process.argv.slice(2);
assert(configFile && action, 'usage: node remote.mjs PRIVATE_CONFIG check-config|inspect|exec|push [ARGS]');
const stat = lstatSync(configFile);
assert(stat.isFile() && !stat.isSymbolicLink(), 'configuration must be a regular file');
const c = JSON.parse(readFileSync(configFile, 'utf8'));
assert(typeof c.host === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9_.@:-]{0,200}$/.test(c.host), 'explicit SSH host required');
for (const field of ['project', 'vm', 'owner']) {
  assert(typeof c[field] === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,100}$/.test(c[field]), `invalid ${field}`);
}
assert(c.vm.startsWith('ebox-layerfs-'), 'fixture name must begin ebox-layerfs-');
assert(typeof c.output === 'string' && isAbsolute(c.output), 'absolute private output directory required');
assert(['check-config', 'inspect', 'exec', 'push'].includes(action), 'unknown action');
if (action === 'check-config') {
  console.log('Configuration shape valid. No host accessed or capacity admitted.');
  process.exit(0);
}
const output = resolve(c.output);
mkdirSync(output, { recursive: true, mode: 0o700 });
const outputStat = lstatSync(output);
assert(outputStat.isDirectory() && !outputStat.isSymbolicLink(), 'output must be a real directory');
chmodSync(output, 0o700);
const quote = value => `'${String(value).replaceAll("'", "'\\''")}'`;
function ssh(argv, options = {}) {
  // Bound the remote process too: stopping SSH alone can leave it alive.
  const command = ['timeout', '--kill-after=5s', '180s', ...argv].map(quote).join(' ');
  const r = spawnSync('ssh', ['-T', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10',
    '-o', 'ServerAliveInterval=10', '-o', 'ServerAliveCountMax=2', c.host, command],
    { encoding: 'utf8', timeout: 195000, maxBuffer: 64 * 1024 * 1024, ...options });
  const log = `${output}/remote-log.jsonl`;
  appendFileSync(log, JSON.stringify({ at: new Date().toISOString(), args: argv,
    status: r.status, error: r.error?.message, stdout: r.stdout, stderr: r.stderr }) + '\n', { mode: 0o600 });
  chmodSync(log, 0o600);
  assert.equal(r.status, 0, `remote operation failed or uncertain; inspect the private receipt before another attempt: ${r.error?.message ?? r.stderr ?? ''}`);
  return r.stdout;
}
const incus = (...argv) => ssh(['sudo', '-n', 'incus', ...argv]);
const vm = JSON.parse(incus('query', `/1.0/instances/${c.vm}?project=${c.project}`));
assert.equal(vm.type, 'virtual-machine', 'fixture must be a VM');
assert.equal(vm.config['user.enginebox.experiment'], c.owner, 'refuse unrelated VM');
if (action === 'inspect') {
  console.log(JSON.stringify({ name: vm.name, type: vm.type, status: vm.status }));
} else if (action === 'exec') {
  assert(args.length, 'exec needs a guest command');
  process.stdout.write(incus('exec', c.vm, '--project', c.project, '--', ...args));
} else if (action === 'push') {
  const [local, guest] = args;
  assert(args.length === 2 && guest.startsWith('/opt/layerfs-validation/') && !guest.split('/').includes('..'), 'push target must be inside the fixture directory');
  ssh(['sudo', '-n', 'incus', 'exec', c.vm, '--project', c.project, '--', 'sh', '-c',
    'umask 077; cat > "$1"', 'sh', guest], { input: readFileSync(local) });
}
