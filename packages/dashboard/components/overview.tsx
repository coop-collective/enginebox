"use client";

import { useCallback, useEffect, useState } from "react";
import { api, errorMessage } from "@/lib/client";
import { useVisibleInterval } from "@/lib/visible-interval";
import { providerLabels } from "@/lib/accounts";
import type { ComputeNode, Fleet, Job, ProviderAccount, TemplateRecord } from "@/lib/types";
import type { WorkspaceCatalog } from "@/lib/workspaces";
import type { Contributor } from "@/components/compute-panel";
import { activeJob, poolReserve, title } from "@/components/fleet";
import { Icon, Meter, Tag, type Tone } from "@/components/ui";

type CiRecipe = { enabled: boolean; manifest: { id: string; label: string }; nodes: { node_id: string; status: string; error: string | null }[] };
type GitHubStatus = {
  configured: boolean;
  runner_app_configured: boolean;
  draining: boolean;
  repositories: { id: number }[];
  pools: { id: string; max_runners: number }[];
  jobs: { id: number; repository: string; status: string; github_status: string; error?: string }[];
};
type Signals = {
  providers?: ProviderAccount[];
  templates?: TemplateRecord[];
  ci?: CiRecipe[];
  github?: GitHubStatus;
  workspaces?: WorkspaceCatalog;
  errors: string[];
};
type Attention = { id: string; tone: Tone; title: string; detail: string; view: string; action: string };

/**
 * The landing view: one row of fleet-wide numbers, what needs a person now,
 * and a compact read of machines, agent accounts and CI. Every list links to
 * the page that fixes it; nothing here mutates state.
 */
export function Overview({
  fleet,
  machines,
  workers,
  jobs,
  go,
}: {
  fleet: Fleet | null;
  machines: ComputeNode[];
  workers: Contributor[];
  jobs: Job[];
  go: (view: string) => void;
}) {
  const [signals, setSignals] = useState<Signals>({ errors: [] });
  const [loaded, setLoaded] = useState(false);
  const load = useCallback(async () => {
    const [providers, templates, ci, github, workspaces] = await Promise.allSettled([
      api<{ providers: ProviderAccount[] }>("/api/control/v1/agents"),
      api<{ templates: TemplateRecord[] }>("/api/control/v1/templates"),
      api<{ templates: CiRecipe[] }>("/api/control/v1/ci/templates"),
      api<GitHubStatus>("/api/control/v1/github"),
      api<WorkspaceCatalog>("/api/control/v1/workspaces"),
    ]);
    const errors: string[] = [];
    const take = <T, R>(result: PromiseSettledResult<T>, label: string, pick: (value: T) => R): R | undefined => {
      if (result.status === "fulfilled") return pick(result.value);
      errors.push(`${label}: ${errorMessage(result.reason)}`);
      return undefined;
    };
    setSignals({
      providers: take(providers, "Agent accounts", (v) => v.providers),
      templates: take(templates, "Templates", (v) => v.templates),
      ci: take(ci, "CI toolchains", (v) => v.templates),
      github: take(github, "GitHub", (v) => v),
      workspaces: take(workspaces, "Workspaces", (v) => v),
      errors,
    });
    setLoaded(true);
  }, []);
  useEffect(() => {
    void load();
  }, [load]);
  useVisibleInterval(() => void load(), 30_000);

  const online = machines.filter((m) => m.online);
  const hosts = fleet?.hosts ?? [];
  const instances = hosts.flatMap((host) => host.instances.map((instance) => ({ host, instance })));
  const assigned = instances.filter(({ instance }) => instance.managed && !poolReserve(instance));
  const running = assigned.filter(({ instance }) => instance.status === "Running").length;
  const providers = Array.isArray(signals.providers) ? signals.providers : [];
  const connected = providers.filter((p) => p.status === "connected");
  const workspaces = Array.isArray(signals.workspaces?.workspaces) ? signals.workspaces.workspaces : [];
  const templates = Array.isArray(signals.templates) ? signals.templates : [];
  const recipes = Array.isArray(signals.ci) ? signals.ci : [];
  const activeRuns = workspaces.flatMap((w) => w.runs ?? []).filter((r) => ["running", "queued"].includes(r.status)).length;
  const ciJobs = Array.isArray(signals.github?.jobs) ? signals.github.jobs : [];
  const pools = Array.isArray(signals.github?.pools) ? signals.github.pools : [];
  const ciActive = ciJobs.filter((j) => !["completed", "succeeded", "failed", "cancelled"].includes(j.status)).length;

  const attention: Attention[] = [];
  for (const machine of machines)
    if (machine.registered && !machine.online)
      attention.push({ id: `m:${machine.id}`, tone: "danger", title: `${machine.label} is offline`, detail: machine.error ?? "The controller has not heard from this machine recently.", view: "compute", action: "Open machines" });
    else if (machine.runtime?.state === "needs_attention")
      attention.push({ id: `m:${machine.id}`, tone: "decal", title: `${machine.label} needs attention`, detail: machine.error ?? "Reconcile the machine's worker state.", view: "compute", action: "Open machines" });
  for (const worker of workers)
    if (!worker.revoked && !worker.enrolled)
      attention.push({ id: `w:${worker.id}`, tone: "decal", title: `Finish registering ${worker.label}`, detail: "The installer has not completed. Renew the code if it expired.", view: "compute", action: "Open machines" });
    else if (worker.blocked)
      attention.push({ id: `w:${worker.id}`, tone: "decal", title: `${worker.label} needs reconciliation`, detail: "Confirm its owned worker stopped before new jobs run.", view: "compute", action: "Open machines" });
  for (const host of hosts)
    for (const pool of host.warm_pools ?? [])
      if (pool.error)
        attention.push({ id: `p:${host.id}/${pool.template}/${pool.size}`, tone: "danger", title: `Warm pool ${pool.template} on ${host.label}`, detail: pool.error, view: "compute", action: "Open sandboxes" });
  for (const provider of providers)
    if (provider.installed && provider.status !== "connected" && provider.status !== "unavailable")
      attention.push({ id: `a:${provider.id}`, tone: "decal", title: `Connect ${providerLabels[provider.id]}`, detail: "Agents cannot use this provider until the controller signs in.", view: "agents", action: "Connect" });
    else if (provider.delegation_status === "reauth_required")
      attention.push({ id: `a:${provider.id}`, tone: "decal", title: `Reconnect ${providerLabels[provider.id]}`, detail: "Sandbox access needs a fresh sign-in.", view: "agents", action: "Reconnect" });
  for (const template of templates)
    for (const node of template.sync?.nodes ?? [])
      if (node.status === "failed")
        attention.push({ id: `t:${template.id}/${node.host_id}`, tone: "danger", title: `${template.label} failed on ${node.label}`, detail: node.error ?? "Template synchronization failed.", view: "templates", action: "Retry sync" });
  for (const recipe of recipes)
    for (const node of recipe.nodes ?? [])
      if (recipe.enabled && node.status === "failed")
        attention.push({ id: `c:${recipe.manifest.id}/${node.node_id}`, tone: "danger", title: `${recipe.manifest.label} failed on ${node.node_id}`, detail: node.error?.replaceAll("-", " ") ?? "Toolchain installation failed.", view: "toolchains", action: "Retry" });
  if (signals.github?.draining)
    attention.push({ id: "gh:drain", tone: "decal", title: "Runner provisioning is paused", detail: "Active runners finish their jobs; no new runners start.", view: "runners", action: "Resume" });
  for (const job of ciJobs.filter((j) => j.error).slice(0, 3))
    attention.push({ id: `gj:${job.id}`, tone: "danger", title: `CI job ${job.id} on ${job.repository}`, detail: job.error!, view: "runners", action: "Open runners" });
  for (const job of jobs.filter((j) => j.status === "failed").slice(0, 3))
    attention.push({ id: `j:${job.id}`, tone: "danger", title: job.label ?? `Operation ${job.id}`, detail: job.error ?? "The operation failed.", view: "compute", action: "Open sandboxes" });

  const stats: { label: string; value: string; note: string; view: string; icon: string }[] = [
    {
      label: "Machines online",
      value: fleet ? `${online.length}` : "—",
      note: `${machines.filter((m) => m.registered).length} registered`,
      view: "compute",
      icon: "server",
    },
    {
      label: "Agent accounts",
      value: signals.providers ? `${connected.length} / ${providers.length}` : "—",
      note: signals.providers ? `${activeRuns} agent run${activeRuns === 1 ? "" : "s"} active` : "Reading accounts",
      view: "agents",
      icon: "agent",
    },
    {
      label: "Projects",
      value: signals.workspaces ? `${workspaces.length}` : "—",
      note: `${running} sandbox${running === 1 ? "" : "es"} running · ${assigned.length} assigned`,
      view: "projects",
      icon: "folder",
    },
    {
      label: "CI runners",
      value: signals.github ? `${pools.length}` : "—",
      note: signals.github
        ? signals.github.configured
          ? `${ciActive} job${ciActive === 1 ? "" : "s"} in flight`
          : "GitHub not configured"
        : "Reading GitHub",
      view: "runners",
      icon: "checks",
    },
  ];

  return (
    <div className="overview">
      <div className="kpi-row">
        {stats.map((stat) => (
          <button key={stat.label} type="button" className="kpi" onClick={() => go(stat.view)}>
            <span className="kpi-label">
              <Icon name={stat.icon} size={16} />
              {stat.label}
            </span>
            <strong>{stat.value}</strong>
            <small>{stat.note}</small>
          </button>
        ))}
      </div>

      <div className="overview-grid">
        <section className="panel" aria-labelledby="attention-title">
          <header className="panel-head">
            <h2 id="attention-title">Needs attention</h2>
            <span className="count">{attention.length}</span>
          </header>
          {!loaded && !fleet ? (
            <p className="muted pad">Reading fleet signals…</p>
          ) : attention.length ? (
            <ul className="attention-list">
              {attention.slice(0, 12).map((item) => (
                <li key={item.id}>
                  <Tag tone={item.tone}>{item.title}</Tag>
                  <p>{item.detail}</p>
                  <button type="button" className="button small" onClick={() => go(item.view)}>
                    {item.action}
                  </button>
                </li>
              ))}
            </ul>
          ) : (
            <div className="all-clear">
              <Icon name="check" size={20} />
              <div>
                <strong>Nothing needs you</strong>
                <p>Machines, agent accounts, templates and CI report no problems.</p>
              </div>
            </div>
          )}
          {!!signals.errors.length && (
            <details className="signal-errors">
              <summary>{signals.errors.length} source{signals.errors.length === 1 ? "" : "s"} could not be read</summary>
              <ul>
                {signals.errors.map((e) => (
                  <li key={e}>{e}</li>
                ))}
              </ul>
            </details>
          )}
        </section>

        <section className="panel" aria-labelledby="machines-title">
          <header className="panel-head">
            <h2 id="machines-title">Machines</h2>
            <button type="button" className="text-button" onClick={() => go("compute")}>
              View all
            </button>
          </header>
          {machines.filter((m) => m.registered).length ? (
            <ul className="machine-list">
              {machines
                .filter((m) => m.registered)
                .slice(0, 6)
                .map((machine) => {
                  const host = hosts.find((h) => h.id === machine.source_id);
                  const cpu = host?.capacity;
                  return (
                    <li key={machine.id}>
                      <div>
                        <strong>{machine.label}</strong>
                        <span className="muted">
                          {machine.platform} · {machine.backend}
                        </span>
                      </div>
                      <Tag tone={machine.online ? (machine.runtime?.state === "busy" ? "agent" : "ok") : "danger"} live={machine.runtime?.state === "busy"}>
                        {machine.online ? title((machine.runtime?.state ?? "unknown").replaceAll("_", " ")) : "Offline"}
                      </Tag>
                      {cpu ? (
                        <Meter used={cpu.allocated_cpu} total={cpu.cpu_total} label={`${machine.label} CPU ${cpu.allocated_cpu} of ${cpu.cpu_total} allocated`} />
                      ) : (
                        <span className="muted mono">{machine.runtime?.running_workspaces ?? 0} running</span>
                      )}
                    </li>
                  );
                })}
            </ul>
          ) : (
            <div className="empty-inline">
              <p>No machines registered yet.</p>
              <button type="button" className="button small" onClick={() => go("compute")}>
                Connect a machine
              </button>
            </div>
          )}
        </section>

        <section className="panel" aria-labelledby="agents-overview-title">
          <header className="panel-head">
            <h2 id="agents-overview-title">Agent accounts</h2>
            <button type="button" className="text-button" onClick={() => go("agents")}>
              Manage
            </button>
          </header>
          {signals.providers ? (
            <ul className="machine-list">
              {providers.map((p) => (
                <li key={p.id}>
                  <div>
                    <strong>{providerLabels[p.id]}</strong>
                    <span className="muted">{p.auth_method === "api_key" ? "API key" : p.auth_method === "subscription" ? "Subscription" : "Native sign-in"}</span>
                  </div>
                  <Tag tone={p.status === "connected" ? (p.delegation_status === "reauth_required" ? "decal" : "ok") : p.status === "unavailable" ? "neutral" : "decal"}>
                    {p.status === "connected" ? (p.delegation_status === "reauth_required" ? "Reconnect" : "Connected") : p.status === "unavailable" ? "Unavailable" : "Not connected"}
                  </Tag>
                  <span className="muted">{p.workspace_count != null ? `${p.workspace_count} sandboxes` : ""}</span>
                </li>
              ))}
            </ul>
          ) : (
            <p className="muted pad">{loaded ? "Agent accounts are unavailable." : "Reading agent accounts…"}</p>
          )}
        </section>

        <section className="panel" aria-labelledby="ci-overview-title">
          <header className="panel-head">
            <h2 id="ci-overview-title">CI</h2>
            <button type="button" className="text-button" onClick={() => go("runners")}>
              Runners
            </button>
          </header>
          {signals.github?.configured || recipes.length > 0 ? (
            <ul className="machine-list">
              {pools.map((pool) => (
                <li key={pool.id}>
                  <div>
                    <strong>{pool.id}</strong>
                    <span className="muted">Runner pool · up to {pool.max_runners}</span>
                  </div>
                  <Tag tone={signals.github!.draining ? "decal" : "ok"}>{signals.github!.draining ? "Draining" : "Provisioning"}</Tag>
                  <span />
                </li>
              ))}
              {recipes.map((recipe) => {
                const nodes = recipe.nodes ?? [];
                const ready = nodes.filter((n) => n.status === "ready").length;
                return (
                  <li key={recipe.manifest.id}>
                    <div>
                      <strong>{recipe.manifest.label}</strong>
                      <span className="muted">Toolchain · {ready} / {nodes.length} nodes ready</span>
                    </div>
                    <Tag tone={!recipe.enabled ? "neutral" : nodes.some((n) => n.status === "failed") ? "danger" : "ok"}>{recipe.enabled ? "Enabled" : "Disabled"}</Tag>
                    <span />
                  </li>
                );
              })}
            </ul>
          ) : (
            <div className="empty-inline">
              <p>Run GitHub Actions on your own machines with prepared LayerFS checkouts.</p>
              <button type="button" className="button small" onClick={() => go("runners")}>
                Set up runners
              </button>
            </div>
          )}
        </section>
      </div>

      {!!jobs.length && (
        <section className="panel" aria-live="polite" aria-labelledby="ops-title">
          <header className="panel-head">
            <h2 id="ops-title">Recent operations</h2>
            <span className="muted">This browser session</span>
          </header>
          <ul className="machine-list">
            {jobs.slice(0, 6).map((job) => (
              <li key={job.id}>
                <div>
                  <strong>{job.label ?? job.id}</strong>
                  <span className="muted mono">{job.id}</span>
                </div>
                <Tag tone={activeJob(job) ? "agent" : job.status === "succeeded" ? "ok" : "danger"} live={activeJob(job)}>
                  {title(job.status)}
                </Tag>
                <span className="muted">{job.error ?? ""}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
