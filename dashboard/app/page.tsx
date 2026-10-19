"use client";

import { FormEvent, useCallback, useEffect, useRef, useState } from "react";
import type {
  Allocation,
  Fleet,
  Host,
  Instance,
  Job,
  Resources,
} from "@/lib/types";
import { api, RequestError } from "@/lib/client";
import { TemplateCatalog } from "@/components/template-catalog";
import { AgentsPanel } from "@/components/agents-panel";
import { coldSlots, matchingPools, validResources } from "@/lib/resources";

const GiB = 1024 ** 3;
const active = (job: Job) =>
  job.status === "queued" || job.status === "running";
const poolReserve = (instance: Instance) =>
  !!instance.warm_pool_state && instance.warm_pool_state !== "claimed";
const bytes = (value?: number) =>
  typeof value === "number" && Number.isFinite(value)
    ? `${(value / GiB).toLocaleString(undefined, { maximumFractionDigits: 1 })} GiB`
    : "—";
const title = (value: string) => value.charAt(0).toUpperCase() + value.slice(1);
const sections = {
  fleet: {
    title: "Fleet overview",
    description: "Manage your machines and give every workspace room to build.",
    icon: "server",
  },
  agents: {
    title: "Agents",
    description: "Manage native agent connections for your entire fleet.",
    icon: "terminal",
  },
  templates: {
    title: "Templates",
    description: "Prepare reusable environments and keep your nodes in sync.",
    icon: "box",
  },
};
type DashboardSection = keyof typeof sections;

function Icon({ name = "box", size = 20 }: { name?: string; size?: number }) {
  const paths: Record<string, React.ReactNode> = {
    box: (
      <>
        <path d="m12 3 9 5v8l-9 5-9-5V8l9-5Z" />
        <path d="m3 8 9 5 9-5M12 13v8M7.5 5.5l9 5" />
      </>
    ),
    server: (
      <>
        <rect x="3" y="3" width="18" height="7" rx="2" />
        <rect x="3" y="14" width="18" height="7" rx="2" />
        <path d="M7 6.5h.01M7 17.5h.01M12 6.5h5M12 17.5h5" />
      </>
    ),
    arrow: <path d="M5 12h14m-5-5 5 5-5 5" />,
    plus: <path d="M12 5v14M5 12h14" />,
    refresh: (
      <>
        <path d="M20 7v5h-5M4 17v-5h5" />
        <path d="M6 7a7 7 0 0 1 12-1l2 3M4 15l2 3a7 7 0 0 0 12-1" />
      </>
    ),
    cpu: (
      <>
        <rect x="6" y="6" width="12" height="12" rx="2" />
        <path d="M10 1v5m4-5v5m-4 12v5m4-5v5M1 10h5m-5 4h5m12-4h5m-5 4h5M10 10h4v4h-4z" />
      </>
    ),
    memory: (
      <>
        <rect x="3" y="6" width="18" height="11" rx="2" />
        <path d="M7 10v3m5-3v3m5-3v3M7 17v3m5-3v3m5-3v3" />
      </>
    ),
    lock: (
      <>
        <rect x="5" y="10" width="14" height="11" rx="2" />
        <path d="M8 10V7a4 4 0 0 1 8 0v3m-4 5v2" />
      </>
    ),
    close: <path d="m6 6 12 12M6 18 18 6" />,
    check: <path d="m5 12 4 4L19 6" />,
    terminal: (
      <>
        <rect x="3" y="4" width="18" height="16" rx="3" />
        <path d="m7 9 3 3-3 3m6 0h4" />
      </>
    ),
    logout: <path d="M10 4H4v16h6m4-12 4 4-4 4m-6-4h12" />,
  };
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.65"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {paths[name] ?? paths.box}
    </svg>
  );
}

function Status({ status, stateful }: { status: string; stateful?: boolean }) {
  const label =
    status === "Stopped" && stateful
      ? "Hibernated"
      : status === "Frozen"
        ? "Paused"
        : status;
  return (
    <span className={`status status-${status.toLowerCase()}`}>
      <i />
      {label}
    </span>
  );
}

function Meter({ used, total }: { used: number; total: number }) {
  const percentage = Math.min(
    100,
    Math.max(0, total ? (used / total) * 100 : 0),
  );
  return (
    <div
      className="meter"
      aria-label={`${Math.round(percentage)} percent allocated`}
    >
      <span style={{ width: `${percentage}%` }} />
    </div>
  );
}

function HostCard({ host }: { host: Host }) {
  const c = host.capacity;
  return (
    <article className={`host-card ${!host.online ? "offline" : ""}`}>
      <div className="host-heading">
        <div className="host-symbol">
          <Icon name="server" />
        </div>
        <div>
          <h3>{host.label}</h3>
          <span className="mono subtle">{host.id}</span>
        </div>
        <Status status={host.online ? "Online" : "Offline"} />
      </div>
      {!host.online || !c ? (
        <p className="host-error" role="status">
          {host.error ??
            "This host is not reachable. Check the connection and refresh."}
        </p>
      ) : (
        <>
          <div className="resource-line">
            <span>CPU allocated</span>
            <strong>
              {c.allocated_cpu} <small>/ {c.cpu_total} cores</small>
            </strong>
          </div>
          <Meter used={c.allocated_cpu} total={c.cpu_total} />
          <div className="resource-line">
            <span>Memory allocated</span>
            <strong>
              {bytes(c.allocated_memory_bytes)}{" "}
              <small>/ {bytes(c.memory_total_bytes)}</small>
            </strong>
          </div>
          <Meter used={c.allocated_memory_bytes} total={c.memory_total_bytes} />
          <div className="host-details">
            <span>
              Available memory<strong>{bytes(c.memory_available_bytes)}</strong>
            </span>
            <span>
              Free disk<strong>{bytes(c.disk_free_bytes)}</strong>
            </span>
            {typeof c.disk_budget_available_bytes === "number" && (
              <span>
                Workspace disk budget
                <strong>{bytes(c.disk_budget_available_bytes)}</strong>
              </span>
            )}
            {typeof c.contribution_cpu === "number" && (
              <span>
                Fleet CPU limit
                <strong>{c.contribution_allocated_cpu ?? 0} / {c.contribution_cpu} cores</strong>
              </span>
            )}
            {typeof c.contribution_memory_bytes === "number" && (
              <span>
                Fleet memory limit
                <strong>{bytes(c.contribution_allocated_memory_bytes)} / {bytes(c.contribution_memory_bytes)}</strong>
              </span>
            )}
          </div>
          <div className="host-foot">
            <span>
              {c.reserve_cpu} cores · {bytes(c.reserve_memory_bytes)} reserved
            </span>
            <span className="capacity-badge">
              {c.max_additional.standard ?? 0} standard cold slots
            </span>
          </div>
          {!!host.warm_pools?.length && (
            <div
              className="pool-summary"
              aria-label="Prepared workspace reserve"
            >
              <h4>Prepared reserve</h4>
              {host.warm_pools.map((pool) => (
                <div
                  className="pool-summary-item"
                  key={`${pool.template}/${pool.size}`}
                >
                  <strong>
                    {pool.template} · {title(pool.size)}
                  </strong>
                  {pool.resources && (
                    <span>
                      {pool.resources.cpu} vCPU · {pool.resources.memory_gib}{" "}
                      GiB RAM · {pool.resources.disk_gib} GiB disk
                    </span>
                  )}
                  <span>
                    {pool.ready} ready · {pool.preparing} preparing · target{" "}
                    {pool.target}
                  </span>
                  {pool.error && <p className="pool-error">{pool.error}</p>}
                </div>
              ))}
              <p>Ready workspaces are paused and keep memory allocated.</p>
            </div>
          )}
        </>
      )}
    </article>
  );
}

function WorkspaceRow({
  host,
  instance,
  busy,
  action,
  connect,
}: {
  host: Host;
  instance: Instance;
  busy: boolean;
  action: (host: string, name: string, action: string) => void;
  connect: () => void;
}) {
  const running = instance.status === "Running";
  const resumable = ["Stopped", "Frozen"].includes(instance.status);
  const reserved = poolReserve(instance);
  return (
    <tr className={reserved ? "pool-reserve-row" : undefined}>
      <td>
        <div className="workspace-name">
          <span className="workspace-icon">
            <Icon name="terminal" size={18} />
          </span>
          <div>
            <strong>{instance.workspace_name || instance.name}</strong>
            {instance.workspace_name &&
              instance.workspace_name !== instance.name && (
                <span className="instance-id">VM: {instance.name}</span>
              )}
            <span>{instance.ipv4?.join(", ") || "No address reported"}</span>
          </div>
        </div>
      </td>
      <td>
        <span className="host-name">{host.label}</span>
      </td>
      <td>
        <Status status={instance.status} stateful={instance.stateful} />
        {reserved && (
          <span className={`pool-state pool-state-${instance.warm_pool_state}`}>
            Pool reserve · {title(instance.warm_pool_state!)}
          </span>
        )}
      </td>
      <td>
        <strong className="allocation">
          {instance.cpu ?? "—"} vCPU · {bytes(instance.memory_bytes)}
        </strong>
        <span className="table-detail">
          {typeof instance.memory_usage_bytes === "number"
            ? `${bytes(instance.memory_usage_bytes)} memory used`
            : "Usage unavailable"}
        </span>
      </td>
      <td className="actions">
        <div className="action-buttons">
          {reserved ? (
            <span
              className="subtle"
              title="Prepared reserve is managed by the warm pool. Allocate a new workspace to claim a ready instance."
            >
              Reserved for allocation
            </span>
          ) : !instance.managed ? (
            <span
              className="subtle"
              title="This instance is not managed by Engine Box. Lifecycle controls are disabled."
            >
              Read only
            </span>
          ) : busy ? (
            <span className="working">
              <span className="spinner" />
              Working
            </span>
          ) : (
            <>
              <button className="button small" onClick={connect}>
                Manage agents
              </button>
              {resumable && (
                <button
                  className="button small"
                  disabled={!host.online}
                  onClick={() => action(host.id, instance.name, "resume")}
                >
                  Resume
                </button>
              )}
              {running && (
                <>
                  <button
                    className="button small"
                    disabled={!host.online}
                    onClick={() => action(host.id, instance.name, "pause")}
                  >
                    Pause
                  </button>
                  <button
                    className="button small"
                    disabled={!host.online}
                    onClick={() => action(host.id, instance.name, "hibernate")}
                  >
                    Hibernate
                  </button>
                  <button
                    className="button small danger-text"
                    disabled={!host.online}
                    onClick={() => {
                      if (
                        window.confirm(
                          `Stop ${instance.name}? Running processes will end. Use Hibernate to preserve memory.`,
                        )
                      )
                        action(host.id, instance.name, "stop");
                    }}
                  >
                    Stop
                  </button>
                </>
              )}
              {!running && !resumable && (
                <span className="subtle">No actions available</span>
              )}
            </>
          )}
        </div>
      </td>
    </tr>
  );
}

function CreateWorkspace({
  fleet,
  busy,
  close,
  create,
}: {
  fleet: Fleet;
  busy: boolean;
  close: () => void;
  create: (host: string, request: Allocation) => Promise<void>;
}) {
  const [host, setHost] = useState(
    fleet.hosts.find((item) => item.online)?.id ?? "",
  );
  const available = fleet.templates.filter(
    (item) => !item.host_ids || item.host_ids.includes(host),
  );
  const [template, setTemplate] = useState(available[0]?.id ?? "");
  const selectedTemplate = available.find((item) => item.id === template);
  const requirements =
    selectedTemplate?.host_resources?.[host] ?? selectedTemplate;
  const [size, setSize] = useState(
    requirements?.recommended
      ? "recommended"
      : fleet.sizes.standard
        ? "standard"
        : (Object.keys(fleet.sizes)[0] ?? ""),
  );
  const [custom, setCustom] = useState({
    cpu: "4",
    memory_gib: "4",
    disk_gib: "40",
  });
  const [name, setName] = useState("");
  const selectedHost = fleet.hosts.find((item) => item.id === host);
  useEffect(() => {
    if (!available.some((item) => item.id === template))
      setTemplate(available[0]?.id ?? "");
  }, [host, template, fleet.templates]);
  useEffect(() => {
    const recommended = requirements?.recommended;
    setSize(
      recommended
        ? "recommended"
        : fleet.sizes.standard
          ? "standard"
          : (Object.keys(fleet.sizes)[0] ?? ""),
    );
    if (recommended)
      setCustom({
        cpu: String(recommended.cpu),
        memory_gib: String(recommended.memory_gib),
        disk_gib: String(recommended.disk_gib),
      });
  }, [template, host]);
  const minimum = requirements?.minimum;
  const resources: Resources | undefined =
    size === "custom"
      ? {
          cpu: Number(custom.cpu),
          memory_gib: Number(custom.memory_gib),
          disk_gib: Number(custom.disk_gib),
        }
      : size === "minimum"
        ? minimum
        : size === "recommended"
          ? requirements?.recommended
          : undefined;
  const resourceValid =
    !resources ||
    validResources(
      resources,
      minimum ?? { cpu: 1, memory_gib: 1, disk_gib: 1 },
    );
  const slots = resources
    ? coldSlots(selectedHost, resources)
    : (selectedHost?.capacity?.max_additional[size] ?? 0);
  const pools = matchingPools(selectedHost, template, size, resources);
  const ready = pools.reduce((sum, pool) => sum + pool.ready, 0);
  const preparing = pools.reduce((sum, pool) => sum + pool.preparing, 0);
  const valid =
    !!selectedHost?.online &&
    !!selectedTemplate &&
    !!size &&
    resourceValid &&
    (ready > 0 || slots > 0);
  async function submit(event: FormEvent) {
    event.preventDefault();
    if (!valid || busy) return;
    await create(
      host,
      size === "custom" && resources
        ? { name, template, resources }
        : { name, template, size },
    );
  }
  return (
    <section className="create-panel" aria-labelledby="create-title">
      <div className="section-title">
        <div>
          <h2 id="create-title">Create a workspace</h2>
          <p>A private workspace from a prepared, credential-free template.</p>
        </div>
        <button
          className="icon-button"
          onClick={close}
          aria-label="Close workspace form"
        >
          <Icon name="close" />
        </button>
      </div>
      <form onSubmit={submit}>
        <div className="create-grid">
          <label>
            Workspace name
            <input
              required
              value={name}
              onChange={(event) => setName(event.target.value)}
              placeholder="ebox-my-project"
              pattern={"ebox-[a-z0-9][a-z0-9\\-]{0,48}"}
              title="Start with ebox-, then use lowercase letters, digits or hyphens."
              maxLength={54}
              autoFocus
            />
          </label>
          <label>
            Host
            <select
              value={host}
              onChange={(event) => setHost(event.target.value)}
              required
            >
              <option value="" disabled>
                Select a host
              </option>
              {fleet.hosts.map((item) => (
                <option key={item.id} value={item.id} disabled={!item.online}>
                  {item.label}
                  {!item.online ? " (offline)" : ""}
                </option>
              ))}
            </select>
          </label>
          <label>
            Prepared template
            <select
              value={template}
              onChange={(event) => setTemplate(event.target.value)}
              required
            >
              <option value="" disabled>
                No installed templates
              </option>
              {available.map((item) => (
                <option key={item.id} value={item.id}>
                  {item.label}
                </option>
              ))}
            </select>
          </label>
          <label>
            Size
            <select
              value={size}
              onChange={(event) => setSize(event.target.value)}
              required
            >
              {minimum && requirements?.recommended ? (
                <>
                  <option value="minimum">
                    Minimum · {minimum.cpu} vCPU / {minimum.memory_gib} GiB /{" "}
                    {minimum.disk_gib} GiB disk
                  </option>
                  <option value="recommended">
                    Recommended · {requirements.recommended.cpu} vCPU /{" "}
                    {requirements.recommended.memory_gib} GiB /{" "}
                    {requirements.recommended.disk_gib} GiB disk
                  </option>
                  <option value="custom">Custom</option>
                </>
              ) : (
                Object.entries(fleet.sizes).map(([id, value]) => (
                  <option key={id} value={id}>
                    {title(id)} · {value.cpu} vCPU / {value.memory_gib} GiB
                  </option>
                ))
              )}
            </select>
          </label>
        </div>
        {size === "custom" && minimum && (
          <div className="custom-resources">
            {(
              [
                ["cpu", "CPU cores"],
                ["memory_gib", "Memory (GiB)"],
                ["disk_gib", "Disk (GiB)"],
              ] as const
            ).map(([key, label]) => (
              <label key={key}>
                {label}
                <input
                  type="number"
                  min={minimum[key]}
                  step="1"
                  required
                  value={custom[key]}
                  onChange={(event) =>
                    setCustom((current) => ({
                      ...current,
                      [key]: event.target.value,
                    }))
                  }
                />
                <span>Minimum {minimum[key]}</span>
              </label>
            ))}
          </div>
        )}
        {resources && resourceValid && (
          <p className="resource-preview">
            {resources.cpu} vCPU · {resources.memory_gib} GiB RAM ·{" "}
            {resources.disk_gib} GiB disk
          </p>
        )}
        <div className="create-footer">
          <div className="allocation-hint" aria-live="polite">
            <p>
              {!available.length
                ? "Install a template on this host from the template catalog first."
                : !selectedHost?.online
                  ? "Choose an online host."
                  : !resourceValid
                    ? "Custom resources must be whole numbers at or above the template minimum."
                    : ready > 0
                      ? `${ready} ready paused workspace${ready === 1 ? " matches" : "s match"} this template and resource allocation. A ready workspace will be claimed if still available.`
                      : slots > 0
                        ? `Cold start: no matching workspace is ready. Capacity is available for ${slots} more workspace${slots === 1 ? "" : "s"} with these resources.`
                        : "No ready workspace or cold-start capacity is available for this selection."}
            </p>
            {ready > 0 && (
              <p>
                {slots > 0
                  ? "If the reserve is claimed first, the request may use a cold start."
                  : "No cold-start capacity remains; another request may claim the reserve first."}
              </p>
            )}
            {preparing > 0 && (
              <p>
                {preparing} matching workspace
                {preparing === 1 ? " is" : "s are"} preparing. Availability
                refreshes automatically.
              </p>
            )}
            {pools
              .filter((pool) => pool.error)
              .map((pool) => (
                <p className="pool-error" key={`${pool.template}/${pool.size}`}>
                  {pool.error}
                </p>
              ))}
          </div>
          <button
            className="button primary"
            disabled={!valid || busy}
            type="submit"
          >
            {busy ? (
              <span className="spinner" />
            ) : (
              <Icon name="plus" size={16} />
            )}
            Create workspace
          </button>
        </div>
      </form>
    </section>
  );
}

export default function Dashboard() {
  const [session, setSession] = useState<"loading" | "login" | "ready">(
    "loading",
  );
  const [password, setPassword] = useState("");
  const [fleet, setFleet] = useState<Fleet | null>(null);
  const [error, setError] = useState("");
  const [refreshing, setRefreshing] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [pendingInstances, setPendingInstances] = useState<string[]>([]);
  const [creating, setCreating] = useState(false);
  const [section, setSection] = useState<DashboardSection>("fleet");
  const [updated, setUpdated] = useState<Date | null>(null);
  const [jobs, setJobs] = useState<Job[]>([]);
  const loading = useRef(false);
  useEffect(() => {
    api<{ authenticated: boolean }>("/api/session")
      .then((data) => setSession(data.authenticated ? "ready" : "login"))
      .catch((err) => {
        setError(err.message);
        setSession("login");
      });
  }, []);
  const refresh = useCallback(async () => {
    if (loading.current) return;
    loading.current = true;
    setRefreshing(true);
    try {
      setFleet(await api<Fleet>("/api/control/v1/fleet"));
      setUpdated(new Date());
      setError("");
    } catch (err) {
      if (err instanceof RequestError && err.status === 401) {
        setSession("login");
        setFleet(null);
      }
      setError((err as Error).message);
    } finally {
      loading.current = false;
      setRefreshing(false);
    }
  }, []);
  useEffect(() => {
    if (session !== "ready") return;
    void refresh();
    const timer = setInterval(refresh, 10_000);
    return () => clearInterval(timer);
  }, [session, refresh]);
  useEffect(() => {
    if (session !== "ready" || !jobs.some(active)) return;
    let cancelled = false;
    const timer = setTimeout(async () => {
      const updates = await Promise.all(
        jobs.filter(active).map(async (job) => {
          try {
            const result = await api<{ job: Job }>(
              `/api/control/v1/jobs/${encodeURIComponent(job.id)}`,
            );
            return { ...job, ...result.job, error: result.job.error };
          } catch (err) {
            return {
              ...job,
              error: `Status unavailable: ${(err as Error).message}`,
            };
          }
        }),
      );
      if (cancelled) return;
      setJobs((current) =>
        current.map(
          (job) => updates.find((update) => update.id === job.id) ?? job,
        ),
      );
      if (updates.some((job) => !active(job))) void refresh();
    }, 1500);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [jobs, session, refresh]);
  async function signIn(event: FormEvent) {
    event.preventDefault();
    setSubmitting(true);
    setError("");
    try {
      await api("/api/session", {
        method: "POST",
        body: JSON.stringify({ password }),
      });
      setPassword("");
      setSession("ready");
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setSubmitting(false);
    }
  }
  async function signOut() {
    try {
      await api("/api/session", { method: "DELETE" });
      setSession("login");
      setFleet(null);
      setJobs([]);
      setPendingInstances([]);
      setSection("fleet");
      setError("");
    } catch (err) {
      setError((err as Error).message);
    }
  }
  async function queue(
    path: string,
    body: object,
    label: string,
    host: string,
    instance: string,
    kind: Job["kind"] = "workspace",
  ) {
    const target = `${host}/${instance}`;
    setPendingInstances((current) => [...current, target]);
    setError("");
    try {
      const { job } = await api<{ job: Job }>(`/api/control/${path}`, {
        method: "POST",
        body: JSON.stringify(body),
      });
      setJobs((current) =>
        [{ ...job, label, host, instance, kind }, ...current].slice(0, 20),
      );
      return true;
    } catch (err) {
      setError((err as Error).message);
      return false;
    } finally {
      setPendingInstances((current) => current.filter((key) => key !== target));
    }
  }
  async function create(host: string, request: Allocation) {
    setSubmitting(true);
    if (
      await queue(
        `v1/hosts/${encodeURIComponent(host)}/allocations`,
        request,
        `Create ${request.name}`,
        host,
        request.name,
      )
    )
      setCreating(false);
    setSubmitting(false);
  }
  function action(host: string, name: string, operation: string) {
    void queue(
      `v1/hosts/${encodeURIComponent(host)}/instances/${encodeURIComponent(name)}/actions`,
      { action: operation },
      `${title(operation)} ${name}`,
      host,
      name,
    );
  }
  function navigate(next: DashboardSection) {
    setSection(next);
    window.scrollTo({ top: 0 });
  }
  const online = fleet?.hosts.filter((host) => host.online) ?? [];
  const instances =
    fleet?.hosts.flatMap((host) =>
      host.instances.map((instance) => ({ host, instance })),
    ) ?? [];
  const assigned = instances.filter(
    ({ instance }) => instance.managed && !poolReserve(instance),
  );
  const readyReserve = online.reduce(
    (total, host) =>
      total +
      (host.warm_pools ?? []).reduce((count, pool) => count + pool.ready, 0),
    0,
  );
  const sum = (key: "cpu_total" | "allocated_cpu" | "memory_total_bytes") =>
    online.reduce((total, host) => total + (host.capacity?.[key] ?? 0), 0);
  const cpuTotal = sum("cpu_total");
  const cpuAllocated = sum("allocated_cpu");
  const memoryTotal = sum("memory_total_bytes");
  const memoryUsed = online.reduce((total, host) => {
    const capacity = host.capacity;
    return (
      total +
      (capacity
        ? Math.max(
            0,
            Math.min(
              capacity.memory_total_bytes,
              capacity.memory_total_bytes - capacity.memory_available_bytes,
            ),
          )
        : 0)
    );
  }, 0);
  const percent = (used: number, total: number) =>
    total > 0
      ? `${Math.min(100, Math.max(0, (used / total) * 100)).toFixed(1)}%`
      : "—";
  return (
    <div className="app-shell">
      <aside className="sidebar">
        <a href="/" className="brand">
          <span className="brand-mark">
            <Icon size={25} />
          </span>
          <span>
            engine<span className="brand-light">box</span>
            <small>WORKSPACE INFRASTRUCTURE</small>
          </span>
        </a>
        <div className="nav-label">CONTROL PLANE</div>
        <nav aria-label="Control plane">
          {(["fleet", "agents", "templates"] as const)
            .filter((item) => session === "ready" || item === "fleet")
            .map((item) => (
              <button
                key={item}
                type="button"
                className={`nav-item${section === item ? " active" : ""}`}
                aria-current={section === item ? "page" : undefined}
                onClick={() => navigate(item)}
              >
                <Icon name={sections[item].icon} />
                {sections[item].title}
                {section === item && <span className="nav-dot" />}
              </button>
            ))}
        </nav>
        <div className="sidebar-bottom">
          <div className="native-note">
            <Icon name="terminal" size={20} />
            <strong>Your tools. Your machines.</strong>
            <p>Native agents, prepared workspaces, and compute you control.</p>
            <div className="harnesses">
              <span>Codex</span>
              <span>Claude</span>
              <span>Cursor</span>
            </div>
          </div>
          <div className="self-hosted">
            <span />
            Self-hosted · Engine Box
          </div>
        </div>
      </aside>
      <div className="main-shell">
        <header className="topbar">
          <div>
            <span className="breadcrumb">Infrastructure</span>
            <span className="slash">/</span>
            {section === "fleet" ? "Fleet" : sections[section].title}
          </div>
          <div className="topbar-right">
            <span className="private-label">
              <Icon name="lock" size={13} />
              Private control plane
            </span>
            {session === "ready" && (
              <button
                className="icon-button"
                onClick={signOut}
                aria-label="Sign out"
              >
                <Icon name="logout" size={18} />
              </button>
            )}
          </div>
        </header>
        <main>
          {session !== "ready" ? (
            <section className="login-panel">
              <span className="login-symbol">
                <Icon name="lock" size={28} />
              </span>
              <span className="eyebrow">WELCOME TO ENGINE BOX</span>
              <h1>Your fleet, under control.</h1>
              <p>Sign in to manage your hosts and workspaces.</p>
              {error && (
                <div role="alert" className="alert">
                  {error}
                </div>
              )}
              {session === "loading" ? (
                <div className="loading">
                  <span className="spinner" />
                  Checking your session…
                </div>
              ) : (
                <form onSubmit={signIn}>
                  <label>
                    Operator password
                    <input
                      type="password"
                      autoComplete="current-password"
                      value={password}
                      onChange={(e) => setPassword(e.target.value)}
                      required
                      autoFocus
                      placeholder="Enter your dashboard password"
                    />
                  </label>
                  <button
                    className="button primary"
                    disabled={submitting}
                    type="submit"
                  >
                    {submitting ? (
                      <span className="spinner" />
                    ) : (
                      <>
                        Open dashboard
                        <Icon name="arrow" size={17} />
                      </>
                    )}
                  </button>
                </form>
              )}
              <span className="login-foot">
                Your session stays in this browser.
              </span>
            </section>
          ) : (
            <>
              <div className="page-heading">
                <div>
                  <span className="eyebrow">YOUR COMPUTE, CONNECTED</span>
                  <h1>
                    {sections[section].title}
                    <span className="title-dot">.</span>
                  </h1>
                  <p>{sections[section].description}</p>
                </div>
                <div className="heading-actions">
                  <button
                    className="button"
                    onClick={refresh}
                    disabled={refreshing}
                  >
                    <Icon name="refresh" size={16} />
                    {refreshing ? "Refreshing" : "Refresh"}
                  </button>
                  {section === "fleet" && (
                    <button
                      className="button primary"
                      onClick={() => setCreating((value) => !value)}
                      disabled={!fleet || !online.length}
                    >
                      <Icon name="plus" size={17} />
                      New workspace
                    </button>
                  )}
                </div>
              </div>
              {error && (
                <div className="alert" role="alert">
                  {error}
                  <button className="text-button" onClick={refresh}>
                    Try again
                  </button>
                </div>
              )}
              {section === "agents" && (
                <AgentsPanel close={() => navigate("fleet")} />
              )}
              {section === "templates" && (
                <TemplateCatalog
                  refreshToken={updated?.getTime() ?? 0}
                  registered={refresh}
                  close={() => navigate("fleet")}
                />
              )}
              {section === "fleet" && (
                <>
                  <div className="stat-grid">
                    {[
                      {
                        label: "Connected hosts",
                        value: fleet ? `${online.length}` : "—",
                        note: fleet
                          ? `${fleet.hosts.length} configured`
                          : "Loading fleet",
                        icon: "server",
                      },
                      {
                        label: "CPU allocated",
                        value: fleet ? (
                          <>
                            {cpuAllocated}{" "}
                            <span className="stat-total">/ {cpuTotal}</span>
                          </>
                        ) : (
                          "—"
                        ),
                        note: `${percent(cpuAllocated, cpuTotal)} allocated · logical cores`,
                        icon: "cpu",
                      },
                      {
                        label: "Host memory used",
                        value: fleet ? (
                          <>
                            {bytes(memoryUsed)}{" "}
                            <span className="stat-total">
                              / {bytes(memoryTotal)}
                            </span>
                          </>
                        ) : (
                          "—"
                        ),
                        note: `${percent(memoryUsed, memoryTotal)} used · across online hosts`,
                        icon: "memory",
                      },
                      {
                        label: "Assigned workspaces",
                        value: fleet ? `${assigned.length}` : "—",
                        note: `${assigned.filter(({ instance }) => instance.status === "Running").length} running · ${readyReserve} ready in reserve`,
                        icon: "box",
                      },
                    ].map((stat) => (
                      <article className="stat-card" key={stat.label}>
                        <div>
                          <span>{stat.label}</span>
                          <Icon name={stat.icon} size={19} />
                        </div>
                        <strong>{stat.value}</strong>
                        <small>{stat.note}</small>
                      </article>
                    ))}
                  </div>
                  {creating && fleet && (
                    <CreateWorkspace
                      fleet={fleet}
                      busy={submitting}
                      close={() => setCreating(false)}
                      create={create}
                    />
                  )}
                  <section className="hosts-section">
                    <div className="section-title">
                      <div className="section-heading">
                        <h2>Hosts</h2>
                        <span className="count">{fleet?.hosts.length ?? 0}</span>
                      </div>
                      <span className="update-label">
                        <span className={`live-dot ${error ? "stale" : ""}`} />
                        {updated
                          ? `Updated ${updated.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}`
                          : "Waiting for fleet"}
                      </span>
                    </div>
                    {!fleet ? (
                      <div className="empty-state">
                        {refreshing ? (
                          <>
                            <span className="spinner" />
                            <h3>Connecting to your fleet</h3>
                            <p>Reading host capacity and workspace status.</p>
                          </>
                        ) : (
                          <>
                            <Icon name="server" size={32} />
                            <h3>Fleet connection unavailable</h3>
                            <p>
                              Check the service connection, then refresh to try
                              again.
                            </p>
                          </>
                        )}
                      </div>
                    ) : !fleet.hosts.length ? (
                      <div className="empty-state">
                        <Icon name="server" size={32} />
                        <h3>Add your first host</h3>
                        <p>
                          Configure a host in your Engine Box fleet inventory. It
                          will appear here after refresh.
                        </p>
                      </div>
                    ) : (
                      <div className="host-grid">
                        {fleet.hosts.map((host) => (
                          <HostCard key={host.id} host={host} />
                        ))}
                      </div>
                    )}
                  </section>
                  <section className="workspaces-section">
                    <div className="section-title">
                      <div className="section-heading">
                        <h2>Instance inventory</h2>
                        <span className="count">{instances.length}</span>
                      </div>
                      <span className="section-caption">
                        Assigned workspaces · Prepared reserves · Bases
                      </span>
                    </div>
                    <div className="table-container">
                      <table>
                        <thead>
                          <tr>
                            <th>Workspace</th>
                            <th>Host</th>
                            <th>Status</th>
                            <th>Resources</th>
                            <th className="actions">Controls</th>
                          </tr>
                        </thead>
                        <tbody>
                          {instances.map(({ host, instance }) => (
                            <WorkspaceRow
                              key={`${host.id}/${instance.name}`}
                              host={host}
                              instance={instance}
                              busy={
                                pendingInstances.includes(
                                  `${host.id}/${instance.name}`,
                                ) ||
                                jobs.some(
                                  (job) =>
                                    active(job) &&
                                    job.host === host.id &&
                                    (job.instance === instance.name ||
                                      (!!instance.workspace_name &&
                                        job.instance === instance.workspace_name)),
                                )
                              }
                              action={action}
                              connect={() => navigate("agents")}
                            />
                          ))}
                        </tbody>
                      </table>
                      {!instances.length && (
                        <div className="empty-state compact">
                          <Icon size={28} />
                          <h3>
                            {fleet
                              ? "No workspaces yet"
                              : "Waiting for workspace inventory"}
                          </h3>
                          <p>
                            {fleet
                              ? "Create a workspace from a prepared template to start building."
                              : "Workspaces appear when the fleet service responds."}
                          </p>
                        </div>
                      )}
                    </div>
                    <p className="workspace-hint">
                      Pause keeps memory allocated. Hibernate saves memory to disk.
                      Stop ends running processes.
                    </p>
                  </section>
                  {!!jobs.length && (
                    <section className="jobs-section" aria-live="polite">
                      <div className="section-title">
                        <h2>Recent operations</h2>
                        <span className="section-caption">
                          This browser session
                        </span>
                      </div>
                      <div className="job-list">
                        {jobs.map((job) => (
                          <article className="job" key={job.id}>
                            <span className={`job-icon ${job.status}`}>
                              {active(job) ? (
                                <span className="spinner" />
                              ) : (
                                <Icon
                                  name={
                                    job.status === "succeeded" ? "check" : "close"
                                  }
                                  size={18}
                                />
                              )}
                            </span>
                            <div>
                              <strong>{job.label ?? job.id}</strong>
                              {job.error && (
                                <p className="job-error">{job.error}</p>
                              )}
                              {job.status === "succeeded" &&
                                job.result?.startup && (
                                  <p className="job-detail">
                                    {job.result.startup === "warm"
                                      ? "Warm allocation"
                                      : "Cold start"}
                                    {typeof job.result.ready_seconds === "number" &&
                                      Number.isFinite(job.result.ready_seconds) &&
                                      ` · ready in ${job.result.ready_seconds.toFixed(2)} s`}
                                    {job.result.name && (
                                      <span className="mono">
                                        VM: {job.result.name}
                                      </span>
                                    )}
                                  </p>
                                )}
                              <small className="mono">{job.id}</small>
                            </div>
                            <span className={`status status-${job.status}`}>
                              {title(job.status)}
                            </span>
                          </article>
                        ))}
                      </div>
                    </section>
                  )}
                </>
              )}
              <footer className="page-footer">
                <span>Engine Box</span>
                <span>
                  Prepared environments. Native harnesses. Your infrastructure.
                </span>
              </footer>
            </>
          )}
        </main>
      </div>
    </div>
  );
}
