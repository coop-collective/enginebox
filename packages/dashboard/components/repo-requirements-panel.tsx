"use client";
import { useCallback, useEffect, useState } from "react";
import { api, errorMessage } from "@/lib/client";

type Repo = { project: string; name: string };
type Requirement = { kind: "secret" | "mcp" | "mcp_server"; name: string; qualifier: string; run_kinds: string[]; required: boolean; status: "met" | "unmet" | "proposed" | "needs_reapproval"; mapping: { connection?: string; source_name?: string; qualifier: string; proposed_by: string; approved_by?: string } | null };
type Generation = { run_id: string; state: "running" | "succeeded" | "failed"; started_at: string; failure?: string; branch?: string; pr_url?: string; pr_number?: number; pr_error?: string };
type Report = { configured: boolean; source?: "repository" | "registered"; generation?: Generation | null; commit?: string | null; registered_at?: string; profiles: Record<string, { default: boolean; description?: string | null; requirements: Requirement[] }>; dev?: Requirement[]; can_manage: boolean };

const statusLabel = { met: "Met", unmet: "Unmet", proposed: "Proposed", needs_reapproval: "Needs re-approval" } as const;

/**
 * Each repository's declared requirements (.enginebox/config.json schema 2)
 * and whether an approved mapping satisfies them. A repository only declares;
 * owners and admins approve which connection satisfies each requirement.
 */
export function RepoRequirementsPanel({ sources, mcps }: { sources: string[]; mcps: string[] }) {
  const [repos, setRepos] = useState<Repo[]>([]);
  const [selected, setSelected] = useState("");
  const [report, setReport] = useState<Report | null>(null);
  const [choice, setChoice] = useState<Record<string, string>>({});
  const [error, setError] = useState("");
  const [busy, setBusy] = useState("");

  useEffect(() => {
    api<{ repositories: Repo[] }>("/api/catalog/repositories").then(
      (r) => { setRepos(r.repositories); setSelected((s) => s || (r.repositories[0] ? `${r.repositories[0].project}/${r.repositories[0].name}` : "")); },
      () => setRepos([]),
    );
  }, []);
  const load = useCallback(async () => {
    if (!selected) return;
    try {
      setReport(await api<Report>(`/api/catalog/repositories/${selected}`));
      setError("");
    } catch (e) {
      setError(errorMessage(e));
    }
  }, [selected]);
  useEffect(() => { void load(); }, [load]);
  // A running generation settles on the controller; check again until it does.
  const running = report?.generation?.state === "running";
  useEffect(() => {
    if (!running) return;
    const timer = setInterval(() => void load(), 10_000);
    return () => clearInterval(timer);
  }, [running, load]);

  async function generate() {
    setBusy("generate");
    setError("");
    try {
      await api(`/api/catalog/repositories/${selected}/generate`, { method: "POST", body: JSON.stringify({}) });
      await load();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy("");
    }
  }

  async function approve(profile: string, r: Requirement) {
    const key = `${profile}:${r.kind}:${r.name}`;
    const connection = choice[key] ?? r.mapping?.connection ?? (r.kind === "mcp" ? r.name : "");
    setBusy(key);
    setError("");
    try {
      await api(`/api/catalog/repositories/${selected}/approve`, { method: "POST", body: JSON.stringify({ repo_profile: profile, kind: r.kind, requirement: r.name, ...(r.kind === "mcp_server" ? {} : { connection }) }) });
      await load();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy("");
    }
  }

  if (repos.length === 0) return null;
  const groups = report ? [...Object.entries(report.profiles).map(([name, p]) => ({ name, label: `${name}${p.default ? " (default)" : ""}`, requirements: p.requirements })), ...(report.dev?.length ? [{ name: ":dev", label: "dev environment", requirements: report.dev }] : [])] : [];
  return (
    <div className="host-card">
      <h3>Repository requirements</h3>
      <p>What each repository's <code>.enginebox/config.json</code> asks for. A repository only declares names and qualifiers; it is satisfied only by a mapping an owner or admin approved for that qualifier. Changing a qualifier in the repository needs a new approval.</p>
      {error && <div role="alert" className="alert">{error}</div>}
      <select value={selected} onChange={(e) => setSelected(e.target.value)} aria-label="Repository">
        {repos.map((r) => <option key={`${r.project}/${r.name}`} value={`${r.project}/${r.name}`}>{r.project}/{r.name}</option>)}
      </select>
      {report && report.source !== "repository" && <GenerateBanner report={report} busy={!!busy} onGenerate={generate} />}
      {report?.configured && (
        <div className="catalog-scroll">
          <table className="table">
            <thead><tr><th>Profile</th><th>Requirement</th><th>Qualifier</th><th>Run kinds</th><th>Status</th><th>Satisfied by</th></tr></thead>
            <tbody>
              {groups.flatMap((g) => g.requirements.map((r) => {
                const key = `${g.name}:${r.kind}:${r.name}`;
                const options = r.kind === "secret" ? sources : mcps;
                return (
                  <tr key={key}>
                    <td>{g.label}</td>
                    <td><strong>{r.name}</strong><span className="table-detail">{r.kind === "secret" ? "secret" : r.kind === "mcp" ? "MCP connection" : "repository MCP server"}{r.required ? " · required" : ""}</span></td>
                    <td className="mono">{r.qualifier}</td>
                    <td>{r.run_kinds.length ? r.run_kinds.join(", ") : "all"}</td>
                    <td className={r.status === "met" ? "status-ok" : "status-bad"}>{statusLabel[r.status]}{r.status === "needs_reapproval" && r.mapping && <span className="table-detail">approved for {r.mapping.qualifier}</span>}{r.status === "proposed" && r.mapping && <span className="table-detail">by {r.mapping.proposed_by}</span>}</td>
                    <td>
                      {r.mapping?.connection && <span className="table-detail">{r.mapping.connection}{r.mapping.source_name ? `.${r.mapping.source_name}` : ""}</span>}
                      {report.can_manage && r.status !== "met" && (
                        <>
                          {r.kind !== "mcp_server" && (
                            <select value={choice[key] ?? r.mapping?.connection ?? ""} onChange={(e) => setChoice((c) => ({ ...c, [key]: e.target.value }))} aria-label={`Connection for ${r.name}`}>
                              <option value="">Choose…</option>
                              {options.map((o) => <option key={o} value={o}>{o}</option>)}
                            </select>
                          )}
                          <button className="button small" disabled={!!busy} onClick={() => approve(g.name, r)}>Approve</button>
                        </>
                      )}
                    </td>
                  </tr>
                );
              }))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

/**
 * A repository without its own .enginebox/config.json: owners and admins can
 * have an agent write one to a branch for review (a pull request on GitHub).
 */
function GenerateBanner({ report, busy, onGenerate }: { report: Report; busy: boolean; onGenerate: () => void }) {
  const g = report.generation;
  return (
    <div className="alert" role="region" aria-label="Agent config">
      <strong>No agent config</strong>
      <p>
        {report.configured
          ? "This repository has no .enginebox/config.json of its own; runs use a config registered through the API."
          : "This repository has no .enginebox/config.json, so runs get no setup, test or dev commands and no declared secrets."}{" "}
        An agent can inspect the repository and propose one for review. Nothing applies until it is merged.
      </p>
      {g?.state === "running" && <p role="status">Generating… (run <span className="mono">{g.run_id.slice(0, 8)}</span>)</p>}
      {g?.state === "succeeded" && g.pr_url && <p role="status">Ready for review: <a href={g.pr_url} target="_blank" rel="noreferrer">pull request #{g.pr_number}</a></p>}
      {g?.state === "succeeded" && !g.pr_url && !g.pr_error && <p role="status">Ready for review on branch <span className="mono">{g.branch}</span>.</p>}
      {g?.state === "succeeded" && g.pr_error && <p role="status">The config is on branch <span className="mono">{g.branch}</span>, but no pull request was opened: {g.pr_error}</p>}
      {g?.state === "failed" && <p role="status">Generation failed: {g.failure}</p>}
      {report.can_manage && g?.state !== "running" && (
        <button className="button small" disabled={busy} onClick={onGenerate}>{g ? "Generate again" : "Generate"}</button>
      )}
    </div>
  );
}
