"use client";
import { useEffect, useState, type FormEvent } from "react";
import { sourcePreview, type Checkout, type StorageTask } from "@/lib/storage";
import type { Job } from "@/lib/types";

type Generation = { project: string; commit: string };
export type GitHubProject = { id: string; repository: string; branch: string; current: Generation | null };
type Run = (key: string, operation: string, body: object, existing?: Job) => Promise<Record<string, unknown> | null>;
export function githubProjects(value: unknown): GitHubProject[] {
  if (!value || typeof value !== "object" || !("github_projects" in value)) return [];
  const projects = value.github_projects;
  return Array.isArray(projects) ? projects.filter((p) => p && typeof p.id === "string" && typeof p.repository === "string" && typeof p.branch === "string" && (!p.current || typeof p.current.project === "string" && typeof p.current.commit === "string")) : [];
}
function Status({ task, run, taskKey }: { task?: StorageTask; run: Run; taskKey: string }) {
  if (!task) return null;
  return <div className="storage-task" aria-live="polite">
    {task.busy ? <p>Working · {task.job?.status ?? "submitting"}</p> : task.error ? <p className="account-error" role="alert">{task.error}</p> : <p className="catalog-notice">Completed.</p>}
    {task.uncertain && task.job && <button className="button small" onClick={() => void run(taskKey, task.operation, {}, task.job)}>Check sync job status</button>}
  </div>;
}
export function GitHubStorage({ projects, rows, tasks, run, refresh }: { projects: GitHubProject[]; rows: Checkout[]; tasks: Record<string, StorageTask>; run: Run; refresh: () => Promise<void> }) {
  const [connected, setConnected] = useState<{ id: number; full_name: string; branch: string }[]>([]);
  useEffect(() => {
    let active = true;
    void fetch("/api/control/v1/github", { signal: AbortSignal.timeout(10000) }).then(async response => {
      if (!response.ok) return;
      const status = await response.json();
      if (!active || !Array.isArray(status.repositories)) return;
      setConnected(status.repositories);
      const params = new URLSearchParams(window.location.search);
      const workspace = params.get("workspace");
      if (workspace && /^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$/.test(workspace)) setProject(workspace);
      const id = params.get("github_repository");
      const selected = status.repositories.find((r: {id: number}) => String(r.id) === id);
      if (selected) { setRepository(selected.full_name); setBranch(selected.branch); }
    }).catch(() => {});
    return () => { active = false; };
  }, []);
  const [project, setProject] = useState("");
  const [repository, setRepository] = useState("");
  const [branch, setBranch] = useState("");
  const [reviewed, setReviewed] = useState(false);
  const [context, setContext] = useState("");
  const [selected, setSelected] = useState("");
  const [checkout, setCheckout] = useState("");
  const [promote, setPromote] = useState("");
  const key = `github:${selected}`;
  const settingsKey = `${key}:settings`;
  const operationKey = `${key}:operation`;
  const devKey = `${key}:dev`;
  const dev = tasks[devKey]?.result as { status?: string; error?: string; urls?: string[] } | undefined;
  const importedDev = tasks["github:import"]?.result?.dev as { status?: string; error?: string } | undefined;
  const settings = tasks[settingsKey]?.result as { main?: { revision: string }; sync?: { enabled: boolean; last_synced_commit?: string; active?: { id: string; status: string; conflicts: string[] } } } | undefined;
  const busy = (k: string) => !!tasks[k] && (tasks[k].busy || !!tasks[k].uncertain);
  const preview = context === `${project}\0${repository}\0${branch}` ? sourcePreview(tasks["github:preview"]?.result) : null;
  const linked = projects.filter((p) => p.current);
  const current = linked.find((p) => p.id === selected);
  const closed = rows.filter((r) => r.github_project === selected && r.project === current?.current?.project && r.status === "closed" && r.last_checkpoint);
  const attempt = settings?.sync?.active;
  useEffect(() => { if (!linked.some((p) => p.id === selected)) setSelected(linked[0]?.id ?? ""); }, [projects, selected]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => { if (selected) void run(settingsKey, "github-settings", { project: selected }); setPromote(""); }, [selected, settingsKey, run]);
  async function inspect(event: FormEvent) {
    event.preventDefault(); setReviewed(false); setContext("");
    const identity = `${project}\0${repository}\0${branch}`;
    const result = await run("github:preview", "github-preview", { project, repository, ...(branch ? { branch } : {}) });
    if (sourcePreview(result)) setContext(identity);
  }
  async function importSource() {
    if (!preview || !reviewed) return;
    const result = await run("github:import", "github-import", { project, revision: preview.commit, approval: preview.approval, credential_free: true });
    setReviewed(false);
    if (result) { await refresh(); setSelected(project); }
  }
  async function operate(operation: string, body: object) {
    const result = await run(operationKey, operation, { project: selected, ...body });
    if (result) { await refresh(); await run(settingsKey, "github-settings", { project: selected }); }
    return result;
  }
  return <section className="storage-source" aria-label="GitHub connections">
    <h4>GitHub connections</h4>
    <p className="workspace-hint">Import files into LayerFS, then optionally sync accepted main revisions back to GitHub. Your working branches stay in LayerFS.</p>
    <details open={connected.some(r => r.full_name === repository)}>
      <summary>Import a GitHub repository</summary>
      {connected.length > 0 && <label>Connected repository<select value={connected.some(r => r.full_name === repository) ? repository : ""} onChange={e => { const selected = connected.find(r => r.full_name === e.target.value); if (selected) { setRepository(selected.full_name); setBranch(selected.branch); setReviewed(false); } }}><option value="">Choose an authorized repository</option>{connected.map(r => <option key={r.id} value={r.full_name}>{r.full_name}</option>)}</select></label>}
      <p><a href="/?panel=github">Connect a GitHub account or change repository access</a></p>
      <form className="storage-form" onSubmit={inspect}>
        <label>GitHub project ID<input value={project} onChange={(e) => { setProject(e.target.value); setReviewed(false); }} pattern="[a-z0-9]([a-z0-9-]{0,46}[a-z0-9])?" maxLength={48} required placeholder="my-app" /></label>
        <label>GitHub repository<input value={repository} onChange={(e) => { setRepository(e.target.value); setReviewed(false); }} required maxLength={200} placeholder="owner/repository" /></label>
        <label>GitHub branch<input value={branch} onChange={(e) => { setBranch(e.target.value); setReviewed(false); }} maxLength={200} placeholder="Repository default" /></label>
        <button className="button" disabled={busy("github:preview")} type="submit">Fetch and review</button>
      </form>
      <Status task={tasks["github:preview"]} taskKey="github:preview" run={run} />
      {preview && <div className="storage-review" aria-label="GitHub source review">
        <p className="mono">Commit {preview.commit}</p><p>{preview.files.length.toLocaleString()} files · {preview.bytes.toLocaleString()} bytes</p>
        <pre className="storage-files" aria-label="GitHub tracked files">{preview.files.join("\n")}</pre>
        <label className="storage-attestation"><input type="checkbox" checked={reviewed} onChange={(e) => setReviewed(e.target.checked)} />I reviewed this source and confirm it contains no credentials and is suitable for reuse.</label>
        <button className="button primary" disabled={!reviewed || busy("github:import")} onClick={() => void importSource()}>Import into LayerFS</button>
      </div>}
      <Status task={tasks["github:import"]} taskKey="github:import" run={run} />
      {importedDev?.status && <p role="status">Dev setup: {importedDev.status}. {importedDev.error}</p>}
    </details>
    {!!linked.length && <>
      <label className="storage-project-select">Connected project<select value={selected} onChange={(e) => setSelected(e.target.value)}>{linked.map((p) => <option key={p.id} value={p.id}>{p.id} · {p.repository}</option>)}</select></label>
      {current && <p className="workspace-hint">GitHub branch: {current.branch}. Import commit: <span className="mono">{current.current?.commit.slice(0, 12)}</span>.</p>}
      <button className="button small" disabled={busy(settingsKey)} onClick={() => void run(settingsKey, "github-settings", { project: selected })}>Refresh connection</button>
      <Status task={tasks[settingsKey]} taskKey={settingsKey} run={run} />
      <div className="action-buttons">
        <button className="button small" disabled={busy(devKey)} onClick={() => void run(devKey, "dev-start", { project: selected })}>Start or retry dev</button>
        <button className="button small" disabled={busy(devKey)} onClick={() => void run(devKey, "dev-status", { project: selected })}>Check dev status</button>
      </div>
      <Status task={tasks[devKey]} taskKey={devKey} run={run} />
      {dev?.status && <p role="status">Dev: {dev.status}. {dev.error}</p>}
      {dev?.urls?.filter(url => /^https?:\/\//.test(url)).map(url => <p key={url}><a href={url} target="_blank" rel="noopener noreferrer">Open dev environment</a></p>)}
      {settings?.sync && <label className="storage-attestation"><input type="checkbox" checked={settings.sync.enabled} disabled={busy(operationKey)} onChange={(e) => void operate("github-settings", { enabled: e.target.checked })} />Sync accepted main revisions to GitHub</label>}
      <form className="storage-form" onSubmit={(e) => { e.preventDefault(); void operate("github-fork", { name: checkout }).then((result) => { if (result) setCheckout(""); }); }}>
        <label>GitHub checkout name<input value={checkout} onChange={(e) => setCheckout(e.target.value)} required pattern="[a-z0-9]([a-z0-9-]{0,46}[a-z0-9])?" maxLength={48} /></label>
        <button className="button" disabled={busy(operationKey)}>Create LayerFS checkout</button>
      </form>
      <p className="workspace-hint">Checkpoint and close a branch before merging or promoting it. Existing checkouts keep their files.</p>
      <label>Branch to publish<select value={promote} onChange={(e) => setPromote(e.target.value)}><option value="">Select a closed checkpoint</option>{closed.map((r) => <option key={r.name} value={r.name}>{r.name}</option>)}</select></label>
      <div className="action-buttons">{[true, false].map((merge) => <button key={String(merge)} className="button" disabled={!promote || !settings?.main?.revision || busy(operationKey)} onClick={() => void operate("github-promote", { name: promote, expected_main: settings?.main?.revision, merge })}>{merge ? "Merge to main" : "Promote to main"}</button>)}</div>
      {attempt && <div className="storage-review" aria-label="GitHub sync status"><p>Sync: {attempt.status.replaceAll("_", " ")}</p>
        {!!attempt.conflicts.length && <pre className="storage-files">{attempt.conflicts.join("\n")}</pre>}
        {attempt.status === "needs_agent" && <p className="workspace-hint">A specialized agent must resolve these files in the dedicated Git environment before the result can return to LayerFS and GitHub.</p>}
        <div className="action-buttons">{(attempt.status === "needs_agent" ? [["agent-task", "Prepare sync agent task"], ["resolve", "Accept agent resolution"]] : attempt.status === "stale" || attempt.status === "preparing" ? [["retry", "Reconcile latest revisions"]] : ["ready", "delivering", "queued", "awaiting_acceptance", "accepting"].includes(attempt.status) ? [["deliver", "Continue sync"]] : []).map(([action, title]) => <button className="button small" key={action} disabled={busy(operationKey)} onClick={() => void operate("github-delivery", { action, attempt: attempt.id })}>{title}</button>)}</div>
      </div>}
      {settings?.sync?.enabled && <button className="button small" disabled={busy(operationKey)} onClick={() => void operate("github-delivery", { action: "export" })}>Sync accepted main</button>}
      <Status task={tasks[operationKey]} taskKey={operationKey} run={run} />
      {tasks[operationKey]?.result?.kind === "github-reconciliation" && <details className="storage-review">
        <summary>Sync agent task prepared</summary>
        <p className="workspace-hint">Assign this task to a native agent in the prepared environment. Stop the environment after its tests finish, then accept the resolution.</p>
        <pre className="storage-files" aria-label="Sync agent task">{JSON.stringify(tasks[operationKey].result, null, 2)}</pre>
      </details>}
      {!!(tasks[operationKey]?.result?.sync as { error?: string })?.error && <p className="account-error" role="alert">{String((tasks[operationKey]?.result?.sync as { error: string }).error)}</p>}
    </>}
  </section>;
}
