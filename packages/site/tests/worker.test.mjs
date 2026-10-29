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
  for (const path of ['/api/session', '/api/node/enroll', '/api/node/poll', '/api/contributors/enrollments', '/_next/static/chunks/app.js', '/healthz',
    '/.well-known/oauth-authorization-server', '/.well-known/oauth-protected-resource', '/oauth/register', '/oauth/token', '/v1/agent-runs', '/v1/pools/p/agent-runs']) {
    const url = 'https://example.com' + path;
    const response = await worker.fetch(new Request(url), { DASHBOARD: { async fetch(request) {
      assert.equal(request.url, url);
      return new Response('unauthorized', { status: 401 });
    } } });
    assert.equal(response.status, 401);
  }
});

test('website and unknown routes stay with static assets, including upstream-looking input', async () => {
  for (const path of ['/', '/docs/quickstart/', '/pricing/', '/missing', '/dashboard/unknown', '/api-other', '/?url=http://169.254.169.254/',
    '/.well-known/security.txt', '/oauthx', '/v1']) {
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

const envelopeHeaders = {
  'content-type': 'application/json',
  'x-enginebox-timestamp': '1800000000',
  'x-enginebox-nonce': '0'.repeat(32),
  'x-enginebox-signature': 'v1=' + 'a'.repeat(64),
};

test('signed repository requests are forwarded over RPC with their headers and body, never cached', async () => {
  const calls = [];
  const env = { ARTIFACTS: { async invoke(envelope) {
    calls.push(envelope);
    return { status: 201, body: { token: { id: 't1' } } };
  } } };
  const body = JSON.stringify({ namespace: 'enginebox-alpha', repo: 'game', scope: 'write', ttl: 720 });
  const response = await worker.fetch(new Request('https://example.com/_artifacts/tokens.create', { method: 'POST', headers: envelopeHeaders, body }), env);
  assert.equal(response.status, 201);
  assert.equal(response.headers.get('cache-control'), 'no-store');
  assert.deepEqual(await response.json(), { token: { id: 't1' } });
  assert.deepEqual(calls, [{
    method: 'POST', path: '/tokens.create', timestamp: '1800000000', nonce: '0'.repeat(32), signature: 'v1=' + 'a'.repeat(64), body,
  }]);
  for (const op of ['repos.create', 'repos.get', 'repos.list', 'tokens.revoke']) {
    const forwarded = await worker.fetch(new Request(`https://example.com/_artifacts/${op}`, { method: 'POST', headers: envelopeHeaders, body: '{}' }), env);
    assert.equal(forwarded.status, 201, op);
    assert.equal(calls.at(-1).path, `/${op}`);
  }
  // The gateway's verdict, a 401 included, passes through unchanged.
  const refused = await worker.fetch(new Request('https://example.com/_artifacts/repos.get', { method: 'POST', headers: envelopeHeaders, body: '{}' }),
    { ARTIFACTS: { async invoke() { return { status: 401, body: { error: 'unauthorized' } }; } } });
  assert.deepEqual([refused.status, await refused.json()], [401, { error: 'unauthorized' }]);
});

test('anything else under /_artifacts is refused before the gateway; an unavailable gateway is a 503', async () => {
  const env = { ARTIFACTS: { invoke() { throw new Error('must not be called'); } }, ASSETS: { fetch() { throw new Error('must not be static'); } } };
  for (const [method, path, status] of [
    ['GET', '/_artifacts/repos.list', 405],
    ['PUT', '/_artifacts/repos.list', 405],
    ['OPTIONS', '/_artifacts/repos.list', 405],
    ['GET', '/_artifacts', 405],
    ['POST', '/_artifacts', 404],
    ['POST', '/_artifacts/', 404],
    ['POST', '/_artifacts/repos.delete', 404],
    ['POST', '/_artifacts/repos.list/extra', 404],
    ['POST', '/_artifacts/repos.list?x=1', 404],
  ]) {
    const response = await worker.fetch(new Request('https://example.com' + path, method === 'POST' ? { method, headers: envelopeHeaders, body: '{}' } : { method }), env);
    assert.equal(response.status, status, `${method} ${path}`);
    assert.equal(response.headers.get('cache-control'), 'no-store');
  }
  const browser = await worker.fetch(new Request('https://example.com/_artifacts/repos.list', { method: 'POST', headers: { ...envelopeHeaders, origin: 'https://evil.example' }, body: '{}' }), env);
  assert.equal(browser.status, 403);
  const big = await worker.fetch(new Request('https://example.com/_artifacts/repos.create', { method: 'POST', headers: envelopeHeaders, body: 'x'.repeat(16385) }), env);
  assert.equal(big.status, 413);
  const down = await worker.fetch(new Request('https://example.com/_artifacts/repos.list', { method: 'POST', headers: envelopeHeaders, body: '{}' }),
    { ARTIFACTS: { invoke() { throw new Error('private diagnostic'); } } });
  assert.equal(down.status, 503);
});

test('paths that only look like /_artifacts stay static', async () => {
  for (const path of ['/_artifactsx', '/_artifacts.json', '/x/_artifacts/repos.list']) {
    const request = new Request('https://example.com' + path, { method: 'GET' });
    const response = await worker.fetch(request, { ASSETS: { async fetch(input) {
      assert.equal(input, request);
      return new Response('static', { status: 404 });
    } }, ARTIFACTS: { invoke() { throw new Error('must not be called'); } } });
    assert.equal(response.status, 404, path);
    assert.equal(await response.text(), 'static');
  }
});

test('signed email requests go to the email gateway over RPC; everything else under /_email is refused', async () => {
  const calls = [];
  const env = { EMAIL_GATEWAY: { async send(envelope) { calls.push(envelope); return { status: 200, body: { sent: true } }; } }, ASSETS: { fetch() { throw new Error('must not be static'); } } };
  const body = JSON.stringify({ template: 'sign_in' });
  const response = await worker.fetch(new Request('https://example.com/_email/send', { method: 'POST', headers: envelopeHeaders, body }), env);
  assert.deepEqual([response.status, await response.json()], [200, { sent: true }]);
  assert.equal(response.headers.get('cache-control'), 'no-store');
  assert.deepEqual(calls, [{ timestamp: '1800000000', nonce: '0'.repeat(32), signature: 'v1=' + 'a'.repeat(64), body }]);
  for (const [method, path, status] of [
    ['GET', '/_email/send', 405],
    ['POST', '/_email', 404],
    ['POST', '/_email/other', 404],
    ['POST', '/_email/send?x=1', 404],
  ]) {
    const r = await worker.fetch(new Request('https://example.com' + path, method === 'POST' ? { method, headers: envelopeHeaders, body: '{}' } : { method }), env);
    assert.equal(r.status, status, `${method} ${path}`);
  }
  assert.equal(calls.length, 1);
  const browser = await worker.fetch(new Request('https://example.com/_email/send', { method: 'POST', headers: { ...envelopeHeaders, origin: 'https://evil.example' }, body }), env);
  assert.equal(browser.status, 403);
  const big = await worker.fetch(new Request('https://example.com/_email/send', { method: 'POST', headers: envelopeHeaders, body: 'x'.repeat(4097) }), env);
  assert.equal(big.status, 413);
  const unbound = await worker.fetch(new Request('https://example.com/_email/send', { method: 'POST', headers: envelopeHeaders, body }), {});
  assert.equal(unbound.status, 503);
  const down = await worker.fetch(new Request('https://example.com/_email/send', { method: 'POST', headers: envelopeHeaders, body }), { EMAIL_GATEWAY: { send() { throw new Error('private'); } } });
  assert.equal(down.status, 503);
});
