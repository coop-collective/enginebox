import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, test, type TestContext } from "node:test";
import { startEmailSignIn, verifyEmailCode } from "../lib/identity/auth";
import { orgAction } from "../lib/identity/org";
import { readIdentity } from "../lib/identity/store";
import { credentials } from "../lib/credentials";
import { HttpError } from "../lib/server";

const origin = "https://enginebox.example";
let dir = "";
beforeEach(() => {
  process.env.ENGINEBOX_DASHBOARD_PASSWORD = "test-only-dashboard-password";
  process.env.ENGINEBOX_SESSION_SECRET = "test-only-session-signing-secret-123456789";
  process.env.ENGINEBOX_API_TOKEN = "test-only-backend-token-never-in-browser";
  process.env.ENGINEBOX_DASHBOARD_ORIGIN = origin;
  process.env.ENGINEBOX_API_URL = "http://127.0.0.1:8765";
  dir = mkdtempSync(join(tmpdir(), "enginebox-credentials-"));
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

/** A fake controller /v1/credentials store. */
function fakeController(t: TestContext) {
  const records: Record<string, unknown>[] = [];
  const sent: { url: string; body: unknown; org?: string }[] = [];
  t.mock.method(globalThis, "fetch", async (url: string, init: RequestInit = {}) => {
    const body = init.body ? JSON.parse(init.body as string) : undefined;
    sent.push({ url, body, org: (init.headers as Record<string, string>)["X-EngineBox-Org"] });
    const path = new URL(url).pathname;
    if (path === "/v1/credentials" && init.method === "GET") return Response.json({ credentials: records });
    if (path === "/v1/credentials" && init.method === "POST") {
      const record = { id: `${records.length}`.padStart(32, "a"), status: body.kind === "api_key" ? "connected" : "pending", has_key: body.kind === "api_key", share_with_org_pools: false, owner_user_id: null, ...body };
      delete (record as { key?: unknown }).key;
      records.push(record);
      return Response.json({ credential: record }, { status: 201 });
    }
    const m = /^\/v1\/credentials\/([a-f0-9]{32})\/(login|share|delete)$/.exec(path);
    if (m) {
      const record = records.find((r) => r.id === m[1])!;
      if (m[2] === "share") record.share_with_org_pools = body.share;
      return Response.json(m[2] === "login" ? { session: { id: "b".repeat(32), credential_id: m[1], status: "starting" } } : { credential: record }, { status: m[2] === "login" ? 202 : 200 });
    }
    return Response.json({ error: "unexpected" }, { status: 500 });
  });
  return { records, sent };
}

test("members connect their own subscription; ownership comes from the session, not the request", async (t) => {
  const { sent } = fakeController(t);
  const owner = await signIn("owner@example.com");
  await orgAction(req("/api/org/members/import", "POST", owner, { members: [{ email: "a@example.com" }, { email: "b@example.com" }] }), ["members", "import"]);
  const alice = await signIn("a@example.com");
  const bob = await signIn("b@example.com");
  const aliceId = readIdentity().users.find((u) => u.email === "a@example.com")!.id;
  const bobId = readIdentity().users.find((u) => u.email === "b@example.com")!.id;

  const created = await (await credentials(req("/api/credentials", "POST", alice, { kind: "personal", provider: "codex", label: "Mine", owner_user_id: bobId }), [])).json();
  assert.equal(created.credential.owner_user_id, aliceId);
  const id = created.credential.id;

  // Bob does not see Alice's unshared subscription, cannot sign it in, share it or delete it.
  const bobView = await (await credentials(req("/api/credentials", "GET", bob), [])).json();
  assert.deepEqual(bobView.credentials, []);
  const notFound = (e: unknown) => e instanceof HttpError && e.status === 404;
  await assert.rejects(credentials(req(`/api/credentials/${id}/login`, "POST", bob, {}), [id, "login"]), notFound);
  await assert.rejects(credentials(req(`/api/credentials/${id}/share`, "POST", bob, { share: true }), [id, "share"]), notFound);

  // Alice connects and shares; then Bob sees it but still cannot manage it.
  assert.equal((await credentials(req(`/api/credentials/${id}/login`, "POST", alice, {}), [id, "login"])).status, 202);
  await credentials(req(`/api/credentials/${id}/share`, "POST", alice, { share: true }), [id, "share"]);
  const shared = await (await credentials(req("/api/credentials", "GET", bob), [])).json();
  assert.equal(shared.credentials.length, 1);
  await assert.rejects(credentials(req(`/api/credentials/${id}/share`, "POST", bob, { share: false }), [id, "share"]), (e: unknown) => e instanceof HttpError && e.status === 403);
  await assert.rejects(credentials(req(`/api/credentials/${id}/login`, "POST", bob, {}), [id, "login"]), (e: unknown) => e instanceof HttpError && e.status === 403);

  // Team accounts and API keys are admin-only; the owner (admin) may add them and remove Alice's.
  await assert.rejects(credentials(req("/api/credentials", "POST", alice, { kind: "team", provider: "claude", label: "Team" }), []), (e: unknown) => e instanceof HttpError && e.status === 403);
  const key = await (await credentials(req("/api/credentials", "POST", owner, { kind: "api_key", provider: "claude", label: "CI", key: "sk-test-only-0123456789" }), [])).json();
  assert.ok(!JSON.stringify(key).includes("sk-test-only"));
  assert.equal((await credentials(req(`/api/credentials/${id}/delete`, "POST", owner, {}), [id, "delete"])).status, 200);
  // Platform-org requests carry no org header.
  assert.ok(sent.every((s) => s.org === undefined));
});

test("in another org the controller is told which org acts", async (t) => {
  const { sent } = fakeController(t);
  const owner = await signIn("owner@example.com");
  const personal = readIdentity().users[0].personal_org_id;
  await orgAction(req("/api/org/switch", "POST", owner, { org_id: personal }), ["switch"]);
  await credentials(req("/api/credentials", "POST", owner, { kind: "personal", provider: "cursor", label: "Mine" }), []);
  assert.equal(sent.at(-1)?.org, personal);
});
