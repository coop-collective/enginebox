"use client";
import { useCallback, useEffect, useState } from "react";
import { api, errorMessage } from "@/lib/client";

type Connection = {
  id: string;
  client_id: string;
  client_name: string;
  redirect_hosts: string[];
  workspaces: { host: string; instance: string; profiles: string[] }[];
  pools?: { pool: string; profile: string | null; project?: string }[];
  repositories?: { project: string; access: "read" | "write" }[];
  /** The connection is restricted to this project's pools. */
  project?: string | null;
  created_at: string;
  refreshed_at: string;
  access_token_active: boolean;
};

/** Apps connected over OAuth, and a way to disconnect each one. */
export function ConnectedAppsPanel() {
  const [connections, setConnections] = useState<Connection[]>([]);
  const [enabled, setEnabled] = useState(true);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState("");
  const refresh = useCallback(async () => {
    try {
      const result = await api<{ enabled: boolean; grants: Connection[] }>("/api/oauth/grants");
      setEnabled(result.enabled);
      setConnections(result.grants);
      setError("");
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  async function disconnect(connection: Connection) {
    if (!window.confirm(`Disconnect ${connection.client_name}? Its access token stops working immediately; runs already started continue.`)) return;
    setBusy(connection.id);
    try {
      await api(`/api/oauth/grants/${connection.id}/revoke`, { method: "POST", body: "{}" });
      await refresh();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy("");
    }
  }
  return (
    <section className="contributor-panel" aria-label="Connected apps">
      <div className="contributor-heading">
        <div>
          <h2>Connected apps</h2>
          <p>Apps you approved to run agents on selected workspaces and pools. Each holds an hour-long access token it renews until you disconnect it.</p>
        </div>
        <button className="button small" onClick={() => void refresh()}>Refresh</button>
      </div>
      {error && <div className="alert" role="alert">{error}</div>}
      {!enabled && <p>App connections are off. Set ENGINEBOX_OAUTH_STATE_DIR on the dashboard service to enable them.</p>}
      {enabled && !connections.length && !error && <p>No apps are connected.</p>}
      <div className="host-grid">
        {connections.map((c) => (
          <article className="host-card" key={c.id} aria-label={`Connection ${c.client_name}`}>
            <h3>{c.client_name}</h3>
            <p>Returns to {c.redirect_hosts.join(", ")} · client <code>{c.client_id}</code></p>
            <p>Connected {new Date(c.created_at).toLocaleString()} · last renewed {new Date(c.refreshed_at).toLocaleString()}</p>
            <p>{c.access_token_active ? "Access token active" : "No active access token"}</p>
            {c.project ? <p>Restricted to pools in project <code>{c.project}</code></p> : null}
            <ul>
              {c.workspaces.map((w) => (
                <li key={`${w.host}/${w.instance}`}>
                  <code>{w.host}/{w.instance}</code>
                  {w.profiles.length ? ` · profiles: ${w.profiles.join(", ")}` : ""}
                </li>
              ))}
              {[...(c.pools ?? [])]
                .sort((a, b) => (a.project ?? "").localeCompare(b.project ?? "") || a.pool.localeCompare(b.pool))
                .map((p) => (
                  <li key={`pool:${p.pool}`}>
                    pool <code>{p.pool}</code>
                    {p.project ? ` · project ${p.project}` : ""}
                    {p.profile ? ` · profile: ${p.profile}` : ""}
                  </li>
                ))}
              {(c.repositories ?? []).map((r) => (
                <li key={`repositories:${r.project}`}>
                  repositories in project <code>{r.project}</code> · {r.access === "write" ? "read and write" : "read only"}
                </li>
              ))}
            </ul>
            <button className="button small" disabled={busy === c.id} onClick={() => void disconnect(c)}>Disconnect</button>
          </article>
        ))}
      </div>
    </section>
  );
}
