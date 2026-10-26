'use client';
import { FormEvent, useCallback, useEffect, useState } from 'react';
import { api } from '@/lib/client';
type Budget = { cpu: number; memory_gib: number; disk_gib: number };
type Node = { id: string; label: string; budget: Budget; workloads: { ci: boolean; agents: boolean }; online: boolean; enrolled: boolean; revoked: boolean; blocked: boolean; tools: { ci?: boolean; workspace?: boolean; backend?: string; worker_running?: boolean } };
type Enrollment = { id: string; expires: number; code: string; command: string };

export function ContributorsPanel() {
  const [nodes, setNodes] = useState<Node[]>([]), [error, setError] = useState(''), [loadError, setLoadError] = useState('');
  const [busy, setBusy] = useState(false), [loaded, setLoaded] = useState(false), [adding, setAdding] = useState(false);
  const [label, setLabel] = useState('Windows workstation'), [ci, setCi] = useState(true), [agents, setAgents] = useState(true);
  const [invitation, setInvitation] = useState<Enrollment>(), [copied, setCopied] = useState<'command' | 'code'>();
  const refresh = useCallback(async () => {
    try { const result = await api<{ nodes: Node[] }>('/api/contributors'); setNodes(result.nodes); setLoaded(true); setLoadError(''); }
    catch (e) { setLoadError((e as Error).message); }
  }, []);
  useEffect(() => { void refresh(); const timer = setInterval(() => void refresh(), 5000); return () => clearInterval(timer); }, [refresh]);
  useEffect(() => {
    if (!invitation) return;
    if (nodes.some(n => n.id === invitation.id && (n.enrolled || n.revoked))) { setInvitation(undefined); return; }
    const timer = setTimeout(() => { setInvitation(undefined); setError('The registration code expired. Open Pending registrations and renew the code for that machine.'); }, Math.max(0, invitation.expires * 1000 - Date.now()));
    return () => clearTimeout(timer);
  }, [invitation, nodes]);
  async function issue(event: FormEvent) {
    event.preventDefault(); setBusy(true); setError(''); setCopied(undefined); setInvitation(undefined);
    try {
      setInvitation(await api<Enrollment>('/api/contributors/enrollments', { method: 'POST', body: JSON.stringify({ label, workloads: { ci, agents } }) }));
      setAdding(false); await refresh();
    } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  }
  async function change(id: string, action: string) {
    setBusy(true); setError('');
    try { await api(`/api/contributors/${id}/${action}`, { method: 'POST', body: JSON.stringify(action === 'reconcile' ? { stopped: true } : {}) }); await refresh(); }
    catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  }
  async function renew(id: string) {
    setBusy(true); setError(''); setCopied(undefined); setInvitation(undefined);
    try { setInvitation(await api<Enrollment>(`/api/contributors/${id}/renew-enrollment`, { method: 'POST', body: '{}' })); await refresh(); }
    catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  }
  async function copy(kind: 'command' | 'code') {
    if (!invitation) return;
    try { await navigator.clipboard.writeText(invitation[kind]); setCopied(kind); setError(''); }
    catch { setError(`Clipboard access was denied. Select and copy the ${kind === 'code' ? 'registration code' : 'install command'} below.`); }
  }
  const registered = nodes.filter(n => n.enrolled && !n.revoked).sort((a, b) => Number(b.online) - Number(a.online) || a.label.localeCompare(b.label) || a.id.localeCompare(b.id));
  const pending = nodes.filter(n => !n.enrolled && !n.revoked), revoked = nodes.filter(n => n.revoked);
  const online = registered.filter(n => n.online).length;
  return <section className="contributor-panel" aria-label="Worker machines">
    <div className="contributor-heading">
      <p>{loaded ? `${online} online · ${registered.length} registered` : 'Loading machines…'}</p>
      <div className="worker-actions"><button className="button small" onClick={() => void refresh()}>Refresh machines</button><button className="button primary" aria-expanded={adding} aria-controls="add-worker-machine" onClick={() => setAdding(!adding)}>{adding ? 'Close setup' : 'Add machine'}</button></div>
    </div>
    {(error || loadError) && <div className="alert" role="alert">{error || loadError}</div>}
    {adding && <form id="add-worker-machine" className="worker-surface contributor-form" onSubmit={issue}>
      <div><h3>Connect a Windows machine</h3><p>Run the installer on your machine. Choose CPU, memory and disk limits there.</p></div>
      <label>Machine name<input required maxLength={80} value={label} onChange={e => setLabel(e.target.value)} /></label>
      <div className="worker-actions"><label className="contributor-choice"><input type="checkbox" checked={ci} onChange={e => setCi(e.target.checked)} /> CI builds and checks</label><label className="contributor-choice"><input type="checkbox" checked={agents} onChange={e => setAgents(e.target.checked)} /> Agents in templates</label></div>
      <button className="button primary" disabled={busy || (!ci && !agents)}>Create install command</button>
    </form>}
    {invitation && <section className="worker-surface worker-enrollment" aria-label="Install this machine">
      <div className="contributor-heading"><div><h3>Connect {nodes.find(n => n.id === invitation.id)?.label || 'your machine'}</h3><p>Code expires at {new Date(invitation.expires * 1000).toLocaleTimeString()}.</p></div><button className="button small" onClick={() => setInvitation(undefined)}>Close instructions</button></div>
      <div className="worker-install-step"><div><h4>Run the installer in PowerShell</h4><p>Already installed? Choose N at “Override existing?” to keep the Linux runtime.</p></div><button className="button primary" onClick={() => void copy('command')}>{copied === 'command' ? 'Command copied' : 'Copy install command'}</button></div>
      <details className="worker-command"><summary>Show install command</summary><textarea readOnly aria-label="PowerShell install command" value={invitation.command} rows={4} spellCheck={false} onFocus={e => e.currentTarget.select()} /></details>
      <div className="worker-code"><div><h4>Installer asking for a registration code?</h4><p>Copy this code into the installer’s code prompt.</p></div><label>Registration code<input readOnly aria-label="Registration code" value={invitation.code} spellCheck={false} onFocus={e => e.currentTarget.select()} /></label><button className="button" onClick={() => void copy('code')}>{copied === 'code' ? 'Code copied' : 'Copy registration code'}</button></div>
      <p className="worker-identity">Machine ID: <code>{invitation.id}</code>. Use this ID to match an existing installation.</p>
    </section>}
    <section className="worker-surface worker-inventory" aria-label="Registered machines">
      <div className="worker-list-heading"><h2>Registered machines</h2><span>{registered.length}</span></div>
      {registered.length ? <ul className="worker-list">{registered.map(n => <li className="worker-row" key={n.id} aria-label={`Machine ${n.label}`}>
        <div className="worker-name"><strong>{n.label}</strong><small>{n.tools.workspace ? 'Templates ready' : n.tools.backend !== 'wsl2-layerfs' ? 'WSL2 upgrade needed' : 'Preparing templates'}</small></div>
        <span className={`worker-status ${n.blocked ? 'attention' : n.online ? 'online' : ''}`}>{n.blocked ? 'Needs attention' : n.online ? 'Online' : 'Offline'}</span>
        <div className="worker-capacity"><span>{n.budget.cpu} CPU · {n.budget.memory_gib} GiB memory</span><small>{n.budget.disk_gib} GiB disk · {[n.workloads.ci && 'CI', n.workloads.agents && 'Agents'].filter(Boolean).join(' + ')}</small></div>
        <details className="worker-manage"><summary>Manage</summary><div><p className="worker-identity">Machine ID: <code>{n.id}</code></p>{n.blocked && <button className="button small" disabled={busy || n.tools.worker_running !== false} onClick={() => void change(n.id, 'reconcile')}>Confirm owned worker stopped</button>}<button className="button small" disabled={busy} onClick={() => void change(n.id, 'revoke')}>Revoke machine</button></div></details>
      </li>)}</ul> : <div className="worker-empty"><strong>{loaded ? 'No machines connected yet' : 'Loading machines…'}</strong><p>Registered machines appear here when setup completes. Use Add machine to connect one.</p></div>}
    </section>
    {pending.length > 0 && <details className="worker-surface worker-history"><summary><span>Pending registrations</span><span className="worker-count">{pending.length}</span></summary><p>Unfinished installations. Renew the code for the matching machine ID to continue setup.</p><ul className="worker-list" aria-label="Pending registrations">{pending.map(n => <li className="worker-pending-row" key={n.id} aria-label={`Pending machine ${n.label}`}><div className="worker-name"><strong>{n.label}</strong><code>{n.id}</code></div><div className="worker-actions"><button className="button small" disabled={busy} onClick={() => void renew(n.id)}>Renew registration code</button><button className="button small" disabled={busy} onClick={() => void change(n.id, 'revoke')}>Cancel invitation</button></div></li>)}</ul></details>}
    {revoked.length > 0 && <details className="worker-surface worker-history"><summary><span>Revoked machines and invitations</span><span className="worker-count">{revoked.length}</span></summary><ul className="worker-list" aria-label="Revoked machines and invitations">{revoked.map(n => <li className="worker-pending-row" key={n.id}><div className="worker-name"><strong>{n.label}</strong><code>{n.id}</code></div><span className="worker-status">Revoked</span></li>)}</ul></details>}
  </section>;
}
