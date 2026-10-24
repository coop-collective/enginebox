'use client';
import { FormEvent, useCallback, useEffect, useState } from 'react';
import { api } from '@/lib/client';
type Route = { id: string; environment: string; service: string; hostname?: string; entrypoint?: string; access: string };
type Catalog = {
  baseDomains: string[];
  transportEntrypoints: { name: string; protocol: string; port: number; public: boolean }[];
  routes: Route[];
  projects?: { id: string; host: string; project: string; status: string; healthy: boolean; error?: string; urls: string[] }[];
  environments: { id: string; source: { kind: string; branch?: string }; services: { name: string; protocol: string }[]; status: string; healthy: boolean; revision?: string; desiredRevision?: string; error?: string; cpu: number; memoryMiB: number }[];
};
export function EnvironmentsPanel() {
  const [catalog, setCatalog] = useState<Catalog>();
  const [error, setError] = useState(''); const [busy, setBusy] = useState(false);
  const [environment, setEnvironment] = useState(''); const [service, setService] = useState('');
  const [routeId, setRouteId] = useState(''); const [hostname, setHostname] = useState('');
  const [entrypoint, setEntrypoint] = useState(''); const [access, setAccess] = useState('authenticated');
  const refresh = useCallback(async () => {
    try { setCatalog(await api<Catalog>('/api/environments')); setError(''); } catch (e) { setError((e as Error).message); }
  }, []);
  useEffect(() => { void refresh(); const timer = setInterval(() => void refresh(), 10000); return () => clearInterval(timer); }, [refresh]);
  const selected = catalog?.environments.find(e => e.id === environment)?.services.find(s => s.name === service);
  async function mutate(body: Route | { remove: string }) {
    setBusy(true); setError('');
    try { setCatalog(await api<Catalog>('/api/environments/routes', { method: 'POST', body: JSON.stringify(body) })); }
    catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  }
  function submit(event: FormEvent) {
    event.preventDefault();
    void mutate({ id: routeId, environment, service, access, ...(['http', 'tls'].includes(selected?.protocol ?? '') ? { hostname } : {}), ...(selected?.protocol !== 'http' ? { entrypoint } : {}) });
  }
  return <section className="storage-panel environment-panel" aria-label="Environment services">
    <div className="section-heading"><div><h2>Live environments</h2><p>Follow accepted code, preserve service data, and manage network routes.</p></div><button className="button small" onClick={() => void refresh()}>Refresh services</button></div>
    {error && <div role="alert" className="alert">{error}</div>}
    {catalog && <>
      {!!catalog.projects?.length && <div className="host-grid">{catalog.projects.map(project => <article key={project.id} className="host-card" aria-label={`Project dev ${project.project}`}>
        <h3>{project.project}</h3><p>{project.healthy ? 'Serving' : 'Not serving'} · {project.status}</p>
        {project.error && <p role="status">{project.error}</p>}
        {project.urls.filter(url => /^https?:\/\//.test(url)).map(url => <p key={url}><a href={url} target="_blank" rel="noopener noreferrer">Open {project.project}</a></p>)}
      </article>)}</div>}
      {catalog.environments.length === 0 && <p>No persistent environments have been provisioned.</p>}
      <div className="host-grid">{catalog.environments.map(env => <article key={env.id} className="host-card" aria-label={`Environment ${env.id}`}>
        <h3>{env.id}</h3><p>{env.healthy ? 'Serving' : 'Not serving'} · {env.status}</p>
        <p>Source: {env.source.kind} {env.source.branch ?? ''}</p>
        <p className="mono">Revision: {env.revision?.slice(0, 12) ?? 'Pending'}</p>
        {env.desiredRevision !== env.revision && <p>Update: {env.desiredRevision?.slice(0, 12) ?? 'Pending'}</p>}
        <p>{env.cpu} CPU · {env.memoryMiB} MiB</p>{env.error && <p role="status">{env.error}</p>}
        <p>{env.services.map(s => `${s.name} (${s.protocol})`).join(', ')}</p>
      </article>)}</div>
      <h3>Service routes</h3>
      <p>Approved domains: {catalog.baseDomains.join(', ')}. Point DNS at the gateway and configure TLS before sharing an HTTPS address.</p>
      {catalog.routes.map(route => <div className="host-card" key={route.id}>
        <strong>{route.id}</strong><p>{route.environment} / {route.service} → {route.hostname ?? route.entrypoint} · {route.access}</p>
        <button className="button small" disabled={busy} onClick={() => void mutate({ remove: route.id })}>Remove {route.id}</button>
      </div>)}
      <form onSubmit={submit} className="host-card">
        <h3>Add or replace a route</h3>
        <label>Route name<input required value={routeId} onChange={e => setRouteId(e.target.value)} pattern="[a-z][a-z0-9-]{0,39}" /></label>
        <label>Environment<select aria-label="Environment" required value={environment} onChange={e => { setEnvironment(e.target.value); setService(''); }}><option value="">Select environment</option>{catalog.environments.map(e => <option key={e.id}>{e.id}</option>)}</select></label>
        <label>Service<select aria-label="Service" required value={service} onChange={e => { setService(e.target.value); setAccess('authenticated'); setEntrypoint(''); }}><option value="">Select service</option>{catalog.environments.find(e => e.id === environment)?.services.map(s => <option key={s.name} value={s.name}>{s.name} ({s.protocol})</option>)}</select></label>
        {['http', 'tls'].includes(selected?.protocol ?? '') && <label>Hostname<input required value={hostname} onChange={e => setHostname(e.target.value)} placeholder={`app.${catalog.baseDomains[0]}`} /></label>}
        {selected && selected.protocol !== 'http' && <label>Transport listener<select aria-label="Transport listener" required value={entrypoint} onChange={e => { setEntrypoint(e.target.value); setAccess(catalog.transportEntrypoints.find(p => p.name === e.target.value)?.public ? 'public' : 'private'); }}><option value="">Select listener</option>{catalog.transportEntrypoints.filter(p => p.protocol === (selected.protocol === 'udp' ? 'udp' : 'tcp')).map(p => <option key={p.name} value={p.name}>{p.name} :{p.port} ({p.public ? 'public' : 'private'})</option>)}</select></label>}
        {selected?.protocol === 'http' && <label>Access<select aria-label="Access" value={access} onChange={e => setAccess(e.target.value)}><option value="authenticated">Require gateway login</option><option value="public">Public</option></select></label>}
        {selected && selected.protocol !== 'http' && <p>TCP and UDP use the selected listener’s access policy. Applications must enforce their own authentication.</p>}
        <button className="button primary" type="submit" disabled={busy || !selected}>Save route</button>
      </form>
    </>}
  </section>;
}
