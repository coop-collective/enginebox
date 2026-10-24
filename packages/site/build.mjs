import { mkdir, readFile, writeFile, cp, rm, rename } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { join, resolve } from 'node:path';
import { guides } from './src/guides.mjs';
import { home } from './src/home.mjs';
import { pricing, pricingTitle, pricingDescription } from './src/pricing.mjs';

const root = fileURLToPath(new URL('.', import.meta.url));
const out = process.env.OUT_DIR ? resolve(process.env.OUT_DIR) : join(root, 'dist');
// Canonical origin for canonical/og:url links and the sitemap. Override with SITE_URL when deploying elsewhere.
const siteUrl = (process.env.SITE_URL || 'https://enginebox.coopcollective.dev').replace(/\/+$/, '');
// Where the "Get a license" button points. No checkout exists yet: when unset, the pricing page shows a disabled button.
const checkoutUrl = (process.env.CHECKOUT_URL || '').trim();
if (checkoutUrl && !/^https:\/\/[^\s"<>]+$/.test(checkoutUrl)) throw new Error('CHECKOUT_URL must be an https:// URL');
if (!checkoutUrl) console.warn('warning: CHECKOUT_URL is not set; /pricing/ shows a disabled "Checkout opening soon" button.');
const esc = value => String(value).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const logo = '<svg viewBox="0 0 32 32" aria-hidden="true"><path d="m16 3 12 7-12 7-12-7Z M4 16l12 7 12-7 M4 22l12 7 12-7"/></svg>';
// Published path of each content-hashed asset, keyed by its source path (filled in below).
const assets = {};
const groups = [...new Set(guides.map(g => g.group))];
const nav = (docs = false, current = '') => `<a class="skip-link" href="#main">Skip to content</a><header class="site-header"><div class="header-inner wrap"><a class="brand" href="/">${logo}<span>engine<span class="brand-light">box</span></span></a>${docs ? '<a class="docs-label" href="/docs/">Documentation</a>' : ''}<nav id="main-nav" aria-label="Main navigation"><a href="/#how-it-works">How it works</a><a href="/#your-agents">Your agents</a><a href="/docs/" ${docs ? 'aria-current="page"' : ''}>Docs</a><a href="/pricing/" ${current === 'pricing' ? 'aria-current="page"' : ''}>Pricing</a><a href="/dashboard/">Dashboard</a><a href="https://github.com/coop-collective/enginebox">GitHub <span aria-hidden="true">↗</span></a></nav><div class="header-actions">${docs ? '<button class="search-trigger" data-search-open><span>⌕</span> <span class="search-word">Search docs</span><kbd>⌘ K</kbd></button>' : '<a class="header-cta" href="/dashboard/">Get started ↗</a>'}<button class="theme-toggle" data-theme-toggle aria-label="Switch color theme">◐</button><button class="nav-toggle" data-nav-toggle aria-label="Toggle navigation" aria-expanded="false" aria-controls="main-nav">☰</button></div></div></header>`;
const footer = `<footer class="site-footer wrap"><a class="brand" href="/">${logo}<span>engine<span class="brand-light">box</span></span></a><p>A home for the environment. Room for every agent.</p><div><a href="/docs/">Documentation</a><a href="/pricing/">Pricing</a><a href="https://github.com/coop-collective/enginebox">GitHub ↗</a><a href="https://github.com/coop-collective/enginebox/blob/main/LICENSE">License</a></div><span class="footer-edition">SOURCE AVAILABLE / 2026</span></footer>`;
const search = `<dialog class="search-dialog" data-search-dialog aria-labelledby="search-title"><form method="dialog" class="search-heading"><label id="search-title" for="docs-search">Search documentation</label><button aria-label="Close search">Esc</button></form><input id="docs-search" type="search" placeholder="Try “data forks” or “MCP”…" autocomplete="off"><div class="search-results" data-search-results></div><p class="search-hint">Search concepts, guides and command examples.</p></dialog>`;
function shell(title, description, body, docs = false, path = '', current = '') {
  const url = path ? siteUrl + path : '';
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="theme-color" content="#f7f5f0"><title>${esc(title)} — Engine Box</title><meta name="description" content="${esc(description)}"><meta property="og:title" content="${esc(title)} — Engine Box"><meta property="og:description" content="${esc(description)}"><meta property="og:type" content="website"><meta name="twitter:card" content="summary">${url ? `<link rel="canonical" href="${url}"><meta property="og:url" content="${url}">` : ''}<link rel="icon" href="/favicon.svg" type="image/svg+xml"><link rel="preload" href="${assets['fonts/dm-sans-latin.woff2']}" as="font" type="font/woff2" crossorigin><script>try{const t=localStorage.getItem('enginebox-theme');if(t==='dark'||t==='light')document.documentElement.dataset.theme=t}catch{}</script><link rel="stylesheet" href="${assets['site.css']}"><script type="module" src="${assets['app.js']}"></script></head><body class="${docs ? 'docs-body' : 'landing-body'}">${nav(docs, current)}${body}${docs ? '' : footer}${search}</body></html>`;
}
function docPage(guide, index, path = `/docs/${guide.slug}/`) {
  const sidebar = groups.map(group => `<div class="doc-nav-group"><p>${esc(group)}</p>${guides.filter(g => g.group === group).map(g => `<a href="/docs/${g.slug}/" ${g.slug === guide.slug ? 'aria-current="page"' : ''}>${esc(g.title)}</a>`).join('')}</div>`).join('');
  const next = guides[index + 1]; const prev = guides[index - 1];
  const body = `<div class="docs-mobile-bar"><button data-docs-menu aria-expanded="false" aria-controls="docs-sidebar">☰ <span>Browse documentation</span></button><span>${esc(guide.title)}</span></div><div class="docs-layout wrap"><aside class="docs-sidebar" id="docs-sidebar"><a class="docs-home" href="/docs/">The Engine Box guide <span>↗</span></a>${sidebar}<div class="sidebar-bottom"><span class="status-dot"></span> Development preview<br><a href="/docs/platform-support/">Capabilities & platform support ↗</a></div></aside><main class="doc-content" id="main"><p class="doc-breadcrumb">Documentation <span>/</span> ${esc(guide.group)}</p><h1>${esc(guide.title)}</h1><p class="doc-summary">${esc(guide.summary)}</p>${guide.sections.map(s => `<section class="doc-section" id="${s.id}"><h2><a href="#${s.id}">${esc(s.title)}<span class="heading-hash" aria-hidden="true">#</span></a></h2>${s.html}</section>`).join('')}<nav class="doc-pagination" aria-label="Documentation pages">${prev ? `<a href="/docs/${prev.slug}/"><span>← PREVIOUS</span><strong>${esc(prev.title)}</strong></a>` : '<span></span>'}${next ? `<a href="/docs/${next.slug}/"><span>NEXT →</span><strong>${esc(next.title)}</strong></a>` : '<a href="/"><span>BACK TO</span><strong>Engine Box home ↗</strong></a>'}</nav><div class="doc-end">Built from the source. <a href="https://github.com/coop-collective/enginebox/tree/main/docs">Browse technical reference ↗</a></div></main><aside class="doc-toc"><p>ON THIS PAGE</p>${guide.sections.map(s => `<a href="#${s.id}">${esc(s.title)}</a>`).join('')}<a class="toc-source" href="https://github.com/coop-collective/enginebox">View the source ↗</a></aside></div>`;
  return shell(guide.title, guide.summary, body, true, path);
}
await rm(out, {recursive: true, force: true});
await mkdir(out, {recursive: true});
await cp(join(root, 'public'), out, {recursive: true});
// Rename a built file to include a hash of its contents, so it can be cached forever.
// Returns the published absolute path. Later files may embed earlier results.
async function hashed(file) {
  const bytes = await readFile(join(out, file));
  const hash = createHash('sha256').update(bytes).digest('hex').slice(0, 10);
  const dot = file.lastIndexOf('.');
  const name = `${file.slice(0, dot)}.${hash}${file.slice(dot)}`;
  await rename(join(out, file), join(out, name));
  return assets[file] = '/' + name;
}
async function rewrite(file, replacements) {
  let text = await readFile(join(out, file), 'utf8');
  for (const [from, to] of replacements) {
    if (!text.includes(from)) throw new Error(`${file} does not reference ${from}`);
    text = text.replaceAll(from, to);
  }
  await writeFile(join(out, file), text);
}
const searchIndex = JSON.stringify(guides.map(g => ({title:g.title,summary:g.summary,url:'/docs/'+g.slug+'/',text:g.sections.map(s => s.title+' '+s.html.replace(/<[^>]*>/g,' ')).join(' ')})));
const fontFiles = ['fonts/dm-sans-latin.woff2', 'fonts/ibm-plex-mono-latin-400.woff2', 'fonts/ibm-plex-mono-latin-500.woff2'];
for (const file of fontFiles) await hashed(file);
await rewrite('site.css', fontFiles.map(file => [`/${file}`, assets[file]]));
await writeFile(join(out, 'search.json'), searchIndex);
await hashed('search.json');
await hashed('demo-model.mjs');
await rewrite('app.js', [['./demo-model.mjs', assets['demo-model.mjs']], ['/search.json', assets['search.json']]]);
await hashed('app.js');
await hashed('site.css');
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
await mkdir(join(out, 'pricing'), {recursive: true});
await writeFile(join(out, 'pricing', 'index.html'), shell(pricingTitle, pricingDescription, pricing(checkoutUrl), false, '/pricing/', 'pricing'));
await writeFile(join(out, 'docs', 'index.html'), docPage(guides[0], 0, '/docs/'));
await writeFile(join(out, '404.html'), shell('Page not found', 'Find your way back to Engine Box.', '<main id="main" class="not-found wrap"><p class="eyebrow">404 / WRONG TURN</p><h1>This environment<br>doesn’t exist.</h1><p>Find your next starting point in the documentation.</p><a class="button primary" href="/docs/">Open the docs ↗</a></main>'));
const pages = ['/', '/pricing/', '/docs/', ...guides.map(g => `/docs/${g.slug}/`)];
await writeFile(join(out, 'sitemap.xml'), `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${pages.map(p => `<url><loc>${siteUrl}${p}</loc></url>`).join('\n')}\n</urlset>\n`);
await writeFile(join(out, 'robots.txt'), `User-agent: *\nAllow: /\nSitemap: ${siteUrl}/sitemap.xml\n`);
// Hashed assets never change; HTML, the sitemap and other stable URLs revalidate on every load.
const immutable = 'Cache-Control: public, max-age=31536000, immutable';
await writeFile(join(out, '_headers'), [
  '/*', '  X-Content-Type-Options: nosniff', '  Referrer-Policy: strict-origin-when-cross-origin', '  Cache-Control: max-age=0, must-revalidate', '',
  ...Object.values(assets).flatMap(path => [path, '  ! Cache-Control', '  ' + immutable, '']),
].join('\n'));
