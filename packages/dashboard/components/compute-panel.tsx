'use client';
import { FormEvent, ReactNode, useEffect, useState } from 'react';
import { api } from '@/lib/client';
import type { Host } from '@/lib/types';
type Budget = { cpu: number; memory_gib: number; disk_gib: number };
export type Contributor = { id: string; label: string; budget: Budget; workloads: { ci: boolean; agents: boolean }; online: boolean; enrolled: boolean; revoked: boolean; blocked: boolean; tools: { ci?: boolean; workspace?: boolean; backend?: string; worker_running?: boolean } };
type Enrollment = { id: string; expires: number; code: string; command: string };

export function ComputePanel({ hosts, hostsLoaded, nodes, loaded, loadError, refresh, hostDetails }: {
  hosts: Host[]; hostsLoaded: boolean; nodes: Contributor[]; loaded: boolean; loadError: string;
  refresh: () => Promise<void>; hostDetails: (host: Host) => ReactNode;
}) {
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false), [adding, setAdding] = useState(false);
  const [label, setLabel] = useState('Windows workstation'), [ci, setCi] = useState(true), [agents, setAgents] = useState(true);
  const [invitation, setInvitation] = useState<Enrollment>(), [copied, setCopied] = useState<'command' | 'code'>();
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
  const machines = [
    ...hosts.map(host => ({ kind: 'host' as const, machine: host })),
    ...registered.map(node => ({ kind: 'worker' as const, machine: node })),
  ].sort((a, b) => Number(b.machine.online) - Number(a.machine.online) || a.machine.label.localeCompare(b.machine.label) || a.machine.id.localeCompare(b.machine.id));
  const online = machines.filter(({ machine }) => machine.online).length;
  const bytes = (value: number) => `${(value / 1024 ** 3).toLocaleString(undefined, { maximumFractionDigits: 1 })} GiB`;
  return <section className="contributor-panel" aria-label="Compute machines">
    <div className="contributor-heading">
      <p>{hostsLoaded && loaded ? `${online} online · ${machines.length} registered` : 'Loading machine inventories…'}</p>
      <div className="worker-actions"><button className="button primary" aria-expanded={adding} aria-controls="add-worker-machine" onClick={() => setAdding(!adding)}>{adding ? 'Close setup' : 'Add machine'}</button></div>
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
      <div className="worker-list-heading"><h2>Machines</h2><span>{machines.length}</span></div>
      {machines.length ? <ul className="worker-list">{machines.map(entry => {
        if (entry.kind === 'host') {
          const h = entry.machine, c = h.capacity;
          return <li className="worker-row" key={`host:${h.id}`} aria-label={`Machine ${h.label}`}>
            <div className="worker-name"><strong>{h.label}</strong><small>Linux VM host</small>{h.error && <small className="compute-host-error">{h.error}</small>}</div>
            <span className={`worker-status ${h.online ? 'online' : ''}`}>{h.online ? 'Online' : 'Offline'}</span>
            <div className="worker-capacity"><span>{c ? `${c.cpu_total} CPU · ${bytes(c.memory_total_bytes)} memory` : 'Capacity unavailable'}</span><small>{c ? `${c.allocated_cpu} CPU allocated · ${bytes(c.disk_free_bytes)} disk free` : 'Reconnect to read host capacity'}</small></div>
            <details className="compute-host-details"><summary>Details</summary>{hostDetails(h)}</details>
          </li>;
        }
        const n = entry.machine;
        return <li className="worker-row" key={`worker:${n.id}`} aria-label={`Machine ${n.label}`}>
        <div className="worker-name"><strong>{n.label}</strong><small>{n.tools.backend === 'wsl2-layerfs' ? 'Windows · WSL2 templates' : 'Windows · legacy worker'}</small><small>{n.tools.workspace ? 'Templates ready' : n.tools.backend !== 'wsl2-layerfs' ? 'WSL2 upgrade needed' : 'Preparing templates'}</small></div>
        <span className={`worker-status ${n.blocked ? 'attention' : n.online ? 'online' : ''}`}>{n.blocked ? 'Needs attention' : n.online ? 'Online' : 'Offline'}</span>
        <div className="worker-capacity"><span>{n.budget.cpu} CPU · {n.budget.memory_gib} GiB memory</span><small>{n.budget.disk_gib} GiB disk allowance · {[n.workloads.ci && 'CI', n.workloads.agents && 'Agents'].filter(Boolean).join(' + ')}</small></div>
        <details className="worker-manage"><summary>Manage</summary><div><p>Limits chosen on this machine. GPU and native desktop execution require a qualified runtime.</p><p className="worker-identity">Machine ID: <code>{n.id}</code></p>{n.blocked && <button className="button small" disabled={busy || n.tools.worker_running !== false} onClick={() => void change(n.id, 'reconcile')}>Confirm owned worker stopped</button>}<button className="button small" disabled={busy} onClick={() => void change(n.id, 'revoke')}>Revoke machine</button></div></details>
      </li>;
      })}</ul> : <div className="worker-empty"><strong>{hostsLoaded && loaded ? 'No machines connected yet' : 'Waiting for machine inventories'}</strong><p>Use Add machine to connect a Windows worker. Dedicated VM hosts appear from the controller’s configured inventory.</p></div>}
    </section>
    {pending.length > 0 && <details className="worker-surface worker-history"><summary><span>Pending registrations</span><span className="worker-count">{pending.length}</span></summary><p>Unfinished installations. Renew the code for the matching machine ID to continue setup.</p><ul className="worker-list" aria-label="Pending registrations">{pending.map(n => <li className="worker-pending-row" key={n.id} aria-label={`Pending machine ${n.label}`}><div className="worker-name"><strong>{n.label}</strong><code>{n.id}</code></div><div className="worker-actions"><button className="button small" disabled={busy} onClick={() => void renew(n.id)}>Renew registration code</button><button className="button small" disabled={busy} onClick={() => void change(n.id, 'revoke')}>Cancel invitation</button></div></li>)}</ul></details>}
    {revoked.length > 0 && <details className="worker-surface worker-history"><summary><span>Revoked machines and invitations</span><span className="worker-count">{revoked.length}</span></summary><ul className="worker-list" aria-label="Revoked machines and invitations">{revoked.map(n => <li className="worker-pending-row" key={n.id}><div className="worker-name"><strong>{n.label}</strong><code>{n.id}</code></div><span className="worker-status">Revoked</span></li>)}</ul></details>}
  </section>;
}
