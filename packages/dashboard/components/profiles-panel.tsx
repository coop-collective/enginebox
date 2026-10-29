"use client";
import { FormEvent, useCallback, useEffect, useState } from "react";
import { api, errorMessage } from "@/lib/client";
import { kindLabels } from "@/components/connections-panel";

type Bundle = { name: string; files: { path: string }[] };
type ProviderBlock = {
  permission_profile?: string;
  max_turns?: number;
  allowed_tools?: string[];
  skills?: Bundle[];
  plugins?: Bundle[];
  mcp_servers?: { name: string; url?: string; command?: string }[];
  env_keys?: string[];
  network_access?: boolean;
};
type Profile = {
  name: string;
  description: string;
  template: string | null;
  org_id: string;
  providers: Record<string, ProviderBlock>;
  connections: string[];
  connection_policy: Record<string, string[]>;
  revision: number;
  updated_at: string;
};
type ConnectionRow = { name: string; kind: keyof typeof kindLabels };
type Run = { id: string; status: string; provider?: string; created_at?: string; finished_at?: string | null; profile?: { revision?: number; run_kind?: string; connections?: { name: string; revision: number; skipped?: string }[] } };

const providerNames: Record<string, string> = { claude: "Claude Code", codex: "Codex", cursor: "Cursor" };

/**
 * Profile builder: what a profile runs with (template, per-provider tools and
 * skills), which shared connections it uses, which of them apply to each kind
 * of run, and the recent runs that used it. Env values never appear.
 */
export function ProfilesPanel() {
  const [profiles, setProfiles] = useState<Profile[] | null>(null);
  const [manage, setManage] = useState(false);
  const [connections, setConnections] = useState<ConnectionRow[]>([]);
  const [selected, setSelected] = useState("");
  const [runs, setRuns] = useState<Run[]>([]);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState("");
  const [attach, setAttach] = useState("");
  const [attachKinds, setAttachKinds] = useState("");
  const [policy, setPolicy] = useState<Record<string, string[]>>({});
  const [newKind, setNewKind] = useState("");

  const refresh = useCallback(async () => {
    try {
      const [p, c] = await Promise.all([
        api<{ profiles: Profile[]; can_manage: boolean }>("/api/catalog/profiles"),
        api<{ connections: ConnectionRow[] }>("/api/catalog/connections"),
      ]);
      setProfiles(p.profiles);
      setManage(p.can_manage);
      setConnections(c.connections);
      setSelected((current) => current || p.profiles[0]?.name || "");
      setError("");
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  const profile = profiles?.find((p) => p.name === selected);
  useEffect(() => {
    if (!profile) return;
    setPolicy(profile.connection_policy ?? {});
    api<{ runs: Run[] }>(`/api/catalog/profiles/${profile.name}/runs`).then((r) => setRuns(r.runs), () => setRuns([]));
  }, [profile]);

  async function act(label: string, work: () => Promise<unknown>) {
    setBusy(label);
    setError("");
    try {
      await work();
      await refresh();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy("");
    }
  }
  const post = <T,>(path: string, body: unknown = {}) => api<T>(path, { method: "POST", body: JSON.stringify(body) });

  if (!profiles) return error ? <div className="alert">{error}</div> : null;
  const kindOf = (name: string) => connections.find((c) => c.name === name)?.kind;
  const unattached = connections.filter((c) => !profile?.connections.includes(c.name));
  const toggle = (kind: string, connection: string, on: boolean) =>
    setPolicy((p) => ({ ...p, [kind]: on ? [...(p[kind] ?? []), connection] : (p[kind] ?? []).filter((c) => c !== connection) }));

  return (
    <section className="contributor-panel" aria-label="Profiles">
      <div className="contributor-heading">
        <div>
          <h2>Profiles</h2>
          <p>How a native CLI runs: its template, tools and skills, and the shared connections it uses. A run states its kind (planner, builder, debug, deploy…) and gets only the connections the profile allows for that kind. Profiles can also be kept in a repository and applied with <code>ebox catalog apply</code>.</p>
        </div>
      </div>
      {error && <div role="alert" className="alert">{error}</div>}
      {profiles.length === 0 ? (
        <div className="host-card"><p>No profiles yet. Create one with <code>POST /v1/profiles</code> or a catalog file.</p></div>
      ) : (
        <div className="profile-pick" role="tablist" aria-label="Profile">
          {profiles.map((p) => (
            <button key={p.name} role="tab" aria-selected={p.name === selected} className={`button small${p.name === selected ? " active" : ""}`} onClick={() => setSelected(p.name)}>{p.name}</button>
          ))}
        </div>
      )}

      {profile && (
        <>
          <div className="host-card">
            <h3>{profile.name} <span className="subtle">revision {profile.revision}</span></h3>
            {profile.description && <p>{profile.description}</p>}
            <p>Template: <strong>{profile.template ?? "any"}</strong></p>
            <div className="catalog-scroll"><table className="table">
              <thead><tr><th>Provider</th><th>Permissions</th><th>Skills and plugins</th><th>Own MCP servers</th><th>Env names</th></tr></thead>
              <tbody>
                {Object.entries(profile.providers).map(([provider, block]) => (
                  <tr key={provider}>
                    <td>{providerNames[provider] ?? provider}</td>
                    <td>{block.permission_profile ?? "edit"}{block.max_turns ? ` · ${block.max_turns} turns` : ""}{block.network_access ? " · network" : ""}{block.allowed_tools && <span className="table-detail">{block.allowed_tools.join(", ")}</span>}</td>
                    <td>{[...(block.skills ?? []).map((s) => `skill ${s.name}`), ...(block.plugins ?? []).map((p) => `plugin ${p.name}`)].join(", ") || "None"}</td>
                    <td>{(block.mcp_servers ?? []).map((s) => s.name).join(", ") || "None"}</td>
                    <td>{(block.env_keys ?? []).join(", ") || "None"}</td>
                  </tr>
                ))}
              </tbody>
            </table></div>
          </div>

          <div className="host-card">
            <h3>Connections</h3>
            <div className="catalog-scroll"><table className="table">
              <thead><tr><th>Connection</th><th>Kind</th><th /></tr></thead>
              <tbody>
                {profile.connections.length === 0 && <tr><td colSpan={3}>No shared connections. Inline MCP servers and env still apply.</td></tr>}
                {profile.connections.map((c) => (
                  <tr key={c}>
                    <td>{c}</td>
                    <td>{kindOf(c) ? kindLabels[kindOf(c)!] : "Unknown"}</td>
                    <td>{manage && <button className="button small" disabled={!!busy} onClick={() => act(`detach:${c}`, () => post(`/api/catalog/profiles/${profile.name}/connections/detach`, { connection: c }))}>Detach</button>}</td>
                  </tr>
                ))}
              </tbody>
            </table></div>
            {manage && unattached.length > 0 && (
              <form onSubmit={(e: FormEvent) => { e.preventDefault(); const kinds = attachKinds.split(/[\s,]+/).filter(Boolean); void act("attach", async () => { await post(`/api/catalog/profiles/${profile.name}/connections/attach`, { connection: attach, ...(kinds.length ? { run_kinds: kinds } : {}) }); setAttach(""); setAttachKinds(""); }); }}>
                <select value={attach} onChange={(e) => setAttach(e.target.value)} required aria-label="Connection to attach">
                  <option value="">Attach a connection…</option>
                  {unattached.map((c) => <option key={c.name} value={c.name}>{c.name} ({kindLabels[c.kind]})</option>)}
                </select>
                <input value={attachKinds} onChange={(e) => setAttachKinds(e.target.value.toLowerCase())} placeholder="Only for run kinds (optional), e.g. debug" aria-label="Run kinds" />
                <button className="button small" type="submit" disabled={busy === "attach"}>Attach</button>
              </form>
            )}
          </div>

          <div className="host-card">
            <h3>Per-run-kind policy</h3>
            <p>A run that states a kind listed here gets exactly the ticked connections. Other kinds use <code>*</code> when it is listed, otherwise every connection. Runs without a kind get every connection.</p>
            <div className="catalog-scroll"><table className="table policy-table">
              <thead><tr><th>Run kind</th><th>Connections</th><th /></tr></thead>
              <tbody>
                {Object.keys(policy).length === 0 && <tr><td colSpan={3}>No policy: every run gets every connection.</td></tr>}
                {Object.entries(policy).map(([kind, list]) => (
                  <tr key={kind}>
                    <td><strong>{kind}</strong></td>
                    <td>
                      {profile.connections.map((c) => (
                        <label key={c}><input type="checkbox" disabled={!manage} checked={list.includes(c)} onChange={(e) => toggle(kind, c, e.target.checked)} />{c}</label>
                      ))}
                      {list.length === 0 && <span className="subtle">none</span>}
                    </td>
                    <td>{manage && <button type="button" className="text-button" onClick={() => setPolicy((p) => Object.fromEntries(Object.entries(p).filter(([k]) => k !== kind)))}>Remove</button>}</td>
                  </tr>
                ))}
              </tbody>
            </table></div>
            {manage && (
              <form onSubmit={(e: FormEvent) => { e.preventDefault(); void act("policy", () => post(`/api/catalog/profiles/${profile.name}/connections/policy`, { connection_policy: policy })); }}>
                <input value={newKind} onChange={(e) => setNewKind(e.target.value.toLowerCase())} placeholder="Run kind, e.g. planner or *" aria-label="New run kind" />
                <button type="button" className="button small" disabled={!newKind || newKind in policy} onClick={() => { setPolicy((p) => ({ ...p, [newKind]: [] })); setNewKind(""); }}>Add kind</button>
                <button className="button small" type="submit" disabled={busy === "policy"}>Save policy</button>
              </form>
            )}
          </div>

          <div className="host-card">
            <h3>Recent runs</h3>
            <div className="catalog-scroll"><table className="table">
              <thead><tr><th>Run</th><th>Status</th><th>Kind</th><th>Connections used</th><th>Started</th></tr></thead>
              <tbody>
                {runs.length === 0 && <tr><td colSpan={5}>No recent runs used this profile.</td></tr>}
                {runs.map((r) => (
                  <tr key={r.id}>
                    <td className="mono">{r.id.slice(0, 8)}{r.provider ? ` · ${providerNames[r.provider] ?? r.provider}` : ""}</td>
                    <td>{r.status}</td>
                    <td>{r.profile?.run_kind ?? "any"}</td>
                    <td>{r.profile?.connections?.map((c) => `${c.name} r${c.revision}${c.skipped ? " (skipped)" : ""}`).join(", ") || "None"}</td>
                    <td>{r.created_at ? new Date(r.created_at).toLocaleString() : ""}</td>
                  </tr>
                ))}
              </tbody>
            </table></div>
          </div>
        </>
      )}
    </section>
  );
}
