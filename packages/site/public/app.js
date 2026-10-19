import { initialState, transition } from './demo-model.mjs';
const esc = value => String(value).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const number = value => value.toLocaleString('en-US');
const demo = document.querySelector('[data-demo]');
if (demo) {
  let state = initialState();
  function render() {
    demo.classList.toggle('sleeping', !state.awake);
    demo.querySelector('[data-core-status]').textContent = state.awake ? 'Running' : 'Sleeping';
    demo.querySelector('[data-core-revision]').textContent = 'main · r' + state.revision;
    demo.querySelector('[data-core-rows]').textContent = number(state.rows);
    demo.querySelector('[data-demo-message]').textContent = state.message;
    demo.querySelector('[data-sleep-button]').innerHTML = '<span aria-hidden="true">◐</span> ' + (state.awake ? 'Sleep core' : 'Wake core');
    demo.querySelector('[data-action="fork"]').disabled = state.forks.length >= 3;
    demo.querySelector('[data-fork-list]').innerHTML = state.forks.length ? state.forks.map(f => `<article class="fork-card" data-fork-id="${f.id}" aria-label="${esc(f.agent)} fork ${f.id}"><div class="fork-heading"><span aria-hidden="true">${f.agent === 'Claude Code' ? '✳' : f.agent === 'Codex' ? '⌘' : '◩'}</span><strong>${esc(f.agent)}</strong><span class="fork-revision">fork / r${f.revision}</span></div><p class="fork-description">${esc(f.task)}</p><div class="fork-bottom"><span class="fork-count ${f.changed ? 'changed' : ''}" data-fork-rows>${number(f.rows)} records${f.changed ? ' · private edits' : ''}</span><button data-action="edit" data-id="${f.id}" aria-label="Edit ${esc(f.agent)} fork ${f.id} data">＋ Try a data change</button><button class="discard-fork" data-action="discard" data-id="${f.id}" aria-label="Discard ${esc(f.agent)} fork ${f.id}">×</button></div></article>`).join('') : '<p class="empty-forks">All experiments cleared. The core is ready for the next idea.</p>';
  }
  demo.addEventListener('click', event => {
    const button = event.target.closest('[data-action]');
    if (!button || button.disabled) return;
    const action = button.dataset.action;
    const id = Number(button.dataset.id);
    state = transition(state, action, id);
    render();
    if (action === 'edit') demo.querySelector(`[data-action="edit"][data-id="${id}"]`)?.focus({preventScroll:true});
    if (action === 'discard') demo.querySelector('[data-action="fork"]').focus({preventScroll:true});
  });
  render();
}
const theme = document.querySelector('[data-theme-toggle]');
const setThemeLabel = () => theme?.setAttribute('aria-label', document.documentElement.dataset.theme === 'dark' ? 'Switch to light theme' : 'Switch to dark theme');
setThemeLabel();
theme?.addEventListener('click', () => {
  const next = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem('enginebox-theme', next); } catch {}
  setThemeLabel();
});
document.querySelector('[data-nav-toggle]')?.addEventListener('click', event => {
  const open = document.body.classList.toggle('nav-open');
  event.currentTarget.setAttribute('aria-expanded', String(open));
});
document.querySelector('[data-docs-menu]')?.addEventListener('click', event => {
  const open = document.body.classList.toggle('docs-nav-open');
  event.currentTarget.setAttribute('aria-expanded', String(open));
});
document.addEventListener('keydown', event => {
  if (event.key === 'Escape') {
    document.body.classList.remove('nav-open', 'docs-nav-open');
    for (const button of document.querySelectorAll('[data-nav-toggle], [data-docs-menu]')) button.setAttribute('aria-expanded','false');
  }
});
for (const pre of document.querySelectorAll('.doc-section pre')) {
  const code = pre.querySelector('code') ?? pre;
  const content = code.textContent;
  const button = document.createElement('button');
  button.className = 'copy-code'; button.textContent = 'Copy'; button.setAttribute('aria-label','Copy code');
  button.addEventListener('click', async () => {
    try { await navigator.clipboard.writeText(content); button.textContent = 'Copied'; }
    catch { const range = document.createRange(); range.selectNodeContents(code); const selection = getSelection(); selection.removeAllRanges(); selection.addRange(range); button.textContent = 'Selected'; }
    setTimeout(() => { button.textContent = 'Copy'; }, 2000);
  });
  pre.append(button);
}
const dialog = document.querySelector('[data-search-dialog]');
const input = document.querySelector('#docs-search');
let searchIndex;
async function openSearch() {
  if (!dialog.open) dialog.showModal();
  input.focus();
  const results = dialog.querySelector('[data-search-results]');
  if (!searchIndex) {
    results.textContent = 'Loading documentation…';
    try { const response = await fetch('/search.json'); if (!response.ok) throw new Error('Search unavailable'); searchIndex = await response.json(); }
    catch { results.innerHTML = '<p class="search-empty">Search is unavailable. <a href="/docs/">Browse the documentation instead.</a></p>'; return; }
  }
  showResults();
}
function showResults() {
  if (!searchIndex) return;
  const terms = input.value.toLowerCase().trim().split(/\s+/).filter(Boolean);
  const result = searchIndex.filter(g => terms.every(term => (g.title+' '+g.summary+' '+g.text).toLowerCase().includes(term)));
  dialog.querySelector('[data-search-results]').innerHTML = result.length ? result.map(g => `<a class="search-result" href="${esc(g.url)}"><strong>${esc(g.title)} <span aria-hidden="true">↗</span></strong><p>${esc(g.summary)}</p></a>`).join('') : '<p class="search-empty">No pages found. Try “storage”, “agents” or “Cloudflare”.</p>';
}
for (const button of document.querySelectorAll('[data-search-open]')) button.addEventListener('click',openSearch);
input?.addEventListener('input',showResults);
input?.addEventListener('keydown',event=>{if(event.key === 'ArrowDown'){event.preventDefault();dialog.querySelector('.search-result')?.focus();}});
document.addEventListener('keydown',event=>{if((event.metaKey||event.ctrlKey)&&event.key.toLowerCase()==='k'){event.preventDefault();openSearch();}});
dialog?.addEventListener('click',event=>{if(event.target===dialog){const rect=dialog.getBoundingClientRect();if(event.clientX<rect.left||event.clientX>rect.right||event.clientY<rect.top||event.clientY>rect.bottom)dialog.close();}});
if ('IntersectionObserver' in window) {
  const observer = new IntersectionObserver(entries=>{
    for(const entry of entries)if(entry.isIntersecting){for(const link of document.querySelectorAll('.doc-toc a[href^="#"]'))link.classList.toggle('active',link.hash==='#'+entry.target.id);}
  },{rootMargin:'-100px 0px -65% 0px'});
  document.querySelectorAll('.doc-section').forEach(section=>observer.observe(section));
}
