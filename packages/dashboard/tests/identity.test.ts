import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, test } from "node:test";
import {
  acceptInvite,
  CHALLENGE_MINUTES,
  claimOwnerStart,
  invitePage,
  LIMITS,
  magicLinkConsume,
  magicLinkPage,
  MAX_CODE_ATTEMPTS,
  signOut,
  startEmailSignIn,
  verifyEmailCode,
} from "../lib/identity/auth";
import { orgAction, orgView } from "../lib/identity/org";
import { DEFAULT_ORG, readIdentity, withIdentity } from "../lib/identity/store";
import { forgetTenancy } from "../lib/tenancy";
import { HttpError, principal, proxy, scopeToOrg, sessionCookie } from "../lib/server";

const origin = "https://enginebox.example";
let dir = "";
const OWNER = "owner@example.com";

beforeEach(() => {
  process.env.ENGINEBOX_DASHBOARD_PASSWORD = "test-only-dashboard-password";
  process.env.ENGINEBOX_SESSION_SECRET = "test-only-session-signing-secret-123456789";
  process.env.ENGINEBOX_API_TOKEN = "test-only-backend-token-never-in-browser";
  process.env.ENGINEBOX_DASHBOARD_ORIGIN = origin;
  process.env.ENGINEBOX_API_URL = "http://127.0.0.1:8765";
  dir = mkdtempSync(join(tmpdir(), "enginebox-identity-"));
  process.env.ENGINEBOX_IDENTITY_STATE_DIR = dir;
  process.env.ENGINEBOX_EMAIL_TRANSPORT = "file";
  process.env.ENGINEBOX_OWNER_EMAIL = OWNER;
  process.env.ENGINEBOX_CLIENT_IP_HEADER = "cf-connecting-ip";
  delete process.env.ENGINEBOX_TENANCY_MODE;
  delete process.env.ENGINEBOX_OPEN_SIGNUP;
  forgetTenancy();
});
afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

type Outbox = { template: string; to: string; code?: string; link?: string };
function outbox(): Outbox[] {
  const file = join(dir, "outbox", "outbox.jsonl");
  if (!existsSync(file)) return [];
  return readFileSync(file, "utf8").trim().split("\n").map((l) => JSON.parse(l));
}
let ipCounter = 0;
function post(path: string, body: unknown, opts: { cookie?: string; ip?: string } = {}) {
  return new Request(`${origin}${path}`, {
    method: "POST",
    headers: {
      origin,
      "content-type": "application/json",
      "cf-connecting-ip": opts.ip ?? `198.51.100.${++ipCounter % 250}`,
      ...(opts.cookie ? { cookie: opts.cookie } : {}),
    },
    body: JSON.stringify(body),
  });
}
function form(path: string, fields: Record<string, string>, ip = "203.0.113.9") {
  return new Request(`${origin}${path}`, {
    method: "POST",
    headers: { origin, "content-type": "application/x-www-form-urlencoded", "cf-connecting-ip": ip },
    body: new URLSearchParams(fields).toString(),
  });
}
const get = (path: string, cookie?: string) => new Request(`${origin}${path}`, { headers: cookie ? { cookie } : {} });
const cookieOf = (response: Response) => (response.headers.get("set-cookie") ?? "").split(";")[0];

/** Full email sign-in; returns the session cookie. */
async function signIn(email: string, ip?: string) {
  const started = await (await startEmailSignIn(post("/api/auth/email", { email }, { ip }))).json();
  const message = outbox().filter((m) => m.to === email.toLowerCase()).at(-1)!;
  const verified = await verifyEmailCode(post("/api/auth/email/verify", { challenge: started.challenge, code: message.code }, { ip }));
  assert.equal(verified.status, 200);
  return cookieOf(verified);
}
const who = (cookie: string) => principal(get("/", cookie));

test("bootstrap: the platform org exists, the env owner is instance owner with a personal org; state is private", () => {
  const state = withIdentity((s) => structuredClone(s));
  const platform = state.orgs.find((o) => o.id === DEFAULT_ORG)!;
  assert.equal(platform.kind, "platform");
  assert.equal(platform.name, "Default Org");
  const owner = state.users.find((u) => u.email === OWNER)!;
  assert.ok(owner.instance_owner);
  assert.equal(state.orgs.find((o) => o.id === owner.personal_org_id)?.kind, "personal");
  assert.deepEqual(
    state.members.filter((m) => m.user_id === owner.id).map((m) => [m.org_id === DEFAULT_ORG ? "platform" : "personal", m.role]).sort(),
    [["personal", "owner"], ["platform", "owner"]],
  );
  assert.equal(statSync(join(dir, "identity.json")).mode & 0o777, 0o600);
  assert.equal(statSync(dir).mode & 0o777, 0o700);
  // Idempotent: a second bootstrap adds nothing.
  withIdentity(() => {});
  assert.equal(readIdentity().users.length, 1);
});

test("sign-in by code: a member session acting in the platform org; nothing secret at rest", async () => {
  const cookie = await signIn(OWNER);
  const p = who(cookie)!;
  assert.equal(p.kind, "member");
  assert.equal(p.org_id, DEFAULT_ORG);
  assert.equal(p.role, "owner");
  assert.ok(p.instance_owner);
  const raw = readFileSync(join(dir, "identity.json"), "utf8");
  const sid = JSON.parse(Buffer.from(cookie.split("=")[1].split(".")[0], "base64url").toString()).sid;
  assert.ok(!raw.includes(sid));
  for (const m of outbox()) assert.ok(!raw.includes(m.code!) && !raw.includes(new URL(m.link!).searchParams.get("t")!));
});

test("no account enumeration: known and unknown addresses get the same response; only the known one gets mail", async () => {
  const known = await startEmailSignIn(post("/api/auth/email", { email: OWNER }));
  const unknown = await startEmailSignIn(post("/api/auth/email", { email: "stranger@example.com" }));
  assert.equal(known.status, unknown.status);
  const [a, b] = [await known.json(), await unknown.json()];
  assert.deepEqual(Object.keys(a).sort(), Object.keys(b).sort());
  assert.equal(a.sent, b.sent);
  assert.equal(a.challenge.length, b.challenge.length);
  assert.deepEqual(outbox().map((m) => m.to), [OWNER]);
  // The unknown address's handle never verifies, for any code.
  await assert.rejects(verifyEmailCode(post("/api/auth/email/verify", { challenge: b.challenge, code: "000000" })), (e: unknown) => e instanceof HttpError && e.status === 401);
  assert.equal(readIdentity().users.length, 1);
});

test("codes are single use, expire, and lock after five wrong attempts", async () => {
  const start = async () => (await (await startEmailSignIn(post("/api/auth/email", { email: OWNER }))).json()).challenge as string;
  const lastCode = () => outbox().at(-1)!.code!;
  const verify = (challenge: string, code: string) => verifyEmailCode(post("/api/auth/email/verify", { challenge, code }));
  const rejected = (e: unknown) => e instanceof HttpError && e.status === 401;

  // Reuse
  let challenge = await start();
  let code = lastCode();
  assert.equal((await verify(challenge, code)).status, 200);
  await assert.rejects(verify(challenge, code), rejected);

  // Lockout: the right code fails after MAX wrong ones.
  challenge = await start();
  code = lastCode();
  const wrong = code === "000000" ? "111111" : "000000";
  for (let i = 0; i < MAX_CODE_ATTEMPTS; i++) await assert.rejects(verify(challenge, wrong), rejected);
  await assert.rejects(verify(challenge, code), rejected);

  // Expiry
  challenge = await start();
  code = lastCode();
  withIdentity((s, now) => {
    for (const c of s.challenges) c.expires_at = now - 1;
  });
  await assert.rejects(verify(challenge, code), rejected);

  // A newer code ends the older one.
  const first = await start();
  const firstCode = lastCode();
  await start();
  await assert.rejects(verify(first, firstCode), rejected);
  assert.equal(CHALLENGE_MINUTES, 10);
});

test("rate limits: per email and per client address, the same for unknown addresses", async () => {
  const limited = (e: unknown) => e instanceof HttpError && e.status === 429;
  for (const email of [OWNER, "nobody@example.com"]) {
    for (let i = 0; i < LIMITS.startPerEmail.limit; i++) await startEmailSignIn(post("/api/auth/email", { email }));
    await assert.rejects(startEmailSignIn(post("/api/auth/email", { email })), limited);
  }
  for (let i = 0; i < LIMITS.startPerClient.limit; i++) await startEmailSignIn(post("/api/auth/email", { email: `p${i}@example.com` }, { ip: "192.0.2.77" }));
  await assert.rejects(startEmailSignIn(post("/api/auth/email", { email: "q@example.com" }, { ip: "192.0.2.77" })), limited);
  // Another address from another client is unaffected.
  assert.equal((await startEmailSignIn(post("/api/auth/email", { email: "r@example.com" }, { ip: "192.0.2.78" }))).status, 200);
  // Rate-limit keys are digests, never the address.
  assert.ok(!readFileSync(join(dir, "identity.json"), "utf8").includes("nobody@example.com"));
});

test("magic link: GET only confirms, POST signs in once", async () => {
  await startEmailSignIn(post("/api/auth/email", { email: OWNER }));
  const link = new URL(outbox().at(-1)!.link!);
  assert.equal(link.origin + link.pathname, `${origin}/api/auth/link`);
  const page = magicLinkPage(get(link.pathname + link.search));
  assert.equal(page.status, 200);
  assert.match(await page.text(), /method="post" action="\/api\/auth\/link"/);
  const fields = { c: link.searchParams.get("c")!, t: link.searchParams.get("t")! };
  const ok = await magicLinkConsume(form("/api/auth/link", fields));
  assert.equal(ok.status, 303);
  assert.equal(who(cookieOf(ok))?.role, "owner");
  const again = await magicLinkConsume(form("/api/auth/link", fields));
  assert.equal(again.status, 401);
  // A cross-site form post is refused.
  const cross = form("/api/auth/link", fields);
  cross.headers.set("origin", "https://attacker.example");
  assert.equal((await magicLinkConsume(cross)).status, 403);
});

test("invites: single use, expire, revocable; a new person gets an account with a personal org", async () => {
  const owner = await signIn(OWNER);
  const created = await orgAction(post("/api/org/invites", { email: "Dev@Example.com", role: "member" }, { cookie: owner }), ["invites"]);
  assert.equal(created.status, 201);
  const body = await created.json();
  assert.equal(body.email_sent, true);
  assert.ok(!JSON.stringify(body).includes("token"));
  const token = new URL(outbox().at(-1)!.link!).searchParams.get("token")!;
  assert.ok(!readFileSync(join(dir, "identity.json"), "utf8").includes(token));
  assert.equal(invitePage(get(`/api/auth/invite?token=${token}`)).status, 200);
  const accepted = await acceptInvite(form("/api/auth/invite", { token }));
  assert.equal(accepted.status, 303);
  const dev = who(cookieOf(accepted))!;
  assert.deepEqual([dev.org_id, dev.role, dev.email, dev.instance_owner], [DEFAULT_ORG, "member", "dev@example.com", false]);
  const user = readIdentity().users.find((u) => u.email === "dev@example.com")!;
  assert.equal(readIdentity().orgs.find((o) => o.id === user.personal_org_id)?.kind, "personal");
  // Reuse
  assert.equal((await acceptInvite(form("/api/auth/invite", { token }))).status, 400);
  assert.equal(invitePage(get(`/api/auth/invite?token=${token}`)).status, 400);

  // Expired
  await orgAction(post("/api/org/invites", { email: "late@example.com" }, { cookie: owner }), ["invites"]);
  const lateToken = new URL(outbox().at(-1)!.link!).searchParams.get("token")!;
  withIdentity((s, now) => {
    for (const i of s.invites) if (i.email === "late@example.com") i.expires_at = now - 1;
  });
  assert.equal((await acceptInvite(form("/api/auth/invite", { token: lateToken }))).status, 400);

  // Revoked
  const r = await (await orgAction(post("/api/org/invites", { email: "gone@example.com" }, { cookie: owner }), ["invites"])).json();
  const goneToken = new URL(outbox().at(-1)!.link!).searchParams.get("token")!;
  await orgAction(post(`/api/org/invites/${r.invite.id}/revoke`, {}, { cookie: owner }), ["invites", r.invite.id, "revoke"]);
  assert.equal((await acceptInvite(form("/api/auth/invite", { token: goneToken }))).status, 400);

  // A member cannot invite; a personal workspace takes no invites.
  const forbidden = (e: unknown) => e instanceof HttpError && e.status === 403;
  await assert.rejects(orgAction(post("/api/org/invites", { email: "x@example.com" }, { cookie: cookieOf(accepted) }), ["invites"]), forbidden);
  const personal = readIdentity().users.find((u) => u.email === OWNER)!.personal_org_id;
  await orgAction(post("/api/org/switch", { org_id: personal }, { cookie: owner }), ["switch"]);
  assert.equal(who(owner)?.org_id, personal);
  await assert.rejects(orgAction(post("/api/org/invites", { email: "x@example.com" }, { cookie: owner }), ["invites"]), forbidden);
});

test("removing a member ends their session in that org; sign-out revokes the session record", async () => {
  const owner = await signIn(OWNER);
  await orgAction(post("/api/org/members/import", { members: [{ email: "dev@example.com", role: "admin" }] }, { cookie: owner }), ["members", "import"]);
  const dev = await signIn("dev@example.com");
  assert.equal(who(dev)?.role, "admin");
  // An admin cannot demote or remove the owner; the last owner cannot be removed.
  const view = await (await orgView(get("/api/org", dev))).json();
  const ownerMember = view.members.find((m: { email: string }) => m.email === OWNER);
  await assert.rejects(orgAction(post(`/api/org/members/${ownerMember.id}/remove`, {}, { cookie: dev }), ["members", ownerMember.id, "remove"]), (e: unknown) => e instanceof HttpError && e.status === 403);
  await assert.rejects(orgAction(post(`/api/org/members/${ownerMember.id}/remove`, {}, { cookie: owner }), ["members", ownerMember.id, "remove"]), (e: unknown) => e instanceof HttpError && e.status === 409);
  const devMember = view.members.find((m: { email: string }) => m.email === "dev@example.com");
  await orgAction(post(`/api/org/members/${devMember.id}/remove`, {}, { cookie: owner }), ["members", devMember.id, "remove"]);
  assert.equal(who(dev), null);
  // Sign out
  assert.ok(who(owner));
  signOut(new Request(`${origin}/api/session`, { method: "DELETE", headers: { origin, cookie: owner } }));
  assert.equal(who(owner), null);
});

test("import carries members over by email without sending mail; an existing account just gains the membership", async () => {
  const owner = await signIn(OWNER);
  const before = outbox().length;
  const result = await (await orgAction(post("/api/org/members/import", { members: [{ email: OWNER, role: "owner" }, { email: "a@example.com", name: "A" }, { email: "b@example.com", role: "admin" }] }, { cookie: owner }), ["members", "import"])).json();
  assert.deepEqual(result, { created_accounts: 2, added: 2, unchanged: 1 });
  assert.equal(outbox().length, before);
  assert.equal(readIdentity().users.length, 3);
  // Each imported account has its own personal org.
  for (const u of readIdentity().users) assert.ok(readIdentity().orgs.some((o) => o.id === u.personal_org_id && o.kind === "personal"));
});

test("break-glass operator: owner of the platform org; claims the instance owner when none is configured", async () => {
  delete process.env.ENGINEBOX_OWNER_EMAIL;
  const operator = sessionCookie().split(";")[0];
  const p = who(operator)!;
  assert.deepEqual([p.kind, p.org_id, p.role, p.instance_owner], ["operator", DEFAULT_ORG, "owner", true]);
  assert.equal((await (await orgView(get("/api/org", operator))).json()).needs_instance_owner, true);
  const started = await (await claimOwnerStart(post("/api/org/claim", { email: "ops@example.com" }, { cookie: operator }))).json();
  const code = outbox().at(-1)!.code!;
  // Finishing needs the operator session.
  await assert.rejects(verifyEmailCode(post("/api/auth/email/verify", { challenge: started.challenge, code })), (e: unknown) => e instanceof HttpError && e.status === 401);
  const again = await (await claimOwnerStart(post("/api/org/claim", { email: "ops@example.com" }, { cookie: operator }))).json();
  const verified = await verifyEmailCode(post("/api/auth/email/verify", { challenge: again.challenge, code: outbox().at(-1)!.code }, { cookie: operator }));
  const owner = who(cookieOf(verified))!;
  assert.deepEqual([owner.kind, owner.org_id, owner.role, owner.instance_owner], ["member", DEFAULT_ORG, "owner", true]);
  await assert.rejects(claimOwnerStart(post("/api/org/claim", { email: "other@example.com" }, { cookie: operator })), (e: unknown) => e instanceof HttpError && e.status === 409);
});

test("managed mode: open sign-up creates a personal org; tenants cannot add people to the platform org", async () => {
  process.env.ENGINEBOX_TENANCY_MODE = "managed";
  const tenant = await signIn("tenant@example.com");
  const p = who(tenant)!;
  assert.equal(p.org_kind, "personal");
  assert.equal(p.role, "owner");
  assert.notEqual(p.org_id, DEFAULT_ORG);
  // The tenant does not see or belong to the platform org.
  const view = await (await orgView(get("/api/org", tenant))).json();
  assert.deepEqual(view.orgs.map((o: { kind: string }) => o.kind), ["personal"]);
  await assert.rejects(orgAction(post("/api/org/switch", { org_id: DEFAULT_ORG }, { cookie: tenant }), ["switch"]), (e: unknown) => e instanceof HttpError && e.status === 403);
  // A team org can invite.
  const team = await (await orgAction(post("/api/org/create", { name: "Studio" }, { cookie: tenant }), ["create"])).json();
  await orgAction(post("/api/org/switch", { org_id: team.org.id }, { cookie: tenant }), ["switch"]);
  assert.equal((await orgAction(post("/api/org/invites", { email: "c@example.com" }, { cookie: tenant }), ["invites"])).status, 201);
  // Self-hosted without open sign-up, an unknown address creates nothing.
  delete process.env.ENGINEBOX_TENANCY_MODE;
  await startEmailSignIn(post("/api/auth/email", { email: "drifter@example.com" }));
  assert.ok(!readIdentity().users.some((u) => u.email === "drifter@example.com"));
});

test("route guards: members run agents and use workspaces; admin-only and cross-org operations are refused", async (t) => {
  const owner = await signIn(OWNER);
  await orgAction(post("/api/org/members/import", { members: [{ email: "m@example.com" }] }, { cookie: owner }), ["members", "import"]);
  const member = await signIn("m@example.com");
  const calls: { url: string; org?: string }[] = [];
  t.mock.method(globalThis, "fetch", async (url: string, init: RequestInit = {}) => {
    const headers = (init.headers ?? {}) as Record<string, string>;
    calls.push({ url, org: headers["X-EngineBox-Org"] });
    if (url.endsWith("/v1/tenancy")) return Response.json({ mode: "self_hosted", host_orgs: { tenant: "org_0123456789abcdef01234567" }, grants: [] });
    return Response.json({ ok: true }, { status: 202 });
  });
  const forbidden = (e: unknown) => e instanceof HttpError && e.status === 403;
  // Member: pool runs and workspace allocation are allowed.
  assert.equal((await proxy(post("/api/control/v1/pools/workers/agent-runs", { prompt: "x" }, { cookie: member }), ["v1", "pools", "workers", "agent-runs"])).status, 202);
  assert.equal((await proxy(post("/api/control/v1/hosts/worker/allocations", { name: "a" }, { cookie: member }), ["v1", "hosts", "worker", "allocations"])).status, 202);
  // Member: templates, GitHub and the org's controller agent accounts need admin.
  await assert.rejects(proxy(post("/api/control/v1/templates", { url: "https://x" }, { cookie: member }), ["v1", "templates"]), forbidden);
  await assert.rejects(proxy(post("/api/control/v1/agents/codex/login", {}, { cookie: member }), ["v1", "agents", "codex", "login"]), forbidden);
  await assert.rejects(proxy(post("/api/control/v1/github/select", {}, { cookie: member }), ["v1", "github", "select"]), forbidden);
  // A host owned by another org is refused even for the owner.
  await assert.rejects(proxy(post("/api/control/v1/hosts/tenant/allocations", { name: "a" }, { cookie: owner }), ["v1", "hosts", "tenant", "allocations"]), forbidden);
  // Platform-org requests carry no org header (unchanged controller requests).
  assert.ok(calls.filter((c) => !c.url.endsWith("/v1/tenancy")).every((c) => c.org === undefined));
  // A personal org acts with its own org header and cannot configure the install.
  const personal = readIdentity().users.find((u) => u.email === OWNER)!.personal_org_id;
  await orgAction(post("/api/org/switch", { org_id: personal }, { cookie: owner }), ["switch"]);
  await assert.rejects(proxy(post("/api/control/v1/templates", { url: "https://x" }, { cookie: owner }), ["v1", "templates"]), forbidden);
  await proxy(post("/api/control/v1/pools/mine/agent-runs", { prompt: "x" }, { cookie: owner }), ["v1", "pools", "mine", "agent-runs"]);
  assert.equal(calls.at(-1)?.org, personal);
});

test("listings: an org sees only its own pools, repositories and hosts, plus platform pools granted to it", () => {
  const tenant = "org_0123456789abcdef01234567";
  const view = { mode: "self_hosted", host_orgs: { "byo-node": tenant }, grants: [{ id: "g", tenant_org: tenant, pool: "shared" }, { id: "r", tenant_org: tenant, pool: "revoked", revoked_at: "2026-01-01T00:00:00Z" }] };
  const listing = {
    pools: [{ name: "shared" }, { name: "private" }, { name: "revoked" }, { name: "theirs", org_id: tenant }],
    repositories: [{ name: "game" }, { name: "tool", org_id: tenant }],
    hosts: [{ id: "worker" }, { id: "byo-node" }],
  };
  const forTenant = scopeToOrg(listing, tenant, view) as typeof listing;
  assert.deepEqual(forTenant.pools.map((p) => p.name), ["shared", "theirs"]);
  assert.deepEqual(forTenant.repositories.map((r) => r.name), ["tool"]);
  assert.deepEqual(forTenant.hosts.map((h) => h.id), ["byo-node"]);
  // The platform org never sees a tenant's records.
  const forPlatform = scopeToOrg(listing, DEFAULT_ORG, view) as typeof listing;
  assert.deepEqual(forPlatform.pools.map((p) => p.name), ["shared", "private", "revoked"]);
  assert.deepEqual(forPlatform.repositories.map((r) => r.name), ["game"]);
  assert.deepEqual(forPlatform.hosts.map((h) => h.id), ["worker"]);
});

test("without ENGINEBOX_IDENTITY_STATE_DIR the dashboard keeps operator-only behaviour", async () => {
  delete process.env.ENGINEBOX_IDENTITY_STATE_DIR;
  await assert.rejects(startEmailSignIn(post("/api/auth/email", { email: OWNER })), (e: unknown) => e instanceof HttpError && e.status === 404);
  const operator = sessionCookie().split(";")[0];
  assert.equal(who(operator)?.kind, "operator");
  assert.equal((await (await orgView(get("/api/org", operator))).json()).enabled, false);
});

test("OAuth consent: org admins approve and own grants in their org; members cannot approve", async () => {
  const { ownerAuthenticator } = await import("../lib/oauth/identity");
  const owner = await signIn(OWNER);
  await orgAction(post("/api/org/members/import", { members: [{ email: "m@example.com" }] }, { cookie: owner }), ["members", "import"]);
  const member = await signIn("m@example.com");
  const asOwner = ownerAuthenticator.current(get("/", owner))!;
  const asMember = ownerAuthenticator.current(get("/", member))!;
  assert.deepEqual([asOwner.org_id, asOwner.may_approve], [DEFAULT_ORG, true]);
  assert.match(asOwner.id, /^usr_/);
  assert.deepEqual([asMember.org_id, asMember.may_approve], [DEFAULT_ORG, false]);
  // The break-glass operator keeps the pre-org grant owner id, so existing grants stay manageable.
  assert.equal(ownerAuthenticator.current(get("/", sessionCookie().split(";")[0]))?.id, "owner");
  assert.match(ownerAuthenticator.signInFields[0].label, /break-glass/);
});

test("a single record of another org reads as not found", () => {
  const tenant = "org_0123456789abcdef01234567";
  const view = { mode: "self_hosted", host_orgs: {}, grants: [] };
  assert.throws(() => scopeToOrg({ repository: { name: "game" } }, tenant, view), (e: unknown) => e instanceof HttpError && e.status === 404);
  assert.throws(() => scopeToOrg({ repository: { name: "tool", org_id: tenant } }, DEFAULT_ORG, view), (e: unknown) => e instanceof HttpError && e.status === 404);
  assert.deepEqual(scopeToOrg({ repository: { name: "tool", org_id: tenant } }, tenant, view), { repository: { name: "tool", org_id: tenant } });
});
