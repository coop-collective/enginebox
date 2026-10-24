import { createHash } from "node:crypto";
import {
  canonical,
  requireThat,
  type Task,
  type TaskActor,
  type TaskApproval,
  type TaskCreate,
  type TaskEvent,
  type TaskMove,
  type TaskStatus,
  type TaskUpdate,
  type CandidateSpec,
} from "@enginebox/fleet-protocol";

type Store = {
  get<T>(key: string): T | undefined;
  put(key: string, value: unknown): void;
};
export type AcceptedTaskEvidence = {
  id: string;
  generation: string;
  validation?: {
    candidate: CandidateSpec;
    taskRequirements?: Record<string, string>;
  };
};
type TaskCandidate = {
  spec: CandidateSpec;
  taskRequirements?: Record<string, string>;
};
function requirements(task: Task) {
  return createHash("sha256")
    .update(
      canonical({
        title: task.title,
        description: task.description,
        type: task.type,
      }),
    )
    .digest("hex");
}

// Project-local records share the Project DO's synchronous transaction boundary.
// No I/O or await occurs between task, operation receipt and event writes.
export class Tasks {
  constructor(
    private store: Store,
    private sql: SqlStorage,
  ) {}
  get(id: string) {
    return this.store.get<Task>(`task:${id}`) ?? null;
  }
  snapshotRequirements(spec: CandidateSpec) {
    return Object.fromEntries(
      spec.changes.flatMap((change) => {
        const task = this.get(change.id);
        return task ? [[task.id, requirements(task)]] : [];
      }),
    );
  }
  validateRequirements(snapshot: Record<string, string> = {}) {
    for (const [id, expected] of Object.entries(snapshot)) {
      const task = this.get(id);
      requireThat(
        task && requirements(task) === expected,
        "Task requirements changed after this candidate was staged",
        409,
      );
    }
  }
  list(cursor: string, limit: number, status?: TaskStatus) {
    const rows = this.sql
      .exec<{ value: string }>(
        "SELECT value FROM records WHERE key LIKE 'task:%' AND key>? AND (? IS NULL OR json_extract(value,'$.status')=?) ORDER BY key LIMIT ?",
        `task:${cursor}`,
        status ?? null,
        status ?? null,
        limit + 1,
      )
      .toArray()
      .map((row) => JSON.parse(row.value) as Task);
    return {
      tasks: rows.slice(0, limit),
      nextCursor: rows.length > limit ? rows[limit - 1].id : null,
    };
  }
  activity(cursor: number, limit: number) {
    const rows = this.sql
      .exec<{ value: string }>(
        "SELECT value FROM records WHERE key LIKE 'task-event:%' AND json_extract(value,'$.sequence')>? ORDER BY json_extract(value,'$.sequence') LIMIT ?",
        cursor,
        limit + 1,
      )
      .toArray()
      .map((row) => JSON.parse(row.value) as TaskEvent);
    return {
      events: rows.slice(0, limit),
      nextCursor: rows.length > limit ? rows[limit - 1].sequence : null,
    };
  }
  private event(
    task: Task,
    action: TaskEvent["action"],
    actor: string,
    decision?: string,
  ) {
    const sequence = (this.store.get<number>("task-event-sequence") ?? 0) + 1;
    this.store.put("task-event-sequence", sequence);
    const event: TaskEvent = {
      sequence,
      taskId: task.id,
      action,
      actor,
      at: task.updatedAt,
      revision: task.revision,
      status: task.status,
      ...(decision ? { decision } : {}),
    };
    this.store.put(`task-event:${sequence}`, event);
  }
  create(spec: TaskCreate, actor: TaskActor) {
    requireThat(
      actor.owner || spec.review === "required",
      "Only the owner may select automatic review",
      403,
    );
    const input = canonical(spec);
    const prior = this.get(spec.id);
    if (prior) {
      requireThat(
        this.store.get(`task-created:${spec.id}`) === input,
        "Task ID already has another creation request",
        409,
      );
      return prior;
    }
    if (spec.source) {
      const existing = this.store.get<string>(
        `task-source:${canonical(spec.source)}`,
      );
      requireThat(
        !existing || existing === spec.id,
        "Chat source already has a task",
        409,
      );
    }
    const now = Date.now();
    const task: Task = {
      ...spec,
      project: this.store.get<string>("id")!,
      status: "backlog",
      revision: 1,
      createdAt: now,
      updatedAt: now,
      createdBy: actor.id,
      updatedBy: actor.id,
      assignee: null,
      resultGeneration: null,
      candidate: null,
      shippedGeneration: null,
    };
    this.store.put(`task:${task.id}`, task);
    this.store.put(`task-created:${task.id}`, input);
    if (spec.source)
      this.store.put(`task-source:${canonical(spec.source)}`, spec.id);
    this.event(task, "created", actor.id);
    return task;
  }
  private prior(
    id: string,
    operationId: string,
    input: unknown,
    actor: TaskActor,
  ) {
    const prior = this.store.get<{ input: string; result: Task }>(
      `task-operation:${id}:${operationId}`,
    );
    if (prior) {
      requireThat(
        prior.input === canonical({ input, actor }),
        "Operation ID already has another request",
        409,
      );
      return prior.result;
    }
    return null;
  }
  private current(id: string, expectedRevision: number) {
    const task = this.get(id);
    requireThat(task, "Unknown task", 404);
    requireThat(
      task.revision === expectedRevision,
      "Task revision changed",
      409,
    );
    requireThat(
      task.status !== "shipped",
      "Shipped task is immutable; create follow-up work",
      409,
    );
    return task;
  }
  private save(
    task: Task,
    action: TaskEvent["action"],
    operationId: string,
    input: unknown,
    actor: TaskActor,
    decision?: string,
  ) {
    task.revision++;
    task.updatedAt = Date.now();
    task.updatedBy = actor.id;
    this.store.put(`task:${task.id}`, task);
    this.store.put(`task-operation:${task.id}:${operationId}`, {
      input: canonical({ input, actor }),
      result: task,
    });
    this.event(task, action, actor.id, decision);
    return task;
  }
  approve(id: string, input: TaskApproval, actor: TaskActor) {
    requireThat(actor.owner, "Only the owner may approve shipping", 403);
    const prior = this.prior(id, input.operationId, input, actor);
    if (prior) return prior;
    const task = this.current(id, input.expectedRevision);
    requireThat(
      task.status === "testing" && task.candidate && task.resultGeneration,
      "A frozen tested candidate is required before approval",
      409,
    );
    const candidate = this.store.get<TaskCandidate>(
      `candidate:${task.candidate}`,
    );
    requireThat(
      candidate?.taskRequirements?.[id] === requirements(task) &&
        candidate.spec.changes.some(
          (change) =>
            change.id === id && change.generation === task.resultGeneration,
        ),
      "Task no longer matches the tested candidate",
      409,
    );
    task.reviewReceipt = {
      candidate: task.candidate,
      generation: task.resultGeneration,
      requirements: requirements(task),
      approvedBy: actor.id,
      approvedAt: Date.now(),
    };
    return this.save(task, "reviewed", input.operationId, input, actor);
  }
  reviewReady(task: Task, candidate: TaskCandidate) {
    return (
      task.status === "testing" &&
      task.candidate === candidate.spec.id &&
      candidate.taskRequirements?.[task.id] === requirements(task) &&
      (task.review === "automatic" ||
        (task.reviewReceipt?.candidate === task.candidate &&
          task.reviewReceipt.generation === task.resultGeneration &&
          task.reviewReceipt.requirements === requirements(task)))
    );
  }
  update(id: string, input: TaskUpdate, actor: TaskActor) {
    const prior = this.prior(id, input.operationId, input, actor);
    if (prior) return prior;
    const task = this.current(id, input.expectedRevision);
    const contentChanged =
      (input.patch.title !== undefined && input.patch.title !== task.title) ||
      (input.patch.description !== undefined &&
        input.patch.description !== task.description) ||
      (input.patch.type !== undefined && input.patch.type !== task.type);
    if (contentChanged)
      requireThat(
        !this.store.get<AcceptedTaskEvidence>("pendingDecision")?.validation
          ?.taskRequirements?.[task.id],
        "Acceptance must settle before changing task requirements",
        409,
      );
    Object.assign(task, input.patch);
    if (contentChanged) {
      delete task.reviewReceipt;
      task.resultGeneration = null;
      task.candidate = null;
      if (task.status === "testing") task.status = "in_progress";
    }
    return this.save(task, "updated", input.operationId, input, actor);
  }
  move(id: string, input: TaskMove, actor: TaskActor) {
    requireThat(
      actor.owner || input.status !== "shipped",
      "Only the owner may approve shipping",
      403,
    );
    const prior = this.prior(id, input.operationId, input, actor);
    if (prior) return prior;
    const task = this.current(id, input.expectedRevision);
    requireThat(
      !this.store.get<AcceptedTaskEvidence>("pendingDecision")?.validation
        ?.taskRequirements?.[id],
      "Acceptance must settle before moving its tasks",
      409,
    );
    if (
      input.status !== "testing" ||
      (input.generation && input.generation !== task.resultGeneration) ||
      (input.candidate && input.candidate !== task.candidate)
    )
      delete task.reviewReceipt;
    if (input.generation) {
      requireThat(
        this.store.get(`generation:${input.generation}`),
        "Task result generation is not published",
        409,
      );
      task.resultGeneration = input.generation;
    }
    if (input.candidate) {
      const candidate = this.store.get<TaskCandidate>(
        `candidate:${input.candidate}`,
      );
      requireThat(
        candidate?.spec.changes.some(
          (change) =>
            change.id === task.id &&
            change.generation === task.resultGeneration,
        ),
        "Candidate does not include this task result",
        409,
      );
      requireThat(
        candidate?.taskRequirements?.[task.id] === requirements(task),
        "Candidate does not match current task requirements",
        409,
      );
      task.candidate = input.candidate;
    }
    let decision: string | undefined;
    if (input.status === "shipped") {
      requireThat(
        task.status === "testing",
        "Task must be in testing before shipping",
        409,
      );
      const evidence = this.acceptedEvidence(task);
      decision = evidence.id;
      task.shippedGeneration = evidence.generation;
    } else if (input.status === "backlog" || input.status === "in_progress") {
      task.resultGeneration = null;
      task.candidate = null;
    }
    task.status = input.status;
    return this.save(
      task,
      input.status === "shipped" ? "shipped" : "moved",
      input.operationId,
      input,
      actor,
      decision,
    );
  }
  private acceptedEvidence(task: Task): AcceptedTaskEvidence {
    requireThat(
      task.resultGeneration && task.candidate,
      "Accepted candidate evidence is required to ship",
      409,
    );
    const candidate = this.store.get<TaskCandidate>(
      `candidate:${task.candidate}`,
    );
    requireThat(candidate, "Unknown task candidate", 409);
    const evidence = this.store.get<AcceptedTaskEvidence>(
      `acceptance:${candidate.spec.generation}`,
    );
    requireThat(
      evidence?.validation?.candidate.id === task.candidate &&
        evidence.validation.taskRequirements?.[task.id] ===
          requirements(task) &&
        evidence.validation.candidate.changes.some(
          (change) =>
            change.id === task.id &&
            change.generation === task.resultGeneration,
        ),
      "This task result is not in an accepted candidate",
      409,
    );
    return evidence;
  }
  accepted(evidence: AcceptedTaskEvidence) {
    this.store.put(`acceptance:${evidence.generation}`, evidence);
    if (!evidence.validation) return;
    const candidate = evidence.validation.candidate;
    for (const change of candidate.changes) {
      const task = this.get(change.id);
      if (
        !task ||
        task.status !== "testing" ||
        !(
          task.review === "automatic" ||
          (task.reviewReceipt?.candidate === candidate.id &&
            task.reviewReceipt.generation === task.resultGeneration &&
            task.reviewReceipt.requirements === requirements(task))
        ) ||
        evidence.validation.taskRequirements?.[task.id] !==
          requirements(task) ||
        task.resultGeneration !== change.generation ||
        (task.candidate && task.candidate !== candidate.id)
      )
        continue;
      task.candidate = candidate.id;
      task.status = "shipped";
      task.shippedGeneration = evidence.generation;
      task.revision++;
      task.updatedAt = Date.now();
      task.updatedBy = task.reviewReceipt?.approvedBy ?? "policy:automatic";
      this.store.put(`task:${task.id}`, task);
      this.event(task, "shipped", task.updatedBy, evidence.id);
    }
  }
}
