export type Instance = {
  name: string;
  status: string;
  stateful: boolean;
  ipv4: string[];
  cpu: number;
  memory_bytes: number;
  memory_usage_bytes?: number | null;
  template?: string;
  managed?: boolean;
};
export type Host = {
  id: string;
  label: string;
  online: boolean;
  error?: string;
  capacity?: {
    cpu_total: number;
    memory_total_bytes: number;
    memory_available_bytes: number;
    disk_free_bytes: number;
    reserve_cpu: number;
    reserve_memory_bytes: number;
    allocated_cpu: number;
    allocated_memory_bytes: number;
    disk_budget_available_bytes?: number;
    max_additional: Record<string, number>;
  };
  instances: Instance[];
};
export type Fleet = {
  hosts: Host[];
  templates: { id: string; label: string }[];
  sizes: Record<string, { cpu: number; memory_gib: number }>;
};
export type Job = {
  id: string;
  status: "queued" | "running" | "succeeded" | "failed";
  result?: unknown;
  error?: string;
  label?: string;
  host?: string;
  instance?: string;
};
