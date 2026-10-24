import { spawn } from 'node:child_process';
import { mkdir, readFile, writeFile, rename, open, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { check, gatewayConfig, validateRoutes } from './config.mjs';
import { acceptedSource, copyAccepted } from './accepted.mjs';
import { layerfsSource } from './layerfs.mjs';

export async function atomic(file, value) {
  const temporary = `${file}.${randomUUID()}.tmp`;
  const handle = await open(temporary, 'wx', 0o600);
  try { await handle.writeFile(JSON.stringify(value, null, 2)); await handle.sync(); } finally { await handle.close(); }
  await rename(temporary, file);
  const parent = await open(join(file, '..'), 'r');
  try { await parent.sync(); } finally { await parent.close(); }
}
export async function load(file, fallback) {
  try { return JSON.parse(await readFile(file, 'utf8')); } catch (error) { if (error.code === 'ENOENT') return fallback; throw error; }
}
export function command(program, args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(program, args, { cwd: options.cwd, env: { PATH: process.env.PATH, LANG: 'C.UTF-8', HOME: options.home ?? '/nonexistent', GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null', GIT_TERMINAL_PROMPT: '0' }, stdio: ['ignore', 'pipe', 'pipe'] });
    let output = ''; let bytes = 0; let failure;
    const timer = setTimeout(() => { failure = 'command-timeout'; child.kill('SIGKILL'); }, options.timeout ?? 120000);
    for (const stream of [child.stdout, child.stderr]) stream.on('data', data => {
      bytes += data.length;
      if (bytes > 2 * 1024 * 1024) { failure = 'command-output-limit'; child.kill('SIGKILL'); }
      else output += data.toString();
    });
    child.once('error', () => { clearTimeout(timer); reject(new Error('command-start-failed')); });
    child.once('close', code => { clearTimeout(timer); if (code === 0 && !failure) resolve(output.trim()); else reject(new Error(failure ?? 'command-failed')); });
  });
}

export class Environments {
  constructor(config, run = command) {
    this.config = config; this.run = run; this.states = {}; this.routes = []; this.busy = false;
    this.gatewayWrites = Promise.resolve();
    this.owner = createHash('sha256').update(config.stateDir).digest('hex').slice(0, 16);
    this.routeFile = join(config.stateDir, 'routes.json');
    this.stateFile = join(config.stateDir, 'environments.json');
    // JSON is valid YAML; Traefik's directory provider selects .yml/.yaml files.
    this.dynamicFile = join(config.stateDir, 'gateway', 'routes.yml');
  }
  async init() {
    await mkdir(join(this.config.stateDir, 'gateway'), { recursive: true, mode: 0o700 });
    const metadata = await stat(this.config.stateDir);
    check((metadata.mode & 0o077) === 0, 'state-directory-must-be-private');
    this.states = await load(this.stateFile, {});
    this.routes = validateRoutes(await load(this.routeFile, []), this.config);
    // No stale healthy flag is trusted across a process restart.
    for (const state of Object.values(this.states)) state.healthy = false;
    await this.render();
  }
  async save() { await atomic(this.stateFile, this.states); }
  async render() {
    const write = this.gatewayWrites.then(() => atomic(this.dynamicFile, gatewayConfig(this.routes, this.config, new Set(Object.entries(this.states).filter(([, s]) => s.healthy).map(([id]) => id)))));
    this.gatewayWrites = write.catch(() => {}); await write;
  }
  catalog() {
    return { schema: 1, baseDomains: this.config.baseDomains, transportEntrypoints: this.config.transportEntrypoints, routes: this.routes,
      environments: this.config.environments.map(e => ({ id: e.id, source: { kind: e.source.kind, branch: e.source.branch }, services: e.ports.map(({ name, protocol }) => ({ name, protocol })), cpu: e.cpu, memoryMiB: e.memoryMiB, ...(this.states[e.id] ?? { status: 'pending', healthy: false }) })) };
  }
  async setRoute(route) {
    const next = this.routes.filter(v => v.id !== route.id).concat([route]);
    validateRoutes(next, this.config);
    await atomic(this.routeFile, next); this.routes = next; await this.render();
  }
  async deleteRoute(id) { const next = this.routes.filter(r => r.id !== id); await atomic(this.routeFile, next); this.routes = next; await this.render(); }
  container(env, suffix = '') { return `ebox-dev-${this.owner}-${env.id}${suffix}`; }
  async docker(args, options) { return this.run(this.config.docker ?? 'docker', args, options); }
  async inspect(env, suffix = '') {
    const name = this.container(env, suffix);
    // Listing and exact identity checks distinguish absence from an unavailable daemon.
    const found = await this.docker(['ps', '-a', '--filter', `name=^/${name}$`, '--format', '{{.Names}}']);
    if (!found) return null;
    check(found === name, 'container-name-mismatch');
    const data = JSON.parse(await this.docker(['inspect', name]));
    check(data.length === 1 && data[0].Config.Labels['enginebox.environment.owner'] === this.owner && data[0].Config.Labels['enginebox.environment.id'] === env.id, 'unowned-container-refused');
    return data[0];
  }
  async remove(env, suffix = '') { if (await this.inspect(env, suffix)) await this.docker(['rm', '-f', this.container(env, suffix)]); }
  async source(env) {
    const root = join(this.config.stateDir, env.id); await mkdir(root, { recursive: true, mode: 0o700 });
    if (env.source.kind === 'accepted') return acceptedSource(env, this.config.stateDir);
    if (env.source.kind === 'layerfs') return layerfsSource(env, this.config.stateDir, this.run);
    const repo = join(root, 'repository.git');
    try { await stat(repo); } catch (error) { if (error.code !== 'ENOENT') throw error; await this.run('git', ['init', '--bare', repo]); }
    await this.run('git', ['-c', 'core.hooksPath=/dev/null', '-C', repo, 'fetch', '--no-tags', '--force', '--', env.source.url, `refs/heads/${env.source.branch}:refs/heads/tracked`]);
    const revision = await this.run('git', ['-C', repo, 'rev-parse', 'refs/heads/tracked^{commit}']);
    check(/^[a-f0-9]{40,64}$/.test(revision), 'invalid-source-revision');
    return { revision, repo, root };
  }
  async prepare(env, source) {
    const release = join(source.root, source.revision); const marker = join(release, '.enginebox-prepared');
    if (await load(marker, null)) return release;
    // Each failed attempt gets an independent checkout; never clean a live tree.
    const checkout = join(source.root, `checkout-${source.revision}-${randomUUID()}`);
    if (source.manifest) await copyAccepted(source, checkout);
    else {
      await this.run('git', ['-c', 'core.hooksPath=/dev/null', 'clone', '--no-hardlinks', '--no-checkout', source.repo, checkout]);
      await this.run('git', ['-c', 'core.hooksPath=/dev/null', '-C', checkout, 'checkout', '--detach', source.revision]);
    }
    if (env.prepare?.length) {
      await this.remove(env, '-prepare');
      try {
        await this.docker([...this.base(env, source.revision, '-prepare'), '--mount', `type=bind,src=${checkout},dst=/app`, env.image, ...env.prepare], { timeout: env.timeoutSeconds * 1000 });
      } finally { await this.remove(env, '-prepare'); }
    }
    await atomic(join(checkout, '.enginebox-prepared'), { revision: source.revision });
    await rename(checkout, release);
    return release;
  }
  base(env, revision, suffix = '') {
    return ['run', '--name', this.container(env, suffix), '--label', `enginebox.environment.owner=${this.owner}`, '--label', `enginebox.environment.id=${env.id}`, '--label', `enginebox.environment.revision=${revision}`, '--read-only', '--cap-drop=ALL', '--security-opt=no-new-privileges:true', '--pids-limit=256', '--cpus', String(env.cpu), '--memory', `${env.memoryMiB}m`, '--memory-swap', `${env.memoryMiB}m`, '--log-driver=local', '--log-opt=max-size=10m', '--log-opt=max-file=2', '--tmpfs', '/tmp:rw,nosuid,nodev,size=256m', '--tmpfs', '/home/app:rw,nosuid,nodev,size=16m', '--user', `${process.getuid()}:${process.getgid()}`, '--env', 'HOME=/home/app', '--env', 'ENGINEBOX_DATA_DIR=/data', '--env', `ENGINEBOX_REVISION=${revision}`, '--workdir', '/app'];
  }
  async start(env, revision) {
    const root = join(this.config.stateDir, env.id); const data = join(root, 'data');
    await mkdir(data, { recursive: true, mode: 0o700 });
    await this.remove(env);
    const args = [...this.base(env, revision), '--detach', '--restart=unless-stopped', '--mount', `type=bind,src=${join(root, revision)},dst=/app,readonly`, '--mount', `type=bind,src=${data},dst=/data`];
    for (const p of env.ports) args.push('--publish', `127.0.0.1:${p.host}:${p.container}/${p.protocol === 'udp' ? 'udp' : 'tcp'}`);
    await this.docker([...args, env.image, ...env.command]);
  }
  async healthy(env) {
    try {
      const p = env.ports.find(v => v.name === env.health.port);
      const response = await fetch(`http://127.0.0.1:${p.host}${env.health.path}`, { redirect: 'error', signal: AbortSignal.timeout(1500) });
      await response.body?.cancel(); return response.ok;
    } catch { return false; }
  }
  async ready(env) {
    const deadline = Date.now() + Math.min(env.timeoutSeconds, 120) * 1000;
    do {
      const container = await this.inspect(env);
      check(container?.State.Running, 'service-exited');
      if (await this.healthy(env)) return;
      await new Promise(r => setTimeout(r, 500));
    } while (Date.now() < deadline);
    throw new Error('readiness-timeout');
  }
  async reconcile(env) {
    const state = this.states[env.id] ??= { healthy: false, status: 'pending' };
    try {
      const recipe = createHash('sha256').update(JSON.stringify(env)).digest('hex');
      check(!state.recipe || state.recipe === recipe, 'recipe-changed-create-new-environment');
      state.recipe = recipe;
      const source = await this.source(env);
      state.desiredRevision = source.revision;
      const container = await this.inspect(env);
      if (state.revision === source.revision || state.failedRevision === source.revision) {
        const matches = container?.Config.Labels['enginebox.environment.revision'] === state.revision;
        if ((!matches || !container?.State.Running) && state.revision) await this.start(env, state.revision);
        state.healthy = !!state.revision && await this.healthy(env);
        state.status = state.healthy ? (state.failedRevision ? 'update-failed' : 'ready') : 'unhealthy';
        return;
      }
      state.status = 'preparing'; await this.save();
      await this.prepare(env, source);
      // A slow preparation must not activate a revision already superseded at
      // the authority. The next poll will prepare the newer accepted head.
      if ((await this.source(env)).revision !== source.revision) { state.status = 'superseded'; return; }
      state.previousRevision = state.revision ?? null;
      state.status = 'updating'; state.healthy = false; await this.save(); await this.render();
      // One writer owns /data. Stop the old process before starting new code.
      if (container) await this.docker(['stop', '--time', '15', this.container(env)]);
      await this.start(env, source.revision); await this.ready(env);
      state.revision = source.revision; state.healthy = true; state.status = 'ready';
      delete state.failedRevision; delete state.error;
      state.updatedAt = new Date().toISOString();
    } catch (error) {
      state.error = /^[a-z0-9-]+$/.test(error.message) ? error.message : 'environment-update-failed';
      state.failedRevision = state.desiredRevision;
      state.status = 'update-failed';
      if (state.revision) {
        try {
          const container = await this.inspect(env);
          if (container?.Config.Labels['enginebox.environment.revision'] !== state.revision || !container.State.Running) await this.start(env, state.revision);
          await this.ready(env); state.healthy = true;
        } catch { state.healthy = false; }
      } else { state.healthy = false; await this.remove(env).catch(() => {}); }
    }
  }
  async tick() {
    if (this.busy) return;
    this.busy = true;
    try { for (const env of this.config.environments) { await this.reconcile(env); await this.save(); await this.render(); } }
    finally { this.busy = false; }
  }
}
