import { version } from './release.mjs';

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    let identity = await env.STATE.get('identity');
    if (!identity) { identity = crypto.randomUUID(); await env.STATE.put('identity', identity); }
    let count = Number(await env.STATE.get('count') || 0);
    if (url.pathname === '/counter' && request.method === 'POST') {
      if (request.headers.has('origin') && new URL(request.headers.get('origin')).host !== url.host) return new Response('Forbidden', { status: 403 });
      count++; await env.STATE.put('count', String(count));
    }
    if (['/health', '/api/state', '/counter'].includes(url.pathname)) return Response.json({ version, identity, count }, { headers: { 'cache-control': 'no-store' } });
    if (url.pathname !== '/') return new Response('Not found', { status: 404 });
    return new Response(`<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Engine Box · Stable dev</title><style>body{background:#0c1516;color:#eef6ee;font:18px system-ui;margin:0}main{max-width:760px;margin:12vh auto;padding:28px}small{color:#b8f894;letter-spacing:.12em}h1{font-size:clamp(38px,7vw,68px);letter-spacing:-.04em}p{line-height:1.7;color:#aec3ba}button{background:#c9fb91;border:0;border-radius:10px;padding:14px;font:600 16px system-ui}#count{margin-left:16px}</style><body><main><small>ENGINE BOX / STABLE DEV</small><h1>Version ${version}</h1><p>This page runs in Wrangler’s local development environment. Keep it open: accepted source changes reload it through the dev server.</p><button id="increment">Save an interaction</button><span id="count">${count} saved</span><p>Simulated application data stays in this environment while source files update.</p></main><script>document.querySelector('#increment').onclick=async()=>{const r=await fetch('/counter',{method:'POST'});const s=await r.json();document.querySelector('#count').textContent=s.count+' saved';};</script></body></html>`, { headers: { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' } });
  }
};
