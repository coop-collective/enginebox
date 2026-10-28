import { spawn } from 'node:child_process';
import { mkdir, rename, stat, lstat } from 'node:fs/promises';
import { join } from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { check, gatewayConfig, validateRoutes } from './config.mjs';
import { acceptedSource, copyAccepted } from './accepted.mjs';
import { layerfsSource } from './layerfs.mjs';
import { syncFiles, writableDirectories } from './files.mjs';
import { assertAdmission, admissionBlocked } from './admission.mjs';

import { atomic, load } from './state.mjs';
import { ProjectBootstrap } from './bootstrap.mjs';
import { WorkspacePreviews } from './previews.mjs';
export { atomic, load } from './state.mjs';

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
    child.once('close', code => { clearTimeout(timer); if (code === 0 && !failure) resolve(output.trim()); else reject(new Error(failure ?? (output.includes('workspace-writer-busy') ? 'workspace-writer-busy' : output.includes('workspace-attached-unqualified') ? 'workspace-attached-unqualified' : 'command-failed'))); });
  });
}

export class Environments {
  constructor(config, run = command) {
    this.config = config; this.run = run; this.states = {}; this.routes = []; this.busy = false;
    this.gatewayWrites = Promise.resolve(); this.routeWrites = Promise.resolve();
    this.staticIds = new Set(config.environments.map(e => e.id));
    this.projects = new ProjectBootstrap(this);
    this.previews = new WorkspacePreviews(this);
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
    await this.projects.init();
    await this.previews.init();
    this.routes = validateRoutes(await load(this.routeFile, []), this.config);
    // Recheck owned live processes before publishing routes. A control restart
    // must not withdraw a working dev server while source fetching catches up.
    for (const state of Object.values(this.states)) state.healthy = false;
    for (const env of this.config.environments) {
      const state = this.states[env.id]; if (!state?.revision) continue;
      try {
        const recipe = createHash('sha256').update(JSON.stringify(env)).digest('hex');
        const container = await this.inspect(env);
        const matches = env.updateMode === 'files' ? container?.Config.Labels['enginebox.environment.recipe'] === recipe : container?.Config.Labels['enginebox.environment.revision'] === state.revision;
        state.healthy = state.recipe === recipe && matches && !!container?.State.Running && await this.healthy(env);
      } catch { state.healthy = false; }
    }
    await this.projects.recover();
    await this.previews.recover();
    await this.render();
  }
  async save() { await atomic(this.stateFile, this.states); }
  async render() {
    const write = this.gatewayWrites.then(() => atomic(this.dynamicFile, gatewayConfig(this.routes, this.config, new Set(Object.entries(this.states).filter(([id, s]) => { const source = this.config.environments.find(e => e.id === id)?.source; return s.healthy && (source?.kind !== 'workspace' || this.previews.live(this.previews.records[source.preview] ?? {})); }).map(([id]) => id)))));
    this.gatewayWrites = write.catch(() => {}); await write;
  }
  catalog() {
    return { schema: 1, projects: this.projects.catalog(), baseDomains: this.config.baseDomains, transportEntrypoints: this.config.transportEntrypoints, routes: this.routes,
      environments: this.config.environments.map(e => ({ id: e.id, updateMode: e.updateMode ?? 'restart', source: { kind: e.source.kind, branch: e.source.branch }, services: e.ports.map(({ name, protocol }) => ({ name, protocol })), cpu: e.cpu, memoryMiB: e.memoryMiB, ...(this.states[e.id] ?? { status: 'pending', healthy: false }) })) };
  }
  async updateRoutes(transform) {
    const write = this.routeWrites.then(async () => {
      const next = transform(this.routes); validateRoutes(next, this.config);
      await atomic(this.routeFile, next); this.routes = next; await this.render();
    });
    this.routeWrites = write.catch(() => {}); await write;
  }
  async setRoute(route) {
    const source = this.config.environments.find(e => e.id === route.environment)?.source;
    if (source?.kind === 'workspace') this.previews.assertSource(source);
    const projectEnvs = [...Object.values(this.projects.records), ...Object.values(this.previews.records)].flatMap(p => [p.pending?.env.id, p.retired?.env.id]).filter(Boolean);
    check(!projectEnvs.includes(route.environment), 'project-environment-not-active');
    await this.updateRoutes(routes => routes.filter(v => v.id !== route.id).concat([route]));
  }
  async deleteRoute(id) { await this.updateRoutes(routes => routes.filter(r => r.id !== id)); }
  container(env, suffix = '') { return `ebox-dev-${this.owner}-${env.id}${suffix}`; }
  async assertAdmission() { await assertAdmission(this.config); }
  async docker(args, options) {
    if (['run', 'exec', 'start', 'restart', 'stop', 'pull'].includes(args[0])) await this.assertAdmission();
    return this.run(this.config.docker ?? 'docker', args, options);
  }
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
  async remove(env, suffix = '', { cleanup = false } = {}) {
    if (await this.inspect(env, suffix)) {
      if (!cleanup) await this.assertAdmission();
      await this.docker(['rm', '-f', this.container(env, suffix)]);
    }
  }
  async source(env) {
    await this.assertAdmission();
    const root = join(this.config.stateDir, env.id); await mkdir(root, { recursive: true, mode: 0o700 });
    if (['accepted', 'layerfs', 'workspace'].includes(env.source.kind)) {
      if (env.source.kind === 'workspace') this.previews.assertSource(env.source);
      const source = env.source.kind === 'accepted' ? await acceptedSource(env, this.config.stateDir) : await layerfsSource(env, this.config.stateDir, this.run);
      if (env.source.kind === 'workspace') await this.previews.bindProjection(env.source, source.manifest);
      for (const [path, hash] of Object.entries(env.sourceInputs ?? {})) {
        const entries = source.manifest.entries.filter(e => e.path === path);
        check(entries.length === 1 && entries[0].kind === 'file' && entries[0].sha256 === hash, 'project-configuration-changed');
      }
      return source;
    }
    const repo = join(root, 'repository.git');
    try { await stat(repo); } catch (error) { if (error.code !== 'ENOENT') throw error; await this.run('git', ['init', '--bare', repo]); }
    await this.run('git', ['-c', 'core.hooksPath=/dev/null', '-C', repo, 'fetch', '--no-tags', '--force', '--', env.source.url, `refs/heads/${env.source.branch}:refs/heads/tracked`]);
    const revision = await this.run('git', ['-C', repo, 'rev-parse', 'refs/heads/tracked^{commit}']);
    check(/^[a-f0-9]{40,64}$/.test(revision), 'invalid-source-revision');
    return { revision, repo, root };
  }
  async prepare(env, source) {
    await this.assertAdmission();
    const release = join(source.root, source.revision); const marker = join(release, '.enginebox-prepared');
    if (await load(marker, null)) return release;
    // Each failed attempt gets an independent checkout; never clean a live tree.
    const checkout = join(source.root, `checkout-${source.revision}-${randomUUID()}`);
    if (source.manifest) await copyAccepted(source, checkout);
    else {
      await this.run('git', ['-c', 'core.hooksPath=/dev/null', 'clone', '--no-hardlinks', '--no-checkout', source.repo, checkout]);
      await this.run('git', ['-c', 'core.hooksPath=/dev/null', '-C', checkout, 'checkout', '--detach', source.revision]);
    }
    if (env.cwd && env.cwd !== '.') {
      let path = checkout;
      for (const part of env.cwd.split('/')) { path = join(path, part); check((await lstat(path)).isDirectory(), 'working-directory-invalid'); }
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
    return ['run', '--name', this.container(env, suffix), '--label', `enginebox.environment.owner=${this.owner}`, '--label', `enginebox.environment.id=${env.id}`, '--label', `enginebox.environment.revision=${revision}`, '--read-only', '--cap-drop=ALL', '--security-opt=no-new-privileges:true', '--pids-limit=256', '--cpus', String(env.cpu), '--memory', `${env.memoryMiB}m`, '--memory-swap', `${env.memoryMiB}m`, '--log-driver=local', '--log-opt=max-size=10m', '--log-opt=max-file=2', '--tmpfs', '/tmp:rw,nosuid,nodev,size=256m', '--tmpfs', `/home/app:rw,nosuid,nodev,size=16m,mode=700,uid=${process.getuid()},gid=${process.getgid()}`, '--user', `${process.getuid()}:${process.getgid()}`, '--env', 'HOME=/home/app', '--env', 'ENGINEBOX_DATA_DIR=/data', '--env', `ENGINEBOX_REVISION=${revision}`, ...(env.publicHost ? ['--env', `ENGINEBOX_PUBLIC_HOST=${env.publicHost}`, '--env', `ENGINEBOX_PUBLIC_URL=${env.publicScheme}://${env.publicHost}${env.publicPort ? `:${env.publicPort}` : ''}`] : []), ...Object.entries(env.env ?? {}).flatMap(([key, value]) => ['--env', `${key}=${value}`]), '--workdir', env.cwd && env.cwd !== '.' ? `/app/${env.cwd}` : '/app'];
  }
  async start(env, revision) {
    await this.assertAdmission();
    if (env.source.kind === 'workspace') this.previews.assertSource(env.source);
    const root = join(this.config.stateDir, env.id); const data = join(this.config.stateDir, env.dataId ?? env.id, 'data');
    await mkdir(data, { recursive: true, mode: 0o700 });
    await this.remove(env);
    const files = env.updateMode === 'files';
    const args = [...this.base(env, revision), '--detach', '--restart=unless-stopped', '--mount', `type=bind,src=${join(root, files ? 'workspace' : revision)},dst=/app,readonly`, '--mount', `type=bind,src=${data},dst=/data`];
    if (files) {
      args.push('--label', `enginebox.environment.recipe=${createHash('sha256').update(JSON.stringify(env)).digest('hex')}`);
      for (const path of env.writableDirs ?? []) args.push('--tmpfs', `/app/${path}:rw,nosuid,nodev,size=256m,mode=700,uid=${process.getuid()},gid=${process.getgid()}`);
    }
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
  async preserveAdmission(env, state, error) {
    state.status = 'admission-blocked'; state.error = error.message;
    try { state.healthy = !!(await this.inspect(env))?.State.Running && await this.healthy(env); }
    catch { state.healthy = false; }
  }
  async reconcile(env) {
    if (env.updateMode === 'files') return this.reconcileFiles(env);
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
        if (state.healthy && state.revision === source.revision) {
          delete state.failedRevision; delete state.error;
        }
        state.status = state.healthy ? (state.failedRevision ? 'update-failed' : 'ready') : 'unhealthy';
        return;
      }
      state.status = 'preparing'; await this.save();
      await this.prepare(env, source);
      // A slow preparation must not activate a revision already superseded at
      // the authority. The next poll will prepare the newer accepted head.
      if ((await this.source(env)).revision !== source.revision) { state.status = 'superseded'; return; }
      await this.assertAdmission();
      state.previousRevision = state.revision ?? null;
      state.status = 'updating'; state.healthy = false; await this.save(); await this.render();
      // One writer owns /data. Stop the old process before starting new code.
      if (container) await this.docker(['stop', '--time', '15', this.container(env)]);
      await this.start(env, source.revision); await this.ready(env);
      state.revision = source.revision; state.healthy = true; state.status = 'ready';
      delete state.failedRevision; delete state.error;
      state.updatedAt = new Date().toISOString();
    } catch (error) {
      if (admissionBlocked(error)) { await this.preserveAdmission(env, state, error); return; }
      state.error = /^[a-z0-9-]+$/.test(error.message) ? error.message : 'environment-update-failed';
      state.failedRevision = state.desiredRevision;
      state.status = 'update-failed';
      if (state.revision) {
        try {
          const container = await this.inspect(env);
          if (container?.Config.Labels['enginebox.environment.revision'] !== state.revision || !container.State.Running) await this.start(env, state.revision);
          await this.ready(env); state.healthy = true;
        } catch (recoveryError) {
          if (admissionBlocked(recoveryError)) await this.preserveAdmission(env, state, recoveryError);
          else state.healthy = false;
        }
      } else { state.healthy = false; await this.remove(env).catch(() => {}); }
    }
  }
  async reconcileFiles(env) {
    const state = this.states[env.id] ??= { healthy: false, status: 'pending' };
    const root = join(this.config.stateDir, env.id); const workspace = join(root, 'workspace');
    const receiptFile = join(root, 'files.json');
    try {
      const recipe = createHash('sha256').update(JSON.stringify(env)).digest('hex');
      check(!state.recipe || state.recipe === recipe, 'recipe-changed-create-new-environment'); state.recipe = recipe;
      const source = await this.source(env); state.desiredRevision = source.revision;
      let receipt = await load(receiptFile, null);
      if (!receipt) {
        let existing; try { existing = await lstat(workspace); } catch (error) { if (error.code !== 'ENOENT') throw error; }
        check(!existing, 'unowned-live-source-directory');
        // Prepare dependencies once, before starting the long-lived dev command.
        const prepared = await this.prepare(env, source);
        check((await this.source(env)).revision === source.revision, 'source-superseded');
        receipt = { schema: 1, initialRevision: source.revision, revision: null, entries: [], pendingEntries: source.manifest.entries };
        await atomic(receiptFile, receipt);
        await rename(prepared, workspace);
      } else {
        check(receipt.schema === 1 && /^[a-f0-9]{64}$/.test(receipt.initialRevision), 'invalid-live-source-receipt');
        try { await stat(workspace); } catch (error) {
          if (error.code !== 'ENOENT') throw error;
          check(receipt.revision === null, 'live-source-directory-missing');
          await rename(join(root, receipt.initialRevision), workspace);
        }
      }
      if (receipt.revision !== source.revision || receipt.pendingEntries) {
        const entries = receipt.pendingEntries ?? receipt.entries;
        state.status = 'syncing'; await this.save();
        state.fileUpdate = await syncFiles(source, workspace, entries, env.writableDirs ?? [], async pendingEntries => {
          check((await this.source(env)).revision === source.revision, 'source-superseded');
          await this.assertAdmission();
          receipt.pendingEntries = pendingEntries; await atomic(receiptFile, receipt);
        });
        await writableDirectories(workspace, env.writableDirs ?? []);
        receipt = { schema: 1, initialRevision: receipt.initialRevision, revision: source.revision, entries: source.manifest.entries };
        await atomic(receiptFile, receipt);
        state.previousRevision = state.revision ?? null; state.revision = source.revision;
        state.updatedAt = new Date().toISOString();
      } else state.revision = receipt.revision;
      const container = await this.inspect(env);
      check(!container || container.Config.Labels['enginebox.environment.recipe'] === recipe, 'live-process-recipe-mismatch');
      if (!container?.State.Running) { await this.start(env, receipt.initialRevision); await this.ready(env); }
      state.healthy = await this.healthy(env);
      state.status = state.healthy ? 'ready' : 'unhealthy';
      delete state.error; delete state.failedRevision;
    } catch (error) {
      if (admissionBlocked(error)) { await this.preserveAdmission(env, state, error); return; }
      state.error = /^[a-z0-9-]+$/.test(error.message) ? error.message : 'environment-update-failed';
      state.status = 'update-failed';
      // Source failures must not stop, replace or rebuild a live dev process.
      try {
        const container = await this.inspect(env);
        const recipe = createHash('sha256').update(JSON.stringify(env)).digest('hex');
        state.healthy = container?.Config.Labels['enginebox.environment.recipe'] === recipe && !!container?.State.Running && await this.healthy(env);
      } catch { state.healthy = false; }
    }
  }
  async tick() {
    if (this.busy) return;
    this.busy = true;
    try {
      for (const env of this.config.environments.filter(e => this.staticIds.has(e.id))) { await this.reconcile(env); await this.save(); await this.render(); }
      await this.projects.tick(); await this.previews.tick(); await this.save(); await this.render();
    }
    finally { this.busy = false; }
  }
}
