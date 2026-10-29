"use client";
import { FormEvent, useCallback, useEffect, useState } from "react";
import { api, errorMessage } from "@/lib/client";
import { RepoRequirementsPanel } from "@/components/repo-requirements-panel";

type ValueRef = { env?: string; secret?: string; secret_source?: string; name?: string; prefix?: string };
type Connection = {
  name: string;
  kind: "mcp_http" | "mcp_stdio" | "secret_source" | "agent_account";
  description?: string;
  url?: string;
  headers?: Record<string, ValueRef>;
  command?: string;
  args?: string[];
  env?: Record<string, ValueRef>;
  template?: string;
  source?: { provider: "stored" | "kitbash"; integration?: string; project?: string; environment?: string };
  names?: string[];
  secret_names: string[];
  used_by: string[];
  revision: number;
  created_by: string;
  updated_at: string;
  last_used_at?: string;
};
type Listing = { connections: Connection[]; role: string; can_manage: boolean };
type TestResult = { ok?: boolean; reachable?: boolean | null; http_status?: number; auth?: string; unresolved?: string[]; needs_run_env?: string[]; present?: string[]; missing?: string[]; error?: string; note?: string };

/** One header or variable row in the form: where its value comes from. */
type Row = { key: string; from: "secret" | "env" | "secret_source"; ref: string; source: string; prefix: string; value: string };
const emptyRow = (): Row => ({ key: "", from: "secret", ref: "", source: "", prefix: "", value: "" });

export const kindLabels = { mcp_http: "MCP (HTTP)", mcp_stdio: "MCP (stdio)", secret_source: "Secret source", agent_account: "Agent account" } as const;

function describeRef(ref: ValueRef) {
  if (ref.env) return `run env ${ref.env}`;
  if (ref.secret) return `${ref.prefix ? `${ref.prefix.trim()} ` : ""}secret ${ref.secret}`;
  return `${ref.prefix ? `${ref.prefix.trim()} ` : ""}${ref.secret_source}.${ref.name}`;
}
function target(c: Connection) {
  if (c.kind === "mcp_http") return c.url;
  if (c.kind === "mcp_stdio") return [c.command, ...(c.args ?? [])].join(" ");
  if (c.source?.provider === "kitbash") return `Kitbash ${c.source.integration}${c.source.project ? ` · ${c.source.project}` : ""}${c.source.environment ? `/${c.source.environment}` : ""}`;
  return `Stored${c.source?.environment ? ` · ${c.source.environment}` : ""}`;
}
function when(value?: string) {
  return value ? new Date(value).toLocaleString() : "Never";
}

/** Turn form rows into references plus the write-only values to send once. */
function rowsToRefs(rows: Row[]) {
  const refs: Record<string, ValueRef> = {};
  const secrets: Record<string, string> = {};
  for (const r of rows) {
    if (!r.key.trim()) continue;
    const prefix = r.prefix ? { prefix: r.prefix } : {};
    if (r.from === "env") refs[r.key] = { env: r.ref };
    else if (r.from === "secret_source") refs[r.key] = { secret_source: r.source, name: r.ref, ...prefix };
    else {
      refs[r.key] = { secret: r.ref, ...prefix };
      if (r.value) secrets[r.ref] = r.value;
    }
  }
  return { refs, secrets };
}

function RefRows({ rows, setRows, keyLabel, sources }: { rows: Row[]; setRows: (rows: Row[]) => void; keyLabel: string; sources: string[] }) {
  const update = (i: number, patch: Partial<Row>) => setRows(rows.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  return (
    <div className="connection-rows">
      {rows.map((r, i) => (
        <div className="connection-row" key={i}>
          <input value={r.key} onChange={(e) => update(i, { key: e.target.value })} placeholder={keyLabel} aria-label={keyLabel} />
          <select value={r.from} onChange={(e) => update(i, { from: e.target.value as Row["from"] })} aria-label="Value from">
            <option value="secret">Secret (stored here)</option>
            <option value="secret_source">Secret source</option>
            <option value="env">Run env</option>
          </select>
          {r.from === "secret_source" && (
            <select value={r.source} onChange={(e) => update(i, { source: e.target.value })} aria-label="Secret source">
              <option value="">Choose a source</option>
              {sources.map((s) => <option key={s} value={s}>{s}</option>)}
            </select>
          )}
          <input value={r.ref} onChange={(e) => update(i, { ref: e.target.value.toUpperCase() })} placeholder={r.from === "env" ? "VARIABLE" : "SECRET_NAME"} aria-label="Name" />
          {r.from !== "env" && <input value={r.prefix} onChange={(e) => update(i, { prefix: e.target.value })} placeholder="Prefix, e.g. Bearer " aria-label="Prefix" />}
          {r.from === "secret" && <input type="password" autoComplete="new-password" value={r.value} onChange={(e) => update(i, { value: e.target.value })} placeholder="Value (write-only)" aria-label="Secret value" />}
          <button type="button" className="text-button" onClick={() => setRows(rows.filter((_, j) => j !== i))}>Remove</button>
        </div>
      ))}
      <button type="button" className="text-button" onClick={() => setRows([...rows, emptyRow()])}>Add {keyLabel.toLowerCase()}</button>
    </div>
  );
}

/**
 * The org's connections catalog: MCP servers and secret sources that profiles
 * share by name. Secret values are write-only; this page never shows one.
 */
export function ConnectionsPanel() {
  const [listing, setListing] = useState<Listing | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState("");
  const [tests, setTests] = useState<Record<string, TestResult>>({});
  // Create form.
  const [name, setName] = useState("");
  const [kind, setKind] = useState<"mcp_http" | "mcp_stdio" | "secret_source">("mcp_http");
  const [description, setDescription] = useState("");
  const [url, setUrl] = useState("");
  const [command, setCommand] = useState("");
  const [args, setArgs] = useState("");
  const [template, setTemplate] = useState("");
  const [rows, setRows] = useState<Row[]>([emptyRow()]);
  const [provider, setProvider] = useState<"stored" | "kitbash">("stored");
  const [integration, setIntegration] = useState("");
  const [project, setProject] = useState("");
  const [environment, setEnvironment] = useState("");
  const [names, setNames] = useState("");
  const [stored, setStored] = useState<{ name: string; value: string }[]>([{ name: "", value: "" }]);
  // Set one secret on an existing connection.
  const [secretFor, setSecretFor] = useState("");
  const [secretName, setSecretName] = useState("");
  const [secretValue, setSecretValue] = useState("");

  const refresh = useCallback(async () => {
    try {
      setListing(await api<Listing>("/api/catalog/connections"));
      setError("");
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);
  useEffect(() => {
    void refresh();
  }, [refresh]);

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

  function resetForm() {
    setName(""); setDescription(""); setUrl(""); setCommand(""); setArgs(""); setTemplate("");
    setRows([emptyRow()]); setIntegration(""); setProject(""); setEnvironment(""); setNames("");
    setStored([{ name: "", value: "" }]);
  }
  function create(e: FormEvent) {
    e.preventDefault();
    const body: Record<string, unknown> = { name, kind, ...(description ? { description } : {}) };
    if (kind === "mcp_http" || kind === "mcp_stdio") {
      const { refs, secrets } = rowsToRefs(rows);
      if (kind === "mcp_http") Object.assign(body, { url, headers: refs });
      else Object.assign(body, { command, args: args.split("\n").map((a) => a.trim()).filter(Boolean), env: refs, ...(template ? { template } : {}) });
      if (Object.keys(secrets).length) body.secrets = secrets;
    } else {
      body.source = { provider, ...(provider === "kitbash" ? { integration } : {}), ...(project ? { project } : {}), ...(environment ? { environment } : {}) };
      if (provider === "kitbash") body.names = names.split(/[\s,]+/).filter(Boolean);
      else body.secrets = Object.fromEntries(stored.filter((s) => s.name && s.value).map((s) => [s.name, s.value]));
    }
    void act("create", async () => {
      await post("/api/catalog/connections", body);
      resetForm();
    });
  }

  if (!listing) return error ? <div className="alert">{error}</div> : null;
  const manage = listing.can_manage;
  const sources = listing.connections.filter((c) => c.kind === "secret_source").map((c) => c.name);

  return (
    <section className="contributor-panel" aria-label="Connections">
      <div className="contributor-heading">
        <div>
          <h2>Connections</h2>
          <p>MCP servers and secret sources this organization's profiles share. Edit a connection once and every profile that uses it picks the change up on its next run. Secret values are write-only: Engine Box stores them and never shows them again.</p>
        </div>
      </div>
      {error && <div role="alert" className="alert">{error}</div>}
      <div className="host-card">
        <div className="catalog-scroll"><table className="table">
          <thead><tr><th>Name</th><th>Kind</th><th>Target</th><th>Values</th><th>Used by</th><th>Last used</th><th /></tr></thead>
          <tbody>
            {listing.connections.length === 0 && <tr><td colSpan={7}>No connections yet. Add an MCP server or a secret source below.</td></tr>}
            {listing.connections.map((c) => {
              const result = tests[c.name];
              const refs = { ...(c.headers ?? {}), ...(c.env ?? {}) };
              return (
                <tr key={c.name}>
                  <td><strong>{c.name}</strong>{c.description && <span className="table-detail">{c.description}</span>}</td>
                  <td>{kindLabels[c.kind]}</td>
                  <td className="mono">{target(c)}</td>
                  <td>
                    {Object.entries(refs).map(([k, r]) => <span key={k} className="table-detail">{k}: {describeRef(r)}</span>)}
                    {c.kind === "secret_source" && <span className="table-detail">Yields {(c.source?.provider === "kitbash" ? c.names : c.secret_names)?.join(", ") || "nothing yet"}</span>}
                    {c.kind !== "secret_source" && c.secret_names.length > 0 && <span className="table-detail">Stored: {c.secret_names.join(", ")}</span>}
                  </td>
                  <td>{c.used_by.length ? c.used_by.join(", ") : "No profiles"}</td>
                  <td>{when(c.last_used_at)}</td>
                  <td>
                    {manage && <button className="button small" disabled={!!busy} onClick={() => act(`test:${c.name}`, async () => { const r = await post<{ test: TestResult }>(`/api/catalog/connections/${c.name}/test`); setTests((t) => ({ ...t, [c.name]: r.test })); })}>{busy === `test:${c.name}` ? "Testing…" : "Test"}</button>}
                    {manage && <button className="button small" disabled={!!busy} onClick={() => { setSecretFor(c.name); setSecretName(""); setSecretValue(""); }}>Set secret</button>}
                    {manage && <button className="button small" disabled={!!busy || c.used_by.length > 0} title={c.used_by.length ? "Detach it from its profiles first" : undefined} onClick={() => window.confirm(`Delete ${c.name}? Its stored secrets are deleted.`) && act(`delete:${c.name}`, () => post(`/api/catalog/connections/${c.name}/delete`))}>Delete</button>}
                    {result && (
                      <span className={`table-detail ${result.ok ? "status-ok" : "status-bad"}`} role="status">
                        {result.ok ? "Working" : "Not working"}
                        {result.http_status !== undefined && ` · HTTP ${result.http_status}`}
                        {result.auth && ` · auth ${result.auth}`}
                        {result.missing?.length ? ` · missing ${result.missing.join(", ")}` : ""}
                        {result.unresolved?.length ? ` · unresolved ${result.unresolved.join(", ")}` : ""}
                        {result.needs_run_env?.length ? ` · needs run env ${result.needs_run_env.join(", ")}` : ""}
                        {result.error && ` · ${result.error}`}
                        {result.note && ` · ${result.note}`}
                      </span>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table></div>
      </div>

      <RepoRequirementsPanel sources={sources} mcps={listing.connections.filter((c) => c.kind === "mcp_http" || c.kind === "mcp_stdio").map((c) => c.name)} />

      {manage && secretFor && (
        <div className="host-card">
          <h3>Set a secret on {secretFor}</h3>
          <p>The value is sent once and never shown again. A header or variable must reference the name.</p>
          <form onSubmit={(e) => { e.preventDefault(); void act("secret", async () => { await post(`/api/catalog/connections/${secretFor}`, { secrets: { [secretName]: secretValue } }); setSecretValue(""); setSecretFor(""); }); }}>
            <input value={secretName} onChange={(e) => setSecretName(e.target.value.toUpperCase())} required placeholder="SECRET_NAME" aria-label="Secret name" />
            <input type="password" autoComplete="new-password" value={secretValue} onChange={(e) => setSecretValue(e.target.value)} required placeholder="Value (write-only)" aria-label="Secret value" />
            <button className="button small" type="submit" disabled={busy === "secret"}>Save secret</button>
            <button className="text-button" type="button" onClick={() => { setSecretFor(""); setSecretValue(""); }}>Cancel</button>
          </form>
        </div>
      )}

      {manage && (
        <div className="host-card">
          <h3>Add a connection</h3>
          <form className="connection-form" onSubmit={create}>
            <div className="connection-row">
              <input value={name} onChange={(e) => setName(e.target.value.toLowerCase())} required pattern="[a-z][a-z0-9-]{0,62}" placeholder="Name, e.g. kitbash" aria-label="Connection name" />
              <select value={kind} onChange={(e) => setKind(e.target.value as typeof kind)} aria-label="Kind">
                <option value="mcp_http">MCP server (HTTP)</option>
                <option value="mcp_stdio">MCP server (stdio, from the template)</option>
                <option value="secret_source">Secret source</option>
              </select>
              <input value={description} onChange={(e) => setDescription(e.target.value)} maxLength={512} placeholder="Description" aria-label="Description" />
            </div>
            {kind === "mcp_http" && (
              <>
                <input value={url} onChange={(e) => setUrl(e.target.value)} required type="url" placeholder="https://kitbash.example.com/mcp" aria-label="MCP URL" />
                <RefRows rows={rows} setRows={setRows} keyLabel="Header" sources={sources} />
              </>
            )}
            {kind === "mcp_stdio" && (
              <>
                <div className="connection-row">
                  <input value={command} onChange={(e) => setCommand(e.target.value)} required placeholder="/usr/local/bin/server" aria-label="Command" />
                  <input value={template} onChange={(e) => setTemplate(e.target.value)} placeholder="Template that installs it (optional)" aria-label="Template" />
                </div>
                <textarea value={args} onChange={(e) => setArgs(e.target.value)} rows={2} placeholder="Arguments, one per line" aria-label="Arguments" />
                <RefRows rows={rows} setRows={setRows} keyLabel="Variable" sources={sources} />
              </>
            )}
            {kind === "secret_source" && (
              <>
                <div className="connection-row">
                  <select value={provider} onChange={(e) => setProvider(e.target.value as typeof provider)} aria-label="Provider">
                    <option value="stored">Values stored here</option>
                    <option value="kitbash">Kitbash delivery</option>
                  </select>
                  {provider === "kitbash" && <input value={integration} onChange={(e) => setIntegration(e.target.value)} required placeholder="Integration, e.g. canvalytics-staging" aria-label="Kitbash integration" />}
                  <input value={project} onChange={(e) => setProject(e.target.value)} placeholder="Project" aria-label="Project" />
                  <input value={environment} onChange={(e) => setEnvironment(e.target.value)} placeholder="Environment, e.g. staging" aria-label="Environment" />
                </div>
                {provider === "kitbash" ? (
                  <input value={names} onChange={(e) => setNames(e.target.value.toUpperCase())} required placeholder="Names it yields: CLOUDFLARE_API_TOKEN, DATABASE_URL" aria-label="Names" />
                ) : (
                  <div className="connection-rows">
                    {stored.map((s, i) => (
                      <div className="connection-row" key={i}>
                        <input value={s.name} onChange={(e) => setStored(stored.map((x, j) => (j === i ? { ...x, name: e.target.value.toUpperCase() } : x)))} placeholder="NAME" aria-label="Secret name" />
                        <input type="password" autoComplete="new-password" value={s.value} onChange={(e) => setStored(stored.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))} placeholder="Value (write-only)" aria-label="Secret value" />
                      </div>
                    ))}
                    <button type="button" className="text-button" onClick={() => setStored([...stored, { name: "", value: "" }])}>Add value</button>
                  </div>
                )}
              </>
            )}
            <button className="button small" type="submit" disabled={busy === "create"}>Add connection</button>
          </form>
        </div>
      )}
    </section>
  );
}
