"use client";
import { FormEvent, useCallback, useEffect, useState } from "react";
import { api, errorMessage } from "@/lib/client";
import { useVisibleInterval } from "@/lib/visible-interval";
import type { Fleet } from "@/lib/types";
import { StoragePanel } from "@/components/storage-panel";
import { EnvironmentsPanel } from "@/components/environments-panel";
import { Tabs } from "@/components/ui";
import { duration, sourceBytes, mergeWorkspaces, type WorkspaceCatalog, type Workspace, type EnvironmentCatalog, type Metrics } from "@/lib/workspaces";

const base = "/api/control/v1";
type GitHub = { repositories?: { id: number; full_name: string; branch: string; private: boolean }[] };
const PROJECT_TABS = ["agents", "code", "previews", "activity"] as const;
type ProjectTab = (typeof PROJECT_TABS)[number];
const LIST_TABS = ["projects", "previews"] as const;
type ListTab = (typeof LIST_TABS)[number];

function Timings({ metrics }: { metrics?: Metrics }) {
  if (!metrics || !Object.keys(metrics).length) return <p>No preparation measurements reported yet.</p>;
  return <div className="workspace-receipts">{Object.entries(metrics).map(([stage, receipt]) => <article className="host-card" key={stage}>
    <strong>{{ preparation: "Source preparation", preparation_reuse: "Prepared revision reuse", startup: "Development startup", source_update: "Source update" }[stage] ?? stage}</strong>
    <p>{receipt.status} · {duration(receipt.elapsed_ms)} · {new Date(receipt.observed_at).toLocaleString()}</p>
    {stage === "preparation" && <p>Source copy {duration(receipt.source_copy_ms)} · Configured preparation command {duration(receipt.prepare_ms)}</p>}
    {"source_bytes" in receipt && <p>Logical source size {sourceBytes(receipt.source_bytes)}</p>}
    <small>Revision {receipt.revision.slice(0, 12)}</small>
  </article>)}</div>;
}

/**
 * Projects: the one place for everything a project owns. The API still calls
 * these "workspaces"; in the dashboard a workspace VM is a sandbox.
 */
export function ProjectsPanel({ fleet, initialStorage = false, initialTab = "", refreshToken = 0 }: { fleet: Fleet | null; initialStorage?: boolean; initialTab?: string; refreshToken?: number }) {
  const [catalog, setCatalog] = useState<WorkspaceCatalog | null>(null);
  const [environments, setEnvironments] = useState<EnvironmentCatalog | null>(null);
  const [github, setGithub] = useState<GitHub>({});
  const [hosted, setHosted] = useState(false);
  const [selected, setSelected] = useState("");
  const [tab, setTab] = useState<ProjectTab>("agents");
  const [listTab, setListTab] = useState<ListTab>("projects");
  const [creating, setCreating] = useState(false);
  const [storageOpen, setStorageOpen] = useState(initialStorage);
  const [error, setError] = useState("");
  const [environmentError, setEnvironmentError] = useState("");
  const [sourceError, setSourceError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);
  const [project, setProject] = useState("");
  const [name, setName] = useState("main");
  const [sourceKind, setSourceKind] = useState("hosted");
  const [githubRepo, setGithubRepo] = useState("");
  const [pool, setPool] = useState("");
  const [provider, setProvider] = useState("codex");
  const [prompt, setPrompt] = useState("");
  const [runRepository, setRunRepository] = useState("");
  const refresh = useCallback(async () => {
    const results = await Promise.allSettled([
      api<WorkspaceCatalog>(`${base}/workspaces`), api<EnvironmentCatalog>("/api/environments"),
      api<GitHub>(`${base}/github`), api<{ backend: { configured: boolean } }>(`${base}/repository-backends`),
    ]);
    if (results[0].status === "fulfilled") { setCatalog(results[0].value); setError(""); } else setError(errorMessage(results[0].reason));
    if (results[1].status === "fulfilled") { setEnvironments(results[1].value); setEnvironmentError(""); } else setEnvironmentError(`Preview observations unavailable: ${errorMessage(results[1].reason)}`);
    if (results[2].status === "fulfilled") setGithub(results[2].value);
    if (results[3].status === "fulfilled") setHosted(results[3].value.backend.configured);
    setSourceError(results.slice(2).some(r => r.status === "rejected") ? "Source connections could not be refreshed. Retry before creating or linking a source." : "");
  }, []);
  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const id = params.get("project") ?? params.get("workspace") ?? "";
    const storage = params.get("storage") === "1" || initialStorage;
    setSelected(id);
    if (storage) setStorageOpen(true);
    if (id) setTab(storage ? "code" : (PROJECT_TABS as readonly string[]).includes(initialTab) ? initialTab as ProjectTab : "agents");
    else if ((LIST_TABS as readonly string[]).includes(initialTab)) setListTab(initialTab as ListTab);
    void refresh();
  }, [refresh, initialStorage, initialTab]);
  useVisibleInterval(() => { void refresh(); }, 15000);
  const all = mergeWorkspaces(catalog, environments);
  const workspace = all.find(w => w.id === selected);
  const unmapped = github.repositories?.filter(repo => !all.some(w => w.repositories.some(source => source.remote === `https://github.com/${repo.full_name}.git`) || w.source_observations.some(source => source.repository === repo.full_name))) ?? [];
  function writeUrl(id: string, nextTab: string) {
    const url = new URL(window.location.href);
    url.search = id ? `panel=projects&workspace=${encodeURIComponent(id)}&tab=${nextTab}` : `panel=projects${nextTab && nextTab !== "projects" ? `&tab=${nextTab}` : ""}`;
    window.history.replaceState(null, "", url);
  }
  function open(id: string, nextTab: ProjectTab = "agents") { setSelected(id); setStorageOpen(false); setProject(id); setCreating(false); setPool(""); setRunRepository(""); setNotice(""); setTab(nextTab); writeUrl(id, id ? nextTab : listTab); }
  function switchTab(next: string) { setTab(next as ProjectTab); writeUrl(selected, next); }
  function switchListTab(next: string) { setListTab(next as ListTab); writeUrl("", next); }
  async function create(event: FormEvent) {
    event.preventDefault(); setBusy(true); setNotice(""); setError("");
    try {
      const target = (workspace?.id ?? project).trim();
      if (!/^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$/.test(target)) throw new Error("Project IDs use lowercase letters, digits and dashes, up to 48 characters.");
      await api(`${base}/projects/${target}/repositories${sourceKind === "github" ? "/github" : ""}`, { method: "POST", body: JSON.stringify(sourceKind === "github" ? { name, full_name: githubRepo } : { name }) });
      await refresh(); open(target, "code"); setNotice("Source connected. Review and prepare it in LayerFS before agents or previews use it.");
    } catch (e) { setError(errorMessage(e)); } finally { setBusy(false); }
  }
  async function run(event: FormEvent) {
    event.preventDefault(); setBusy(true); setError(""); setNotice("");
    try {
      const result = await api<{ run: { id: string } }>(`${base}/pools/${encodeURIComponent(pool)}/agent-runs`, { method: "POST", body: JSON.stringify({ provider, prompt, ...(runRepository ? { repository: runRepository } : {}) }) });
      setNotice(`Agent run ${result.run.id} submitted. Its result still requires review and acceptance.`); setPrompt(""); await refresh();
    } catch (e) { setError(`Run submission was not confirmed: ${errorMessage(e)} Inspect recent runs before trying again.`); } finally { setBusy(false); }
  }
  const devProjects = environments?.projects?.filter(p => p.project === workspace?.id) ?? [];
  const devEnvironments = environments?.environments.filter(e => e.source.project === workspace?.id) ?? [];
  const members = [...new Map((workspace?.pools ?? []).flatMap(p => p.member_status ?? []).map(m => [`${m.host_id}/${m.name}`, m])).values()];
  const observedMembers = members.map(m => ({ ...m, host: fleet?.hosts.find(h => h.id === m.host_id), instance: fleet?.hosts.find(h => h.id === m.host_id)?.instances.find(i => i.name === m.name) }));
  const knownMembers = observedMembers.filter(m => m.host?.online && m.instance);
  const activeMembers = knownMembers.filter(m => m.instance?.status.toLowerCase() === "running").length;
  function state(w: Workspace) { return environments?.projects?.some(p => p.project === w.id && p.healthy) || environments?.environments.some(e => e.source.project === w.id && e.healthy) ? "Preview serving" : w.source_observations.length || w.checkouts.length ? "LayerFS source observed" : w.repositories.length ? "Source connected · preparation needed" : "Preview or storage registered"; }
  const activeRuns = (w: Workspace) => w.runs.filter(r => ["running", "queued"].includes(r.status)).length;

  const sourceForm = (creating || workspace) && <details className="host-card" open={creating}><summary>{workspace ? "Add source" : "Create project source"}</summary><form className="template-import" onSubmit={create} aria-label="Project source">
    {!workspace && <label>Project ID<input required value={project} onChange={e => setProject(e.target.value)} placeholder="my-project" /></label>}
    <label>Source type<select value={sourceKind} onChange={e => setSourceKind(e.target.value)}><option value="hosted">New hosted repository</option><option value="github">Existing GitHub repository</option></select></label>
    <label>Source name<input required pattern="[a-z0-9][a-z0-9._-]{0,62}" value={name} onChange={e => setName(e.target.value)} /></label>
    {sourceKind === "github" && <label>Selected GitHub repository<select required value={githubRepo} onChange={e => setGithubRepo(e.target.value)}><option value="">Choose an approved repository</option>{github.repositories?.map(r => <option key={r.id} value={r.full_name}>{r.full_name}{r.private ? " (private)" : ""} · {r.branch}</option>)}</select></label>}
    {sourceKind === "hosted" && !hosted && <p>Hosted repository service is not configured. You can link an approved GitHub source.</p>}
    {sourceKind === "github" && !github.repositories?.length && <p>Connect GitHub under Settings → GitHub and select repositories with the same App installation.</p>}
    <button className="button primary" disabled={busy || !!sourceError || (sourceKind === "hosted" ? !hosted : !githubRepo)}>{sourceKind === "hosted" ? "Create hosted source" : "Link GitHub source"}</button>
  </form></details>;

  return <section aria-label="Projects" className="contributor-panel workspaces-panel">
    <div className="contributor-heading"><div><h2>{workspace ? workspace.id : creating ? "New project" : "All projects"}</h2><p>{workspace ? state(workspace) : "A project owns its LayerFS files and accepted main, its links to GitHub or hosted Git, its agent runs and its previews."}</p></div><div className="github-actions"><button className="button small" onClick={() => void refresh()}>Refresh projects</button>{!workspace && !creating && listTab === "projects" && <button className="button small" onClick={() => setStorageOpen(!storageOpen)}>{storageOpen ? "Close source and storage" : "Source and storage"}</button>}<button className="button primary" onClick={() => { open(""); setProject(""); setCreating(true); }}>New project</button></div></div>
    {error && <p className="alert" role="alert">{error}</p>}{environmentError && <p role="status">{environmentError}</p>}{sourceError && <p role="status">{sourceError}</p>}{notice && <p role="status">{notice}</p>}
    {(workspace || creating) && <button className="text-button back-link" onClick={() => open("")}>← All projects</button>}
    {selected && catalog && !workspace && <p role="status">Project {selected} was not found in the available observations.</p>}

    {!workspace && !creating && <Tabs label="Projects view" tabs={[{ id: "projects", label: "Projects", count: all.length }, { id: "previews", label: "Previews & routes" }]} value={listTab} onChange={switchListTab} />}
    {!workspace && !creating && listTab === "projects" && <>
      <div className="host-grid">{all.map(w => <article className="host-card" key={w.id}><h3><button className="text-button" onClick={() => open(w.id)}>{w.id}</button></h3><p>{state(w)}</p><p>{w.repositories.length} sources · {w.pools.length} agent pools · {activeRuns(w)} active runs</p><p>{w.repositories.map(r => r.backend === "github" ? "GitHub" : "Hosted Git").join(" · ") || "LayerFS / preview"}</p><button className="button small" onClick={() => open(w.id)}>Open project {w.id}</button></article>)}{catalog && !all.length && <p>No projects yet. Create a hosted source or link a selected GitHub repository.</p>}{!catalog && !error && <p>Loading projects…</p>}</div>
      {!!unmapped.length && <div><h3>GitHub repositories not linked to a project</h3><div className="host-grid">{unmapped.map(repo => <article className="host-card" key={repo.id}><h3>{repo.full_name}</h3><p>{repo.private ? "Private" : "Public"} · {repo.branch} · Selected, not linked</p><button className="button small" onClick={() => { open(""); setCreating(true); setProject(""); setSourceKind("github"); setGithubRepo(repo.full_name); setName("main"); }}>Link {repo.full_name} to a project</button></article>)}</div></div>}
      {!selected && storageOpen && <StoragePanel refreshToken={refreshToken} />}
    </>}
    {!workspace && !creating && listTab === "previews" && <EnvironmentsPanel />}
    {!workspace && creating && sourceForm}

    {workspace && <>
      <Tabs label="Project sections" tabs={[
        { id: "agents", label: "Agents", count: activeRuns(workspace) || undefined },
        { id: "code", label: "Code", count: workspace.repositories.length || undefined },
        { id: "previews", label: "Previews", count: devProjects.length + devEnvironments.length || undefined },
        { id: "activity", label: "Activity" },
      ]} value={tab} onChange={switchTab} />

      {tab === "agents" && <>
        <h3>Sandboxes</h3>
        <p>{members.length} agent sandboxes allocated · {members.filter(m => m.busy).length} working or reserved · {activeMembers} observed running{knownMembers.length < members.length ? ` · ${members.length - knownMembers.length} runtime states unknown` : ""}.</p>
        {observedMembers.map(m => <p key={`${m.host_id}/${m.name}`}><strong>{m.name}</strong> on {m.host_id} · {m.busy ? "Working / reserved" : "Available to pool"} · {m.host?.online ? m.instance?.status ?? "Not observed" : "Host unavailable"}</p>)}
        <h3>Run an agent</h3>{!workspace.pools.length ? <p>No agent pool is assigned to this project. Assign a project pool before submitting runs.</p> : <form className="template-import" onSubmit={run} aria-label="Run project agent"><label>Agent pool<select required value={pool} onChange={e => setPool(e.target.value)}><option value="">Choose a pool</option>{workspace.pools.map(p => <option key={p.name}>{p.name}</option>)}</select></label><label>Native provider<select value={provider} onChange={e => setProvider(e.target.value)}><option value="codex">Codex</option><option value="claude">Claude</option><option value="cursor">Cursor</option></select></label><label>Repository access<select value={runRepository} onChange={e => setRunRepository(e.target.value)}><option value="">Use the pool's prepared source</option>{workspace.repositories.map(r => <option key={r.name} value={`${workspace.id}/${r.name}`}>{r.name} (Git compatibility access)</option>)}</select></label><label>Task<textarea required maxLength={6000} value={prompt} onChange={e => setPrompt(e.target.value)} /></label><p>The native harness owns the task. Repository access delegates a short-lived Git credential; it does not prepare source or accept changes.</p><button className="button primary" disabled={busy || !pool || !prompt.trim()}>Submit agent run</button></form>}
        <h3>Recent runs</h3>{!workspace.runs.length && <p>No attributed runs in retained history.</p>}{workspace.runs.map(r => <article className="host-card" key={r.id}><strong>{r.provider} · {r.status}</strong><p>{r.pool ?? r.name} · Run {r.id}</p><p>Total run time {duration(r.elapsed_seconds == null ? null : r.elapsed_seconds * 1000)}{r.execution_uncertain ? " · Execution needs reconciliation" : ""}</p></article>)}
      </>}

      {tab === "code" && <>
        <p>LayerFS owns this project&apos;s files, branches and accepted main. GitHub and hosted Git are import and export links; connecting one does not materialize a LayerFS checkout.</p>
        <div className="host-grid">{workspace.repositories.map(repo => <article className="host-card" key={repo.name}><h3>{repo.name}</h3><p>{repo.backend === "github" ? "GitHub" : "Hosted Git"} · {repo.default_branch}</p><code className="workspace-remote">{repo.remote}</code>{repo.backend === "github" && <p><a className="button small" href={`/?panel=projects&storage=1&workspace=${encodeURIComponent(workspace.id)}${github.repositories?.find(r => repo.remote === `https://github.com/${r.full_name}.git`) ? `&github_repository=${github.repositories.find(r => repo.remote === `https://github.com/${r.full_name}.git`)!.id}` : ""}`}>Review and prepare source</a></p>}</article>)}</div>
        {sourceForm}
        <h3>Checkouts and integration</h3>{workspace.checkouts.map(c => <p key={`${c.host_id}/${c.name}`}>{c.name} · {c.status} · {c.host_id} · observed {new Date(c.observed_at).toLocaleString()}{c.last_checkpoint && ` · Checkpoint ${c.last_checkpoint.head ?? c.last_checkpoint.kind ?? "Recorded"}`}</p>)}{workspace.integration_observations.map(s => <p key={s.host_id}>Accepted main {s.main?.revision ?? "Not reported"} · GitHub export {s.sync_status ?? (s.sync_enabled === false ? "disabled" : "not reported")} · {s.host_id} · observed {new Date(s.observed_at).toLocaleString()}</p>)}<p>Successful runs and checkpoints are not proof of a merge. Agents cannot approve their own results.</p>
        {storageOpen ? <StoragePanel key={workspace.id} projectId={workspace.id} repository={workspace.repositories.find(repo => repo.backend === "github")?.remote.match(/^https:\/\/github\.com\/(.+)\.git$/)?.[1]} refreshToken={refreshToken} /> : <p><button className="button" onClick={() => setStorageOpen(true)}>Open source, storage and promotion</button></p>}
      </>}

      {tab === "previews" && <>
        <p>Previews run the project&apos;s accepted code from the preview runtime its repository declares in <code>.enginebox/config.json</code>, and expose its ports as routes.</p>
        {environmentError || !environments ? <p>Preview state unavailable.</p> : !devProjects.length && !devEnvironments.length && <p>No previews for this project yet.</p>}
        {devProjects.map(p => <article className="host-card" key={p.id}><strong>Preview · {p.project} on {p.host}</strong><p>{p.status} · {p.healthy ? "Serving" : "Not serving"}</p>{p.urls.filter(url => /^https?:\/\//.test(url)).map(url => <p key={url}><a href={url} target="_blank" rel="noopener noreferrer">Open preview</a></p>)}<Timings metrics={p.metrics} /></article>)}
        {devEnvironments.filter(e => !devProjects.some(p => p.metrics && JSON.stringify(p.metrics) === JSON.stringify(e.metrics))).map(e => <article className="host-card" key={e.id}><strong>{e.id}</strong><p>{e.status} · {e.healthy ? "Serving" : "Not serving"}</p><Timings metrics={e.metrics} /></article>)}
        <p><button className="button small" onClick={() => open("")}>Manage routes for all previews</button></p>
      </>}

      {tab === "activity" && <>
        <h3>Storage, reuse and command timings</h3><p>Source sizes are logical file bytes. Preparation reuse confirms a matching prepared revision; it is not a dependency-cache hit rate. Command timings cover explicitly measured native executor calls. Agent run durations are not test timings.</p>
        {!workspace.storage_usage.length && <p>Disk usage has not been observed. Open source and storage on the Code tab and refresh this host to collect a bounded usage sample.</p>}
        {workspace.storage_usage.map(usage => <article className="host-card" key={usage.host_id}>
          <strong>Storage on {usage.host_id}</strong><p>{usage.status} · observed {new Date(usage.observed_at_unix_ms).toLocaleString()}</p>
          {usage.projects.map(p => <p key={p.project}>Project Store {p.project} · {p.status} · Logical file size {sourceBytes(p.apparent_bytes)} · Allocated blocks {sourceBytes(p.allocated_bytes)} · {p.files ?? "Unknown"} files</p>)}
          {usage.shared_caches.map(cache => <p key={cache.kind}>Shared node cache {cache.kind} · {cache.status} · Logical file size {sourceBytes(cache.apparent_bytes)} · Allocated blocks {sourceBytes(cache.allocated_bytes)} · {cache.files ?? "Unknown"} files</p>)}
          <small>Node caches are shared across projects; these are not this project&apos;s exclusive bytes. Partial measurements are lower bounds. Allocated blocks may share copy-on-write extents, so they are not unique physical disk usage. Samples may be cached for five minutes.</small>
        </article>)}
        {workspace.checkouts.flatMap(checkout => (checkout.command_timings ?? []).map(receipt => <p key={`${checkout.host_id}/${checkout.name}/${receipt.kind}`}><strong>{receipt.kind} command</strong> · {receipt.status} · {duration(receipt.elapsed_ms)} · {checkout.name} on {checkout.host_id} · observed {new Date(receipt.observed_at_unix_ms).toLocaleString()}</p>))}
        {!workspace.checkouts.some(checkout => checkout.command_timings?.length) && <p>No explicitly measured test/build commands reported yet. Native executor calls can opt into a timing category; arbitrary harness commands are not automatically classified.</p>}
        {workspace.operations.length === 0 && <p>No project-attributed storage operations measured yet.</p>}{workspace.operations.map(op => <p key={op.id}><strong>{op.operation}</strong> · {op.status} · {duration(op.duration_seconds == null ? null : op.duration_seconds * 1000)} · {op.host_id} · {new Date(op.created_at).toLocaleString()}{op.bytes != null && ` · Source ${sourceBytes(op.bytes)}`}</p>)}
        {workspace.launch_events.slice(-10).reverse().map((e, i) => <p key={`${e.at}-${i}`}>Sandbox {e.stage} · {e.outcome} · {duration(e.duration_ms)}{e.queue_wait_ms != null && ` · Queue ${duration(e.queue_wait_ms)}`} · {new Date(e.at).toLocaleString()}</p>)}
        <p className="workspace-hint">History covers up to {catalog?.coverage.run_history_limit ?? 256} recent native runs and {catalog?.coverage.operations_per_workspace_limit ?? 30} operations per project from the controller&apos;s retained {catalog?.coverage.operation_history_limit ?? 200} ordinary jobs.{catalog?.coverage.runs_truncated ? " Older runs are omitted." : ""}{catalog?.coverage.unattributed_runs ? ` ${catalog.coverage.unattributed_runs} retained runs lack reliable project attribution.` : ""} Storage inventory is the latest successful list receipt, not a live scan.</p>
      </>}
    </>}
  </section>;
}
