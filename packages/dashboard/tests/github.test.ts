import assert from "node:assert/strict";
import { test } from "node:test";
import { flowCookie, validFlow, connectGitHub, githubCallback } from "../lib/github-server";
process.env.ENGINEBOX_DASHBOARD_PASSWORD = "fixture-dashboard-password";
process.env.ENGINEBOX_SESSION_SECRET = "fixture-session-secret-at-least-32-characters";
process.env.ENGINEBOX_API_TOKEN = "fixture-api-token-at-least-32-characters";
process.env.ENGINEBOX_DASHBOARD_ORIGIN = "https://example.invalid";

test("GitHub callback cookie is signed, expiring and survives only its OAuth flow", () => {
  const state = "a".repeat(43), now = Date.now(), cookie = flowCookie(state, now);
  const request = new Request("https://example.invalid/api/github/callback", { headers: { cookie } });
  assert.ok(cookie.includes("HttpOnly") && cookie.includes("SameSite=Lax") && cookie.includes("Secure"));
  assert.equal(validFlow(request, state, now), true);
  assert.equal(validFlow(request, "b".repeat(43), now), false);
  assert.equal(validFlow(request, state, now + 600_001), false);
  assert.equal(validFlow(new Request(request.url, { headers: { cookie: cookie.replace(state, "b".repeat(43)) } }), "b".repeat(43), now), false);
});
test("OAuth cannot originate without an authenticated dashboard and callback cannot skip its cookie", async () => {
  await assert.rejects(connectGitHub(new Request("https://example.invalid/api/github/connect", { method: "POST" })), /Sign in/);
  await assert.rejects(githubCallback(new Request("https://example.invalid/api/github/callback?state=attacker&code=secret")), /did not start/);
});

test("webhooks pass the supervisor bearer gate while retaining controller signature validation", async () => {
  const { githubWebhook } = await import("../lib/github-server");
  const previous = globalThis.fetch;
  const raw = '{ "action": "queued", "spacing": true }';
  let calls = 0;
  globalThis.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
    calls++;
    assert.ok(String(input).endsWith("/v1/github/webhook/runners"));
    assert.equal(Buffer.from(init?.body as Uint8Array).toString(), raw);
    const headers = new Headers(init?.headers);
    // The production supervisor requires its own credential before forwarding mutations.
    if (headers.get("authorization") !== `Bearer ${process.env.ENGINEBOX_API_TOKEN}`) {
      return Response.json({ error: "authentication required" }, { status: 401 });
    }
    if (headers.get("x-hub-signature-256") !== "sha256=fixture") {
      return Response.json({ error: "invalid signature" }, { status: 403 });
    }
    return Response.json({ accepted: true }, { status: 202 });
  }) as typeof fetch;
  try {
    const result = await githubWebhook(new Request("https://example.invalid/api/github/webhook/runners", { method: "POST", headers: { "x-hub-signature-256": "sha256=fixture", "x-github-event": "workflow_job" }, body: raw }), "runners");
    assert.equal(result.status, 202);
    assert.deepEqual(await result.json(), { accepted: true });
    const rejected = await githubWebhook(new Request("https://example.invalid/api/github/webhook/runners", { method: "POST", headers: { authorization: "Bearer attacker", "x-hub-signature-256": "invalid", "x-github-event": "workflow_job" }, body: raw }), "runners");
    assert.equal(rejected.status, 403);
    assert.equal(rejected.headers.get("authorization"), null);
    assert.ok(!(await rejected.text()).includes(process.env.ENGINEBOX_API_TOKEN!));
    await assert.rejects(githubWebhook(new Request("https://example.invalid/api/github/webhook/runners", { method: "POST", headers: { origin: "https://example.invalid" }, body: raw }), "runners"), /Unknown webhook/);
    assert.equal(calls, 2);
  } finally { globalThis.fetch = previous; }
});
