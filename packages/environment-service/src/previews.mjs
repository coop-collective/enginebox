// Leased draft environments reuse project template preparation and lifecycle.
// The projection command is operator-owned; request bodies never supply paths,
// commands, source bytes, runtime credentials or an accepted-main replacement.
// A draft comes from an open LayerFS workspace (`workspace`) or from one branch
// of a hosted or linked repository (`repository` + `branch`, exported by the
// host's operator-owned `repositoryCommand`; src/repository-export.mjs).
import { randomBytes } from 'node:crypto';
import { join } from 'node:path';
import { ProjectBootstrap } from './bootstrap.mjs';
import { branchName, check, fields, id, repositoryRef, validateConfig } from './config.mjs';
import { digest, projectId } from './project-template.mjs';
import { load } from './state.mjs';

export class WorkspacePreviews extends ProjectBootstrap {
  constructor(manager) {
    super(manager); this.file = join(this.config.stateDir, 'previews.json'); this.operations = Promise.resolve(); this.timers = new Map(); this.expirations = new Set(); this.closed = false;
  }
  exclusive(work) {
    const next = this.operations.then(work); this.operations = next.catch(() => {}); return next;
  }
  live(p) { return !p.revoked && Date.parse(p.expiresAt) > Date.now(); }
  async init() {
    this.records = await load(this.file, {});
    check(this.records && typeof this.records === 'object' && !Array.isArray(this.records) && Object.keys(this.records).length <= 256, 'invalid-preview-registry');
    for (const p of Object.values(this.records)) {
      check(id(p.id) && projectId(p.host) && projectId(p.task) && (p.repository !== undefined ? repositoryRef(p.repository) && branchName(p.branch) : projectId(p.project) && projectId(p.workspace))
        && /^[a-f0-9]{64}$/.test(p.lease) && Number.isSafeInteger(p.revision) && Number.isFinite(Date.parse(p.expiresAt)), 'invalid-preview-record');
      for (const release of [p.active, p.pending, p.retired].filter(Boolean)) this.add(release.env);
    }
    validateConfig(this.config);
    for (const p of Object.values(this.records)) this.arm(p);
  }
  source(p) {
    check(this.live(p), 'preview-lease-expired');
    const host = this.policy?.hosts.find(h => h.id === p.host);
    if (p.repository !== undefined) {
      check(host?.repositoryCommand && p.binding === digest(host), 'preview-source-binding-changed');
      return { kind: 'repository', repository: p.repository, branch: p.branch, preview: p.id, lease: p.lease,
        command: [...host.repositoryCommand, p.repository, p.branch] };
    }
    check(host?.workspaceCommand && p.binding === digest(host), 'preview-source-binding-changed');
    return { kind: 'workspace', project: p.project, workspace: p.workspace, preview: p.id, lease: p.lease,
      command: [...host.workspaceCommand, p.project, p.workspace] };
  }
  assertSource(source) {
    const p = this.records[source.preview];
    check(p && p.lease === source.lease && (source.kind === 'repository' ? p.repository === source.repository && p.branch === source.branch : p.project === source.project && p.workspace === source.workspace) && this.live(p), 'preview-lease-expired');
    check(digest(this.source(p)) === digest(source), 'preview-source-binding-changed');
  }
  async bindProjection(source, manifest) {
    this.assertSource(source);
    const p = this.records[source.preview]; const identity = { sandbox: manifest.sandbox, branch: manifest.branch };
    check(!p.projection || digest(p.projection) === digest(identity), 'workspace-projection-identity-changed');
    if (!p.projection) { p.projection = identity; await this.save(); }
  }
  // The branch commit last exported, for display: the draft's source revision
  // is the content hash of that commit's tree.
  async noteCommit(source, commit) {
    this.assertSource(source);
    const p = this.records[source.preview];
    if (p.commit !== commit) { p.commit = commit; await this.save(); }
  }
  summary(p) {
    const value = super.summary(p);
    return { ...value, workspace: p.workspace, repository: p.repository, branch: p.branch, commit: p.commit, task: p.task, lease: p.lease, revision: p.revision,
      sourceRevision: value.revision, desiredSourceRevision: value.desiredRevision, desiredRevision: undefined,
      expiresAt: p.expiresAt, healthy: this.live(p) && value.healthy,
      urls: this.live(p) ? value.urls : [], status: !this.live(p) ? (p.revoked ? 'revoked' : 'expired') : value.status };
  }
  async changeLease(value) {
    fields(value, ['id', 'lease', 'expectedRevision', 'ttlSeconds', 'revoke', 'retry']);
    const p = this.records[value.id];
    check(p && value.lease === p.lease && value.expectedRevision === p.revision, 'stale-preview-lease');
    check(this.live(p), 'preview-lease-expired');
    if (value.revoke === true) { check(value.ttlSeconds === undefined && value.retry === undefined, 'invalid-preview-request'); p.revoked = true; }
    else {
      check(value.revoke === undefined && (value.retry === undefined || typeof value.retry === 'boolean'), 'invalid-preview-request');
      await this.manager.assertAdmission();
      // Admission is asynchronous; another renewal/revocation may win while it
      // is in flight. Preserve optimistic lease ownership across that boundary.
      check(value.expectedRevision === p.revision, 'stale-preview-lease');
      check(this.live(p), 'preview-lease-expired');
      p.expiresAt = this.expiry(value.ttlSeconds); if (value.retry === true) p.retry = true;
    }
    // Fence immediately, even while a slow immutable export is in flight.
    p.revision++; await this.save(); this.arm(p);
    if (p.revoked) { await this.withdraw(p); await this.exclusive(() => this.cleanup(p)); }
    return this.summary(p);
  }
  async register(value) {
    if (value.id !== undefined) return this.changeLease(value);
    if (value.repository !== undefined) return this.registerRepository(value);
    return this.exclusive(async () => {
      check(this.policy, 'project-bootstrap-not-configured');
      fields(value, ['host', 'project', 'workspace', 'task', 'ttlSeconds']);
      for (const field of ['host', 'project', 'workspace', 'task']) check(projectId(value[field]), 'invalid-preview-request');
      const expiresAt = this.expiry(value.ttlSeconds);
      const host = this.policy.hosts.find(h => h.id === value.host);
      check(host?.workspaceCommand, 'workspace-preview-not-configured');
      const current = Object.values(this.records).find(p => this.live(p) && ['host', 'project', 'workspace', 'task'].every(k => p[k] === value[k]));
      if (current) return this.summary(current);
      await this.manager.assertAdmission();
      check(Object.values(this.records).filter(p => this.live(p)).length < this.policy.maxProjects && Object.keys(this.records).length < 256, 'preview-capacity-limit');
      const p = { id: `w-${randomBytes(10).toString('hex')}`, host: value.host, project: value.project, workspace: value.workspace, task: value.task,
        binding: digest(host), lease: randomBytes(32).toString('hex'), revision: 1, expiresAt, status: 'queued' };
      this.records[p.id] = p; await this.save(); this.arm(p); return this.summary(p);
    });
  }
  // One live draft per host, repository and branch: a task asking again gets
  // the same draft (and lease) back, never a second copy of the branch.
  async registerRepository(value) {
    return this.exclusive(async () => {
      check(this.policy, 'project-bootstrap-not-configured');
      fields(value, ['host', 'repository', 'branch', 'task', 'ttlSeconds']);
      check(projectId(value.host) && repositoryRef(value.repository) && branchName(value.branch) && projectId(value.task), 'invalid-preview-request');
      const expiresAt = this.expiry(value.ttlSeconds);
      const host = this.policy.hosts.find(h => h.id === value.host);
      check(host?.repositoryCommand, 'repository-preview-not-configured');
      const current = Object.values(this.records).find(p => this.live(p) && p.host === value.host && p.repository === value.repository && p.branch === value.branch);
      if (current) return this.summary(current);
      await this.manager.assertAdmission();
      check(Object.values(this.records).filter(p => this.live(p)).length < this.policy.maxProjects && Object.keys(this.records).length < 256, 'preview-capacity-limit');
      const p = { id: `r-${randomBytes(10).toString('hex')}`, host: value.host, repository: value.repository, branch: value.branch, task: value.task,
        binding: digest(host), lease: randomBytes(32).toString('hex'), revision: 1, expiresAt, status: 'queued' };
      this.records[p.id] = p; await this.save(); this.arm(p); return this.summary(p);
    });
  }
  arm(p) {
    clearTimeout(this.timers.get(p.id)); this.timers.delete(p.id);
    if (this.closed || !this.live(p)) return;
    const timer = setTimeout(() => {
      this.timers.delete(p.id);
      if (this.closed) return;
      // Timer delays and the wall clock can disagree by a tick (or after a
      // clock adjustment). Keep the lease armed until its actual deadline.
      if (this.live(p)) { this.arm(p); return; }
      // Withdrawal does not wait for a slow source fetch/dependency install.
      // Reconciliation checks the same lease before starting or routing a draft.
      const action = this.withdraw(p).then(() => this.exclusive(() => this.cleanup(p)));
      this.expirations.add(action);
      void action.catch(() => {}).finally(() => this.expirations.delete(action));
    }, Math.max(1, Date.parse(p.expiresAt) - Date.now()));
    timer.unref(); this.timers.set(p.id, timer);
  }
  async close() {
    this.closed = true; for (const timer of this.timers.values()) clearTimeout(timer); this.timers.clear();
    await Promise.allSettled([...this.expirations]); await this.operations;
  }
  routePlan(p, release, routes) {
    if (release) check(this.live(p), 'preview-lease-expired');
    return super.routePlan(p, release, routes);
  }
  async withdraw(p) {
    const owned = new Set([p.active, p.pending, p.retired].filter(Boolean).map(r => r.env.id));
    await this.manager.updateRoutes(routes => routes.filter(r => !owned.has(r.environment)));
  }
  expiry(ttl) {
    check(Number.isInteger(ttl) && ttl >= 30 && ttl <= 3600, 'invalid-preview-ttl');
    return new Date(Date.now() + ttl * 1000).toISOString();
  }
  async cleanup(p) {
    // Withdraw every alias to these owned draft releases before removing them.
    const releases = [p.active, p.pending, p.retired].filter(Boolean);
    clearTimeout(this.timers.get(p.id)); this.timers.delete(p.id);
    await this.withdraw(p);
    // Lease expiry/revocation remains an independent cleanup authority. A node
    // admission failure never enters this path or cancels a live preview lease.
    for (const release of releases) { await this.manager.remove(release.env, '-prepare', { cleanup: true }); await this.discard(release, { cleanup: true }); }
    delete p.active; delete p.pending; delete p.retired; delete p.phase; p.routeIds = [];
    p.status = p.revoked ? 'revoked' : 'expired'; await this.save();
  }
  async recover() {
    for (const p of Object.values(this.records)) if (!this.live(p)) await this.cleanup(p);
    await super.recover();
  }
  async tick() {
    return this.exclusive(async () => {
      for (const p of Object.values(this.records)) {
        if (!this.live(p)) { if (p.active || p.pending || p.retired) await this.cleanup(p); continue; }
        await this.reconcile(p);
        if (!this.live(p)) await this.cleanup(p);
      }
    });
  }
}
