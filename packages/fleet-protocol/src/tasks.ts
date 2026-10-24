import {
  exactKeys,
  identifier,
  integer,
  digest,
  record,
  requireThat,
} from "./index.ts";

export const TASK_STATUSES = [
  "backlog",
  "in_progress",
  "testing",
  "shipped",
] as const;
export type TaskStatus = (typeof TASK_STATUSES)[number];
export type TaskSource = {
  kind: "chat";
  conversation: string;
  messageIds: string[];
};
export type TaskCreate = {
  id: string;
  title: string;
  description: string;
  type: "idea" | "issue";
  source: TaskSource | null;
  review: "required" | "automatic";
};
export type TaskReviewReceipt = {
  candidate: string;
  generation: string;
  requirements: string;
  approvedBy: string;
  approvedAt: number;
};
export type TaskApproval = { operationId: string; expectedRevision: number };
export function validateTaskApproval(value: unknown): TaskApproval {
  record(value);
  exactKeys(value, ["operationId", "expectedRevision"]);
  identifier(value.operationId);
  integer(value.expectedRevision, 1, Number.MAX_SAFE_INTEGER);
  return {
    operationId: value.operationId,
    expectedRevision: value.expectedRevision,
  };
}
export type Task = TaskCreate & {
  reviewReceipt?: TaskReviewReceipt;
  project: string;
  status: TaskStatus;
  revision: number;
  createdAt: number;
  updatedAt: number;
  createdBy: string;
  updatedBy: string;
  assignee: string | null;
  resultGeneration: string | null;
  candidate: string | null;
  shippedGeneration: string | null;
};
export type TaskActor = { id: string; owner: boolean };
export type TaskPatch = {
  title?: string;
  description?: string;
  type?: "idea" | "issue";
  assignee?: string | null;
};
export type TaskUpdate = {
  operationId: string;
  expectedRevision: number;
  patch: TaskPatch;
};
export type TaskMove = {
  operationId: string;
  expectedRevision: number;
  status: TaskStatus;
  generation?: string;
  candidate?: string;
};
export type TaskEvent = {
  sequence: number;
  taskId: string;
  action: "created" | "updated" | "moved" | "shipped" | "reviewed";
  actor: string;
  at: number;
  revision: number;
  status: TaskStatus;
  decision?: string;
};
function text(
  value: unknown,
  min: number,
  max: number,
): asserts value is string {
  requireThat(
    typeof value === "string" &&
      value.length >= min &&
      value.length <= max &&
      !/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/.test(value),
    "Invalid task text",
  );
}
function title(value: unknown): string {
  text(value, 1, 200);
  requireThat(value.trim().length > 0, "Task title required");
  return value.trim();
}
function kind(value: unknown): asserts value is "idea" | "issue" {
  requireThat(value === "idea" || value === "issue", "Invalid task type");
}
export function taskStatus(value: unknown): asserts value is TaskStatus {
  requireThat(
    TASK_STATUSES.includes(value as TaskStatus),
    "Invalid task status",
  );
}
export function validateTaskCreate(value: unknown): TaskCreate {
  record(value);
  exactKeys(value, ["id", "title", "description", "type", "source", "review"]);
  identifier(value.id);
  requireThat(value.id !== "activity", "Reserved task ID");
  kind(value.type);
  const description = value.description ?? "";
  text(description, 0, 16000);
  let source: TaskSource | null = null;
  if (value.source !== undefined && value.source !== null) {
    record(value.source);
    exactKeys(value.source, ["kind", "conversation", "messageIds"]);
    requireThat(value.source.kind === "chat", "Invalid task source");
    text(value.source.conversation, 1, 128);
    requireThat(
      !/[\r\n\t]/.test(value.source.conversation),
      "Invalid source conversation",
    );
    requireThat(
      Array.isArray(value.source.messageIds) &&
        value.source.messageIds.length > 0 &&
        value.source.messageIds.length <= 32,
      "Invalid source messages",
    );
    const messageIds = value.source.messageIds.map((id) => {
      text(id, 1, 128);
      requireThat(!/[\r\n\t]/.test(id), "Invalid source message");
      return id;
    });
    requireThat(
      new Set(messageIds).size === messageIds.length,
      "Duplicate source message",
    );
    source = {
      kind: "chat",
      conversation: value.source.conversation,
      messageIds: messageIds.sort(),
    };
  }
  const review = value.review ?? "required";
  requireThat(
    review === "required" || review === "automatic",
    "Invalid review policy",
  );
  return {
    id: value.id,
    title: title(value.title),
    description,
    type: value.type,
    source,
    review,
  };
}
function operation(value: Record<string, unknown>) {
  identifier(value.operationId);
  integer(value.expectedRevision, 1, Number.MAX_SAFE_INTEGER);
  return {
    operationId: value.operationId,
    expectedRevision: value.expectedRevision,
  };
}
export function validateTaskUpdate(value: unknown): TaskUpdate {
  record(value);
  exactKeys(value, ["operationId", "expectedRevision", "patch"]);
  const op = operation(value);
  record(value.patch);
  exactKeys(value.patch, ["title", "description", "type", "assignee"]);
  requireThat(Object.keys(value.patch).length > 0, "Task patch required");
  const patch: TaskPatch = {};
  if (value.patch.title !== undefined) patch.title = title(value.patch.title);
  if (value.patch.description !== undefined) {
    text(value.patch.description, 0, 16000);
    patch.description = value.patch.description;
  }
  if (value.patch.type !== undefined) {
    kind(value.patch.type);
    patch.type = value.patch.type;
  }
  if (value.patch.assignee !== undefined) {
    if (value.patch.assignee !== null) identifier(value.patch.assignee);
    patch.assignee = value.patch.assignee;
  }
  return { ...op, patch };
}
export function validateTaskMove(value: unknown): TaskMove {
  record(value);
  exactKeys(value, [
    "operationId",
    "expectedRevision",
    "status",
    "generation",
    "candidate",
  ]);
  const op = operation(value);
  taskStatus(value.status);
  if (value.generation !== undefined) digest(value.generation);
  if (value.candidate !== undefined) identifier(value.candidate);
  return {
    ...op,
    status: value.status,
    ...(value.generation ? { generation: value.generation as string } : {}),
    ...(value.candidate ? { candidate: value.candidate as string } : {}),
  };
}
