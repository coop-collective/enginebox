"use client";
import { useCallback, useEffect, useState } from "react";
import { api } from "@/lib/client";
import { preparationWorkflow } from "@/lib/github-preparation";

type Repository = { id: number; installation_id: number; full_name: string; private: boolean; branch: string; default_branch?: string; permissions?: { push?: boolean } };
type Installation = { id: number; account: string; permissions: Record<string, string>; suspended: boolean };
type Status = { configured: boolean; shared_app?: boolean; runner_app_configured: boolean; accounts: { kind: string; login: string }[]; repositories: Repository[]; draining: boolean; pools: { id: string; organization: string; labels: string[]; runner_group_id: number; max_runners: number; preparation?: { repository_id: number; workflows: string[] } | null }[]; jobs: { id: number; repository: string; status: string; github_status: string; error?: string }[] };
const base = "/api/control/v1/github";
export function GitHubPanel() {
  const [status, setStatus] = useState<Status>();
  const [error, setError] = useState(""); const [busy, setBusy] = useState(false);
  const [installations, setInstallations] = useState<Installation[]>([]);
  const [installUrl, setInstallUrl] = useState(""); const [runnerInstallUrl, setRunnerInstallUrl] = useState("");
  const [runnerInstallations, setRunnerInstallations] = useState<Installation[]>([]);
  const [installation, setInstallation] = useState(""); const [repos, setRepos] = useState<Repository[]>([]);
  const [profile, setProfile] = useState(""); const [groupName, setGroupName] = useState("");
  const [actionRef, setActionRef] = useState(""); const [checkoutRef, setCheckoutRef] = useState("");
  const [workflow, setWorkflow] = useState("");
  const [repoId, setRepoId] = useState(""); const [branch, setBranch] = useState("");
  const refresh = useCallback(async () => { setStatus(await api<Status>(base)); }, []);
  useEffect(() => { void refresh().catch(e => setError(e.message)); }, [refresh]);
  async function work(fn: () => Promise<void>) { setBusy(true); setError(""); try { await fn(); await refresh(); } catch (e) { setError((e as Error).message); } finally { setBusy(false); } }
  async function post(path: string, body: object) { return api<Record<string, unknown>>(`${base}/${path}`, { method: "POST", body: JSON.stringify(body) }); }
  async function connect(kind: string) { const result = await api<{ url: string }>("/api/github/connect", { method: "POST", body: JSON.stringify({ kind }) }); window.location.assign(result.url); }
  async function loadInstallations(kind: string) {
    const result = await api<{ installations: Installation[]; install_url: string }>(`${base}/${kind}/installations`);
    if (kind === "repos") { setInstallations(result.installations); setInstallUrl(result.install_url); }
    else { setRunnerInstallations(result.installations); setRunnerInstallUrl(result.install_url); }
  }
  const accountKind = status?.shared_app ? "github" : "repos";
  const runnerAccountKind = status?.shared_app ? "github" : "runners";
  return <section className="github-panel" aria-label="GitHub">
    {error && <p className="alert" role="alert">{error}</p>}
    {!status ? <p>Loading GitHub connections…</p> : !status.configured ? <article className="github-card"><h2>Connect GitHub</h2><p>Configure your GitHub App on your controller to connect an account and select private repositories.</p></article> : <>
      <article className="github-card"><h2>{status.shared_app ? "GitHub connection" : "Repository connection"}</h2>{status.shared_app && <p>One App installation and account connection support repository sync and Actions runners.</p>}<p>Choose repositories to import into LayerFS and sync accepted main revisions back to GitHub.</p>
        <div className="github-actions"><button className="button primary" disabled={busy} onClick={() => void work(() => connect("repos"))}>{status.accounts.some(a => a.kind === accountKind) ? "Reconnect GitHub" : "Connect GitHub account"}</button>
        {status.accounts.filter(a => a.kind === accountKind).map(a => <span key={a.kind}>Connected as {a.login}</span>)}
        <button className="button" disabled={busy || !status.accounts.some(a => a.kind === accountKind)} onClick={() => void work(() => loadInstallations("repos"))}>Choose repositories</button>
        <button className="button" disabled={busy || !status.accounts.some(a => a.kind === accountKind)} onClick={() => void work(async () => { await post("repos/disconnect", {}); setRepos([]); setInstallations([]); setRunnerInstallations([]); setRunnerInstallUrl(""); setInstallUrl(""); setInstallation(""); setRepoId(""); setBranch(""); })}>{status.shared_app ? "Disconnect GitHub" : "Disconnect repository account"}</button></div>
        {installUrl && <p><a href={installUrl} target="_blank" rel="noreferrer">Install App or update repository access on GitHub</a></p>}
        {installations.map(i => <p key={i.id}>{i.account}: {i.permissions.contents === "write" && !i.suspended ? "Repository sync approved" : "Contents read/write approval required for repository sync"}</p>)}
        {installations.length > 0 && <div className="github-fields"><label>GitHub installation<select value={installation} disabled={busy} onChange={e => { const id = e.target.value; setInstallation(id); setRepoId(""); setBranch(""); setRepos([]); if (id) void work(async () => setRepos((await api<{ repositories: Repository[] }>(`${base}/installations/${id}/repositories`)).repositories)); }}><option value="">Select an account or organization</option>{installations.filter(i => !i.suspended).map(i => <option value={i.id} key={i.id}>{i.account}</option>)}</select></label>
        <label>Repository<select value={repoId} disabled={busy} onChange={e => { setRepoId(e.target.value); setBranch(repos.find(r => String(r.id) === e.target.value)?.default_branch ?? ""); }}><option value="">Select a repository</option>{repos.map(r => <option key={r.id} value={r.id} disabled={!r.permissions?.push}>{r.full_name}{r.private ? " (private)" : ""}{!r.permissions?.push ? " — read only" : ""}</option>)}</select></label>
        <label>Sync branch<input value={branch} onChange={e => setBranch(e.target.value)} /></label><button className="button primary" disabled={busy || !repoId || !branch} onClick={() => void work(async () => { await post("select", { installation_id: Number(installation), repository_id: Number(repoId), branch }); })}>Add repository</button></div>}
      </article>
      <article className="github-card"><h2>Selected repositories</h2>{!status.repositories.length && <p>No repositories selected.</p>}
        {status.repositories.map(repo => <div className="github-repo" key={repo.id}><div><strong>{repo.full_name}</strong><p>{repo.private ? "Private" : "Public"} · {repo.branch}</p></div><div className="github-actions"><a className="button" href={`/?panel=workspaces&storage=1&github_repository=${repo.id}`}>Prepare source and sync</a><button className="button" disabled={busy} onClick={() => void work(async () => { await post("remove", { repository_id: repo.id }); })}>Remove selection</button></div></div>)}
        <p>LayerFS owns project files and accepted main revisions. Open workspace source and storage controls to review imports, publish accepted changes, and resolve sync conflicts. Removing a selection retains project files.</p>
      </article>
      <article className="github-card"><h2>GitHub Actions runners</h2><p>{status.shared_app ? "Your connected GitHub App also manages runners once the organization approves Self-hosted runners read/write and Actions read." : "The configured runner App requests organization runner administration."} Workflows opt in with the pool label and runner group. Existing runners remain available during migration.</p>
        {!status.runner_app_configured ? <p>Configure runner access and a disposable CI worker to enable Actions.</p> : <div className="github-actions">{!status.shared_app && <button className="button primary" disabled={busy} onClick={() => void work(() => connect("runners"))}>Connect runner administration</button>}<button className="button" disabled={busy || !status.accounts.some(a => a.kind === runnerAccountKind)} onClick={() => void work(() => loadInstallations("runners"))}>Review runner permissions</button></div>}
        {runnerInstallUrl && <p><a href={runnerInstallUrl} target="_blank" rel="noreferrer">{status.shared_app ? "Approve permissions for the connected App on GitHub" : "Install runner App or approve organization permissions"}</a></p>}
        {runnerInstallations.map(i => <p key={i.id}>{i.account}: {i.permissions.organization_self_hosted_runners === "write" && ["read", "write"].includes(i.permissions.actions) && !i.suspended ? "Runner administration approved" : "Organization approval required"}</p>)}
        {status.pools.map(p => <div className="github-repo" key={p.id}><strong>{p.organization} / {p.id}</strong><p>Group {p.runner_group_id} · up to {p.max_runners} runners</p><code>{p.labels.join(", ")}</code><p>{p.preparation ? "LayerFS preparation required" : "Standard disposable runner"}</p>{p.preparation && <p>Repository {p.preparation.repository_id} · {p.preparation.workflows.length} approved workflows</p>}</div>)}
        {status.pools.some(p => p.preparation) && <div className="github-fields">
          <label>Prepared runner profile<select value={profile} onChange={e => { setProfile(e.target.value); setWorkflow(""); }}><option value="">Select a profile</option>{status.pools.filter(p => p.preparation).map(p => <option key={p.id} value={p.id}>{p.organization} / {p.id} · repository {p.preparation!.repository_id}</option>)}</select></label>
          <label>Runner group name<input value={groupName} onChange={e => setGroupName(e.target.value)} /></label>
          <label>Enginebox action at commit SHA<input placeholder="owner/repo/packages/enginebox-actions@40-character-sha" value={actionRef} onChange={e => setActionRef(e.target.value)} /></label>
          <label>Checkout action at commit SHA<input placeholder="actions/checkout@40-character-sha" value={checkoutRef} onChange={e => setCheckoutRef(e.target.value)} /></label>
          <button className="button" disabled={!profile} onClick={() => { try { const pool = status.pools.find(p => p.id === profile)!; setWorkflow(preparationWorkflow({ group: groupName, label: `enginebox-${pool.id}`, action: actionRef, checkout: checkoutRef })); setError(""); } catch (e) { setError((e as Error).message); } }}>Preview prepared workflow</button>
          {workflow && <><p>Review this fragment in a workflow PR. Preserve required checkout options, replace the dependency install step, and review redundant package cache restores. The selected group must approve the destination workflow. This recipe explicitly disables install scripts.</p><label>Prepared workflow fragment<textarea rows={22} readOnly value={workflow} /></label><a className="button" download="enginebox-workflow-fragment.yml" href={`data:text/yaml;charset=utf-8,${encodeURIComponent(workflow)}`}>Download workflow fragment</a></>}
        </div>}
        <div className="github-actions"><button className="button" disabled={busy} onClick={() => void work(async () => { await post("drain", { draining: !status.draining }); })}>{status.draining ? "Resume runner provisioning" : "Drain runners"}</button><button className="button" disabled={busy} onClick={() => void work(refresh)}>Refresh GitHub status</button></div>
        <p>{status.draining ? "Provisioning paused; active runners finish their jobs." : status.pools.length ? "Provisioning enabled for configured pools." : "Configure a runner pool to start running jobs."}</p>
        {status.jobs.map(j => <div className="github-repo" key={j.id}><strong>{j.repository} · job {j.id}</strong><p>{j.status} · GitHub: {j.github_status}</p>{j.error && <p role="alert">{j.error}</p>}</div>)}
      </article>
    </>}
  </section>;
}
