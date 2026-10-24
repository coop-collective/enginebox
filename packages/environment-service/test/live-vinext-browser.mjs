// Opt-in public HMR qualification against test/vinext. Promote a new LayerFS
// revision only after OUTPUT.ready exists; this probe never reloads the page.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { writeFile } from 'node:fs/promises';
const require = createRequire(new URL('../../dashboard/package.json', import.meta.url));
const { chromium } = require('@playwright/test');
const [url, before, after, output] = process.argv.slice(2);
assert(process.env.ENGINEBOX_LIVE_VINEXT === '1' && url && before && after && output, 'Set ENGINEBOX_LIVE_VINEXT=1 and supply URL BEFORE_VERSION AFTER_VERSION OUTPUT');
assert(['http:', 'https:'].includes(new URL(url).protocol));
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1440, height: 950 } });
const evidence = { url, handshakes: [], messages: [], navigations: [], errors: [] };
const cdp = await page.context().newCDPSession(page); await cdp.send('Network.enable'); await cdp.send('Page.enable');
cdp.on('Network.webSocketHandshakeResponseReceived', event => evidence.handshakes.push(event.response.status));
// Playwright's framenavigated also fires for history.replaceState during React
// hydration. CDP frameNavigated counts document loads, not same-document history.
cdp.on('Page.frameNavigated', event => { if (!event.frame.parentId) evidence.navigations.push(event.frame.url); });
page.on('pageerror', error => evidence.errors.push(error.message));
page.on('websocket', socket => {
  socket.on('framereceived', frame => {
    try { const value = JSON.parse(String(frame.payload)); evidence.messages.push({ type: value.type, event: value.event, paths: value.updates?.map(update => update.path) }); }
    catch { evidence.messages.push({ type: 'unparsed' }); }
  });
  socket.on('socketerror', () => evidence.errors.push('websocket-error'));
});
try {
  await page.goto(url, { waitUntil: 'networkidle' });
  await page.getByRole('heading', { name: `Version ${before}`, exact: true }).waitFor();
  evidence.document = await page.evaluate(() => window.__engineboxDocument = crypto.randomUUID());
  await page.getByRole('button', { name: 'Local interaction', exact: true }).click({ clickCount: 2 });
  await page.locator('#local-count').filter({ hasText: '2 local' }).waitFor();
  const state = () => page.evaluate(() => fetch('/api/state').then(response => response.json()));
  evidence.before = await state();
  await page.getByRole('button', { name: 'Save an interaction', exact: true }).click();
  await page.locator('#saved-count').filter({ hasText: `${evidence.before.count + 1} saved` }).waitFor();
  evidence.saved = await state();
  assert(evidence.handshakes.includes(101), 'Public HMR WebSocket did not upgrade');
  assert.deepEqual(evidence.errors, []);
  const readyFrames = evidence.messages.length;
  await writeFile(`${output}.ready`, JSON.stringify(evidence, null, 2));
  console.log('Public browser ready; promote the next fixture revision now');
  await page.getByRole('heading', { name: `Version ${after}`, exact: true }).waitFor({ timeout: 180000 });
  evidence.after = await state();
  assert.equal(evidence.after.version, Number(after));
  assert.equal(await page.evaluate(() => window.__engineboxDocument), evidence.document, 'Document reloaded');
  assert.equal(evidence.navigations.length, 1);
  // Framework refresh boundaries may remount a component. Record this separately
  // from document/process continuity and durable application state.
  evidence.localStateAfter = await page.locator('#local-count').textContent();
  evidence.localStatePreserved = evidence.localStateAfter === '2 local';
  assert.equal(evidence.after.identity, evidence.saved.identity);
  assert.equal(evidence.after.count, evidence.saved.count);
  assert(evidence.messages.slice(readyFrames).some(value => ['update', 'custom'].includes(value.type)), 'No HMR update received');
  assert.deepEqual(evidence.errors, []);
  await page.screenshot({ path: `${output}.png`, fullPage: true });
  await writeFile(output, JSON.stringify(evidence, null, 2));
  console.log(JSON.stringify(evidence));
} catch (error) {
  evidence.failure = error.message;
  await writeFile(output, JSON.stringify(evidence, null, 2));
  throw error;
} finally { await browser.close(); }
