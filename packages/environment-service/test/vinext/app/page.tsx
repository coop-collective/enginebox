'use client';
import { useEffect, useState } from 'react';
import { version } from './release';

export default function Page() {
  const [clicks, setClicks] = useState(0);
  const [saved, setSaved] = useState(0);
  useEffect(() => { fetch('/api/state').then(r => r.json()).then(s => setSaved(s.count)); }, []);
  async function save() { const response = await fetch('/api/state', { method: 'POST' }); setSaved((await response.json()).count); }
  return <main>
    <small>ENGINE BOX / VINEXT STABLE DEV</small>
    <h1>Version {version}</h1>
    <p>Keep this page open. A LayerFS promotion updates the files; vinext handles the refresh while the dev process keeps running.</p>
    <section><button onClick={() => setClicks(value => value + 1)}>Local interaction</button><span id="local-count">{clicks} local</span></section>
    <section><button onClick={save}>Save an interaction</button><span id="saved-count">{saved} saved</span></section>
    <p>Local interactions check React state across refresh. Saved interactions use simulated Cloudflare KV persisted in this environment.</p>
  </main>;
}
