import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, test, type TestContext } from "node:test";
import { startEmailSignIn, verifyEmailCode } from "../lib/identity/auth";
import { engineSessions } from "../lib/engine-sessions";
import { contributors, enginePeerProxy, nodeProxy } from "../lib/contributors";
import { HttpError } from "../lib/server";

const origin = "https://enginebox.example";
const nodeToken = `${"a".repeat(32)}.${"b".repeat(64)}`;
let dir = "";
beforeEach(() => {
  process.env.ENGINEBOX_SESSION_SECRET = "test-only-session-signing-secret-123456789";
  process.env.ENGINEBOX_API_TOKEN = "test-only-backend-token-never-in-browser";
  process.env.ENGINEBOX_DASHBOARD_ORIGIN = origin;
  process.env.ENGINEBOX_API_URL = "http://127.0.0.1:8765";
  dir = mkdtempSync(join(tmpdir(), "enginebox-engine-sessions-"));
  process.env.ENGINEBOX_IDENTITY_STATE_DIR = dir;
  process.env.ENGINEBOX_EMAIL_TRANSPORT = "file";
  process.env.ENGINEBOX_OWNER_EMAIL = "owner@example.com";
  delete process.env.ENGINEBOX_TENANCY_MODE;
});
afterEach(() => rmSync(dir, { recursive: true, force: true }));

let ip = 0;
const req = (path: string, method: string, cookie: string, body?: unknown) =>
  new Request(`${origin}${path}`, {
    method,
    headers: { origin, "content-type": "application/json", cookie, "cf-connecting-ip": `198.51.100.${++ip}` },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
async function signIn(email: string) {
  const r = await (await startEmailSignIn(req("/api/auth/email", "POST", "", { email }))).json();
  const file = join(dir, "outbox", "outbox.jsonl");
  const code = existsSync(file) ? readFileSync(file, "utf8").trim().split("\n").map((l) => JSON.parse(l)).filter((m) => m.to === email).at(-1).code : "";
  const verified = await verifyEmailCode(req("/api/auth/email/verify", "POST", "", { challenge: r.challenge, code }));
  return (verified.headers.get("set-cookie") ?? "").split(";")[0];
}
function fakeController(t: TestContext) {
  const sent: { url: string; method: string; body: unknown; actor?: string; org?: string }[] = [];
  t.mock.method(globalThis, "fetch", async (url: string, init: RequestInit = {}) => {
    const headers = init.headers as Record<string, string>;
    sent.push({ url, method: init.method ?? "GET", body: init.body ? JSON.parse(init.body as string) : undefined, actor: headers?.["X-EngineBox-Actor"], org: headers?.["X-EngineBox-Org"] });
    const path = new URL(url).pathname;
    if (path === "/v1/tenancy") return Response.json({ mode: "self_hosted", host_orgs: {}, grants: [] });
    if (path.startsWith("/v1/projects/qoh/engine-sessions")) return Response.json({ id: "c".repeat(32), state: "queued" }, { status: init.method === "POST" && path.endsWith("engine-sessions") ? 202 : 200 });
    if (path.endsWith("/engine-owner")) return Response.json({ id: "d".repeat(32), engine_owner: (JSON.parse(init.body as string) as { member: string }).member });
    if (path.startsWith("/v1/node/")) return Response.json({ sessions: [] });
    return Response.json({ error: "unexpected" }, { status: 500 });
  });
  return sent;
}

test("members start and read their own engine sessions; the member comes from the session", async (t) => {
  const sent = fakeController(t);
  const owner = await signIn("owner@example.com");
  const start = { request_id: "qoh-1", repository: "queen-of-hearts", ref: "main", project_path: "unity-project", engine: { kind: "unity", version: "6000.6.5f1" } };
  const created = await engineSessions(req("/api/engine-sessions/qoh", "POST", owner, start), ["qoh"]);
  assert.equal(created.status, 202);
  const forwarded = sent.at(-1)!;
  assert.equal(forwarded.url, "http://127.0.0.1:8765/v1/projects/qoh/engine-sessions");
  assert.match(forwarded.actor ?? "", /^usr_[a-f0-9]{24}$/);
  assert.deepEqual(forwarded.body, start);
  const id = "c".repeat(32);
  await engineSessions(req(`/api/engine-sessions/qoh/${id}`, "GET", owner), ["qoh", id]);
  assert.equal(sent.at(-1)!.url, `http://127.0.0.1:8765/v1/projects/qoh/engine-sessions/${id}`);
  for (const action of ["renew", "end", "watch-token"]) {
    await engineSessions(req(`/api/engine-sessions/qoh/${id}/${action}`, "POST", owner, {}), ["qoh", id, action]);
    assert.equal(sent.at(-1)!.url, `http://127.0.0.1:8765/v1/projects/qoh/engine-sessions/${id}/${action}`);
  }
  const count = sent.length;
  // Unknown operations, other paths and query strings never reach the controller.
  for (const [method, segments] of [
    ["POST", ["qoh", id, "start"]],
    ["POST", ["qoh", id]],
    ["GET", ["qoh", id, "end"]],
    ["GET", ["qoh", id, "watch-token"]],
    ["GET", ["QOH"]],
    ["GET", ["qoh", "../x"]],
    ["GET", []],
  ] as [string, string[]][]) {
    await assert.rejects(engineSessions(req(`/api/engine-sessions/${segments.join("/")}`, method, owner, method === "POST" ? {} : undefined), segments), (e: unknown) => e instanceof HttpError && e.status === 404);
  }
  await assert.rejects(engineSessions(req("/api/engine-sessions/qoh?x=1", "GET", owner), ["qoh"]), (e: unknown) => e instanceof HttpError && e.status === 400);
  // Cross-origin writes and anonymous callers are refused.
  const cross = req("/api/engine-sessions/qoh", "POST", owner, start);
  cross.headers.set("origin", "https://attacker.example");
  await assert.rejects(engineSessions(cross, ["qoh"]));
  await assert.rejects(engineSessions(req("/api/engine-sessions/qoh", "GET", ""), ["qoh"]), (e: unknown) => e instanceof HttpError && e.status === 401);
  assert.equal(sent.length, count);
});

test("an admin claims an engine machine as their own; the member is never taken from the request", async (t) => {
  const sent = fakeController(t);
  const owner = await signIn("owner@example.com");
  const id = "d".repeat(32);
  await contributors(req(`/api/contributors/${id}/engine-owner`, "POST", owner, {}), [id, "engine-owner"]);
  const claim = sent.find((s) => s.url.endsWith("/engine-owner"))!;
  assert.equal(claim.url, `http://127.0.0.1:8765/v1/contributors/${id}/engine-owner`);
  assert.match((claim.body as { member: string }).member, /^usr_[a-f0-9]{24}$/);
  await assert.rejects(contributors(req(`/api/contributors/${id}/engine-owner`, "POST", owner, { member: "usr_bbbbbbbbbbbbbbbbbbbbbbbb" }), [id, "engine-owner"]), (e: unknown) => e instanceof HttpError && e.status === 400);
  // A request without a member session cannot claim a machine.
  await assert.rejects(contributors(req(`/api/contributors/${id}/engine-owner`, "POST", "", {}), [id, "engine-owner"]), (e: unknown) => e instanceof HttpError && e.status === 401);
});

test("the node proxy allows the engine channel and nothing else new", async (t) => {
  const sent = fakeController(t);
  const node = (action: string) => new Request(`${origin}/api/node/${action}`, { method: "POST", headers: { Authorization: `Bearer ${nodeToken}`, "Content-Type": "application/json" }, body: "{}" });
  for (const action of ["engine-poll", "engine-report"]) {
    await nodeProxy(node(action), action);
    assert.equal(sent.at(-1)!.url, `http://127.0.0.1:8765/v1/node/${action}`);
  }
  for (const action of ["engine-start", "engine-poll/../poll", "engine"]) {
    await assert.rejects(nodeProxy(node(action), action), (e: unknown) => e instanceof HttpError && e.status === 404);
  }
});

test("the stream host's peer routes forward its host token and nothing else", async (t) => {
  const sent = fakeController(t);
  const host = `ehp_${"c".repeat(64)}`;
  const peer = "e".repeat(32);
  const call = (path: string, method = "POST", token = host, extra: Record<string, string> = {}) =>
    enginePeerProxy(new Request(`${origin}/api/node/engine-peers/${path}`, { method, headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json", ...extra }, body: method === "POST" ? "{}" : undefined }), path.split("?")[0].split("/"));
  await call("poll");
  assert.equal(sent.at(-1)!.url, "http://127.0.0.1:8765/v1/node/engine-peers/poll");
  await call(`${peer}/signals?after=0`, "GET");
  assert.equal(sent.at(-1)!.url, `http://127.0.0.1:8765/v1/node/engine-peers/${peer}/signals?after=0`);
  assert.equal(sent.at(-1)!.method, "GET");
  await call(`${peer}/signals`);
  await call(`${peer}/end`);
  assert.equal(sent.at(-1)!.url, `http://127.0.0.1:8765/v1/node/engine-peers/${peer}/end`);
  const count = sent.length;
  for (const [path, method] of [["poll", "GET"], [`${peer}/end`, "GET"], [`${peer}/signals?after=x`, "GET"], ["nope/signals", "POST"], [`${peer}/answer`, "POST"], [`${peer}/signals/extra`, "POST"]] as const)
    await assert.rejects(call(path, method), (e: unknown) => e instanceof HttpError && e.status === 404, `${method} ${path}`);
  await assert.rejects(call("poll", "POST", nodeToken), (e: unknown) => e instanceof HttpError && e.status === 401);
  await assert.rejects(call("poll", "POST", host, { origin }), (e: unknown) => e instanceof HttpError && e.status === 403);
  assert.equal(sent.length, count);
});
