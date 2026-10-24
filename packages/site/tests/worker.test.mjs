import test from 'node:test';
import assert from 'node:assert/strict';
import worker from '../worker.mjs';

test('dashboard requests retain credentials, query and streamed body on the fixed binding', async () => {
  const upstream = new Response('streamed response', { status: 202, headers: { 'set-cookie': 'session=fixture; HttpOnly; Secure' } });
  const result = await worker.fetch(new Request('https://example.com/dashboard/?_rsc=fixture', {
    method: 'POST', body: 'request body',
    headers: { authorization: 'Bearer fixture', cookie: 'session=fixture', origin: 'https://example.com', RSC: '1' },
  }), { DASHBOARD: { async fetch(request, options) {
    assert.equal(request.url, 'https://example.com/?_rsc=fixture');
    assert.equal(request.method, 'POST');
    assert.equal(request.headers.get('authorization'), 'Bearer fixture');
    assert.equal(request.headers.get('cookie'), 'session=fixture');
    assert.equal(request.headers.get('origin'), 'https://example.com');
    assert.equal(request.headers.get('RSC'), '1');
    assert.equal(options.redirect, 'manual');
    assert.equal(await request.text(), 'request body');
    return upstream;
  } } });
  assert.equal(result, upstream);
  assert.equal(await result.text(), 'streamed response');
});

test('API, assets and health requests reach the controller without path rewrites', async () => {
  for (const path of ['/api/session', '/api/node/enroll', '/api/node/poll', '/api/contributors/enrollments', '/_next/static/chunks/app.js', '/healthz']) {
    const url = 'https://example.com' + path;
    const response = await worker.fetch(new Request(url), { DASHBOARD: { async fetch(request) {
      assert.equal(request.url, url);
      return new Response('unauthorized', { status: 401 });
    } } });
    assert.equal(response.status, 401);
  }
});

test('website and unknown routes stay with static assets, including upstream-looking input', async () => {
  for (const path of ['/', '/docs/quickstart/', '/pricing/', '/missing', '/dashboard/unknown', '/api-other', '/?url=http://169.254.169.254/']) {
    const request = new Request('https://example.com' + path);
    const response = await worker.fetch(request, { ASSETS: { async fetch(input) {
      assert.equal(input, request);
      return new Response('static', { status: 404 });
    } } });
    assert.equal(response.status, 404);
  }
});

test('dashboard canonical redirect stays relative and preserves the query', async () => {
  const response = await worker.fetch(new Request('https://example.com/dashboard?next=https://other.invalid'), {});
  assert.equal(response.status, 307);
  assert.equal(response.headers.get('location'), '/dashboard/?next=https://other.invalid');
});

test('an unavailable dashboard fails without serving a misleading static page', async () => {
  const response = await worker.fetch(new Request('https://example.com/api/node/enroll'), {
    DASHBOARD: { fetch() { throw new Error('private diagnostic'); } },
  });
  assert.equal(response.status, 503);
  assert.equal(response.headers.get('cache-control'), 'no-store');
  assert.doesNotMatch(await response.text(), /private diagnostic/);
});
