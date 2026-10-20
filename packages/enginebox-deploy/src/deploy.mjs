import { mkdtemp, writeFile, chmod, rm, realpath, copyFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, resolve, join, basename } from 'node:path';
import { fileURLToPath } from 'node:url';
import { isIP } from 'node:net';
import { check, cli, loadConfig, version, selectArtifact, command, run, requireUnixClient } from './lib.mjs';
import { workflowTemporary, cleanupOnSignal } from './temporary.mjs';

export function sshSettings(env) {
  const host = env.ENGINEBOX_DEPLOY_HOST; const user = env.ENGINEBOX_DEPLOY_USER;
  const port = Number(env.ENGINEBOX_DEPLOY_PORT ?? '22');
  check(typeof host === 'string' && (isIP(host) || /^[A-Za-z0-9][A-Za-z0-9.-]{0,252}$/.test(host)), 'invalid-deployment-host');
  check(typeof user === 'string' && /^[a-z_][a-z0-9_-]{0,31}$/.test(user), 'invalid-deployment-user');
  check(Number.isInteger(port) && port >= 1 && port <= 65535, 'invalid-deployment-port');
  const key = env.ENGINEBOX_DEPLOY_SSH_KEY; const known = env.ENGINEBOX_DEPLOY_KNOWN_HOSTS;
  check(typeof key === 'string' && key.includes('PRIVATE KEY-----') && key.length <= 16384, 'deployment-key-required');
  check(typeof known === 'string' && known.length <= 65536, 'verified-host-key-required');
  const expected = port === 22 ? host : `[${host}]:${port}`;
  const lines = known.trim().split('\n');
  check(lines.length > 0 && lines.length <= 16 && lines.every(line => {
    const parts = line.trim().split(/\s+/);
    return parts.length >= 3 && parts[0] === expected && /^(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp(256|384|521))$/.test(parts[1]) && /^[A-Za-z0-9+/]+=*$/.test(parts[2]);
  }), 'known-hosts-must-pin-exact-host');
  return { host, user, port, key, known };
}
export function sshArgs(settings, temporary, remoteCommand) {
  return ['-F', '/dev/null', '-T', '-o', 'BatchMode=yes', '-o', 'IdentitiesOnly=yes', '-o', 'StrictHostKeyChecking=yes', '-o', `UserKnownHostsFile=${join(temporary, 'known_hosts')}`, '-o', 'GlobalKnownHostsFile=/dev/null', '-o', 'ForwardAgent=no', '-o', 'ClearAllForwardings=yes', '-o', 'PermitLocalCommand=no', '-o', 'ConnectTimeout=15', '-i', join(temporary, 'identity'), '-p', String(settings.port), '-l', settings.user, '--', settings.host, remoteCommand];
}
export async function main(args = process.argv.slice(2)) {
  requireUnixClient();
  const options = cli(args, ['config', 'version', 'operation', 'artifacts']);
  check(options.config && ['update', 'rollback', 'status'].includes(options.operation), 'deployment-arguments');
  const pin = options.operation === 'status' ? null : version(options.version);
  const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  const request = { operation: options.operation, version: pin, ...await loadConfig(options.config, root, options.operation === 'update') };
  check(options.operation !== 'update' || options.artifacts, 'artifacts-required');
  const selected = options.operation === 'update' ? await selectArtifact(options.artifacts, pin, request.config.target) : null;
  request.manifest_sha256 = selected?.manifest_sha256 ?? null;
  const settings = sshSettings(process.env);
  const temporaryRoot = process.env.ENGINEBOX_DEPLOY_WORKFLOW_TEMP === '1' ? await workflowTemporary('check') : process.env.RUNNER_TEMP ?? tmpdir();
  const temporary = await realpath(await mkdtemp(join(temporaryRoot, 'enginebox-deploy-')));
  await chmod(temporary, 0o700);
  const unregisterSignals = cleanupOnSignal(temporary);
  // SSH inherits no unrelated CI credentials. The key is never placed in argv.
  const env = { PATH: process.env.PATH, HOME: process.env.HOME, LANG: 'C.UTF-8' };
  const ssh = async (argv, input, timeout, safeJsonErrors = false) => run('ssh', sshArgs(settings, temporary, argv), { input, timeout, env, safeJsonErrors });
  let remote; let completed = false;
  try {
    await writeFile(join(temporary, 'identity'), `${settings.key.trim()}\n`, { mode: 0o600 });
    await writeFile(join(temporary, 'known_hosts'), `${settings.known.trim()}\n`, { mode: 0o600 });
    await writeFile(join(temporary, 'request.json'), JSON.stringify(request), { mode: 0o600 });
    remote = (await ssh('umask 077; mktemp -d /tmp/enginebox-deploy.XXXXXXXXXX')).trim();
    check(/^\/tmp\/enginebox-deploy\.[A-Za-z0-9]{10}$/.test(remote), 'remote-staging-path');
    // Scripts/config are tiny. Upload the already checksummed archive separately
    // without buffering it in Node or copying gigabytes into another local temp.
    for (const file of ['lib.mjs', 'remote.mjs']) await copyFile(join(root, 'src', file), join(temporary, file));
    const tar = join(temporary, 'request.tar');
    await run('tar', ['-cf', tar, '-C', temporary, 'request.json', 'lib.mjs', 'remote.mjs']);
    const { readFile } = await import('node:fs/promises');
    await ssh(command(['tar', '-xf', '-', '-C', remote]), await readFile(tar));
    if (selected) {
      // scp uses its default SFTP transport. All paths and host/user values are
      // independently bounded; filenames come from the exact validated pin.
      const common = ['-F', '/dev/null', '-o', 'BatchMode=yes', '-o', 'IdentitiesOnly=yes', '-o', 'StrictHostKeyChecking=yes', '-o', `UserKnownHostsFile=${join(temporary, 'known_hosts')}`, '-o', 'GlobalKnownHostsFile=/dev/null', '-o', 'ForwardAgent=no', '-o', 'ClearAllForwardings=yes', '-i', join(temporary, 'identity'), '-P', String(settings.port)];
      const address = isIP(settings.host) === 6 ? `[${settings.host}]` : settings.host;
      for (const file of [selected.manifest, selected.archive]) await run('scp', [...common, file, `${settings.user}@${address}:${remote}/${basename(file)}`], { env, timeout: 1200000 });
    }
    const invocation = `umask 077; exec ${command(['flock', '-n', `${request.config.controller.prefix}/deploy.lock`, request.config.remote_node, `${remote}/remote.mjs`, `${remote}/request.json`])}`;
    const result = await ssh(invocation, undefined, (request.config.templates.timeout_seconds + 2400) * 1000, true);
    const parsed = JSON.parse(result);
    check(parsed.ok === true && (pin === null || parsed.version === pin), 'deployment-not-confirmed');
    completed = true;
    process.stdout.write(`${JSON.stringify(parsed)}\n`);
  } finally {
    // A lost SSH response leaves an unknown remote outcome. Retain only the
    // owned artifact directory for inspection; never remove files under an
    // operation which might still be staging a release.
    if (completed && remote) await ssh(command(['rm', '-rf', '--', remote])).catch(() => {});
    try { await rm(temporary, { recursive: true, force: true }); } finally { unregisterSignals(); }
  }
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch(error => {
    const code = /^[a-z0-9-]+$/.test(error.message) ? error.message : 'deployment-failed-inspect-private-server-logs';
    process.stderr.write(`${code}\n`); process.exitCode = 1;
  });
}
