"use client";
import { FormEvent, useCallback, useEffect, useState } from "react";
import { api, errorMessage } from "@/lib/client";

export type Repository = {
  project: string;
  name: string;
  backend: string;
  description?: string;
  remote: string;
  default_branch: string;
  created_at: string;
  last_push_at?: string | null;
};
type Backend = { kind: string; configured: boolean; account_id?: string; namespace_prefix?: string };

const PROJECT = /^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$/;
const NAME = /^[a-z0-9][a-z0-9._-]{0,62}$/;

/** Every project that has a pool or a repository, sorted, plus any the owner typed this session. */
export function repositoryProjects(pools: { project?: unknown }[], repositories: Repository[], extra: string[] = []): string[] {
  const projects = new Set<string>(extra.filter((p) => PROJECT.test(p)));
  for (const p of pools) projects.add(typeof p.project === "string" && PROJECT.test(p.project) ? p.project : "default");
  for (const r of repositories) projects.add(r.project);
  return [...projects].sort();
}

/** Hosted Git repositories per project: list, remote, last push, and create. */
export function RepositoriesPanel() {
  const [backend, setBackend] = useState<Backend | null>(null);
  const [projects, setProjects] = useState<string[]>([]);
  const [byProject, setByProject] = useState<Record<string, Repository[]>>({});
  const [extra, setExtra] = useState<string[]>([]);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);
  const [project, setProject] = useState("");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const refresh = useCallback(async () => {
    try {
      const [state, pools, all] = await Promise.all([
        api<{ backend: Backend }>("/api/control/v1/repository-backends/artifacts"),
        api<{ pools?: { project?: unknown }[] }>("/api/control/v1/pools").catch(() => ({ pools: [] })),
        api<{ repositories: Repository[] }>("/api/control/v1/repositories"),
      ]);
      setBackend(state.backend);
      const listed = repositoryProjects(pools.pools ?? [], all.repositories, extra);
      setProjects(listed);
      // Per-project listings carry the last push time when the backend answers.
      const entries = await Promise.all(
        listed
          .filter((p) => all.repositories.some((r) => r.project === p))
          .map(async (p) => {
            const result = await api<{ repositories: Repository[] }>(`/api/control/v1/projects/${p}/repositories`).catch(() => ({
              repositories: all.repositories.filter((r) => r.project === p),
            }));
            return [p, result.repositories] as const;
          }),
      );
      setByProject(Object.fromEntries(entries));
      setError("");
    } catch (e) {
      setError(errorMessage(e));
    }
  }, [extra]);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  async function create(event: FormEvent) {
    event.preventDefault();
    setNotice("");
    const target = project.trim();
    if (!PROJECT.test(target)) return setError("Project IDs use lowercase letters, digits and dashes.");
    if (!NAME.test(name) || name.endsWith(".git")) return setError("Repository names use lowercase letters, digits, '.', '_' or '-' and do not end in .git.");
    setBusy(true);
    try {
      const result = await api<{ repository: Repository }>(`/api/control/v1/projects/${target}/repositories`, {
        method: "POST",
        body: JSON.stringify(description.trim() ? { name, description: description.trim() } : { name }),
      });
      setNotice(`Repository ${result.repository.project}/${result.repository.name} is ready.`);
      setName("");
      setDescription("");
      if (!projects.includes(target)) setExtra((current) => [...current, target]);
      await refresh();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  }
  return (
    <section className="contributor-panel" aria-label="Repositories">
      <div className="contributor-heading">
        <div>
          <h2>Repositories</h2>
          <p>
            Hosted Git repositories for each project. Agents in Engine Box workspaces clone and push them with short-lived tokens; runs that name a
            repository receive one automatically.
          </p>
        </div>
        <button className="button small" onClick={() => void refresh()}>Refresh</button>
      </div>
      {error && <div className="alert" role="alert">{error}</div>}
      {notice && <p className="catalog-notice" role="status">{notice}</p>}
      {backend && !backend.configured && (
        <p>
          No repository backend is configured. The owner sets one with <code>PUT /v1/repository-backends/artifacts</code> (see docs/REPOSITORIES.md).
        </p>
      )}
      {backend?.configured && (
        <p>
          Backend: Cloudflare Artifacts · account <code>{backend.account_id}</code> · namespaces <code>{backend.namespace_prefix}-&lt;project&gt;</code>
        </p>
      )}
      <form className="template-import" onSubmit={create} aria-label="Create a repository">
        <label>
          Project
          <input list="repository-projects" value={project} onChange={(e) => setProject(e.target.value)} placeholder="default" required />
          <datalist id="repository-projects">
            {projects.map((p) => (
              <option key={p} value={p} />
            ))}
          </datalist>
        </label>
        <label>
          Name
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="game" maxLength={63} required />
        </label>
        <label className="import-content">
          Description
          <input value={description} onChange={(e) => setDescription(e.target.value)} maxLength={512} placeholder="Optional" />
        </label>
        <button className="button primary" type="submit" disabled={busy || !backend?.configured}>
          {busy ? "Creating…" : "Create repository"}
        </button>
      </form>
      {projects.map((p) => (
        <article className="host-card" key={p} aria-label={`Project ${p} repositories`}>
          <h3>Project {p}</h3>
          {(byProject[p] ?? []).length === 0 ? (
            <p>No repositories yet.</p>
          ) : (
            <ul>
              {(byProject[p] ?? []).map((r) => (
                <li key={r.name}>
                  <strong>{r.name}</strong>
                  {r.description ? ` · ${r.description}` : ""} · <code>{r.remote}</code> · default branch <code>{r.default_branch}</code> ·{" "}
                  {r.last_push_at ? `last push ${new Date(r.last_push_at).toLocaleString()}` : "no push recorded"}
                </li>
              ))}
            </ul>
          )}
        </article>
      ))}
    </section>
  );
}
