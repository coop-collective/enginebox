"use client";

import { FormEvent, useCallback, useEffect, useRef, useState } from "react";
import { api, RequestError } from "@/lib/client";
import type { Job } from "@/lib/types";
import { checkoutRows, pending, sourcePreview, workspaceBusy, type Checkout, type StorageCatalog, type StorageNode, type StorageTask } from "@/lib/storage";

import { GitHubStorage, githubProjects, type GitHubProject } from "./github-storage";

const base = "/api/control/v1";
const blocked = (task?: StorageTask) => !!task && (task.busy || !!task.uncertain);
const label = (value: string) => value.charAt(0).toUpperCase() + value.slice(1);

function useStorageJobs(host: string) {
  const [tasks, setTasks] = useState<Record<string, StorageTask>>({});
  const controllers = useRef(new Map<string, AbortController>());
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      for (const controller of controllers.current.values()) controller.abort();
      controllers.current.clear();
    };
  }, []);
  const run = useCallback(async (key: string, operation: string, body: object, existing?: Job) => {
    if (!mounted.current || controllers.current.has(key)) return null;
    const controller = new AbortController();
    controllers.current.set(key, controller);
    let job = existing;
    const update = (task: StorageTask) => {
      if (mounted.current && !controller.signal.aborted) setTasks((current) => ({ ...current, [key]: task }));
    };
    const signal = () => AbortSignal.any([controller.signal, AbortSignal.timeout(30_000)]);
    update({ operation, busy: true, job });
    try {
      if (!job) {
        const response = await api<{ job: Job }>(`${base}/storage/hosts/${encodeURIComponent(host)}/${operation}`, {
          method: "POST", body: JSON.stringify(body), signal: signal(),
        });
        job = response.job;
      }
      if (!job || typeof job.id !== "string") throw new Error("The controller did not return a job ID. Inspect receipts before submitting again.");
      const deadline = Date.now() + 15 * 60_000;
      while (pending(job)) {
        update({ operation, busy: true, job });
        if (Date.now() >= deadline) throw new Error("Status polling reached its time limit. Check this job again; its operation may still be running.");
        job = (await api<{ job: Job }>(`${base}/jobs/${encodeURIComponent(job.id)}`, { signal: signal() })).job;
        if (pending(job)) await new Promise<void>((resolve, reject) => {
          const abort = () => { clearTimeout(timer); reject(new Error("Observation stopped")); };
          const timer = setTimeout(() => { controller.signal.removeEventListener("abort", abort); resolve(); }, 1500);
          controller.signal.addEventListener("abort", abort, { once: true });
          if (controller.signal.aborted) abort();
        });
      }
      if (job.status !== "succeeded") {
        update({ operation, busy: false, job, error: job.error ?? "Storage operation failed. Inspect its receipt before retrying." });
        return null;
      }
      const result = job.result?.result;
      if (!result || typeof result !== "object" || Array.isArray(result)) throw new Error("Storage job returned an invalid result.");
      update({ operation, busy: false, job, result: result as Record<string, unknown> });
      return result as Record<string, unknown>;
    } catch (error) {
      // Once a job was admitted, even a 401/404 observation can be temporary
      // during handoff. It says nothing about whether the command is running.
      const rejected = !job && error instanceof RequestError && error.status >= 400 && error.status < 500;
      update({ operation, busy: false, job, uncertain: !rejected,
        error: `${(error as Error).message}${rejected ? "" : " No command was resubmitted."}` });
      return null;
    } finally {
      if (controllers.current.get(key) === controller) controllers.current.delete(key);
    }
  }, [host]);
  return { tasks, run };
}

function TaskStatus({ task, check }: { task?: StorageTask; check?: () => void }) {
  if (!task) return null;
  return <div className="storage-task" aria-live="polite">
    {task.busy ? <span className="working"><span className="spinner" />{label(task.operation)} · {task.job?.status ?? "submitting"}</span>
      : task.error ? <p className="account-error" role="alert">{task.error}</p>
        : <p className="catalog-notice">{label(task.operation)} completed.</p>}
    {task.job && <small className="mono">Job {task.job.id}</small>}
    {task.uncertain && task.job && check && <button className="button small" type="button" onClick={check}>Check job status</button>}
  </div>;
}

function NodeStorage({ node }: { node: StorageNode }) {
  const { tasks, run } = useStorageJobs(node.host_id);
  const [github, setGithub] = useState<GitHubProject[]>([]);
  const [rows, setRows] = useState<Checkout[] | null>(null);
  const [project, setProject] = useState(node.projects[0]?.id ?? "");
  const [revision, setRevision] = useState("HEAD");
  const [reviewed, setReviewed] = useState(false);
  const [previewContext, setPreviewContext] = useState("");
  const [name, setName] = useState("");
  const [from, setFrom] = useState("");
  const [message, setMessage] = useState("Prepared workspace checkpoint");
  const [listError, setListError] = useState("");
  const refreshing = useRef(false);
  const refreshNeeded = useRef(false);
  const refresh = useCallback(async () => {
    refreshNeeded.current = true;
    if (refreshing.current) return;
    refreshing.current = true;
    try {
      while (refreshNeeded.current) {
        refreshNeeded.current = false;
        const result = await run("list", "list", {});
        if (!result) continue;
        const rows = checkoutRows(result);
        if (!rows) { setListError("The node returned invalid workspace receipts."); continue; }
        setRows(rows);
        setGithub(githubProjects(result));
        setListError("");
      }
    } finally { refreshing.current = false; }
  }, [run]);
  useEffect(() => { void refresh(); }, [refresh]);
  useEffect(() => {
    if (!node.projects.some((p) => p.id === project)) setProject(node.projects[0]?.id ?? "");
  }, [node.projects, project]);
  const previewKey = `preview:${project}`;
  const importKey = `import:${project}`;
  const preview = previewContext === `${project}\0${revision}` ? sourcePreview(tasks[previewKey]?.result) : null;
  const prepared = rows?.filter((row) => row.project === project && row.last_checkpoint && ["ready", "closed"].includes(row.status)) ?? [];
  async function previewSource(event: FormEvent) {
    event.preventDefault();
    setReviewed(false);
    setPreviewContext("");
    const context = `${project}\0${revision}`;
    const result = await run(previewKey, "preview", { project, revision });
    if (sourcePreview(result)) setPreviewContext(context);
  }
  async function importSource() {
    if (!preview || !reviewed || blocked(tasks[importKey])) return;
    await run(importKey, "import", { project, revision: preview.commit, approval: preview.approval, credential_free: true });
    setReviewed(false);
  }
  async function fork(event: FormEvent) {
    event.preventDefault();
    const result = await run("fork", "fork", { project, name, ...(from ? { from } : {}) });
    if (result) setName("");
    await refresh();
  }
  async function action(row: Checkout, operation: string) {
    await run(`workspace:${row.name}`, operation, { name: row.name, ...(operation === "checkpoint" ? { message } : {}) });
    await refresh();
  }
  async function check(key: string) {
    const task = tasks[key];
    if (!task?.job) return;
    await run(key, task.operation, {}, task.job);
    if (key.startsWith("workspace:")) await refresh();
  }
  return <article className="catalog-card storage-node" aria-label={`${node.host_id} checkout storage`}>
    <div className="catalog-card-heading">
      <div><h3>{node.host_id}</h3><p className="catalog-description">Registered node · LayerFS checkouts</p></div>
      <button className="button small" disabled={tasks.inspect?.busy} onClick={() => void run("inspect", "inspect", {})}>Inspect node</button>
      <button className="button small" disabled={tasks.list?.busy} onClick={() => void refresh()}>Refresh receipts</button>
    </div>
    <TaskStatus task={tasks.inspect} check={() => void check("inspect")} />
    {tasks.inspect?.result && <p className="catalog-notice" role="status">Node prerequisites passed. Admission checks run again for each operation.</p>}
    <TaskStatus task={tasks.list?.error ? tasks.list : undefined} check={() => void check("list")} />
    {listError && <p className="account-error" role="alert">{listError}</p>}
    {node.github?.enabled && <GitHubStorage projects={github} rows={rows ?? []} tasks={tasks} run={run} refresh={refresh} />}
    {node.projects.length ? <>
      <label className="storage-project-select">Project
        <select value={project} onChange={(event) => { setProject(event.target.value); setReviewed(false); setFrom(""); setPreviewContext(""); }}>
          {node.projects.map((project) => <option value={project.id} key={project.id}>{project.id}</option>)}
        </select>
      </label>
      <details className="storage-source">
        <summary>Review and import a source revision</summary>
        <form className="storage-form" onSubmit={previewSource}>
          <label>Git revision<input value={revision} onChange={(event) => { setRevision(event.target.value); setReviewed(false); }} required maxLength={256} /></label>
          <button className="button" disabled={tasks[previewKey]?.busy || !project} type="submit">Review source</button>
        </form>
        <TaskStatus task={tasks[previewKey]} check={() => void check(previewKey)} />
        {preview && <div className="storage-review" aria-label="Source review">
          <dl><dt>Exact commit</dt><dd className="mono">{preview.commit}</dd><dt>Approval digest</dt><dd className="mono">{preview.approval}</dd></dl>
          <p>{preview.files.length.toLocaleString()} tracked files · {preview.bytes.toLocaleString()} bytes</p>
          <pre className="storage-files" aria-label="Tracked file list">{preview.files.join("\n")}</pre>
          <p className="workspace-hint">Review the tracked content before importing. Local credentials, Git configuration and earlier history are excluded; path checks cannot identify every secret.</p>
          <label className="storage-attestation"><input type="checkbox" checked={reviewed} onChange={(event) => setReviewed(event.target.checked)} />I reviewed this commit and confirm its tracked content is credential-free and suitable for reuse.</label>
          <button className="button primary" disabled={!reviewed || blocked(tasks[importKey])} onClick={() => void importSource()}>Import reviewed commit</button>
        </div>}
        <TaskStatus task={tasks[importKey]} check={() => void check(importKey)} />
      </details>
      <form className="storage-form storage-fork" onSubmit={fork}>
        <label>Checkout name<input value={name} onChange={(event) => setName(event.target.value)} pattern={"[a-z0-9]([a-z0-9\\-]{0,46}[a-z0-9])?"} maxLength={48} required placeholder="agent-a" /></label>
        <label>Start from<select value={from} onChange={(event) => setFrom(event.target.value)}><option value="">Imported project base</option>{prepared.map((row) => <option key={row.name} value={row.name}>Prepared checkpoint · {row.name}</option>)}</select></label>
        <button className="button primary" type="submit" disabled={!project || blocked(tasks.fork)}>Create checkout</button>
      </form>
      {from && <p className="workspace-hint">Reuses only the selected completed checkpoint, including installed files. This stays within the project's trust boundary and is not published as a fleet template.</p>}
      <TaskStatus task={tasks.fork} check={() => void check("fork")} />
    </> : <p className="workspace-hint">Register repository aliases on this node before importing a project.</p>}
    <div className="section-title storage-checkouts-heading"><div><h4>Checkouts</h4><p>Durable receipts; refresh to observe operations started elsewhere.</p></div>
      <label>Checkpoint message<input value={message} maxLength={4096} onChange={(event) => setMessage(event.target.value)} /></label>
    </div>
    {rows === null ? <p className="catalog-description">{tasks.list?.busy ? "Loading checkout receipts…" : "Checkout receipts are unavailable."}</p> : !rows.length ? <p className="catalog-description">No checkouts on this node yet.</p> : <div className="storage-checkouts">
      {rows.map((row) => {
        const key = `workspace:${row.name}`;
        const busy = workspaceBusy(tasks, row.name);
        const ready = row.status === "ready";
        const recoverable = !["ready", "closed"].includes(row.status);
        return <section className="storage-checkout" key={row.name} aria-label={`Checkout ${row.name}`}>
          <div className="storage-checkout-heading"><div><strong>{row.name}</strong><span className="table-detail">Project {row.github_project ?? row.project}</span></div><span className="storage-receipt-status">{label(row.status)}</span></div>
          <div className="action-buttons">
            {ready && ["deps", "checkpoint", "remount", "close"].map((operation) => <button className="button small" key={operation} disabled={busy} onClick={() => void action(row, operation)}>{operation === "deps" ? "Install cached dependencies" : label(operation)}</button>)}
            {recoverable && <button className="button small" disabled={busy} onClick={() => void action(row, "recover")}>Inspect recovery</button>}
          </div>
          <TaskStatus task={tasks[key]} check={() => void check(key)} />
          {row.last_error && <p className="account-error">{row.last_error}</p>}
          {recoverable && <p className="workspace-hint">This receipt is blocked. Recovery checks live evidence and never replays the interrupted command.</p>}
        </section>;
      })}
    </div>}
  </article>;
}

export function StoragePanel({ refreshToken }: { refreshToken: number }) {
  const [catalog, setCatalog] = useState<StorageCatalog | null>(null);
  const [error, setError] = useState("");
  const [retry, setRetry] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    void api<StorageCatalog>(`${base}/storage`, { signal: controller.signal }).then((data) => {
      if (!controller.signal.aborted) { setCatalog(data); setError(""); }
    }).catch((error) => { if (!controller.signal.aborted) setError(error.message); });
    return () => controller.abort();
  }, [refreshToken, retry]);
  return <section className="catalog-panel" aria-labelledby="storage-title">
    <div className="section-title"><div><h2 id="storage-title">Shared checkout storage</h2><p>Import a project once, then fork independent writable checkouts with prepared dependencies.</p></div></div>
    <p className="workspace-hint">Checkout sandboxes are managed separately from Incus VMs and private agent homes. VM attachment is not available from this dashboard yet.</p>
    {error && <div className="alert" role="alert">{error}<button className="text-button" onClick={() => setRetry((n) => n + 1)}>Retry storage catalog</button></div>}
    {!catalog ? <p className="catalog-description">Loading storage catalog…</p> : !catalog.nodes.length ? <div className="empty-state compact"><h3>Storage setup required</h3><p>Register qualified Linux nodes and repository aliases in the controller's private storage configuration, then activate the controller. The registered nodes and projects will then appear here.</p></div> : <div className="storage-nodes">{catalog.nodes.map((node) => <NodeStorage key={node.host_id} node={node} />)}</div>}
  </section>;
}
