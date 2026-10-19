export type Resources = {
  cpu: number;
  memory_gib: number;
  disk_gib: number;
};
export type PreparedTemplate = {
  id: string;
  label: string;
  minimum?: Resources;
  recommended?: Resources;
  host_ids?: string[];
  host_resources?: Record<
    string,
    { minimum: Resources; recommended: Resources }
  >;
};
export type TemplateRecord = PreparedTemplate & {
  schema_version: number;
  revision: number;
  description?: string;
  recipe: string;
  source_image: string;
  minimum: Resources;
  recommended: Resources;
  native_clients?: string[];
  config?: Record<string, boolean>;
  manifest_digest: string;
  sync?: {
    desired_revision: number;
    nodes: {
      host_id: string;
      label: string;
      status: "pending" | "syncing" | "ready" | "failed";
      error?: string;
      job_id?: string;
    }[];
  };
  deployments: {
    host_id: string;
    source: string;
    snapshot: string;
    manifest_digest: string;
    revision: number;
    in_sync: boolean;
    installed_at?: string;
  }[];
};
export type Allocation = {
  name: string;
  template: string;
} & (
  { size: string; resources?: never } | { resources: Resources; size?: never }
);
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
  workspace_name?: string;
  warm_pool_state?: "preparing" | "ready" | "claimed" | "quarantined";
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
    contribution_cpu?: number | null;
    contribution_memory_bytes?: number | null;
    contribution_allocated_cpu?: number;
    contribution_allocated_memory_bytes?: number;
    cpu_available?: number;
    memory_budget_available_bytes?: number;
    disk_budget_available_bytes?: number;
    instance_slots_available?: number;
    max_additional: Record<string, number>;
  };
  instances: Instance[];
  warm_pools?: {
    template: string;
    size: string;
    resources?: Resources;
    target: number;
    ready: number;
    preparing: number;
    error?: string;
  }[];
};
export type Fleet = {
  hosts: Host[];
  templates: PreparedTemplate[];
  sizes: Record<string, { cpu: number; memory_gib: number }>;
};
export type Job = {
  id: string;
  status: "queued" | "running" | "succeeded" | "failed";
  result?: {
    name?: string;
    workspace_name?: string;
    startup?: "warm" | "cold";
    ready_seconds?: number;
    [key: string]: unknown;
  } | null;
  error?: string;
  label?: string;
  host?: string;
  instance?: string;
  kind?: "workspace" | "template";
};

export type ProviderId = "codex" | "claude" | "cursor";
export type AuthSession = {
  id: string;
  host_id?: string;
  name?: string;
  provider: ProviderId;
  status:
    "starting" | "waiting" | "succeeded" | "failed" | "cancelled" | "expired";
  authorization_url?: string | null;
  user_code?: string | null;
  input_required: boolean;
  message: string;
  expires_at: string;
};
export type ProviderAccount = {
  id: ProviderId;
  label: string;
  installed: boolean;
  status: "connected" | "disconnected" | "unknown" | "unavailable";
  auth_method: "subscription" | "account" | "api_key" | "unknown" | "none";
  detail?: string;
  session?: AuthSession | null;
  delegation_status?: "ready" | "unsupported" | "reauth_required" | "unknown";
  workspace_count?: number;
};
export type AgentUsage = {
  provider: ProviderId;
  scope: "controller_account";
  status: "available" | "unavailable";
  detail: string;
  source: string | null;
  observed_at: string | null;
  freshness: "current" | "stale" | "unknown";
  plan: string | null;
  billing_cycle: null;
  allowance_windows: {
    id: string;
    label: string;
    used_percentage: number;
    remaining_percentage: number | null;
    window_minutes: number | null;
    resets_at: string;
    freshness: "current" | "stale";
  }[];
  credits: { label: string; balance: string | null; unlimited: boolean; unit: string }[];
  refresh_after_seconds: number;
};
