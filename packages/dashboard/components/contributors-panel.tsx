'use client';
import { FormEvent, useCallback, useEffect, useState } from 'react';
import { api } from '@/lib/client';
type Budget = { cpu: number; memory_gib: number; disk_gib: number };
type Node = { id: string; label: string; budget: Budget; workloads: { ci: boolean; agents: boolean }; online: boolean; enrolled: boolean; revoked: boolean; blocked: boolean; tools: { ci?: boolean; codex?: boolean; worker_running?: boolean } };
type Enrollment = { id: string; expires: number; command: string };
export function ContributorsPanel() {
  const [nodes, setNodes] = useState<Node[]>([]), [error, setError] = useState(''), [busy, setBusy] = useState(false);
  const [label, setLabel] = useState('Windows workstation'), [budget, setBudget] = useState<Budget>({ cpu: 4, memory_gib: 8, disk_gib: 50 });
  const [ci, setCi] = useState(true), [agents, setAgents] = useState(true), [invitation, setInvitation] = useState<Enrollment>(), [copied, setCopied] = useState(false);
  const refresh = useCallback(async () => {
    try { const result = await api<{ nodes: Node[] }>('/api/contributors'); setNodes(result.nodes); setError(''); }
    catch (e) { setError((e as Error).message); }
  }, []);
  useEffect(() => { void refresh(); const timer = setInterval(() => void refresh(), 5000); return () => clearInterval(timer); }, [refresh]);
  useEffect(() => {
    if (!invitation) return;
    if (nodes.some(n => n.id === invitation.id && (n.enrolled || n.revoked))) { setInvitation(undefined); return; }
    const timer = setTimeout(() => setInvitation(undefined), Math.max(0, invitation.expires * 1000 - Date.now()));
    return () => clearTimeout(timer);
  }, [invitation, nodes]);
  async function issue(event: FormEvent) {
    event.preventDefault(); setBusy(true); setError(''); setCopied(false); setInvitation(undefined);
    try { setInvitation(await api<Enrollment>('/api/contributors/enrollments', { method: 'POST', body: JSON.stringify({ label, budget, workloads: { ci, agents } }) })); await refresh(); }
    catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  }
  async function change(id: string, action: string) {
    setBusy(true); try { await api(`/api/contributors/${id}/${action}`, { method: 'POST', body: JSON.stringify(action === 'reconcile' ? { stopped: true } : {}) }); await refresh(); }
    catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  }
  return <section className="storage-panel" aria-label="Worker machines">
    <div className="section-heading"><div><h2>Worker machines</h2><p>Connect a Windows machine to this controller for CI and native agents.</p></div><button className="button small" onClick={() => void refresh()}>Refresh machines</button></div>
    {error && <div className="alert" role="alert">{error}</div>}
    <form className="host-card" onSubmit={issue}>
      <h3>Add a machine</h3>
      <label>Machine name<input required maxLength={80} value={label} onChange={e => setLabel(e.target.value)} /></label>
      <div className="contributor-resources">
        {([['cpu', 'CPU cores', 1, 256], ['memory_gib', 'Memory (GiB)', 2, 4096], ['disk_gib', 'Disk allowance (GiB)', 10, 65536]] as const).map(([key, text, min, max]) =>
          <label key={key}>{text}<input type="number" min={min} max={max} step={1} required value={budget[key]} onChange={e => setBudget({ ...budget, [key]: Number(e.target.value) })} /></label>)}
      </div>
      <label><input type="checkbox" checked={ci} onChange={e => setCi(e.target.checked)} /> CI builds and checks</label>
      <label><input type="checkbox" checked={agents} onChange={e => setAgents(e.target.checked)} /> Native agents</label>
      <p>One workload runs at a time within the shared CPU and memory limit. Workspace disk use is monitored; work stops at the allowance. Native jobs run as your Windows user. Choose trusted projects.</p>
      <button className="button primary" disabled={busy || (!ci && !agents)}>Create install command</button>
    </form>
    {invitation && <article className="host-card" aria-label="Install this machine">
      <h3>Run once in PowerShell</h3><p>This code expires at {new Date(invitation.expires * 1000).toLocaleTimeString()} and registers one machine. The installer checks the local resource allowance before joining.</p>
      <textarea readOnly aria-label="PowerShell install command" value={invitation.command} rows={5} spellCheck={false} />
      <button className="button primary" onClick={async () => { try { await navigator.clipboard.writeText(invitation.command); setCopied(true); } catch { setError('Clipboard access was denied. Select and copy the command above.'); } }}>{copied ? 'Copied' : 'Copy install command'}</button>
    </article>}
    <div className="host-grid">{nodes.map(n => <article className="host-card" key={n.id} aria-label={`Machine ${n.label}`}>
      <h3>{n.label}</h3><p>{n.revoked ? 'Revoked' : !n.enrolled ? 'Waiting for registration' : n.blocked ? 'Needs reconciliation' : n.online ? 'Online' : 'Offline'}</p>
      <p>{n.budget.cpu} CPU · {n.budget.memory_gib} GiB memory · {n.budget.disk_gib} GiB disk allowance</p>
      {n.workloads.ci && <p>CI: {n.tools.ci ? 'Ready' : 'Waiting for Git and PowerShell'}</p>}
      {n.workloads.agents && <p>Codex: {n.tools.codex ? 'Ready' : 'Waiting for installation and sign-in'}</p>}
      {n.blocked && <button className="button small" disabled={busy || n.tools.worker_running !== false} onClick={() => void change(n.id, 'reconcile')}>Confirm owned worker stopped</button>}
      {!n.revoked && <button className="button small" disabled={busy} onClick={() => void change(n.id, 'revoke')}>Revoke machine</button>}
    </article>)}</div>
  </section>;
}
