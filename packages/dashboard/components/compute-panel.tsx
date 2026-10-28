'use client';
import { FormEvent, ReactNode, useEffect, useState } from 'react';
import { api } from '@/lib/client';
import type { ComputeNode, Host } from '@/lib/types';
type Budget = { cpu: number; memory_gib: number; disk_gib: number };
export type Contributor = { id: string; label: string; budget: Budget; workloads: { ci: boolean; agents: boolean }; online: boolean; enrolled: boolean; revoked: boolean; blocked: boolean; tools: { ci?: boolean; workspace?: boolean; backend?: string; worker_running?: boolean } };
type Enrollment = { id: string; expires: number; code: string; command: string };

export function ComputePanel({ hosts, hostsLoaded, nodes, machines: inventory, loaded, loadError, refresh, hostDetails }: {
  hosts: Host[]; hostsLoaded: boolean; nodes: Contributor[]; machines: ComputeNode[]; loaded: boolean; loadError: string;
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
  const pending = nodes.filter(n => !n.enrolled && !n.revoked), revoked = nodes.filter(n => n.revoked);
  const machines = inventory.filter(n => n.registered).toSorted((a, b) => Number(b.online) - Number(a.online) || a.label.localeCompare(b.label) || a.id.localeCompare(b.id));
  const online = machines.filter(n => n.online).length;
  const runtimeName = (n: ComputeNode) => `${({linux: 'Linux', windows: 'Windows', macos: 'macOS', unknown: 'Platform pending'} as Record<string,string>)[n.platform] ?? n.platform} · ${({incus: 'Incus', 'wsl2-layerfs': 'WSL2', 'legacy-windows-ci': 'Legacy worker', unknown: 'Runtime pending'} as Record<string,string>)[n.backend] ?? n.backend}`;
  const readiness = (n: ComputeNode) => ({ready: 'Runtime ready', busy: 'Running a workload', preparing: 'Templates unavailable', capacity_limited: 'Capacity limited', needs_attention: 'Needs attention', offline: 'Runtime disconnected'}[n.runtime.state]);
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
      {machines.length ? <ul className="worker-list">{machines.map(n => {
        const host = n.registration.method === 'controller_config' ? hosts.find(h => h.id === n.source_id) : undefined;
        const budget = n.resources.budget, available = n.resources.available;
        return <li className="worker-row" key={n.id} aria-label={`Machine ${n.label}`}>
          <div className="worker-name"><strong>{n.label}</strong><small>Registered · {runtimeName(n)}</small><small>{readiness(n)}</small>{n.error && <small className="compute-host-error">{n.error}</small>}</div>
          <span className={`worker-status ${n.runtime.state === 'needs_attention' ? 'attention' : n.online ? 'online' : ''}`}>{n.online ? 'Online' : 'Offline'}</span>
          <div className="worker-capacity"><span>{budget ? `${budget.cpu} CPU · ${bytes(budget.memory_bytes)} memory` : 'Allowance unavailable'}</span><small>{budget ? `${bytes(budget.disk_bytes)} disk allowance` : 'Reconnect to read machine allowance'}</small></div>
          <details className="compute-machine-details"><summary>Details</summary><div>
            <p className="worker-identity">Machine ID: <code>{n.id}</code></p>
            <p>{n.registration.method === 'controller_config' ? 'Registered through controller configuration. Resource reserves and limits are configured on the controller.' : 'Registered with a machine credential. Resource limits are chosen in the installer on this machine.'}</p>
            <p>{n.online ? 'Runtime connected' : 'Runtime disconnected'}{n.runtime.last_seen ? ` · Last verified ${new Date(n.runtime.last_seen * 1000).toLocaleString()}` : ''}</p>
            {n.templates.length > 0 && <p>Available templates: {n.templates.join(', ')}</p>}
            {available && <p>Available for new work: {available.cpu} CPU · {bytes(available.memory_bytes)} memory · {bytes(available.disk_bytes)} disk.</p>}
            {n.runtime.state === 'capacity_limited' && <p>The runtime is connected. Existing work can continue, but new work must fit the remaining CPU, memory, disk and workspace limits.</p>}
            {n.runtime.running_workspaces !== null && <p>{n.runtime.running_workspaces} running VM workspaces</p>}
            {n.runtime.active_job && <p>Active job: <code>{n.runtime.active_job}</code></p>}
            {n.management.limits === 'local_installer' && <p>GPU and native desktop execution require a qualified runtime. Free capacity is checked locally when work is admitted.</p>}
            {host && hostDetails(host)}
            {n.runtime.state === 'needs_attention' && n.management.revoke && <button className="button small" disabled={busy || !n.management.reconcile} onClick={() => void change(n.source_id, 'reconcile')}>Confirm owned worker stopped</button>}
            {n.management.revoke && <button className="button small" disabled={busy} onClick={() => void change(n.source_id, 'revoke')}>Revoke machine</button>}
          </div></details>
        </li>;
      })}</ul> : <div className="worker-empty"><strong>{hostsLoaded && loaded ? 'No machines connected yet' : 'Waiting for machine inventories'}</strong><p>Use Add machine to connect a Windows worker. Configured servers are registered automatically by the controller.</p></div>}
    </section>
    {pending.length > 0 && <details className="worker-surface worker-history"><summary><span>Pending registrations</span><span className="worker-count">{pending.length}</span></summary><p>Unfinished installations. Renew the code for the matching machine ID to continue setup.</p><ul className="worker-list" aria-label="Pending registrations">{pending.map(n => <li className="worker-pending-row" key={n.id} aria-label={`Pending machine ${n.label}`}><div className="worker-name"><strong>{n.label}</strong><code>{n.id}</code></div><div className="worker-actions"><button className="button small" disabled={busy} onClick={() => void renew(n.id)}>Renew registration code</button><button className="button small" disabled={busy} onClick={() => void change(n.id, 'revoke')}>Cancel invitation</button></div></li>)}</ul></details>}
    {revoked.length > 0 && <details className="worker-surface worker-history"><summary><span>Revoked machines and invitations</span><span className="worker-count">{revoked.length}</span></summary><ul className="worker-list" aria-label="Revoked machines and invitations">{revoked.map(n => <li className="worker-pending-row" key={n.id}><div className="worker-name"><strong>{n.label}</strong><code>{n.id}</code></div><span className="worker-status">Revoked</span></li>)}</ul></details>}
  </section>;
}
