import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";
import { authorize, listGrants } from "../lib/oauth/server";
import { addMember, createUser, DEFAULT_ORG, readIdentity, userByEmail, withIdentity } from "../lib/identity/store";
import {
  authorizeUrl,
  call,
  consentPost,
  cookie,
  exchange,
  fakeController,
  newClient,
  origin,
  pkce,
  refreshWith,
  registerClient,
  requestId,
  standardParams,
} from "./oauth-fixture";
import { memberCookie } from "./session-fixture";

// Engine sessions over OAuth (docs/ENGINE-SESSIONS.md, "Who can call it").

const ENGINE_OPERATIONS = ["engine_sessions.start", "engine_sessions.get", "engine_sessions.renew", "engine_sessions.end", "engine_sessions.watch"];
const userId = (email: string) => userByEmail(readIdentity(), email)!.id;
/** A plain member (not an admin) of the platform org, signed in. */
function plainMember(email = "dev@fixture.example") {
  cookie(); // the instance owner exists first
  withIdentity((state, now) => {
    const user = userByEmail(state, email) ?? createUser(state, email, "env", now);
    addMember(state, DEFAULT_ORG, user.id, "member", "invite", now);
  });
  return memberCookie(email);
}
let shared: Awaited<ReturnType<typeof newClient>> | null = null;
// Each test has a fresh OAuth state directory, so a fresh client.
beforeEach(() => {
  shared = null;
});
async function consent(scope: string | undefined, as = cookie(), extra: Record<string, string> = {}) {
  // Registration is rate limited per process; one client serves every consent here.
  const client = (shared ??= await newClient());
  const { verifier, challenge } = pkce();
  const page = await authorize(new Request(authorizeUrl(standardParams(client, challenge, { scope, ...extra })), { headers: { cookie: as } }));
  assert.equal(page.status, 200);
  const html = await page.text();
  return { client, verifier, html, id: requestId(html) };
}

test("engine sessions are offered only for the engine-sessions scope; the token acts as the approving member", async (t) => {
  const controller = fakeController(t);
  // Existing clients (scope agent-runs or none) see no engine sessions.
  for (const scope of ["agent-runs", undefined]) assert.doesNotMatch((await consent(scope)).html, /name="engine_sessions"/);
  const { client, verifier, html, id } = await consent("agent-runs engine-sessions");
  for (const project of ["alpha", "beta", "default", "gamma"])
    assert.match(html, new RegExp(`name="engine_sessions" value="${project}"> Run live engine sessions in project <code>${project}</code>`));
  assert.match(html, /name="pool" value="workers"/, "agent runs are still offered alongside");
  const unknown = await consentPost([["request", id], ["action", "approve"], ["engine_sessions", "delta"]]);
  assert.match(await unknown.text(), /engine sessions are not available/);
  const ok = await consentPost([["request", id], ["action", "approve"], ["engine_sessions", "alpha"]]);
  assert.equal(ok.status, 303);
  const response = await exchange(client, new URL(ok.headers.get("location")!).searchParams.get("code")!, verifier);
  const tokens = await response.json();
  const owner = userId("owner@fixture.example");
  assert.equal(tokens.scope, "engine-sessions", "the granted scope, not merely the requested one");
  assert.deepEqual(tokens.enginebox_grants, [{ kind: "engine_sessions", project: "alpha", member: owner }]);
  const [minted] = [...controller.tokens.values()];
  assert.deepEqual(minted.operations, ENGINE_OPERATIONS, "an engine-only grant holds no agent-run operation");
  assert.deepEqual(minted.engine_sessions, [{ project: "alpha" }]);
  assert.equal(minted.member, owner);
  assert.deepEqual(minted.scopes, []);
  // The five routes reach the controller with the token alone: no actor or org header from the caller.
  const base = "v1/projects/alpha/engine-sessions";
  const started = await call(base, tokens.access_token, "POST", { request_id: "r1" });
  assert.equal(started.status, 202);
  const session = await started.json();
  assert.equal(session.started_by, owner);
  assert.equal((await (await call(base, tokens.access_token)).json()).sessions.length, 1);
  const read = await (await call(`${base}/${session.id}`, tokens.access_token)).json();
  assert.equal(read.stream.audience.token, "subscribe-jwt");
  assert.equal((await call(`${base}/${session.id}/renew`, tokens.access_token, "POST", { lease: session.lease, ttl_seconds: 600 })).status, 200);
  const watch = await call(`${base}/${session.id}/watch-token`, tokens.access_token, "POST", {});
  assert.equal(watch.status, 200);
  assert.equal((await watch.json()).token, "watch-jwt");
  // A world session's TURN ICE servers (the watch operation).
  const ice = await call(`${base}/${session.id}/ice`, tokens.access_token, "POST", {});
  assert.equal(ice.status, 200);
  assert.equal((await ice.json()).ice_transport_policy, "relay");
  assert.equal((await call(`${base}/${session.id}/end`, tokens.access_token, "POST", {})).status, 200);
  for (const c of controller.calls.filter((c) => c.path.includes("engine-sessions"))) {
    assert.equal(c.auth, `Bearer ${tokens.access_token}`);
    assert.equal(c.actor, null);
    assert.equal(c.org, null);
  }
  // Another project, and agent runs, are refused by the controller with the right scope hint.
  const elsewhere = await call("v1/projects/beta/engine-sessions", tokens.access_token);
  assert.equal(elsewhere.status, 403);
  assert.match(elsewhere.headers.get("www-authenticate")!, /scope="engine-sessions"/);
  assert.equal((await call("v1/agent-runs", tokens.access_token)).status, 403);
  // Owner-wide routes and other shapes never leave the edge.
  for (const [method, path] of [
    ["GET", "v1/engine-sessions"],
    ["POST", `v1/engine-sessions/${session.id}/end`],
    ["GET", `${base}/not-an-id`],
    ["POST", `${base}/${session.id}`],
    ["GET", `${base}/${session.id}/renew`],
    ["GET", `${base}/${session.id}/watch-token`],
    ["GET", `${base}/${session.id}/ice`],
    ["POST", `${base}/${session.id}/ice/x`],
    ["POST", `${base}/${session.id}/stop`],
    ["GET", `${base}/${session.id}/peers`],
    ["POST", `${base}/${session.id}/peers/not-a-peer/offer`],
    ["POST", `${base}/${session.id}/peers/${"e".repeat(32)}/answer`],
    ["GET", "v1/projects/Alpha/engine-sessions"],
  ] as const)
    assert.equal((await call(path, tokens.access_token, method, method === "POST" ? {} : undefined)).status, 404, `${method} ${path}`);
  // Refresh keeps it; the connection lists it.
  const refreshed = await (await refreshWith(client, tokens.refresh_token)).json();
  assert.deepEqual(refreshed.enginebox_grants, tokens.enginebox_grants);
  assert.equal(refreshed.scope, "engine-sessions");
  assert.equal(controller.tokens.get(refreshed.access_token.slice(5, 37))!.member, owner);
  const listed = await listGrants(new Request(`${origin}/api/oauth/grants`, { headers: { cookie: cookie() } })).json();
  assert.deepEqual(listed.grants[0].engine_sessions, [{ project: "alpha" }]);
  assert.equal(listed.grants[0].approver_id, owner);
});

test("one token never sees another member's engine sessions", async (t) => {
  const controller = fakeController(t);
  const tokenFor = async (as: string) => {
    const { client, verifier, id } = await consent("engine-sessions", as);
    const ok = await consentPost([["request", id], ["action", "approve"], ["engine_sessions", "alpha"]], { origin, cookie: as });
    assert.equal(ok.status, 303);
    return (await (await exchange(client, new URL(ok.headers.get("location")!).searchParams.get("code")!, verifier)).json()).access_token as string;
  };
  const ownerToken = await tokenFor(cookie());
  const devToken = await tokenFor(plainMember());
  const base = "v1/projects/alpha/engine-sessions";
  const session = await (await call(base, ownerToken, "POST", { request_id: "r1" })).json();
  assert.deepEqual((await (await call(base, devToken)).json()).sessions, []);
  for (const [method, path] of [
    ["GET", `${base}/${session.id}`],
    ["POST", `${base}/${session.id}/renew`],
    ["POST", `${base}/${session.id}/end`],
    ["POST", `${base}/${session.id}/watch-token`],
  ] as const) {
    const refused = await call(path, devToken, method, method === "POST" ? { lease: session.lease, ttl_seconds: 600 } : undefined);
    assert.equal(refused.status, 404, `${method} ${path}`);
    assert.doesNotMatch(await refused.text(), /subscribe-jwt/);
  }
  assert.equal(controller.engineSessions.get(session.id)!.state, "ready");
});

test("a plain member may connect only their own engine sessions; leaving the org ends the connection", async (t) => {
  const controller = fakeController(t);
  const dev = plainMember();
  const { client, verifier, html, id } = await consent("agent-runs engine-sessions", dev);
  assert.match(html, /name="engine_sessions" value="alpha"/);
  assert.doesNotMatch(html, /name="pool"|name="workspace"|name="repositories:/, "agent runs are not offered to a member");
  assert.match(html, /only an admin or owner of your organization can allow that/);
  const pool = await consentPost([["request", id], ["action", "approve"], ["pool", "workers"], ["engine_sessions", "alpha"]], { origin, cookie: dev });
  assert.equal(pool.status, 403);
  const ok = await consentPost([["request", id], ["action", "approve"], ["engine_sessions", "alpha"]], { origin, cookie: dev });
  assert.equal(ok.status, 303);
  const tokens = await (await exchange(client, new URL(ok.headers.get("location")!).searchParams.get("code")!, verifier)).json();
  const member = userId("dev@fixture.example");
  assert.deepEqual(tokens.enginebox_grants, [{ kind: "engine_sessions", project: "alpha", member }]);
  // The member manages their own connection.
  const listed = await listGrants(new Request(`${origin}/api/oauth/grants`, { headers: { cookie: dev } })).json();
  assert.equal(listed.grants.length, 1);
  // Removed from the org: the next refresh ends the connection and revokes the live token.
  withIdentity((state, now) => {
    state.members.find((m) => m.org_id === DEFAULT_ORG && m.user_id === member)!.removed_at = now;
  });
  const refused = await refreshWith(client, tokens.refresh_token);
  assert.equal(refused.status, 400);
  assert.equal((await refused.json()).error, "invalid_grant");
  assert.equal((await call("v1/projects/alpha/engine-sessions", tokens.access_token)).status, 401);
  assert.ok([...controller.tokens.values()].every((x) => x.revoked));
});

test("engine-session scope rules: only known scopes, nothing unrequested, and a requested project restricts the token", async (t) => {
  const controller = fakeController(t);
  const client = (shared ??= await newClient());
  const { challenge } = pkce();
  const bad = await authorize(new Request(authorizeUrl(standardParams(client, challenge, { scope: "engine-sessions admin" }))));
  assert.equal(new URL(bad.headers.get("location")!).searchParams.get("error"), "invalid_scope");
  const registered = await registerClient({ client_name: "Gamejam", redirect_uris: ["https://gamejam.example/cb"], scope: "engine-sessions agent-runs" });
  assert.equal(registered.status, 201);
  assert.equal((await registered.json()).scope, "agent-runs engine-sessions");
  assert.equal((await registerClient({ redirect_uris: ["https://gamejam.example/cb"], scope: "engine-sessions owner" })).status, 400);
  // Engine sessions only: no agent-run sections, and none can be posted.
  const only = await consent("engine-sessions");
  assert.doesNotMatch(only.html, /name="pool"|name="workspace"|name="repositories:/);
  const sneaked = await consentPost([["request", only.id], ["action", "approve"], ["workspace", "worker/ebox-a"]]);
  assert.match(await sneaked.text(), /did not ask to run agents/);
  // project=alpha: only alpha is offered, and the token carries the restriction.
  const scoped = await consent("engine-sessions", cookie(), { project: "alpha" });
  assert.match(scoped.html, /name="engine_sessions" value="alpha"/);
  assert.doesNotMatch(scoped.html, /name="engine_sessions" value="beta"/);
  const outside = await consentPost([["request", scoped.id], ["action", "approve"], ["engine_sessions", "beta"]]);
  assert.match(await outside.text(), /engine sessions are not available/);
  const ok = await consentPost([["request", scoped.id], ["action", "approve"], ["engine_sessions", "alpha"]]);
  await exchange(scoped.client, new URL(ok.headers.get("location")!).searchParams.get("code")!, scoped.verifier);
  const minted = [...controller.tokens.values()].at(-1)!;
  assert.equal(minted.project, "alpha");
  // A mixed grant keeps every agent-run operation and adds the engine ones.
  const mixed = await consent("agent-runs engine-sessions");
  const both = await consentPost([["request", mixed.id], ["action", "approve"], ["pool", "workers"], ["engine_sessions", "alpha"]]);
  const tokens = await (await exchange(mixed.client, new URL(both.headers.get("location")!).searchParams.get("code")!, mixed.verifier)).json();
  assert.equal(tokens.scope, "agent-runs engine-sessions");
  assert.deepEqual([...controller.tokens.values()].at(-1)!.operations, [
    "agent_runs.start",
    "agent_runs.get",
    "agent_runs.list",
    "agent_runs.cancel",
    ...ENGINE_OPERATIONS,
  ]);
});
