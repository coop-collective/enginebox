import { test } from 'node:test';
import assert from 'node:assert/strict';
import { environmentProxy } from '../lib/environments';
import { sessionCookie } from '../lib/server';

test('environment proxy requires a dashboard session and origin and keeps credentials server-side', async () => {
  Object.assign(process.env, { ENGINEBOX_DASHBOARD_PASSWORD: 'test-only-dashboard-password', ENGINEBOX_SESSION_SECRET: 'test-only-session-secret-long-enough', ENGINEBOX_API_TOKEN: 'test-only-controller-token-long-enough', ENGINEBOX_DASHBOARD_ORIGIN: 'http://127.0.0.1:3000', ENGINEBOX_ENVIRONMENTS_URL: 'http://127.0.0.1:18790', ENGINEBOX_ENVIRONMENTS_TOKEN: 'test-only-environments-token-long-enough' });
  const request = (session: boolean, origin = '', method = 'GET') => new Request('http://127.0.0.1:3000/api/environments', { method, headers: { cookie: session ? sessionCookie().split(';')[0] : '', origin, 'content-type': 'application/json' }, ...(method === 'POST' ? { body: '{}' } : {}) });
  await assert.rejects(environmentProxy(request(false), 'v1/environments'), /Sign in/);
  await assert.rejects(environmentProxy(request(true, 'https://attacker.invalid', 'POST'), 'v1/environments/routes'), /origin/);
  await assert.rejects(environmentProxy(request(true), 'v1/environments/exec'), /Unknown/);
  const original = globalThis.fetch;
  try {
    globalThis.fetch = async (_url, options) => { assert.equal((options?.headers as Record<string, string>).authorization, `Bearer ${process.env.ENGINEBOX_ENVIRONMENTS_TOKEN}`); assert.equal(options?.redirect, 'error'); return Response.json({ environments: [] }); };
    const response = await environmentProxy(request(true), 'v1/environments');
    assert.deepEqual(await response.json(), { environments: [] });
  } finally { globalThis.fetch = original; }
});
