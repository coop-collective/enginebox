import type { Job } from "./types";

export type StorageNode = {
  host_id: string;
  status: "registered";
  projects: { id: string }[];
};
export type StorageCatalog = { configured: boolean; nodes: StorageNode[] };
export type Checkout = {
  name: string;
  project: string;
  status: string;
  last_error?: string | null;
  last_checkpoint?: unknown;
};
export type SourcePreview = { commit: string; approval: string; files: string[]; bytes: number };
export type StorageTask = {
  operation: string;
  busy: boolean;
  uncertain?: boolean;
  job?: Job;
  error?: string;
  result?: Record<string, unknown>;
};

export function sourcePreview(value: unknown): SourcePreview | null {
  if (!value || typeof value !== "object") return null;
  const p = value as SourcePreview;
  return /^([a-f0-9]{40}|[a-f0-9]{64})$/.test(p.commit)
    && /^[a-f0-9]{64}$/.test(p.approval)
    && Number.isSafeInteger(p.bytes) && p.bytes >= 0
    && Array.isArray(p.files) && p.files.every((file) => typeof file === "string")
    ? p : null;
}

export function checkoutRows(value: unknown): Checkout[] | null {
  if (!value || typeof value !== "object" || !("workspaces" in value)) return null;
  const rows = value.workspaces;
  if (!Array.isArray(rows) || !rows.every((row) => row && typeof row.name === "string"
    && typeof row.project === "string" && typeof row.status === "string")) return null;
  return rows;
}

export function workspaceBusy(tasks: Record<string, StorageTask>, name: string) {
  const task = tasks[`workspace:${name}`];
  return !!task && (task.busy || !!task.uncertain);
}

export function pending(job: Job) {
  return job.status === "queued" || job.status === "running";
}
