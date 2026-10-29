"use client";
import { useCallback, useEffect, useState } from "react";
import { api, errorMessage } from "@/lib/client";
import { mcpConfiguration } from "@/lib/mcp-setup";

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
  const [copied, setCopied] = useState(false);
  const [publicOrigin, setPublicOrigin] = useState("");
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
    setPublicOrigin(window.location.origin);
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
    <section className="contributor-panel" aria-label="MCP">
      <div className="contributor-heading">
        <div>
          <h2>MCP</h2>
          <p>Connect a trusted local client to inspect workspaces, prepare source, and run native agents.</p>
        </div>
        <button className="button small" onClick={() => void refresh()}>Refresh</button>
      </div>
      <article className="host-card">
        <h3>Connect a local MCP client</h3>
        <ol>
          <li>Install the Enginebox CLI on the machine running your MCP client. Make <code>enginebox-fleet-mcp</code> available on its PATH, or use its absolute executable path.</li>
          <li>Add the stdio server configuration below to your client's MCP settings. Replace the token-file placeholder with a private file on that machine and set the controller API URL it can reach. The default loopback address works when the controller is on the same machine. For a remote controller, point this URL at an authorized SSH tunnel to its private API; OAuth apps use the separate dashboard origin.</li>
          <li>Restart or reconnect the client, then ask it to list workspaces with <code>workspace_catalog</code>. Use <code>fleet_inventory</code> to inspect available machines before allocating anything.</li>
        </ol>
        <label>MCP stdio configuration<textarea className="mcp-config" readOnly value={mcpConfiguration} rows={14} spellCheck={false} /></label>
        <button className="button small" onClick={() => { void navigator.clipboard.writeText(mcpConfiguration).then(() => setCopied(true)).catch(() => setError("Copy was unavailable. Select and copy the configuration above.")); }}>{copied ? "Configuration copied" : "Copy MCP configuration"}</button>
        <p>Keep the operator token on your trusted operator machine. It authorizes administrative tools and must not be installed inside project environments or given to task agents. The browser never displays or copies the token.</p>
        <p><a href="https://github.com/coop-collective/enginebox/blob/main/docs/INSTALL.md" target="_blank" rel="noopener noreferrer">Install the CLI</a> · <a href="https://github.com/coop-collective/enginebox/blob/main/docs/FLEET.md#mcp-connection" target="_blank" rel="noopener noreferrer">MCP configuration guide</a></p>
      </article>
      <article className="host-card">
        <h3>Control a workspace through tools</h3>
        <dl className="mcp-tool-list">
          <dt>Inspect source and activity</dt><dd><code>workspace_catalog</code> and <code>repository_list</code> show project sources, observations and repository bindings.</dd>
          <dt>Create or map source</dt><dd><code>repository_create</code> creates a hosted source. <code>repository_link_github</code> maps an owner-selected GitHub source; this requires owner authority.</dd>
          <dt>Prepare and reuse storage</dt><dd><code>storage_catalog</code>, <code>storage_github_preview</code> and <code>storage_github_import</code> support reviewed imports. <code>storage_fork</code> creates an independent checkout; use <code>fleet_job</code> to observe operations.</dd>
          <dt>Set up connections and profiles</dt><dd><code>connection_create</code>, <code>connection_test</code> and <code>profile_attach_connection</code> share an MCP server or secret source across profiles; <code>catalog_apply</code> applies a reviewed catalog file. These require owner authority; secret values are never returned.</dd>
          <dt>Run and follow an agent</dt><dd><code>pool_agent_launch</code> submits to a granted project pool. Follow it with <code>pool_agent_run</code> or <code>pool_agent_cancel</code>. <code>agent_runs</code> lists permitted work; <code>agent_launch</code> targets an explicitly selected environment.</dd>
        </dl>
        <p>Tool availability follows the installed CLI and controller versions. If a listed tool is missing, update both to a version that includes it and reconnect the client.</p>
        <p>Start with: “Show my workspaces and their latest preparation measurements.” Then select a project and an existing pool before requesting a run with pool, provider and prompt. If submission is unconfirmed, inspect agent_runs before trying again. Imports require source review; successful runs do not approve their own changes or promote accepted main.</p>
      </article>
      <article className="host-card">
        <h3>Connect an app with OAuth</h3>
        <p>App integrations use OAuth and the scoped HTTP API at <code>{publicOrigin || "this dashboard's public origin"}</code>. This is a separate connection method: there is currently no remote <code>/mcp</code> endpoint to paste into a URL-only MCP client.</p>
        <p>An app discovers <code>/.well-known/oauth-authorization-server</code>, registers at <code>/oauth/register</code>, and sends you through <code>/oauth/authorize</code>. Approve only the project pools, environments and repository access it needs. The app exchanges and refreshes its own scoped token at <code>/oauth/token</code>.</p>
        <p><a href="https://github.com/coop-collective/enginebox/blob/main/docs/OAUTH.md" target="_blank" rel="noopener noreferrer">OAuth integration guide and supported API routes</a></p>
      </article>
      <h3>Connected apps and grants</h3>
      <p>Review apps you approved to run agents on selected environments and pools. Access tokens last one hour and renew until you disconnect the app.</p>
      {error && <div className="alert" role="alert">{error}</div>}
      {!enabled && <p>OAuth app connections are not enabled on this dashboard. Local stdio MCP remains available. An administrator can enable app connections by configuring a private ENGINEBOX_OAUTH_STATE_DIR.</p>}
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
