import { test } from 'node:test';
import assert from 'node:assert/strict';
import worker from '../edge/worker.mjs';
test('edge preserves the route hostname and stream while preventing client-selected upstreams', async () => {
  let called = false;
  const env = { PUBLIC_HOSTS: 'app.example.com', GATEWAY: { fetch: async (request, options) => {
    called = true; assert.equal(request.url, 'http://app.example.com/path?x=1');
    assert.equal(request.headers.get('x-forwarded-host'), 'app.example.com');
    assert.equal(request.headers.get('forwarded'), null); assert.equal(request.headers.get('x-forwarded-for'), null);
    assert.equal(request.headers.get('authorization'), 'Basic example');
    assert.equal(options.redirect, 'manual'); assert.equal(await request.text(), 'payload');
    return new Response('response', { status: 202 });
  } } };
  const result = await worker.fetch(new Request('https://app.example.com/path?x=1', { method: 'POST', headers: { authorization: 'Basic example', forwarded: 'host=169.254.169.254', 'x-forwarded-host': 'evil.invalid', 'x-forwarded-for': '10.0.0.1' }, body: 'payload' }), env);
  assert(called); assert.equal(result.status, 202); assert.equal(await result.text(), 'response'); assert.equal(result.headers.get('cache-control'), 'no-store');
  called = false; assert.equal((await worker.fetch(new Request('https://unknown.example.com'), env)).status, 404); assert(!called);
  assert.equal((await worker.fetch(new Request('http://app.example.com'), env)).status, 307);
});
