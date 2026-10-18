"use client";

import { FormEvent, useCallback, useEffect, useRef, useState } from "react";
import type { Fleet, Host, Instance, Job } from "@/lib/types";

const GiB = 1024 ** 3;
const active = (job: Job) =>
  job.status === "queued" || job.status === "running";
const bytes = (value?: number) =>
  typeof value === "number" && Number.isFinite(value)
    ? `${(value / GiB).toLocaleString(undefined, { maximumFractionDigits: 1 })} GiB`
    : "—";
const title = (value: string) => value.charAt(0).toUpperCase() + value.slice(1);

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

class RequestError extends Error {
  constructor(
    message: string,
    public status: number,
  ) {
    super(message);
  }
}
async function api<T>(path: string, options: RequestInit = {}): Promise<T> {
  const response = await fetch(path, {
    ...options,
    headers: { "Content-Type": "application/json", ...options.headers },
    credentials: "same-origin",
    cache: "no-store",
  });
  const data = await response.json();
  if (!response.ok)
    throw new RequestError(
      typeof data.error === "string"
        ? data.error
        : (data.error?.message ??
          data.detail ??
          "The request failed. Try again."),
      response.status,
    );
  return data as T;
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
          </div>
          <div className="host-foot">
            <span>
              {c.reserve_cpu} cores · {bytes(c.reserve_memory_bytes)} reserved
            </span>
            <span className="capacity-badge">
              {c.max_additional.standard ?? 0} standard slots
            </span>
          </div>
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
}: {
  host: Host;
  instance: Instance;
  busy: boolean;
  action: (host: string, name: string, action: string) => void;
}) {
  const running = instance.status === "Running";
  const resumable = ["Stopped", "Frozen"].includes(instance.status);
  return (
    <tr>
      <td>
        <div className="workspace-name">
          <span className="workspace-icon">
            <Icon name="terminal" size={18} />
          </span>
          <div>
            <strong>{instance.name}</strong>
            <span>{instance.ipv4?.join(", ") || "No address reported"}</span>
          </div>
        </div>
      </td>
      <td>
        <span className="host-name">{host.label}</span>
      </td>
      <td>
        <Status status={instance.status} stateful={instance.stateful} />
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
          {!instance.managed ? (
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
  create: (
    host: string,
    name: string,
    template: string,
    size: string,
  ) => Promise<void>;
}) {
  const [host, setHost] = useState(fleet.hosts.find((h) => h.online)?.id ?? "");
  const [template, setTemplate] = useState(fleet.templates[0]?.id ?? "");
  const [size, setSize] = useState(
    fleet.sizes.standard ? "standard" : (Object.keys(fleet.sizes)[0] ?? ""),
  );
  const [name, setName] = useState("");
  const selectedHost = fleet.hosts.find((h) => h.id === host);
  const slots = selectedHost?.capacity?.max_additional[size] ?? 0;
  const valid = !!host && !!template && !!size && slots > 0;
  async function submit(event: FormEvent) {
    event.preventDefault();
    await create(host, name, template, size);
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
              onChange={(e) => setName(e.target.value)}
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
              onChange={(e) => setHost(e.target.value)}
              required
            >
              <option value="" disabled>
                Select a host
              </option>
              {fleet.hosts.map((h) => (
                <option key={h.id} value={h.id} disabled={!h.online}>
                  {h.label}
                  {!h.online ? " (offline)" : ""}
                </option>
              ))}
            </select>
          </label>
          <label>
            Prepared template
            <select
              value={template}
              onChange={(e) => setTemplate(e.target.value)}
              required
            >
              <option value="" disabled>
                No templates configured
              </option>
              {fleet.templates.map((t) => (
                <option key={t.id} value={t.id}>
                  {t.label}
                </option>
              ))}
            </select>
          </label>
          <label>
            Size
            <select
              value={size}
              onChange={(e) => setSize(e.target.value)}
              required
            >
              {Object.entries(fleet.sizes).map(([id, s]) => (
                <option key={id} value={id}>
                  {title(id)} · {s.cpu} vCPU / {s.memory_gib} GiB
                </option>
              ))}
            </select>
          </label>
        </div>
        <div className="create-footer">
          <p>
            {!fleet.templates.length
              ? "Add a prepared template to the fleet configuration first."
              : slots > 0
                ? `${slots} ${size} workspace${slots === 1 ? "" : "s"} fit within this host's configured capacity.`
                : "This host has no capacity for the selected size."}
          </p>
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
  const [creating, setCreating] = useState(false);
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
  ) {
    setSubmitting(true);
    setError("");
    try {
      const { job } = await api<{ job: Job }>(`/api/control/${path}`, {
        method: "POST",
        body: JSON.stringify(body),
      });
      setJobs((current) =>
        [{ ...job, label, host, instance }, ...current].slice(0, 20),
      );
      return true;
    } catch (err) {
      setError((err as Error).message);
      return false;
    } finally {
      setSubmitting(false);
    }
  }
  async function create(
    host: string,
    name: string,
    template: string,
    size: string,
  ) {
    if (
      await queue(
        `v1/hosts/${encodeURIComponent(host)}/instances`,
        { name, template, size },
        `Create ${name}`,
        host,
        name,
      )
    )
      setCreating(false);
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
  const online = fleet?.hosts.filter((host) => host.online) ?? [];
  const instances =
    fleet?.hosts.flatMap((host) =>
      host.instances.map((instance) => ({ host, instance })),
    ) ?? [];
  const sum = (key: "cpu_total" | "memory_total_bytes") =>
    online.reduce((total, host) => total + (host.capacity?.[key] ?? 0), 0);
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
        <nav>
          <a href="/" className="nav-item active">
            <Icon name="server" />
            Fleet overview
            <span className="nav-dot" />
          </a>
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
            <span className="slash">/</span>Fleet
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
                    Fleet overview<span className="title-dot">.</span>
                  </h1>
                  <p>
                    Manage your machines and give every workspace room to build.
                  </p>
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
                  <button
                    className="button primary"
                    onClick={() => setCreating((value) => !value)}
                    disabled={!fleet || !online.length}
                  >
                    <Icon name="plus" size={17} />
                    New workspace
                  </button>
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
                    label: "CPU capacity",
                    value: fleet ? `${sum("cpu_total")}` : "—",
                    note: "logical cores online",
                    icon: "cpu",
                  },
                  {
                    label: "Memory capacity",
                    value: fleet ? bytes(sum("memory_total_bytes")) : "—",
                    note: "across connected hosts",
                    icon: "memory",
                  },
                  {
                    label: "Workspaces",
                    value: fleet ? `${instances.length}` : "—",
                    note: `${instances.filter(({ instance }) => instance.status === "Running").length} running`,
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
                    <h2>Workspaces</h2>
                    <span className="count">{instances.length}</span>
                  </div>
                  <span className="section-caption">
                    Private environments · Shared foundations
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
                            submitting ||
                            jobs.some(
                              (job) =>
                                active(job) &&
                                job.host === host.id &&
                                job.instance === instance.name,
                            )
                          }
                          action={action}
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
