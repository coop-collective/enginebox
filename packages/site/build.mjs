import { mkdir, readFile, writeFile, cp, rm } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
import { guides } from './src/guides.mjs';
import { home } from './src/home.mjs';

const root = fileURLToPath(new URL('.', import.meta.url));
const out = join(root, 'dist');
const esc = value => String(value).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const logo = '<svg viewBox="0 0 32 32" aria-hidden="true"><path d="m16 3 12 7-12 7-12-7Z M4 16l12 7 12-7 M4 22l12 7 12-7"/></svg>';
const groups = [...new Set(guides.map(g => g.group))];
const nav = (docs = false) => `<a class="skip-link" href="#main">Skip to content</a><header class="site-header"><div class="header-inner wrap"><a class="brand" href="/">${logo}<span>engine<span class="brand-light">box</span></span></a>${docs ? '<a class="docs-label" href="/docs/">Documentation</a>' : ''}<nav id="main-nav" aria-label="Main navigation"><a href="/#how-it-works">How it works</a><a href="/#your-agents">Your agents</a><a href="/docs/" ${docs ? 'aria-current="page"' : ''}>Docs</a><a href="https://github.com/coop-collective/enginebox">GitHub <span aria-hidden="true">↗</span></a></nav><div class="header-actions">${docs ? '<button class="search-trigger" data-search-open><span>⌕</span> <span class="search-word">Search docs</span><kbd>⌘ K</kbd></button>' : '<a class="header-cta" href="/docs/quickstart/">Get started ↗</a>'}<button class="theme-toggle" data-theme-toggle aria-label="Switch color theme">◐</button><button class="nav-toggle" data-nav-toggle aria-label="Toggle navigation" aria-expanded="false" aria-controls="main-nav">☰</button></div></div></header>`;
const footer = `<footer class="site-footer wrap"><a class="brand" href="/">${logo}<span>engine<span class="brand-light">box</span></span></a><p>A home for the environment. Room for every agent.</p><div><a href="/docs/">Documentation</a><a href="https://github.com/coop-collective/enginebox">GitHub ↗</a><a href="https://github.com/coop-collective/enginebox/blob/main/LICENSE">License</a></div><span class="footer-edition">SOURCE AVAILABLE / 2026</span></footer>`;
const search = `<dialog class="search-dialog" data-search-dialog aria-labelledby="search-title"><form method="dialog" class="search-heading"><label id="search-title" for="docs-search">Search documentation</label><button aria-label="Close search">Esc</button></form><input id="docs-search" type="search" placeholder="Try “data forks” or “MCP”…" autocomplete="off"><div class="search-results" data-search-results></div><p class="search-hint">Search concepts, guides and command examples.</p></dialog>`;
function shell(title, description, body, docs = false) {
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="theme-color" content="#f7f5f0"><title>${esc(title)} — Engine Box</title><meta name="description" content="${esc(description)}"><meta property="og:title" content="${esc(title)} — Engine Box"><meta property="og:description" content="${esc(description)}"><meta property="og:type" content="website"><link rel="icon" href="/favicon.svg" type="image/svg+xml"><link rel="preconnect" href="https://fonts.googleapis.com"><link rel="preconnect" href="https://fonts.gstatic.com" crossorigin><link href="https://fonts.googleapis.com/css2?family=DM+Sans:wght@400;450;500;550;600;650;700&family=IBM+Plex+Mono:wght@400;450;500;600&display=swap" rel="stylesheet"><script>try{const t=localStorage.getItem('enginebox-theme');if(t==='dark'||t==='light')document.documentElement.dataset.theme=t}catch{}</script><link rel="stylesheet" href="/site.css"><script type="module" src="/app.js"></script></head><body class="${docs ? 'docs-body' : 'landing-body'}">${nav(docs)}${body}${docs ? '' : footer}${search}</body></html>`;
}
function docPage(guide, index) {
  const sidebar = groups.map(group => `<div class="doc-nav-group"><p>${esc(group)}</p>${guides.filter(g => g.group === group).map(g => `<a href="/docs/${g.slug}/" ${g.slug === guide.slug ? 'aria-current="page"' : ''}>${esc(g.title)}</a>`).join('')}</div>`).join('');
  const next = guides[index + 1]; const prev = guides[index - 1];
  const body = `<div class="docs-mobile-bar"><button data-docs-menu aria-expanded="false" aria-controls="docs-sidebar">☰ <span>Browse documentation</span></button><span>${esc(guide.title)}</span></div><div class="docs-layout wrap"><aside class="docs-sidebar" id="docs-sidebar"><a class="docs-home" href="/docs/">The Engine Box guide <span>↗</span></a>${sidebar}<div class="sidebar-bottom"><span class="status-dot"></span> Development preview<br><a href="/docs/platform-support/">Capabilities & platform support ↗</a></div></aside><main class="doc-content" id="main"><p class="doc-breadcrumb">Documentation <span>/</span> ${esc(guide.group)}</p><h1>${esc(guide.title)}</h1><p class="doc-summary">${esc(guide.summary)}</p>${guide.sections.map(s => `<section class="doc-section" id="${s.id}"><h2><a href="#${s.id}">${esc(s.title)}<span class="heading-hash" aria-hidden="true">#</span></a></h2>${s.html}</section>`).join('')}<nav class="doc-pagination" aria-label="Documentation pages">${prev ? `<a href="/docs/${prev.slug}/"><span>← PREVIOUS</span><strong>${esc(prev.title)}</strong></a>` : '<span></span>'}${next ? `<a href="/docs/${next.slug}/"><span>NEXT →</span><strong>${esc(next.title)}</strong></a>` : '<a href="/"><span>BACK TO</span><strong>Engine Box home ↗</strong></a>'}</nav><div class="doc-end">Built from the source. <a href="https://github.com/coop-collective/enginebox/tree/main/docs">Browse technical reference ↗</a></div></main><aside class="doc-toc"><p>ON THIS PAGE</p>${guide.sections.map(s => `<a href="#${s.id}">${esc(s.title)}</a>`).join('')}<a class="toc-source" href="https://github.com/coop-collective/enginebox">View the source ↗</a></aside></div>`;
  return shell(guide.title, guide.summary, body, true);
}
await rm(out, {recursive: true, force: true});
await mkdir(out, {recursive: true});
await cp(join(root, 'public'), out, {recursive: true});
const slugs = new Set();
for (const [i, guide] of guides.entries()) {
  if (!/^[a-z][a-z0-9-]*$/.test(guide.slug) || slugs.has(guide.slug)) throw new Error('Invalid or duplicate guide slug');
  slugs.add(guide.slug);
  const ids = new Set();
  for (const s of guide.sections) { if (!/^[a-z][a-z0-9-]*$/.test(s.id) || ids.has(s.id)) throw new Error('Invalid or duplicate section id'); ids.add(s.id); }
  await mkdir(join(out, 'docs', guide.slug), {recursive: true});
  await writeFile(join(out, 'docs', guide.slug, 'index.html'), docPage(guide, i));
}
await writeFile(join(out, 'index.html'), shell('Keep the world. Fork the work.', 'Prepared environments for native coding agents. Persistent development state, disposable experiments and your own hardware.', home));
await writeFile(join(out, 'docs', 'index.html'), docPage(guides[0], 0));
await writeFile(join(out, '404.html'), shell('Page not found', 'Find your way back to Engine Box.', '<main id="main" class="not-found wrap"><p class="eyebrow">404 / WRONG TURN</p><h1>This environment<br>doesn’t exist.</h1><p>Find your next starting point in the documentation.</p><a class="button primary" href="/docs/">Open the docs ↗</a></main>'));
await writeFile(join(out, 'search.json'), JSON.stringify(guides.map(g => ({title:g.title,summary:g.summary,url:'/docs/'+g.slug+'/',text:g.sections.map(s => s.title+' '+s.html.replace(/<[^>]*>/g,' ')).join(' ')}))));
await writeFile(join(out, 'robots.txt'), 'User-agent: *\nAllow: /\n');
await writeFile(join(out, '_headers'), '/*\n  X-Content-Type-Options: nosniff\n  Referrer-Policy: strict-origin-when-cross-origin\n');
// Validate internal route targets in emitted pages, including authored guide links.
const pages = [join(out,'index.html'),...guides.map(g=>join(out,'docs',g.slug,'index.html'))];
for (const path of pages) {
  const html = await readFile(path,'utf8');
  for (const match of html.matchAll(/href="(\/[^"#]*)(?:#[^"]*)?"/g)) {
    const url = match[1];
    if (url.startsWith('/docs/') && url !== '/docs/' && !slugs.has(url.split('/')[2])) throw new Error('Broken documentation link: '+url);
  }
}
console.log('Built Engine Box site and '+guides.length+' documentation pages.');
