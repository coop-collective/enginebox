import { DurableObject } from "cloudflare:workers";
import {
  canonical,
  LEASE_MS,
  requireThat,
  type Assignment,
  type Generation,
  type JobSpec,
  type NodeSpec,
  type NodeView,
  type CandidateSpec,
  type CheckRequirement,
  type TaskActor,
  type TaskCreate,
  type TaskMove,
  type TaskStatus,
  type TaskUpdate,
} from "@enginebox/fleet-protocol";
import { Tasks } from "./tasks.ts";

export class State extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS records (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    );
  }
  get<T>(key: string): T | undefined {
    const row = this.ctx.storage.sql
      .exec<{ value: string }>("SELECT value FROM records WHERE key = ?", key)
      .toArray()[0];
    return row ? (JSON.parse(row.value) as T) : undefined;
  }
  put(key: string, value: unknown) {
    this.ctx.storage.sql.exec(
      "INSERT INTO records(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
      key,
      JSON.stringify(value),
    );
  }
  remove(key: string) {
    this.ctx.storage.sql.exec("DELETE FROM records WHERE key = ?", key);
  }
  rows<T>(prefix: string): T[] {
    return this.ctx.storage.sql
      .exec<{ value: string }>(
        "SELECT value FROM records WHERE key LIKE ? ORDER BY key",
        `${prefix}%`,
      )
      .toArray()
      .map((row) => JSON.parse(row.value) as T);
  }
}

export class Registry extends State {
  async add(kind: "node" | "project", id: string) {
    this.put(`${kind}:${id}`, id);
  }
  async list(kind: "node" | "project") {
    return this.rows<string>(`${kind}:`);
  }
  async registerNode(spec: NodeSpec) {
    this.put(`node:${spec.id}`, spec.id);
    if (spec.role === "storage")
      for (const project of spec.projects)
        this.put(`storage:${project}:${spec.id}`, spec.id);
  }
  async storage(project: string) {
    return this.rows<string>(`storage:${project}:`);
  }
}

type NodeRecord = NodeView & { secretHash: string };
type Reservation = Assignment & {
  state: "reserved" | "running" | "finished";
  outcome?: string;
};
export class FleetNode extends State {
  async create(spec: NodeSpec, secretHash: string) {
    requireThat(!this.get("node"), "Node already exists", 409);
    this.put("node", {
      ...spec,
      secretHash,
      epoch: 0,
      session: null,
      lastSeen: 0,
      revoked: false,
      ready: {},
      stats: {},
    } satisfies NodeRecord);
  }
  async authenticate(secretHash: string) {
    const node = this.get<NodeRecord>("node");
    requireThat(
      node && !node.revoked && node.secretHash === secretHash,
      "Node authorization failed",
      401,
    );
    const { secretHash: _secret, ...view } = node;
    return view;
  }
  async view() {
    const node = this.get<NodeRecord>("node");
    if (!node) return null;
    const { secretHash: _secret, ...view } = node;
    return view;
  }
  async hello(session: string) {
    const node = this.get<NodeRecord>("node")!;
    requireThat(!node.revoked, "Node revoked", 401);
    if (node.session !== session) {
      node.epoch++;
      node.session = session;
      node.ready = {};
    }
    node.lastSeen = Date.now();
    this.put("node", node);
    return {
      epoch: node.epoch,
      session,
      reservations: this.rows<Reservation>("lease:").filter(
        (r) => r.state !== "finished",
      ),
    };
  }
  current(epoch: number, session: string) {
    const node = this.get<NodeRecord>("node");
    requireThat(
      node && !node.revoked && node.epoch === epoch && node.session === session,
      "Stale node session",
      409,
    );
    return node;
  }
  async heartbeat(
    epoch: number,
    session: string,
    stats: Record<string, number>,
  ) {
    const node = this.current(epoch, session);
    node.lastSeen = Date.now();
    node.stats = stats;
    this.put("node", node);
    return { epoch, now: Date.now() };
  }
  async reserve(job: JobSpec, epoch: number, session: string) {
    const node = this.current(epoch, session);
    requireThat(
      node.role === "agent" && node.projects.includes(job.project),
      "Node cannot execute this project",
      403,
    );
    const prior = this.get<Reservation>(`lease:${job.project}:${job.id}`);
    if (prior) {
      requireThat(
        prior.epoch === epoch &&
          prior.session === session &&
          prior.state !== "finished",
        "Existing reservation requires reconciliation",
        409,
      );
      return prior;
    }
    requireThat(
      this.rows<Reservation>("lease:").filter((r) => r.state !== "finished")
        .length < node.slots,
      "Node has no free slots",
      409,
    );
    const reservation: Reservation = {
      ...job,
      node: node.id,
      epoch,
      session,
      expires: Date.now() + LEASE_MS,
      attempt: crypto.randomUUID(),
      state: "reserved",
    };
    this.put(`lease:${job.project}:${job.id}`, reservation);
    return reservation;
  }
  async discard(project: string, id: string, attempt: string) {
    const key = `lease:${project}:${id}`;
    const lease = this.get<Reservation>(key);
    if (lease?.attempt === attempt && lease.state === "reserved")
      this.remove(key);
  }
  async start(assignment: Assignment) {
    this.current(assignment.epoch, assignment.session);
    const key = `lease:${assignment.project}:${assignment.id}`;
    const lease = this.get<Reservation>(key);
    requireThat(
      lease?.attempt === assignment.attempt &&
        lease.expires > Date.now() &&
        lease.state !== "finished",
      "Reservation expired",
      409,
    );
    lease.state = "running";
    this.put(key, lease);
    return lease;
  }
  async renew(assignment: Assignment) {
    this.current(assignment.epoch, assignment.session);
    const key = `lease:${assignment.project}:${assignment.id}`;
    const lease = this.get<Reservation>(key);
    requireThat(
      lease?.attempt === assignment.attempt &&
        lease.state === "running" &&
        lease.expires > Date.now(),
      "Lease expired",
      409,
    );
    lease.expires = Date.now() + LEASE_MS;
    this.put(key, lease);
    return lease.expires;
  }
  async finish(assignment: Assignment, outcome: string) {
    this.current(assignment.epoch, assignment.session);
    const key = `lease:${assignment.project}:${assignment.id}`;
    const lease = this.get<Reservation>(key);
    requireThat(lease?.attempt === assignment.attempt, "Unknown attempt", 409);
    if (lease.state === "finished") return;
    lease.state = "finished";
    lease.outcome = outcome;
    this.put(key, lease);
  }
  async reservations(epoch: number, session: string) {
    this.current(epoch, session);
    return this.rows<Reservation>("lease:").filter(
      (r) => r.state !== "finished",
    );
  }
  async retire(assignment: Assignment) {
    const key = `lease:${assignment.project}:${assignment.id}`;
    const lease = this.get<Reservation>(key);
    requireThat(lease?.attempt === assignment.attempt, "Unknown attempt", 409);
    lease.state = "finished";
    this.put(key, lease);
  }
  async ready(
    project: string,
    generation: string,
    epoch: number,
    session: string,
    ready: boolean,
  ) {
    const node = this.current(epoch, session);
    requireThat(
      node.role === "storage" && node.projects.includes(project),
      "Not assigned storage",
      403,
    );
    const values = new Set(node.ready[project] ?? []);
    if (ready) values.add(generation);
    else values.delete(generation);
    requireThat(values.size <= 4096, "Too many ready generations");
    node.ready[project] = [...values];
    this.put("node", node);
  }
  async revoke() {
    const node = this.get<NodeRecord>("node")!;
    node.revoked = true;
    node.epoch++;
    node.ready = {};
    this.put("node", node);
  }
  async grant(project: string, generation: string, reader: string) {
    const node = this.get<NodeRecord>("node")!;
    requireThat(
      !node.revoked && node.projects.includes(project),
      "Storage unavailable",
      403,
    );
    const payload = btoa(
      JSON.stringify({
        project,
        generation,
        reader,
        expires: Date.now() + 60_000,
        epoch: node.epoch,
      }),
    );
    const key = await crypto.subtle.importKey(
      "raw",
      new TextEncoder().encode(node.secretHash),
      { name: "HMAC", hash: "SHA-256" },
      false,
      ["sign"],
    );
    const signature = new Uint8Array(
      await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(payload)),
    );
    return `${payload}.${Array.from(signature, (b) => b.toString(16).padStart(2, "0")).join("")}`;
  }
}

type GenerationRecord = {
  id: string;
  manifest: Generation;
  sequence: number;
  created: number;
};
type JobRecord = {
  spec: JobSpec;
  status: "queued" | "assigned" | "completed" | "failed";
  assignment?: Assignment;
  result?: string;
};
type Decision = {
  id: string;
  project: string;
  previous: string | null;
  generation: string;
  created: number;
  validation?: {
    candidate: CandidateSpec;
    checks: CheckRequirement[];
    jobs: Record<string, string>;
  };
};
type CandidateRecord = {
  spec: CandidateSpec;
  checks: CheckRequirement[];
  fullJobs: Record<string, string>;
};
export class Project extends State {
  private tasks() {
    return new Tasks(this, this.ctx.storage.sql);
  }
  async task(id: string) {
    return this.tasks().get(id);
  }
  async listTasks(cursor: string, limit: number, status?: TaskStatus) {
    return this.tasks().list(cursor, limit, status);
  }
  async taskActivity(cursor: number, limit: number) {
    return this.tasks().activity(cursor, limit);
  }
  async createTask(spec: TaskCreate, actor: TaskActor) {
    return this.tasks().create(spec, actor);
  }
  async updateTask(id: string, input: TaskUpdate, actor: TaskActor) {
    return this.tasks().update(id, input, actor);
  }
  async moveTask(id: string, input: TaskMove, actor: TaskActor) {
    return this.tasks().move(id, input, actor);
  }
  async create(id: string) {
    if (!this.get("id")) {
      this.put("id", id);
      this.put("head", null);
      this.put("sequence", 0);
    }
  }
  async identity() {
    return this.get<string>("id") ?? null;
  }
  async inventory() {
    const head = this.get<string | null>("head") ?? null;
    const generations = this.ctx.storage.sql
      .exec<{ value: string }>(
        "SELECT value FROM records WHERE key LIKE 'generation:%' ORDER BY json_extract(value,'$.sequence') DESC LIMIT 32",
      )
      .toArray()
      .map((r) => JSON.parse(r.value) as GenerationRecord);
    if (head && !generations.some((g) => g.id === head)) {
      const accepted = this.get<GenerationRecord>(`generation:${head}`);
      if (accepted) generations.push(accepted);
    }
    const jobs = this.ctx.storage.sql
      .exec<{ value: string }>(
        "SELECT value FROM records WHERE key LIKE 'job:%' ORDER BY rowid DESC LIMIT 256",
      )
      .toArray()
      .map((r) => JSON.parse(r.value) as JobRecord);
    return {
      project: this.get<string>("id"),
      head,
      sequence: this.get<number>("sequence") ?? 0,
      generations: generations.map((g) => ({
        id: g.id,
        sequence: g.sequence,
        created: g.created,
        snapshotBytes: g.manifest.snapshotBytes,
        parent: g.manifest.parent,
      })),
      jobs,
      requiredChecks: this.get<CheckRequirement[]>("checks") ?? [],
    };
  }
  async job(id: string) {
    return this.get<JobRecord>(`job:${id}`) ?? null;
  }
  async generation(id: string) {
    return this.get<GenerationRecord>(`generation:${id}`) ?? null;
  }
  async configureChecks(checks: CheckRequirement[]) {
    requireThat(
      !this.get("pendingDecision"),
      "Acceptance must settle before changing validation policy",
      409,
    );
    this.put("checks", checks);
    return checks;
  }
  async candidate(id: string) {
    return this.get<CandidateRecord>(`candidate:${id}`) ?? null;
  }
  async stage(spec: CandidateSpec) {
    const generation = this.get<GenerationRecord>(
      `generation:${spec.generation}`,
    );
    requireThat(
      generation && generation.manifest.parent === spec.base,
      "Candidate must name its integrated base",
      409,
    );
    for (const change of spec.changes)
      requireThat(
        this.get(`generation:${change.generation}`),
        "Candidate change generation is not published",
        409,
      );
    const checks = this.get<CheckRequirement[]>("checks") ?? [];
    requireThat(
      checks.length > 0,
      "Configure required checks before staging a candidate",
      409,
    );
    const prior = this.get<CandidateRecord>(`candidate:${spec.id}`);
    if (prior) {
      requireThat(
        canonical(prior.spec) === canonical(spec) &&
          canonical(prior.checks) === canonical(checks),
        "Candidate identity or check policy changed",
        409,
      );
      return prior;
    }
    const value: CandidateRecord = { spec, checks, fullJobs: {} };
    this.put(`candidate:${spec.id}`, value);
    return value;
  }
  async publish(id: string, manifest: Generation) {
    requireThat(this.get("id") === manifest.project, "Unknown project", 404);
    const previous = this.get<GenerationRecord>(`generation:${id}`);
    if (previous) return previous;
    if (manifest.parent !== null)
      requireThat(
        this.get(`generation:${manifest.parent}`),
        "Unknown parent generation",
        409,
      );
    const sequence = (this.get<number>("sequence") ?? 0) + 1;
    const value = { id, manifest, sequence, created: Date.now() };
    this.put(`generation:${id}`, value);
    this.put("sequence", sequence);
    return value;
  }
  async accept(
    generation: string,
    expected: string | null,
    candidateId?: string,
  ) {
    const project = this.get<string>("id")!;
    requireThat(
      this.get(`generation:${generation}`),
      "Unknown generation",
      404,
    );
    const head = this.get<string | null>("head") ?? null;
    if (head === generation)
      return { head, decision: this.get<string>("lastDecision") };
    const checks = this.get<CheckRequirement[]>("checks") ?? [];
    let validation: Decision["validation"];
    if (checks.length > 0) {
      requireThat(
        candidateId,
        "Validated candidate required by project policy",
        409,
      );
      const candidate = this.get<CandidateRecord>(`candidate:${candidateId}`);
      requireThat(
        candidate &&
          candidate.spec.generation === generation &&
          candidate.spec.base === expected &&
          canonical(candidate.checks) === canonical(checks),
        "Candidate or validation policy changed",
        409,
      );
      for (const check of checks) {
        const job = this.get<JobRecord>(
          `job:${candidate.fullJobs[check.name]}`,
        );
        requireThat(
          job?.status === "completed" &&
            job.spec.generation === generation &&
            job.spec.check?.scope === "full" &&
            job.spec.check.recipe === check.recipe &&
            job.spec.kind === check.kind,
          "Required full candidate check is not green",
          409,
        );
      }
      validation = {
        candidate: candidate.spec,
        checks,
        jobs: candidate.fullJobs,
      };
    }
    requireThat(head === expected, "Accepted head changed", 409);
    requireThat(
      this.get<GenerationRecord>(`generation:${generation}`)!.manifest
        .parent === expected,
      "Generation must incorporate the accepted head",
      409,
    );
    let pending = this.get<Decision>("pendingDecision");
    if (pending)
      requireThat(
        pending.generation === generation &&
          pending.previous === expected &&
          canonical(pending.validation ?? null) ===
            canonical(validation ?? null),
        "Another acceptance is pending",
        409,
      );
    else {
      pending = {
        id: crypto.randomUUID(),
        project,
        previous: head,
        generation,
        created: Date.now(),
        ...(validation ? { validation } : {}),
      };
      this.put("pendingDecision", pending);
    }
    // The durable decision is written before the head acknowledgment. A retry
    // completes the same decision after a crash between R2 and DO persistence.
    await this.env.STORE.put(
      `projects/${project}/decisions/${pending.id}.json`,
      canonical(pending),
      { onlyIf: { etagDoesNotMatch: "*" } },
    );
    const current = this.get<Decision>("pendingDecision");
    if (!current && this.get("head") === generation)
      return { head: generation, decision: pending.id };
    requireThat(
      current?.id === pending.id && (this.get("head") ?? null) === expected,
      "Acceptance changed",
      409,
    );
    this.put("head", generation);
    this.put("lastDecision", pending.id);
    this.tasks().accepted(pending);
    this.remove("pendingDecision");
    return { head: generation, decision: pending.id };
  }
  async enqueue(spec: JobSpec) {
    requireThat(
      this.get(`generation:${spec.generation}`),
      "Generation is not published",
      409,
    );
    const prior = this.get<JobRecord>(`job:${spec.id}`);
    if (prior) {
      requireThat(
        canonical(prior.spec) === canonical(spec),
        "Job ID already has another specification",
        409,
      );
      return prior;
    }
    const job: JobRecord = { spec, status: "queued" };
    if (spec.check) {
      const candidate = this.get<CandidateRecord>(
        `candidate:${spec.check.candidate}`,
      );
      const check = candidate?.checks.find((c) => c.name === spec.check!.name);
      requireThat(
        candidate &&
          candidate.spec.generation === spec.generation &&
          check?.recipe === spec.check.recipe &&
          check.kind === spec.kind &&
          canonical(check.argv ?? null) === canonical(spec.argv ?? null) &&
          check.timeoutSeconds === spec.timeoutSeconds,
        "Check does not match the candidate recipe",
        409,
      );
      if (spec.check.scope === "full") {
        const pending = this.get<Decision>("pendingDecision");
        requireThat(
          pending?.validation?.candidate.id !== candidate.spec.id,
          "Acceptance must settle before replacing its validation attempt",
          409,
        );
        candidate.fullJobs[check.name] = spec.id;
        this.put(`candidate:${candidate.spec.id}`, candidate);
      }
    }
    this.put(`job:${spec.id}`, job);
    return job;
  }
  async next() {
    const row = this.ctx.storage.sql
      .exec<{
        value: string;
      }>(
        "SELECT value FROM records WHERE key LIKE 'job:%' AND json_extract(value,'$.status')='queued' ORDER BY rowid LIMIT 1",
      )
      .toArray()[0];
    return row ? (JSON.parse(row.value) as JobRecord).spec : null;
  }
  async assign(assignment: Assignment) {
    const key = `job:${assignment.id}`;
    const job = this.get<JobRecord>(key);
    requireThat(
      job &&
        canonical(job.spec) ===
          canonical({
            id: assignment.id,
            project: assignment.project,
            generation: assignment.generation,
            kind: assignment.kind,
            ...(assignment.check ? { check: assignment.check } : {}),
            ...(assignment.argv
              ? {
                  argv: assignment.argv,
                  timeoutSeconds: assignment.timeoutSeconds,
                }
              : {}),
          }),
      "Job specification changed",
      409,
    );
    if (job.assignment?.attempt === assignment.attempt) return job;
    requireThat(
      job.status === "queued" && assignment.expires > Date.now(),
      "Job already assigned",
      409,
    );
    job.status = "assigned";
    job.assignment = assignment;
    this.put(key, job);
    return job;
  }
  async renew(assignment: Assignment, expires: number) {
    const key = `job:${assignment.id}`;
    const job = this.get<JobRecord>(key);
    requireThat(
      job?.status === "assigned" &&
        job.assignment?.attempt === assignment.attempt &&
        job.assignment.epoch === assignment.epoch &&
        job.assignment.expires > Date.now(),
      "Stale project lease",
      409,
    );
    job.assignment.expires = expires;
    this.put(key, job);
  }
  async finish(assignment: Assignment, result: string, ok: boolean) {
    const key = `job:${assignment.id}`;
    const job = this.get<JobRecord>(key);
    requireThat(
      job?.assignment?.attempt === assignment.attempt &&
        job.assignment.epoch === assignment.epoch,
      "Stale result",
      409,
    );
    if (job.status === "completed" || job.status === "failed") {
      requireThat(
        job.result === result && (job.status === "completed") === ok,
        "Result differs from recorded outcome",
        409,
      );
      return job;
    }
    requireThat(
      !ok || job.assignment.expires > Date.now(),
      "Expired result cannot claim success",
      409,
    );
    if (ok && job.spec.check) {
      const report = JSON.parse(result);
      requireThat(
        canonical(report.check) === canonical(job.spec.check),
        "Check result scope or identity differs",
        409,
      );
    }
    job.status = ok ? "completed" : "failed";
    job.result = result;
    this.put(key, job);
    return job;
  }
  async interrupt(assignment: Assignment) {
    const key = `job:${assignment.id}`;
    const job = this.get<JobRecord>(key);
    if (!job?.assignment) return;
    requireThat(
      job.assignment.attempt === assignment.attempt,
      "Attempt changed",
      409,
    );
    if (job.status !== "assigned") return;
    job.status = "failed";
    job.result = canonical({
      error: "Interrupted attempt reconciled; submit a new job ID to retry",
    });
    this.put(key, job);
  }
}
