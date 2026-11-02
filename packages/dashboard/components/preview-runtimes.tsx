"use client";
import { useCallback, useEffect, useState } from "react";
import { api, errorMessage } from "@/lib/client";

type Catalog = { projects?: { id: string; project: string; template?: { id: string; revision: number; digest: string } | false | null }[] };

/**
 * Preview runtimes are container templates declared by each repository in
 * `.enginebox/config.json`, not registered here. This lists the ones that
 * active previews use, so the Templates page shows every runtime in one place.
 */
export function PreviewRuntimes() {
  const [catalog, setCatalog] = useState<Catalog | null>(null);
  const [error, setError] = useState("");
  const refresh = useCallback(async () => {
    try { setCatalog(await api<Catalog>("/api/environments")); setError(""); } catch (e) { setError(errorMessage(e)); }
  }, []);
  useEffect(() => { void refresh(); }, [refresh]);
  const runtimes = new Map<string, { id: string; revision: number; digest: string; projects: string[] }>();
  for (const p of catalog?.projects ?? []) {
    if (!p.template) continue;
    const key = `${p.template.id}@${p.template.revision}`;
    const entry = runtimes.get(key) ?? { ...p.template, projects: [] };
    if (!entry.projects.includes(p.project)) entry.projects.push(p.project);
    runtimes.set(key, entry);
  }
  return <section className="catalog-panel" aria-label="Preview runtimes">
    <div className="section-heading"><div><h2>Preview runtimes</h2><p>Container templates for previews: a pinned image, a setup command, a start command and the ports to expose. Each repository declares its own in <code>.enginebox/config.json</code> and <code>.enginebox/templates/</code>, so they change through the project&apos;s code, not here.</p></div><button className="button small" onClick={() => void refresh()}>Refresh runtimes</button></div>
    {error && <p role="status">Preview runtimes unavailable: {error}</p>}
    {catalog && !runtimes.size && <p>No preview is running from a declared runtime yet.</p>}
    <div className="host-grid">{[...runtimes.entries()].map(([key, r]) => <article className="host-card" key={key} aria-label={`Preview runtime ${key}`}>
      <h3>{r.id}</h3><p>Revision {r.revision} · Container</p>
      <p>Used by {r.projects.join(", ")}</p>
      <small className="mono">{r.digest.slice(0, 19)}</small>
    </article>)}</div>
  </section>;
}
