import { mkdir } from 'node:fs/promises';
import { join } from 'node:path';
import { check, fields, validateConfig, validateRoutes } from './config.mjs';
import { atomic, load } from './state.mjs';
import { digest, projectId, validateTemplate, resolveProject, templateEnvironment } from './project-template.mjs';
import { admissionBlocked } from './admission.mjs';

// Adding a draft exporter must not invalidate an accepted-main source binding.
const mainBinding = host => { const value = { ...host }; delete value.workspaceCommand; return digest(value); };
const safeError = error => /^[a-z0-9-]+$/.test(error.message) ? error.message : 'project-bootstrap-failed';

export class ProjectBootstrap {
  constructor(manager) {
    this.manager = manager; this.config = manager.config; this.policy = this.config.bootstrap;
    this.file = join(this.config.stateDir, 'projects.json'); this.records = {}; this.writes = Promise.resolve();
  }
  save() {
    const write = this.writes.then(() => atomic(this.file, this.records));
    this.writes = write.catch(() => {}); return write;
  }
  async init() {
    const refs = new Set();
    const catalogFile = join(this.config.stateDir, 'dev-template-catalog.json');
    const versions = await load(catalogFile, {});
    for (const template of this.policy?.templates ?? []) {
      validateTemplate(template); const ref = `${template.id}@${template.revision}`;
      check(!refs.has(ref), 'duplicate-dev-template'); refs.add(ref);
      check(!versions[ref] || versions[ref] === digest(template), 'registered-template-revision-changed');
      versions[ref] = digest(template);
    }
    if (this.policy) await atomic(catalogFile, versions);
    this.records = await load(this.file, {});
    check(this.records && typeof this.records === 'object' && !Array.isArray(this.records) && Object.keys(this.records).length <= 24, 'invalid-project-registry');
    for (const p of Object.values(this.records)) {
      check(projectId(p.host) && projectId(p.project) && p.id === this.key(p.host, p.project), 'invalid-project-record');
      for (const release of [p.active, p.pending, p.retired].filter(Boolean)) this.add(release.env);
    }
    validateConfig(this.config);
  }
  key(host, project) { return `p-${digest([host, project]).slice(0, 20)}`; }
  routeId(p, port) { return `${p.id}-${digest(port.name).slice(0, 10)}`; }
  source(p) {
    const host = this.policy?.hosts.find(h => h.id === p.host);
    check(host && p.binding === mainBinding(host), 'project-source-binding-changed');
    return { kind: 'layerfs', project: p.project, command: [...host.command, p.project] };
  }
  add(env) {
    const existing = this.config.environments.find(e => e.id === env.id);
    check(!existing || digest(existing) === digest(env), 'project-environment-id-conflict');
    if (!existing) this.config.environments.push(env);
  }
  summary(p) {
    const state = this.manager.states[p.active?.env.id];
    return { id: p.id, host: p.host, project: p.project, status: p.status, error: p.error, warnings: state?.warnings ?? [], healthy: state?.healthy ?? false,
      environment: p.active?.env.id, revision: state?.revision, desiredRevision: p.desiredRevision, metrics: state?.metrics ?? null,
      template: p.active && { id: p.active.template.id, revision: p.active.template.revision, digest: p.active.templateDigest },
      urls: this.manager.routes.filter(r => r.environment === p.active?.env.id && r.hostname).map(r => `${this.policy?.urlScheme ?? (this.config.httpTls ? 'https' : 'http')}://${r.hostname}${this.policy?.urlPort ? `:${this.policy.urlPort}` : ''}`) };
  }
  catalog() { return Object.values(this.records).map(p => this.summary(p)); }
  async register(value) {
    fields(value, ['host', 'project', 'retry']);
    check(this.policy, 'project-bootstrap-not-configured');
    check(projectId(value.host) && projectId(value.project) && (value.retry === undefined || typeof value.retry === 'boolean'), 'invalid-project-request');
    const host = this.policy.hosts.find(h => h.id === value.host); check(host, 'unknown-project-host');
    const key = this.key(value.host, value.project);
    let p = this.records[key];
    if (!p || value.retry) {
      await this.manager.assertAdmission();
      p = this.records[key];
    }
    if (!p) {
      check(Object.keys(this.records).length < this.policy.maxProjects, 'project-capacity-limit');
      p = this.records[key] = { id: key, host: value.host, project: value.project, binding: mainBinding(host), status: 'queued' };
    }
    if (value.retry) p.retry = true;
    await this.save(); return this.summary(p);
  }
  allocate(count) {
    const used = new Set([this.config.listenPort, ...this.config.transportEntrypoints.map(p => p.port), ...this.config.environments.flatMap(e => e.ports.map(p => p.host))]);
    const ports = [];
    for (let n = this.policy.portRange[0]; n <= this.policy.portRange[1] && ports.length < count; n++) if (!used.has(n)) ports.push(n);
    check(ports.length === count, 'project-port-capacity-limit'); return ports;
  }
  async install(release) {
    await this.manager.assertAdmission();
    const directory = join(this.config.stateDir, 'dev-templates'); await mkdir(directory, { recursive: true, mode: 0o700 });
    const file = join(directory, `${release.templateDigest}.json`);
    const receipt = { schema: 1, id: release.template.id, revision: release.template.revision, digest: release.templateDigest, image: release.env.image };
    try {
      try { await this.manager.docker(['image', 'inspect', release.env.image]); }
      catch { await this.manager.docker(['pull', release.env.image], { timeout: release.env.timeoutSeconds * 1000 }); }
      await atomic(file, { ...receipt, status: 'ready' });
    } catch (error) { await atomic(file, { ...receipt, status: 'failed', error: safeError(error) }); throw error; }
  }
  routePlan(p, release, routes) {
    const desired = release ? release.template.ports.filter(port => port.expose).map(port => ({ id: this.routeId(p, port), environment: release.env.id, service: port.name, hostname: `${p.id}-${port.name}.${this.policy.domain}`, access: this.policy.access })) : [];
    const ownedEnvs = [p.active?.env.id, p.pending?.env.id, p.retired?.env.id].filter(Boolean);
    for (const route of routes.filter(r => (p.routeIds ?? []).includes(r.id))) check(ownedEnvs.includes(route.environment), 'project-route-ownership-changed');
    for (const route of desired) {
      const old = routes.find(r => r.id === route.id);
      check(!old || ((p.routeIds ?? []).includes(old.id) && ownedEnvs.includes(old.environment)), 'project-route-conflict');
    }
    // Preserve operator-created aliases and their access policy across generations.
    // An incompatible service change must fail before stopping the live process.
    const aliases = routes.filter(r => ownedEnvs.includes(r.environment) && !(p.routeIds ?? []).includes(r.id));
    const other = routes.filter(r => !ownedEnvs.includes(r.environment) && !(p.routeIds ?? []).includes(r.id));
    const next = [...other, ...desired, ...aliases.map(r => ({ ...r, environment: release?.env.id ?? r.environment }))];
    validateRoutes(next, this.config); return next;
  }
  async routes(p, release) {
    await this.manager.updateRoutes(routes => this.routePlan(p, release, routes));
    p.routeIds = release ? release.template.ports.filter(port => port.expose).map(port => this.routeId(p, port)) : [];
  }
  async discard(release, options) {
    if (!release) return;
    await this.manager.remove(release.env, '', options);
    if (this.manager.states[release.env.id]) this.manager.states[release.env.id].healthy = false;
    // Retain private files/receipts for diagnosis. Do not delete source or data.
    if (!this.manager.routes.some(r => r.environment === release.env.id)) this.config.environments = this.config.environments.filter(e => e.id !== release.env.id);
  }
  async rollback(p) {
    await this.manager.assertAdmission();
    // Only one process may own the project's persistent data, including recovery.
    if (p.pending) await this.manager.remove(p.pending.env);
    if (p.active) {
      const env = p.active.env; const state = this.manager.states[env.id];
      const container = await this.manager.inspect(env);
      if (!container?.State.Running && state?.revision) { await this.manager.start(env, state.revision); await this.manager.ready(env); }
      if (state) state.healthy = !!state.revision && await this.manager.healthy(env);
    }
    await this.routes(p, p.active);
    await this.discard(p.pending); delete p.pending; delete p.phase;
  }
  async recover() {
    for (const p of Object.values(this.records)) {
      try {
        if (p.phase === 'switching' || p.retired) await this.manager.assertAdmission();
        if (p.phase === 'switching') {
          await this.rollback(p); p.status = 'failed'; p.error = 'project-switch-interrupted';
        }
        if (p.retired) { await this.routes(p, p.active); await this.discard(p.retired); delete p.retired; }
      } catch (error) {
        if (!admissionBlocked(error)) throw error;
        p.status = 'admission-blocked'; p.error = error.message;
      }
    }
    await this.save();
  }
  async reconcile(p) {
    let desired;
    try {
      const sourceConfig = this.source(p);
      const source = await this.manager.source({ id: p.id, source: sourceConfig });
      p.desiredRevision = source.revision;
      desired = await resolveProject(source, this.policy.templates);
      if (!desired) {
        check(!p.active, 'project-config-removed'); p.status = 'not-configured'; delete p.error; return;
      }
      desired.fingerprint = digest([desired.fingerprint, this.policy.domain, this.policy.urlScheme ?? (this.config.httpTls ? 'https' : 'http'), this.policy.access, ...(this.policy.urlPort === undefined ? [] : [this.policy.urlPort])]);
      if (p.active?.fingerprint === desired.fingerprint) {
        if (p.pending) { await this.discard(p.pending); delete p.pending; }
        await this.manager.reconcile(p.active.env);
        const state = this.manager.states[p.active.env.id]; p.status = state.status; p.error = state.error;
        return;
      }
      if (p.failedRevision === source.revision && p.failedFingerprint === desired.fingerprint && !p.retry) return;
      delete p.retry;
      if (p.pending && p.pending.fingerprint !== desired.fingerprint) { await this.discard(p.pending); delete p.pending; }
      if (!p.pending) {
        const env = templateEnvironment(desired.template, `${p.id}-${desired.fingerprint.slice(0, 12)}`, sourceConfig, this.allocate(desired.template.ports.length));
        env.dataId = p.id; env.sourceInputs = desired.inputs;
        const publicPort = desired.template.ports.find(port => port.expose && port.name === desired.template.health.port) ?? desired.template.ports.find(port => port.expose);
        if (publicPort) { env.publicHost = `${p.id}-${publicPort.name}.${this.policy.domain}`; env.publicScheme = this.policy.urlScheme ?? (this.config.httpTls ? 'https' : 'http'); if (this.policy.urlPort) env.publicPort = this.policy.urlPort; }
        check(env.cpu <= this.policy.maxCpu && env.memoryMiB <= this.policy.maxMemoryMiB, 'project-resource-policy-exceeded');
        // Validate before publishing to the shared runtime registry.
        validateConfig({ ...this.config, environments: [...this.config.environments, env] });
        p.pending = { ...desired, env }; this.add(env);
      }
      p.status = 'preparing'; await this.save();
      const next = p.pending;
      const routeIds = next.template.ports.filter(port => port.expose).map(port => this.routeId(p, port));
      this.routePlan(p, next, this.manager.routes);
      await this.install(next);
      const current = await this.manager.source(next.env);
      // Dependencies are built away from the live workspace and persistent data.
      if (!await load(join(this.config.stateDir, next.env.id, 'files.json'), null)) await this.manager.prepare(next.env, current);
      check((await this.manager.source(next.env)).revision === current.revision, 'source-superseded');
      await this.manager.assertAdmission();
      p.phase = 'switching'; await this.save();
      if (p.active) {
        const old = p.active.env; const container = await this.manager.inspect(old);
        if (container?.State.Running) await this.manager.docker(['stop', '--time', '15', this.manager.container(old)]);
        this.manager.states[old.id].healthy = false; await this.manager.render();
      }
      await this.manager.reconcile(next.env);
      check(this.manager.states[next.env.id]?.healthy && this.manager.states[next.env.id].status === 'ready', this.manager.states[next.env.id]?.error ?? 'project-start-failed');
      await this.manager.assertAdmission();
      // Record route ownership before publication so crash recovery can restore it.
      p.routeIds = [...new Set([...(p.routeIds ?? []), ...routeIds])];
      await this.save(); await this.routes(p, next);
      p.retired = p.active; p.active = next; delete p.pending; delete p.phase;
      p.status = 'ready'; delete p.error; delete p.failedRevision; delete p.failedFingerprint;
      await this.save(); await this.discard(p.retired); delete p.retired;
    } catch (error) {
      p.error = safeError(error);
      if (admissionBlocked(error)) p.status = 'admission-blocked';
      else {
        p.status = 'failed'; p.failedRevision = p.desiredRevision; p.failedFingerprint = desired?.fingerprint;
        if (p.phase === 'switching') {
          try { await this.rollback(p); }
          catch (recoveryError) {
            p.error = admissionBlocked(recoveryError) ? recoveryError.message : 'project-recovery-required';
            if (admissionBlocked(recoveryError)) p.status = 'admission-blocked';
          }
        }
      }
      // A failed preparation/source read leaves the previous runtime and route up.
      if (p.active) {
        const state = this.manager.states[p.active.env.id];
        if (state) state.healthy = !!(await this.manager.inspect(p.active.env).catch(() => null))?.State.Running && await this.manager.healthy(p.active.env);
      }
    } finally { await this.save(); }
  }
  async tick() {
    if (!this.policy) return;
    for (const p of Object.values(this.records)) await this.reconcile(p);
  }
}
