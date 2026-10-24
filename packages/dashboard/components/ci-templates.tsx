"use client";

import { FormEvent, useCallback, useEffect, useRef, useState } from "react";
import { api } from "@/lib/client";

type Installation = {
  node_id: string;
  status: string;
  error: string | null;
  verified_at: number | null;
};
type Recipe = {
  enabled: boolean;
  manifest: {
    id: string;
    label: string;
    revision: number;
    os: string;
    architecture: string;
    capabilities: string[];
    minimum: { cpu: number; memory_mib: number; disk_mib: number };
  };
  nodes: Installation[];
};
const endpoint = "/api/control/v1/ci/templates";

function Entry({ recipe, refresh }: { recipe: Recipe; refresh: () => Promise<void> }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const { manifest, nodes, enabled } = recipe;
  async function change(action: string, node?: string) {
    setBusy(true);
    setError("");
    try {
      await api(`${endpoint}/${encodeURIComponent(manifest.id)}/${action}`, {
        method: "POST", body: JSON.stringify(node ? { node } : {}),
      });
      await refresh();
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setBusy(false);
    }
  }
  return (
    <article className="catalog-card" aria-label={`${manifest.label} CI template`}>
      <div className="catalog-card-heading">
        <div>
          <h3>{manifest.label}</h3>
          <span className="mono">{manifest.id} · revision {manifest.revision}</span>
        </div>
        <span className="catalog-recipe">{enabled ? "Enabled" : "Disabled"}</span>
        <button className="button small" disabled={busy} onClick={() => change(enabled ? "disable" : "enable")}>
          {enabled ? "Disable CI template" : "Enable CI template"}
        </button>
      </div>
      <p className="catalog-description">{manifest.os} / {manifest.architecture} · {manifest.capabilities.join(", ")}</p>
      <p className="deployment-status">Minimum: {manifest.minimum.cpu} CPU · {manifest.minimum.memory_mib} MiB available RAM · {manifest.minimum.disk_mib} MiB free disk</p>
      <p className="deployment-status">{nodes.filter((node) => node.status === "ready").length} / {nodes.length} nodes ready</p>
      {error && <p className="account-error" role="alert">{error}</p>}
      {nodes.length ? <ul className="template-nodes" aria-label={`${manifest.label} installations`}>
        {nodes.map((node) => <li key={node.node_id} aria-label={`${node.node_id} installation`}>
          <div className="template-node-heading">
            <strong>{node.node_id}</strong>
            <span className={`node-status node-${node.status}`}>{node.status.replaceAll("-", " ")}</span>
            {enabled && node.status === "failed" && <button className="button small" disabled={busy} onClick={() => change("retry", node.node_id)}>Retry installation</button>}
          </div>
          {node.verified_at && <p className="deployment-status">Capabilities verified {new Date(node.verified_at * 1000).toLocaleString()}</p>}
          {node.error && <p className="account-error">{node.error.replaceAll("-", " ")}</p>}
        </li>)}
      </ul> : <p className="catalog-description">No CI nodes registered. Enabled templates install when a compatible dedicated worker joins.</p>}
    </article>
  );
}

export function CiTemplates() {
  const [recipes, setRecipes] = useState<Recipe[] | null>(null);
  const [loadError, setLoadError] = useState("");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [manifest, setManifest] = useState("");
  const [busy, setBusy] = useState(false);
  const current = useRef<AbortController | null>(null);
  const refresh = useCallback(async () => {
    if (current.current) return;
    const controller = new AbortController();
    current.current = controller;
    try {
      const result = await api<{ templates: Recipe[] }>(endpoint, { signal: controller.signal });
      if (!controller.signal.aborted) { setRecipes(result.templates); setLoadError(""); }
    } catch (err) {
      if (!controller.signal.aborted) { setLoadError((err as Error).message); setRecipes(null); }
    } finally {
      if (current.current === controller) current.current = null;
    }
  }, []);
  useEffect(() => {
    void refresh();
    const timer = setInterval(() => void refresh(), 2000);
    return () => { clearInterval(timer); current.current?.abort(); current.current = null; };
  }, [refresh]);
  async function register(event: FormEvent) {
    event.preventDefault(); setBusy(true); setError(""); setNotice("");
    try {
      let parsed;
      try { parsed = JSON.parse(manifest); } catch { throw new Error("The CI template must be valid JSON."); }
      if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("The CI template must be a JSON object.");
      await api(endpoint, { method: "POST", body: JSON.stringify({ manifest: parsed }) });
      await refresh();
      setNotice("CI template registered. Enable it to install on compatible workers. Updates to enabled templates synchronize automatically.");
    } catch (err) { setError((err as Error).message); }
    finally { setBusy(false); }
  }
  return <section className="catalog-panel" aria-labelledby="ci-templates-title">
    <div className="section-title"><div>
      <h2 id="ci-templates-title">CI toolchain templates</h2>
      <p>Enable a versioned toolchain to install and verify it on compatible dedicated CI workers, including workers added later.</p>
    </div></div>
    <p className="workspace-hint">Workers need the matching operating system and Engine Box CLI. Installation shares the worker’s build lock. Disabling stops new template jobs and keeps running jobs and installed files.</p>
    {loadError && <p className="account-error" role="alert">{loadError} <button className="text-button" onClick={refresh}>Retry CI catalog</button></p>}
    {error && <p className="account-error" role="alert">{error}</p>}
    {notice && <p className="catalog-notice" role="status">{notice}</p>}
    <form className="template-import" onSubmit={register}>
      <label className="import-content">CI recipe JSON<textarea required spellCheck={false} rows={6} maxLength={32768} value={manifest} onChange={(event) => setManifest(event.target.value)} /></label>
      <button className="button primary" disabled={busy}>{busy ? "Registering…" : "Register CI template"}</button>
    </form>
    <p className="workspace-hint">Review installer scripts and pinned download checksums before enabling a template. Recipes run as the CI worker account.</p>
    {recipes === null ? <p className="catalog-description">{loadError ? "CI templates are unavailable." : "Loading CI templates…"}</p> : recipes.length ? <div className="catalog-grid">{recipes.map((recipe) => <Entry key={recipe.manifest.id} recipe={recipe} refresh={refresh} />)}</div> : <p className="catalog-description">No CI templates registered.</p>}
  </section>;
}
