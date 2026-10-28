import type { Repository } from "@/components/repositories-panel";
export type StageReceipt = { revision: string; observed_at: string; status: string; elapsed_ms: number; source_copy_ms?: number | null; source_bytes?: number | null; prepare_ms?: number | null; written_files?: number; removed_files?: number };
export type Metrics = Partial<Record<"preparation" | "preparation_reuse" | "startup" | "source_update", StageReceipt>>;
export type Environment = { id: string; source: { kind: string; project?: string; workspace?: string }; status: string; healthy: boolean; revision?: string; metrics?: Metrics };
export type DevProject = { id: string; host: string; project: string; status: string; healthy: boolean; urls: string[]; metrics?: Metrics };
export type EnvironmentCatalog = { environments: Environment[]; projects?: DevProject[] };
export type Pool = { name: string; profile?: string; member_status?: { host_id: string; name: string; busy: boolean }[]; queued_runs?: number; running_runs?: number; creating?: number };
export type Operation = { id: string; host_id: string; operation: string; name: string; status: string; created_at: string; finished_at?: string; duration_seconds?: number; bytes?: number; files?: number };
export type StorageMeasurement = { kind?: string; project?: string; status: string; apparent_bytes: number | null; allocated_bytes: number | null; files: number | null };
export type StorageUsage = { host_id: string; observed_at_unix_ms: number; status: string; shared_caches: StorageMeasurement[]; projects: StorageMeasurement[] };
export type CommandTiming = { kind: "test" | "build" | "dependencies" | "dev" | "other"; status: "succeeded" | "failed" | "uncertain"; elapsed_ms: number | null; observed_at_unix_ms: number };
export type Workspace = { id: string; storage_usage: StorageUsage[]; repositories: Repository[]; pools: Pool[]; runs: { id: string; provider: string; pool?: string; name?: string; status: string; elapsed_seconds?: number; created_at?: string; execution_uncertain?: boolean }[]; storage_nodes: string[]; checkouts: { name: string; host_id: string; status: string; observed_at: string; last_checkpoint?: { kind?: string; head?: string }; command_timings?: CommandTiming[] }[]; operations: Operation[]; launch_events: { at: string; kind: string; outcome: string; duration_ms?: number; queue_wait_ms?: number; stage: string; reason: string }[]; integration_observations: { host_id: string; observed_at: string; main?: { revision?: string; sequence?: number }; sync_enabled?: boolean; sync_status?: string }[]; source_observations: { id: string; repository?: string; host_id: string; observed_at: string; main?: { revision?: string } }[] };
export type WorkspaceCatalog = { workspaces: Workspace[]; coverage: { runs_truncated?: boolean; unattributed_runs?: number; run_history_limit: number; operation_history_limit: number; operations_per_workspace_limit: number } };
export const duration = (ms?: number | null) => typeof ms === "number" && Number.isFinite(ms) && ms >= 0 ? ms < 1000 ? `${ms.toFixed(0)} ms` : `${(ms / 1000).toFixed(2)} s` : "Not measured";
export const sourceBytes = (bytes?: number | null) => typeof bytes === "number" && Number.isFinite(bytes) && bytes >= 0 ? `${(bytes / 1024 / 1024).toFixed(2)} MiB` : "Not measured";
export function mergeWorkspaces(catalog: WorkspaceCatalog | null, environments: EnvironmentCatalog | null): Workspace[] {
  const all = new Map((catalog?.workspaces ?? []).map(w => [w.id, w]));
  const ids = [...(environments?.projects ?? []).map(p => p.project), ...(environments?.environments ?? []).map(e => e.source.project).filter((id): id is string => !!id)];
  for (const id of ids) if (!all.has(id)) all.set(id, { id, storage_usage: [], repositories: [], pools: [], runs: [], storage_nodes: [], checkouts: [], operations: [], launch_events: [], source_observations: [], integration_observations: [] });
  return [...all.values()].sort((a, b) => a.id.localeCompare(b.id));
}
