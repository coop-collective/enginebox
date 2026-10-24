// Opt-in fixture probe. Keep this browser open while a separate operator promotes
// the next LayerFS revision. No reload/goto is issued after the first navigation.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { writeFile } from 'node:fs/promises';
const require = createRequire(new URL('../../dashboard/package.json', import.meta.url));
const { chromium } = require('@playwright/test');
const [url, before, after, output] = process.argv.slice(2);
assert(process.env.ENGINEBOX_LIVE_WATCH === '1' && url && before && after && output, 'Set ENGINEBOX_LIVE_WATCH=1 and supply URL BEFORE_VERSION AFTER_VERSION OUTPUT');
assert(['http:', 'https:'].includes(new URL(url).protocol));
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1440, height: 950 } });
const evidence = { url, handshakes: [], frames: [], navigations: [], errors: [] };
const cdp = await page.context().newCDPSession(page); await cdp.send('Network.enable');
cdp.on('Network.webSocketHandshakeResponseReceived', event => evidence.handshakes.push(event.response.status));
page.on('pageerror', error => evidence.errors.push(error.message));
page.on('framenavigated', frame => { if (frame === page.mainFrame()) evidence.navigations.push({ url: frame.url(), time: new Date().toISOString() }); });
page.on('websocket', socket => {
  socket.on('framereceived', frame => evidence.frames.push(String(frame.payload)));
  socket.on('socketerror', error => evidence.errors.push(String(error)));
});
try {
  await page.goto(url, { waitUntil: 'networkidle' });
  await page.getByRole('heading', { name: `Version ${before}`, exact: true }).waitFor();
  const state = () => page.evaluate(() => fetch('/api/state').then(response => response.json()));
  evidence.before = await state();
  await page.getByRole('button', { name: 'Save an interaction' }).click();
  await page.locator('#count').filter({ hasText: `${evidence.before.count + 1} saved` }).waitFor();
  evidence.saved = await state();
  assert(evidence.handshakes.includes(101), 'Dev server WebSocket did not upgrade through this URL');
  assert.deepEqual(evidence.errors, []);
  await writeFile(`${output}.ready`, JSON.stringify(evidence, null, 2));
  console.log('Browser ready; promote the next fixture revision now');
  await page.getByRole('heading', { name: `Version ${after}`, exact: true }).waitFor({ timeout: 180000 });
  evidence.after = await state();
  assert.equal(evidence.after.identity, evidence.saved.identity);
  assert.equal(evidence.after.count, evidence.saved.count);
  assert(evidence.frames.length > 0 && evidence.navigations.length >= 2);
  assert.deepEqual(evidence.errors, []);
  await page.screenshot({ path: `${output}.png`, fullPage: true });
  await writeFile(output, JSON.stringify(evidence, null, 2));
  console.log(JSON.stringify(evidence));
} finally { await browser.close(); }
