'use client';
import { FormEvent, useCallback, useEffect, useState } from 'react';
import { api } from '@/lib/client';
type Budget = { cpu: number; memory_gib: number; disk_gib: number };
type Node = { id: string; label: string; budget: Budget; workloads: { ci: boolean; agents: boolean }; online: boolean; enrolled: boolean; revoked: boolean; blocked: boolean; tools: { ci?: boolean; workspace?: boolean; backend?: string; worker_running?: boolean } };
type Enrollment = { id: string; expires: number; code: string; command: string };
export function ContributorsPanel() {
  const [nodes, setNodes] = useState<Node[]>([]), [error, setError] = useState(''), [busy, setBusy] = useState(false);
  const [label, setLabel] = useState('Windows workstation');
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
    try { setInvitation(await api<Enrollment>('/api/contributors/enrollments', { method: 'POST', body: JSON.stringify({ label, workloads: { ci, agents } }) })); await refresh(); }
    catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  }
  async function change(id: string, action: string) {
    setBusy(true); try { await api(`/api/contributors/${id}/${action}`, { method: 'POST', body: JSON.stringify(action === 'reconcile' ? { stopped: true } : {}) }); await refresh(); }
    catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  }
  async function renew(id: string) {
    setBusy(true); setError(''); setCopied(false); setInvitation(undefined);
    try { setInvitation(await api<Enrollment>(`/api/contributors/${id}/renew-enrollment`, { method: 'POST', body: '{}' })); await refresh(); }
    catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  }
  return <section className="contributor-panel" aria-label="Worker machines">
    <div className="contributor-heading"><div><h2>Worker machines</h2><p>Connect a Windows machine to this controller for template workspaces in WSL2.</p></div><button className="button small" onClick={() => void refresh()}>Refresh machines</button></div>
    {error && <div className="alert" role="alert">{error}</div>}
    <form className="host-card contributor-form" onSubmit={issue}>
      <h3>Add a machine</h3>
      <label>Machine name<input required maxLength={80} value={label} onChange={e => setLabel(e.target.value)} /></label>
      <label className="contributor-choice"><input type="checkbox" checked={ci} onChange={e => setCi(e.target.checked)} /> CI builds and checks</label>
      <label className="contributor-choice"><input type="checkbox" checked={agents} onChange={e => setAgents(e.target.checked)} /> Agents in templates</label>
      <p>The installer asks for CPU, memory and disk limits on the Windows machine. It enables WSL2 when needed and installs the Linux runtime and template tools. Provider sign-in happens inside a workspace.</p>
      <button className="button primary" disabled={busy || (!ci && !agents)}>Create install command</button>
    </form>
    {invitation && <article className="host-card" aria-label="Install this machine">
      <h3>Run once in PowerShell</h3><p>This code expires at {new Date(invitation.expires * 1000).toLocaleTimeString()} and registers one machine. Choose resources in the installer. To retry a failed registration, renew the code on the existing machine below. Match its ID to the pending identity saved by the installer.</p>
      <p>Machine ID: <code>{invitation.id}</code></p>
      <label>Registration code for an installer already open<input readOnly aria-label="Registration code" value={invitation.code} /></label>
      <textarea readOnly aria-label="PowerShell install command" value={invitation.command} rows={5} spellCheck={false} />
      <button className="button primary" onClick={async () => { try { await navigator.clipboard.writeText(invitation.command); setCopied(true); } catch { setError('Clipboard access was denied. Select and copy the command above.'); } }}>{copied ? 'Copied' : 'Copy install command'}</button>
    </article>}
    <div className="host-grid">{nodes.map(n => <article className="host-card" key={n.id} aria-label={`Machine ${n.label}`}>
      <h3>{n.label}</h3><p>{n.revoked ? 'Revoked' : !n.enrolled ? 'Waiting for registration' : n.blocked ? 'Needs reconciliation' : n.online ? 'Online' : 'Offline'}</p>
      <p>Machine ID: <code>{n.id}</code></p>
      <p>{n.enrolled ? `${n.budget.cpu} CPU · ${n.budget.memory_gib} GiB memory · ${n.budget.disk_gib} GiB disk allowance` : 'Resources chosen during installation'}</p>
      <p>Templates: {n.tools.workspace ? 'Ready in WSL2' : n.enrolled && n.tools.backend !== 'wsl2-layerfs' ? 'Upgrade to the WSL2 installer' : 'Waiting for WSL2 setup'}</p>
      {n.blocked && <button className="button small" disabled={busy || n.tools.worker_running !== false} onClick={() => void change(n.id, 'reconcile')}>Confirm owned worker stopped</button>}
      {!n.revoked && !n.enrolled && <button className="button small" disabled={busy} onClick={() => void renew(n.id)}>Renew registration code</button>}
      {!n.revoked && <button className="button small" disabled={busy} onClick={() => void change(n.id, 'revoke')}>Revoke machine</button>}
    </article>)}</div>
  </section>;
}
