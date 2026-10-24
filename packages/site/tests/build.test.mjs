import test from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const build = fileURLToPath(new URL('../build.mjs', import.meta.url));
function buildSite(env = {}) {
  const dir = mkdtempSync(join(tmpdir(), 'enginebox-site-'));
  const result = spawnSync(process.execPath, [build], { env: { ...process.env, CHECKOUT_URL: '', ...env, OUT_DIR: dir }, encoding: 'utf8' });
  return { dir, result, read: file => readFileSync(join(dir, file), 'utf8') };
}

test('pricing page builds with the price, nav, sitemap and canonical link', () => {
  const site = buildSite();
  try {
    assert.equal(site.result.status, 0, site.result.stderr);
    assert.match(site.result.stderr, /CHECKOUT_URL is not set/);
    const html = site.read('pricing/index.html');
    assert.match(html, /\$10/);
    assert.match(html, /<link rel="canonical" href="https:\/\/enginebox\.coopcollective\.dev\/pricing\/">/);
    assert.match(html, /<a href="\/pricing\/" aria-current="page">Pricing<\/a>/);
    assert.match(html, /Checkout opening soon/);
    assert.doesNotMatch(html, /data-checkout[^>]*href=/);
    assert.match(html, /\$30/);
    assert.match(site.read('index.html'), /<nav id="main-nav"[^]*href="\/pricing\/"[^]*<\/nav>/);
    assert.match(site.read('index.html'), /site-footer[^]*href="\/pricing\/"/);
    assert.match(site.read('sitemap.xml'), /<loc>https:\/\/enginebox\.coopcollective\.dev\/pricing\/<\/loc>/);
    assert.match(site.read('docs/licensing/index.html'), /\$10 per node per month/);
    assert.doesNotMatch(site.read('index.html'), /Sustainable Use/);
  } finally { rmSync(site.dir, { recursive: true, force: true }); }
});

test('CHECKOUT_URL turns the plan button into a link', () => {
  const site = buildSite({ CHECKOUT_URL: 'https://pay.example.com/enginebox' });
  try {
    assert.equal(site.result.status, 0, site.result.stderr);
    const html = site.read('pricing/index.html');
    assert.match(html, /<a class="button primary" href="https:\/\/pay\.example\.com\/enginebox" data-checkout>Get a license/);
    assert.doesNotMatch(html, /Checkout opening soon/);
  } finally { rmSync(site.dir, { recursive: true, force: true }); }
});

test('CHECKOUT_URL must be https', () => {
  const site = buildSite({ CHECKOUT_URL: 'javascript:alert(1)' });
  try { assert.notEqual(site.result.status, 0); } finally { rmSync(site.dir, { recursive: true, force: true }); }
});
