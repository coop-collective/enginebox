"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { Allocation, ComputeNode, Fleet, Job } from "@/lib/types";
import { api, errorMessage, RequestError } from "@/lib/client";
import { usePageVisible, useVisibleInterval } from "@/lib/visible-interval";
import { TemplateCatalog } from "@/components/template-catalog";
import { CiTemplates } from "@/components/ci-templates";
import { AgentsPanel } from "@/components/agents-panel";
import { ComputePanel, type Contributor } from "@/components/compute-panel";
import { ConnectedAppsPanel } from "@/components/connected-apps-panel";
import { ProjectsPanel } from "@/components/projects-panel";
import { PreviewRuntimes } from "@/components/preview-runtimes";
import { GitHubPanel } from "@/components/github-panel";
import { EmailSignIn } from "@/components/email-sign-in";
import { OrgPanel } from "@/components/org-panel";
import { CredentialsPanel } from "@/components/credentials-panel";
import { ConnectionsPanel } from "@/components/connections-panel";
import { ProfilesPanel } from "@/components/profiles-panel";
import { Overview } from "@/components/overview";
import { CommandMenu, type Command } from "@/components/command-menu";
import { Icon, PageHeader, Tabs } from "@/components/ui";
import {
  activeJob,
  bytes,
  CreateWorkspace,
  HostCard,
  hostAdmitting,
  poolReserve,
  title,
  WorkspaceRow,
} from "@/components/fleet";

/**
 * Information architecture (docs/DASHBOARD-IA.md). Everything a person works
 * on is a Project. Agents and CI are the jobs that run; Infrastructure is the
 * capacity (machines and the sandboxes on them) and the templates they run;
 * Settings holds what the repository must never contain: secrets, access and
 * the GitHub connection.
 */
type View = {
  id: string;
  label: string;
  group: string;
  icon: string;
  title: string;
  description: string;
  keywords?: string;
};
const VIEWS: View[] = [
  { id: "overview", label: "Overview", group: "Home", icon: "overview", title: "Overview", description: "What your machines, agents and CI are doing, and what needs you." },
  { id: "projects", label: "Projects", group: "Home", icon: "folder", title: "Projects", description: "Each project's agents, code, previews and activity in one place.", keywords: "workspaces repositories source layerfs storage checkouts runs previews environments routes" },
  { id: "profiles", label: "Profiles", group: "Agents", icon: "profile", title: "Profiles", description: "How and where an agent runs: template, tools, skills, connections and secrets.", keywords: "run kinds policy skills pools" },
  { id: "agents", label: "Accounts", group: "Agents", icon: "agent", title: "Accounts", description: "Codex, Claude and Cursor sign-ins, shared with the organization or personal, and their usage.", keywords: "codex claude cursor providers usage credentials" },
  { id: "runners", label: "Actions runners", group: "CI", icon: "checks", title: "Actions runners", description: "Run GitHub Actions jobs in disposable VMs on your machines.", keywords: "github ci jobs pools drain" },
  { id: "compute", label: "Machines", group: "Infrastructure", icon: "server", title: "Machines", description: "Computers connected to this controller, their roles and capacity, and the sandboxes they host.", keywords: "fleet hosts nodes workers enroll sandboxes vm virtual machines instances" },
  { id: "templates", label: "Templates", group: "Infrastructure", icon: "box", title: "Templates", description: "Credential-free images and how they run: agent sandboxes, build toolchains and preview runtimes.", keywords: "images catalog sync toolchains ci recipes runtimes" },
  { id: "connections", label: "Connections & secrets", group: "Settings", icon: "key", title: "Connections & secrets", description: "MCP servers and secret sources that profiles reference. Values are write-only.", keywords: "mcp secrets env tokens" },
  { id: "github", label: "GitHub", group: "Settings", icon: "branch", title: "GitHub", description: "Connect GitHub and choose which repositories projects may import from and export to.", keywords: "github repositories import export sync" },
  { id: "apps", label: "MCP & apps", group: "Settings", icon: "plug", title: "MCP & apps", description: "Connect MCP clients, review OAuth app access and revoke grants.", keywords: "oauth grants clients tokens" },
  { id: "org", label: "Organization", group: "Settings", icon: "team", title: "Organization", description: "Members, invites and the organization you are working in.", keywords: "members invites roles team" },
];
const GROUPS = ["Home", "Agents", "CI", "Infrastructure", "Settings"];
/** Older links and redirects (`?panel=`) that now live elsewhere, with the tab they open. */
const LEGACY: Record<string, [string, string?]> = {
  fleet: ["compute"],
  instances: ["compute"],
  workspaces: ["projects"],
  repositories: ["projects"],
  storage: ["projects"],
  environments: ["projects", "previews"],
  mcp: ["apps"],
  ci: ["templates", "toolchains"],
  toolchains: ["templates", "toolchains"],
};
const TEMPLATE_TABS = [
  { id: "sandboxes", label: "Sandbox images" },
  { id: "toolchains", label: "Build toolchains" },
  { id: "runtimes", label: "Preview runtimes" },
];
function resolve(id: string | null, tab: string | null): { view: string; tab: string } {
  if (!id) return { view: "overview", tab: "" };
  const [mapped, legacyTab] = LEGACY[id] ?? [id];
  if (!VIEWS.some((v) => v.id === mapped)) return { view: "overview", tab: "" };
  return { view: mapped, tab: legacyTab ?? tab ?? "" };
}

type Me = { role: string; email: string | null; label: string; org_id: string };

export default function Dashboard() {
  const [session, setSession] = useState<"loading" | "login" | "ready">("loading");
  const [me, setMe] = useState<Me | null>(null);
  const [orgName, setOrgName] = useState("");
  const [emailDelivery, setEmailDelivery] = useState(true);
  const [fleet, setFleet] = useState<Fleet | null>(null);
  const [workers, setWorkers] = useState<Contributor[]>([]);
  const [workerNodes, setWorkerNodes] = useState<ComputeNode[]>([]);
  const [workersLoaded, setWorkersLoaded] = useState(false);
  const [workersError, setWorkersError] = useState("");
  const workerRequest = useRef(0);
  const [error, setError] = useState("");
  const [refreshing, setRefreshing] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [pendingInstances, setPendingInstances] = useState<string[]>([]);
  const [creating, setCreating] = useState(false);
  const [view, setView] = useState("overview");
  const [tab, setTab] = useState("");
  const [storageFirst, setStorageFirst] = useState(false);
  const [updated, setUpdated] = useState<Date | null>(null);
  const [jobs, setJobs] = useState<Job[]>([]);
  const [menuOpen, setMenuOpen] = useState(false);
  const [navOpen, setNavOpen] = useState(false);
  const loading = useRef(false);

  const readSession = useCallback(() => {
    api<{ authenticated: boolean; email_delivery?: boolean; me?: Me | null }>("/api/session")
      .then((data) => {
        setEmailDelivery(data.email_delivery !== false);
        setMe(data.me ?? null);
        setSession(data.authenticated ? "ready" : "login");
      })
      .catch((err) => {
        setError(errorMessage(err));
        setSession("login");
      });
  }, []);
  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const panel = params.get("panel");
    const initial = resolve(panel, params.get("tab"));
    setView(initial.view);
    setTab(initial.tab);
    setStorageFirst(panel === "storage" || params.get("storage") === "1");
    readSession();
    const onPop = () => {
      const next = new URLSearchParams(window.location.search);
      const resolved = resolve(next.get("panel"), next.get("tab"));
      setView(resolved.view);
      setTab(resolved.tab);
    };
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, [readSession]);
  useEffect(() => {
    if (session !== "ready") return;
    api<{ org?: { name: string } }>("/api/org")
      .then((data) => setOrgName(data.org?.name ?? ""))
      .catch(() => setOrgName(""));
  }, [session]);
  useEffect(() => {
    function onKey(event: KeyboardEvent) {
      const target = event.target as HTMLElement | null;
      const typing = !!target && (target.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName));
      if ((event.key === "k" && (event.metaKey || event.ctrlKey)) || (event.key === "/" && !typing)) {
        if (session !== "ready") return;
        event.preventDefault();
        setMenuOpen((open) => !open);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [session]);

  const refreshWorkers = useCallback(async () => {
    const request = ++workerRequest.current;
    try {
      const result = await api<{ nodes: Contributor[]; compute_nodes?: ComputeNode[] }>("/api/contributors");
      if (request !== workerRequest.current) return;
      setWorkers(result.nodes);
      setWorkerNodes(result.compute_nodes ?? []);
      setWorkersLoaded(true);
      setWorkersError(result.compute_nodes ? "" : "Update the controller to load its registered compute nodes.");
    } catch (err) {
      if (request !== workerRequest.current) return;
      if (err instanceof RequestError && err.status === 401) {
        setSession("login");
        setWorkers([]);
        setWorkerNodes([]);
        setWorkersLoaded(false);
      }
      setWorkersError(`Worker inventory: ${errorMessage(err)}`);
    }
  }, []);
  const refresh = useCallback(async () => {
    if (loading.current) return;
    loading.current = true;
    setRefreshing(true);
    const workerRefresh = refreshWorkers();
    try {
      setFleet(await api<Fleet>("/api/control/v1/fleet"));
      setUpdated(new Date());
      setError("");
    } catch (err) {
      if (err instanceof RequestError && err.status === 401) {
        setSession("login");
        setFleet(null);
      }
      setError(errorMessage(err));
    } finally {
      await workerRefresh;
      loading.current = false;
      setRefreshing(false);
    }
  }, [refreshWorkers]);
  const visible = usePageVisible();
  useEffect(() => {
    if (session !== "ready") return;
    void refresh();
  }, [session, refresh]);
  useVisibleInterval(refresh, session === "ready" ? 10_000 : null);
  useEffect(() => {
    if (session !== "ready" || !visible || !jobs.some(activeJob)) return;
    let cancelled = false;
    const timer = setTimeout(async () => {
      const updates = await Promise.all(
        jobs.filter(activeJob).map(async (job) => {
          try {
            const result = await api<{ job: Job }>(`/api/control/v1/jobs/${encodeURIComponent(job.id)}`);
            return { ...job, ...result.job, error: result.job.error };
          } catch (err) {
            return { ...job, error: `Status unavailable: ${errorMessage(err)}` };
          }
        }),
      );
      if (cancelled) return;
      setJobs((current) => current.map((job) => updates.find((update) => update.id === job.id) ?? job));
      if (updates.some((job) => !activeJob(job))) void refresh();
    }, 1500);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [jobs, session, visible, refresh]);

  async function signOut() {
    try {
      await api("/api/session", { method: "DELETE" });
      setSession("login");
      setMe(null);
      setFleet(null);
      ++workerRequest.current;
      setWorkers([]);
      setWorkerNodes([]);
      setWorkersLoaded(false);
      setWorkersError("");
      setJobs([]);
      setPendingInstances([]);
      setError("");
      go("overview");
    } catch (err) {
      setError(errorMessage(err));
    }
  }
  async function queue(path: string, body: object, label: string, host: string, instance: string, kind: Job["kind"] = "workspace") {
    const target = `${host}/${instance}`;
    setPendingInstances((current) => [...current, target]);
    setError("");
    try {
      const { job } = await api<{ job: Job }>(`/api/control/${path}`, { method: "POST", body: JSON.stringify(body) });
      setJobs((current) => [{ ...job, label, host, instance, kind }, ...current].slice(0, 20));
      return true;
    } catch (err) {
      setError(errorMessage(err));
      return false;
    } finally {
      setPendingInstances((current) => current.filter((key) => key !== target));
    }
  }
  async function create(host: string, request: Allocation) {
    setSubmitting(true);
    if (await queue(`v1/hosts/${encodeURIComponent(host)}/allocations`, request, `Create ${request.name}`, host, request.name)) setCreating(false);
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
  function go(next: string, nextTab = "") {
    const resolved = resolve(next, nextTab || null);
    const id = resolved.view;
    setView(id);
    setTab(resolved.tab);
    setStorageFirst(false);
    setNavOpen(false);
    setCreating(false);
    const url = new URL(window.location.href);
    url.search = id === "overview" ? "" : `panel=${id}${resolved.tab ? `&tab=${encodeURIComponent(resolved.tab)}` : ""}`;
    window.history.pushState(null, "", url);
    window.scrollTo({ top: 0 });
  }
  function switchTab(next: string) {
    setTab(next);
    const url = new URL(window.location.href);
    url.search = `panel=${view}&tab=${encodeURIComponent(next)}`;
    window.history.replaceState(null, "", url);
  }
  const templateTab = TEMPLATE_TABS.some((t) => t.id === tab) ? tab : "sandboxes";

  const current = VIEWS.find((v) => v.id === view) ?? VIEWS[0];
  const commands: Command[] = useMemo(
    () => [
      ...VIEWS.map((v) => ({ id: v.id, label: v.title, group: v.group, icon: v.icon, keywords: v.keywords })),
      { id: "templates:toolchains", label: "Build toolchains", group: "Infrastructure", icon: "template", keywords: "ci templates recipes" },
      { id: "projects:previews", label: "Previews & routes", group: "Projects", icon: "globe", keywords: "environments routes dns services dev" },
    ],
    [],
  );
  const online = fleet?.hosts.filter((host) => host.online) ?? [];
  const admittingHosts = fleet ? fleet.hosts.filter((host) => hostAdmitting(fleet, host)) : [];
  const computeMachines = [...(fleet?.compute_nodes ?? []), ...workerNodes].filter((node) => node.registered);
  const onlineMachines = computeMachines.filter((node) => node.online).length;
  const instances = fleet?.hosts.flatMap((host) => host.instances.map((instance) => ({ host, instance }))) ?? [];
  const assigned = instances.filter(({ instance }) => instance.managed && !poolReserve(instance));
  const readyReserve = online.reduce(
    (total, host) => total + (host.warm_pools ?? []).reduce((count, pool) => count + pool.ready, 0),
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
        ? Math.max(0, Math.min(capacity.memory_total_bytes, capacity.memory_total_bytes - capacity.memory_available_bytes))
        : 0)
    );
  }, 0);
  const percent = (used: number, total: number) =>
    total > 0 ? `${Math.min(100, Math.max(0, (used / total) * 100)).toFixed(1)}%` : "—";
  const attentionCount = computeMachines.filter((m) => !m.online).length + workers.filter((w) => w.blocked || (!w.enrolled && !w.revoked)).length;

  if (session !== "ready")
    return (
      <div className="signin-shell">
        <main className="signin-card">
          <div className="brand brand-center">
            <span className="brand-mark">
              <Icon name="box" size={22} />
            </span>
            <span className="brand-word">Engine Box</span>
          </div>
          <h1>Sign in</h1>
          <p className="muted">We email you a one-time code. No password to remember or leak.</p>
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
            <>
              {!emailDelivery && (
                <div className="alert" role="status">
                  Email delivery is not configured on this server, so sign-in codes cannot be sent. Configure the email gateway; see docs/SELF-HOSTING-ORGS.md.
                </div>
              )}
              <EmailSignIn onSignedIn={readSession} />
            </>
          )}
          <p className="signin-foot muted">
            <Icon name="lock" size={13} /> Private control plane. Your session stays in this browser for eight hours.
          </p>
        </main>
      </div>
    );

  return (
    <div className={`app-shell${navOpen ? " nav-open" : ""}`}>
      <aside className="sidebar">
        <div className="sidebar-top">
          <a href="/" className="brand" onClick={(e) => { e.preventDefault(); go("overview"); }}>
            <span className="brand-mark">
              <Icon name="box" size={18} />
            </span>
            <span className="brand-word">Engine Box</span>
          </a>
          <button type="button" className="icon-button nav-toggle" aria-label={navOpen ? "Close navigation" : "Open navigation"} aria-expanded={navOpen} onClick={() => setNavOpen((o) => !o)}>
            <Icon name={navOpen ? "close" : "menu"} />
          </button>
        </div>
        <button type="button" className="org-switch" onClick={() => go("org")} title="Organization settings">
          <span className="org-avatar" aria-hidden="true">{(orgName || "E").slice(0, 1).toUpperCase()}</span>
          <span>
            <strong>{orgName || "Organization"}</strong>
            <small>{me?.role ?? "member"}</small>
          </span>
        </button>
        <button type="button" className="jump" onClick={() => setMenuOpen(true)}>
          <Icon name="search" size={15} />
          <span>Jump to…</span>
          <kbd>⌘K</kbd>
        </button>
        <nav aria-label="Control plane" className="nav">
          {GROUPS.map((group) => (
            <div className="nav-group" key={group}>
              {group !== "Home" && <div className="nav-label">{group}</div>}
              {VIEWS.filter((v) => v.group === group).map((item) => (
                <button
                  key={item.id}
                  type="button"
                  className={`nav-item${view === item.id ? " active" : ""}`}
                  aria-current={view === item.id ? "page" : undefined}
                  onClick={() => go(item.id)}
                  aria-describedby={item.id === "compute" && attentionCount > 0 ? "compute-attention" : undefined}
                >
                  <Icon name={item.icon} size={16} />
                  <span>{item.label}</span>
                  {item.id === "compute" && attentionCount > 0 && (
                    <span className="nav-badge" aria-hidden="true">
                      {attentionCount}
                    </span>
                  )}
                </button>
              ))}
            </div>
          ))}
        </nav>
        {attentionCount > 0 && (
          <span id="compute-attention" hidden>
            {attentionCount} machine{attentionCount === 1 ? "" : "s"} need attention
          </span>
        )}
        <div className="sidebar-bottom">
          <div className="me">
            <span className="me-avatar" aria-hidden="true">{(me?.email ?? "?").slice(0, 1).toUpperCase()}</span>
            <span className="me-text">
              <strong title={me?.email ?? undefined}>{me?.label || me?.email || "Signed in"}</strong>
              <small>{me?.label && me.label !== me.email ? me.email : "Signed in with email"}</small>
            </span>
            <button className="icon-button" onClick={signOut} aria-label="Sign out" title="Sign out">
              <Icon name="logout" size={16} />
            </button>
          </div>
        </div>
      </aside>
      <div className="main-shell">
        <header className="topbar">
          <nav aria-label="Breadcrumb" className="crumbs">
            <span>{current.group}</span>
            <span className="slash" aria-hidden="true">/</span>
            <strong>{current.title}</strong>
          </nav>
          <div className="topbar-right">
            <span className="update-label">
              <span className={`live-dot${error ? " stale" : ""}`} />
              {updated ? `Updated ${updated.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}` : "Connecting"}
            </span>
            <button className="button small" onClick={refresh} disabled={refreshing}>
              <Icon name="refresh" size={14} />
              {refreshing ? "Refreshing" : "Refresh"}
            </button>
          </div>
        </header>
        <main className="content">
          <PageHeader
            title={current.title}
            description={current.description}
            actions={
              view === "compute" ? (
                <button className="button primary" onClick={() => setCreating((value) => !value)} disabled={!fleet || !admittingHosts.length}>
                  <Icon name="plus" size={15} />
                  New sandbox
                </button>
              ) : undefined
            }
          />
          {error && (
            <div className="alert" role="alert">
              {error}
              <button className="text-button" onClick={refresh}>
                Try again
              </button>
            </div>
          )}
          {view === "overview" && <Overview fleet={fleet} machines={computeMachines} workers={workers} jobs={jobs} go={go} />}
          {view === "projects" && <ProjectsPanel fleet={fleet} initialStorage={storageFirst} initialTab={tab} refreshToken={updated?.getTime() ?? 0} />}
          {view === "github" && <GitHubPanel show="repositories" />}
          {view === "agents" && (
            <>
              <AgentsPanel />
              <CredentialsPanel />
            </>
          )}
          {view === "profiles" && <ProfilesPanel />}
          {view === "runners" && <GitHubPanel show="runners" />}
          {view === "templates" && (
            <>
              <Tabs label="Template runtime" tabs={TEMPLATE_TABS} value={templateTab} onChange={switchTab} />
              {templateTab === "sandboxes" && <TemplateCatalog refreshToken={updated?.getTime() ?? 0} registered={refresh} />}
              {templateTab === "toolchains" && <CiTemplates />}
              {templateTab === "runtimes" && <PreviewRuntimes />}
            </>
          )}
          {view === "connections" && <ConnectionsPanel />}
          {view === "apps" && <ConnectedAppsPanel />}
          {view === "org" && <OrgPanel onSwitched={() => window.location.reload()} />}
          {view === "compute" && (
            <>
              <div className="stat-grid">
                {[
                  {
                    label: "Online machines",
                    value: fleet && workersLoaded ? `${onlineMachines}` : "—",
                    note: fleet && workersLoaded ? `${computeMachines.length} registered · ${workersError || error ? "refresh needed" : "servers and workstations"}` : "Loading machine inventories",
                    icon: "server",
                  },
                  {
                    label: "VM CPU allocated",
                    value: fleet ? `${cpuAllocated} / ${cpuTotal}` : "—",
                    note: `${percent(cpuAllocated, cpuTotal)} allocated · VM hosts`,
                    icon: "cpu",
                  },
                  {
                    label: "VM host memory used",
                    value: fleet ? `${bytes(memoryUsed)} / ${bytes(memoryTotal)}` : "—",
                    note: `${percent(memoryUsed, memoryTotal)} used · online VM hosts`,
                    icon: "memory",
                  },
                  {
                    label: "Sandboxes",
                    value: fleet ? `${assigned.length}` : "—",
                    note: `${assigned.filter(({ instance }) => instance.status === "Running").length} running · ${readyReserve} ready in reserve`,
                    icon: "vm",
                  },
                ].map((stat) => (
                  <article className="stat-card" key={stat.label}>
                    <div>
                      <span>{stat.label}</span>
                      <Icon name={stat.icon} size={16} />
                    </div>
                    <strong>{stat.value}</strong>
                    <small>{stat.note}</small>
                  </article>
                ))}
              </div>
              <ComputePanel
                hosts={fleet?.hosts ?? []}
                hostsLoaded={fleet !== null}
                nodes={workers}
                machines={computeMachines}
                loaded={workersLoaded}
                loadError={workersError}
                refresh={refresh}
                hostDetails={(host) => <HostCard host={host} />}
              />
              {creating && fleet && <CreateWorkspace fleet={fleet} busy={submitting} close={() => setCreating(false)} create={create} />}
              <section className="panel workspaces-section">
                <header className="panel-head">
                  <h2>Sandboxes</h2>
                  <span className="count">{instances.length}</span>
                  <span className="section-caption">Agent VMs on these machines · Prepared reserves · Template bases</span>
                </header>
                <div className="table-container">
                  <table>
                    <thead>
                      <tr>
                        <th>Sandbox</th>
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
                            pendingInstances.includes(`${host.id}/${instance.name}`) ||
                            jobs.some(
                              (job) =>
                                activeJob(job) &&
                                job.host === host.id &&
                                (job.instance === instance.name || (!!instance.workspace_name && job.instance === instance.workspace_name)),
                            )
                          }
                          action={action}
                          connect={() => go("agents")}
                        />
                      ))}
                    </tbody>
                  </table>
                  {!instances.length && (
                    <div className="empty-state compact">
                      <Icon name="vm" size={26} />
                      <h3>{fleet ? "No sandboxes yet" : "Waiting for sandbox inventory"}</h3>
                      <p>{fleet ? "Create a sandbox from a prepared template to start building." : "Sandboxes appear when the fleet service responds."}</p>
                    </div>
                  )}
                </div>
                <p className="workspace-hint">Pause keeps memory allocated. Hibernate saves memory to disk. Stop ends running processes.</p>
              </section>
              {!!jobs.length && (
                <section className="panel jobs-section" aria-live="polite">
                  <header className="panel-head">
                    <h2>Recent operations</h2>
                    <span className="section-caption">This browser session</span>
                  </header>
                  <div className="job-list">
                    {jobs.map((job) => (
                      <article className="job" key={job.id}>
                        <span className={`job-icon ${job.status}`}>
                          {activeJob(job) ? <span className="spinner" /> : <Icon name={job.status === "succeeded" ? "check" : "close"} size={16} />}
                        </span>
                        <div>
                          <strong>{job.label ?? job.id}</strong>
                          {job.error && <p className="job-error">{job.error}</p>}
                          {job.status === "succeeded" && job.result?.startup && (
                            <p className="job-detail">
                              {job.result.startup === "warm" ? "Warm allocation" : "Cold start"}
                              {typeof job.result.ready_seconds === "number" &&
                                Number.isFinite(job.result.ready_seconds) &&
                                ` · ready in ${job.result.ready_seconds.toFixed(2)} s`}
                              {job.result.name && <span className="mono">VM: {job.result.name}</span>}
                            </p>
                          )}
                          <small className="mono">{job.id}</small>
                        </div>
                        <span className={`tag status status-${job.status}`} data-tone={activeJob(job) ? "agent" : job.status === "succeeded" ? "ok" : "danger"}>
                          <i className="tag-dot" aria-hidden="true" />
                          {title(job.status)}
                        </span>
                      </article>
                    ))}
                  </div>
                </section>
              )}
            </>
          )}
        </main>
      </div>
      <CommandMenu open={menuOpen} commands={commands} close={() => setMenuOpen(false)} run={(id) => { const [next, nextTab] = id.split(":"); go(next, nextTab); }} />
    </div>
  );
}
