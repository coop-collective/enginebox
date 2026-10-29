import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, test, type TestContext } from "node:test";
import { startEmailSignIn, verifyEmailCode } from "../lib/identity/auth";
import { orgAction } from "../lib/identity/org";
import { readIdentity } from "../lib/identity/store";
import { catalog } from "../lib/catalog";
import { HttpError } from "../lib/server";

const origin = "https://enginebox.example";
let dir = "";
beforeEach(() => {
  process.env.ENGINEBOX_DASHBOARD_PASSWORD = "test-only-dashboard-password";
  process.env.ENGINEBOX_SESSION_SECRET = "test-only-session-signing-secret-123456789";
  process.env.ENGINEBOX_API_TOKEN = "test-only-backend-token-never-in-browser";
  process.env.ENGINEBOX_DASHBOARD_ORIGIN = origin;
  process.env.ENGINEBOX_API_URL = "http://127.0.0.1:8765";
  dir = mkdtempSync(join(tmpdir(), "enginebox-catalog-"));
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
  const sent: { path: string; method: string; body: unknown; actor?: string; org?: string }[] = [];
  t.mock.method(globalThis, "fetch", async (url: string, init: RequestInit = {}) => {
    const headers = init.headers as Record<string, string>;
    const body = init.body ? JSON.parse(init.body as string) : undefined;
    const path = new URL(url).pathname;
    sent.push({ path, method: init.method ?? "GET", body, actor: headers["X-EngineBox-Actor"], org: headers["X-EngineBox-Org"] });
    if (path === "/v1/connections" && init.method === "GET")
      return Response.json({ connections: [{ name: "kitbash", kind: "mcp_http", secret_names: ["KITBASH_TOKEN"], used_by: ["coder"] }] });
    if (path === "/v1/connections" && init.method === "POST")
      return Response.json({ connection: { name: body.name, secret_names: Object.keys(body.secrets ?? {}) }, secrets_set: Object.keys(body.secrets ?? {}) }, { status: 201 });
    if (path === "/v1/profiles/coder") return Response.json({ profile: { name: "coder" } });
    if (path === "/v1/profiles/other") return Response.json({ error: "Resource unavailable or unknown endpoint." }, { status: 404 });
    if (path === "/v1/agent-runs")
      return Response.json({ runs: [{ id: "a", profile: { name: "coder", run_kind: "debug" } }, { id: "b", profile: { name: "other" } }, { id: "c" }] });
    if (path.startsWith("/v1/profiles/coder/connections/")) return Response.json({ profile: { name: "coder" }, changed: true });
    return Response.json({ error: "unexpected" }, { status: 500 });
  });
  return sent;
}

test("members read the catalog; only admins write it, as themselves; values pass through once", async (t) => {
  const sent = fakeController(t);
  const owner = await signIn("owner@example.com");
  await orgAction(req("/api/org/members/import", "POST", owner, { members: [{ email: "m@example.com" }] }), ["members", "import"]);
  const member = await signIn("m@example.com");
  const ownerId = readIdentity().users.find((u) => u.email === "owner@example.com")!.id;

  const listed = await (await catalog(req("/api/catalog/connections", "GET", member), ["connections"])).json();
  assert.equal(listed.connections[0].name, "kitbash");
  assert.equal(listed.can_manage, false);

  const forbidden = (e: unknown) => e instanceof HttpError && e.status === 403;
  await assert.rejects(catalog(req("/api/catalog/connections", "POST", member, { name: "x", kind: "mcp_http" }), ["connections"]), forbidden);
  await assert.rejects(catalog(req("/api/catalog/profiles/coder/connections/attach", "POST", member, { connection: "kitbash" }), ["profiles", "coder", "connections", "attach"]), forbidden);
  await assert.rejects(catalog(req("/api/catalog/audit", "GET", member), ["audit"]), forbidden);
  await assert.rejects(catalog(req("/api/catalog/repositories/alpha/site/approve", "POST", member, { repo_profile: "deploy", requirement: "X" }), ["repositories", "alpha", "site", "approve"]), forbidden);
  assert.ok(!sent.some((s) => s.method === "POST"), "a member write reached the controller");

  const value = "secret-value-never-returned-1";
  const created = await catalog(req("/api/catalog/connections", "POST", owner, { name: "kb", kind: "mcp_http", secrets: { KITBASH_TOKEN: value } }), ["connections"]);
  assert.equal(created.status, 201);
  assert.ok(!(await created.text()).includes(value));
  const write = sent.find((s) => s.method === "POST")!;
  assert.equal(write.path, "/v1/connections");
  assert.equal(write.actor, ownerId);
  assert.equal(write.org, undefined, "platform-org requests carry no org header");

  const attached = await catalog(req("/api/catalog/profiles/coder/connections/policy", "POST", owner, { connection_policy: { planner: [] } }), ["profiles", "coder", "connections", "policy"]);
  assert.equal(attached.status, 200);

  // Runs are listed per profile, only for a profile the org can see.
  const runs = await (await catalog(req("/api/catalog/profiles/coder/runs", "GET", member), ["profiles", "coder", "runs"])).json();
  assert.deepEqual(runs.runs.map((r: { id: string }) => r.id), ["a"]);
  await assert.rejects(catalog(req("/api/catalog/profiles/other/runs", "GET", member), ["profiles", "other", "runs"]), (e: unknown) => e instanceof HttpError && e.status === 404);

  // Unknown routes and path tricks are refused.
  const unknown = (e: unknown) => e instanceof HttpError && e.status === 404;
  await assert.rejects(catalog(req("/api/catalog/connections/x/rename", "POST", owner, {}), ["connections", "x", "rename"]), unknown);
  await assert.rejects(catalog(req("/api/catalog/connections/..", "GET", owner), ["connections", ".."]), unknown);
  await assert.rejects(catalog(req("/api/catalog/catalog/apply", "POST", owner, {}), ["catalog", "apply"]), unknown);
  // Proposals are not a dashboard write: only approve and revoke are.
  await assert.rejects(catalog(req("/api/catalog/repositories/alpha/site/propose", "POST", owner, {}), ["repositories", "alpha", "site", "propose"]), unknown);
});
