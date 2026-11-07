import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { authorize, listGrants } from "../lib/oauth/server";
import { addMember, createUser, DEFAULT_ORG, readIdentity, userByEmail, withIdentity } from "../lib/identity/store";
import { authorizeUrl, call, consentPost, cookie, exchange, fakeController, newClient, origin, pkce, refreshWith, requestId, standardParams } from "./oauth-fixture";
import { memberCookie } from "./session-fixture";

// Member subscriptions over OAuth (docs/MEMBER-SUBSCRIPTIONS.md): the `agent-credentials` scope.

const CREDENTIAL_OPERATIONS = ["agent_credentials.manage", "agent_credentials.run"];
const userId = (email: string) => userByEmail(readIdentity(), email)!.id;
function plainMember(email = "dev@fixture.example") {
  cookie();
  withIdentity((state, now) => {
    const user = userByEmail(state, email) ?? createUser(state, email, "env", now);
    addMember(state, DEFAULT_ORG, user.id, "member", "invite", now);
  });
  return memberCookie(email);
}
let shared: Awaited<ReturnType<typeof newClient>> | null = null;
beforeEach(() => {
  shared = null;
  process.env.ENGINEBOX_MEMBER_CREDENTIALS = "1";
});
afterEach(() => {
  delete process.env.ENGINEBOX_MEMBER_CREDENTIALS;
});
async function consent(scope: string | undefined, as = cookie(), extra: Record<string, string> = {}) {
  const client = (shared ??= await newClient());
  const { verifier, challenge } = pkce();
  const page = await authorize(new Request(authorizeUrl(standardParams(client, challenge, { scope, ...extra })), { headers: { cookie: as } }));
  const html = page.status === 200 ? await page.text() : "";
  return { client, verifier, page, html, id: html ? requestId(html) : "" };
}

test("a plain member grants their own subscriptions; the token acts only as them", async (t) => {
  const controller = fakeController(t);
  const as = plainMember();
  // Other scopes never offer subscriptions.
  for (const scope of ["agent-runs", "engine-sessions", undefined]) assert.doesNotMatch((await consent(scope, as)).html, /name="agent_credentials"/);
  const { client, verifier, html, id } = await consent("agent-credentials", as, { project: "alpha" });
  assert.match(html, /name="agent_credentials" value="alpha"> Connect and use your own AI subscriptions in project <code>alpha<\/code>/);
  assert.doesNotMatch(html, /name="agent_credentials" value="beta"/, "an app that asked for one project is offered only it");
  assert.doesNotMatch(html, /name="pool"/, "a member is not offered the org's pools");
  const other = await consentPost([["request", id], ["action", "approve"], ["agent_credentials", "beta"]], { origin, cookie: as });
  assert.match(await other.text(), /subscriptions are not available/);
  const ok = await consentPost([["request", id], ["action", "approve"], ["agent_credentials", "alpha"]], { origin, cookie: as });
  assert.equal(ok.status, 303);
  const tokens = await (await exchange(client, new URL(ok.headers.get("location")!).searchParams.get("code")!, verifier)).json();
  const member = userId("dev@fixture.example");
  assert.equal(tokens.scope, "agent-credentials");
  assert.deepEqual(tokens.enginebox_grants, [{ kind: "agent_credentials", project: "alpha", member }]);
  const [minted] = [...controller.tokens.values()];
  assert.deepEqual(minted.operations, CREDENTIAL_OPERATIONS, "a subscription-only grant holds no agent-run operation");
  assert.deepEqual(minted.agent_credentials, [{ project: "alpha" }]);
  assert.equal(minted.member, member);
  assert.equal(minted.project, "alpha");
  assert.deepEqual(minted.scopes, []);
  assert.equal(minted.pools, undefined);
  // The member routes reach the controller with the token alone (no actor or org header).
  const base = "v1/projects/alpha/me/credentials";
  assert.equal((await call(base, tokens.access_token)).status, 200);
  assert.equal((await call(base, tokens.access_token, "POST", { provider: "claude" })).status, 201);
  for (const path of [`${base}/${"c".repeat(32)}/login`, `${base}/${"c".repeat(32)}/delete`, `${base}/${"c".repeat(32)}/sessions/${"d".repeat(32)}/cancel`])
    assert.equal((await call(path, tokens.access_token, "POST", {})).status, 202, path);
  assert.equal((await call(`${base}/${"c".repeat(32)}/sessions/${"d".repeat(32)}/input`, tokens.access_token, "POST", { text: "code" })).status, 202);
  assert.equal((await call(`${base}/${"c".repeat(32)}/sessions/${"d".repeat(32)}`, tokens.access_token)).status, 202);
  for (const c of controller.calls.filter((c) => c.path.includes("/me/"))) {
    assert.equal(c.actor, null);
    assert.equal(c.org, null);
  }
  // Shapes outside the allowlist never reach the controller.
  const before = controller.calls.length;
  for (const path of [`${base}/not-an-id/login`, `${base}/${"c".repeat(32)}/share`, "v1/projects/alpha/me/other", "v1/credentials"])
    assert.equal((await call(path, tokens.access_token, "POST", {})).status, 404, path);
  assert.equal(controller.calls.length, before);
  // A 403 names the agent-credentials scope.
  const denied = await call("v1/projects/beta/me/credentials", tokens.access_token);
  assert.equal(denied.status, 403);
  assert.match(denied.headers.get("www-authenticate") ?? "", /scope="agent-credentials"/);
  // Refresh keeps acting as the member.
  const refreshed = await (await refreshWith(client, tokens.refresh_token)).json();
  assert.deepEqual(refreshed.enginebox_grants, [{ kind: "agent_credentials", project: "alpha", member }]);
  const latest = [...controller.tokens.values()].at(-1)!;
  assert.equal(latest.member, member);
  // The member manages the connection; it lists the subscription grant.
  const listed = await (await listGrants(new Request("http://localhost/api/oauth/grants", { headers: { cookie: as } }))).json();
  assert.deepEqual(listed.grants[0].agent_credentials, [{ project: "alpha" }]);
});

test("subscriptions are not offered or minted while the Engine Box has them off", async (t) => {
  fakeController(t);
  delete process.env.ENGINEBOX_MEMBER_CREDENTIALS;
  const { page } = await consent("agent-credentials", plainMember());
  assert.notEqual(page.status, 200);
  const location = page.headers.get("location") ?? "";
  assert.match(location, /error=invalid_scope/);
});

test("turning member subscriptions off ends the connection at refresh", async (t) => {
  fakeController(t);
  const as = plainMember("leaver@fixture.example");
  const { client, verifier, id } = await consent("agent-credentials", as);
  const ok = await consentPost([["request", id], ["action", "approve"], ["agent_credentials", "alpha"]], { origin, cookie: as });
  const tokens = await (await exchange(client, new URL(ok.headers.get("location")!).searchParams.get("code")!, verifier)).json();
  delete process.env.ENGINEBOX_MEMBER_CREDENTIALS;
  const refused = await refreshWith(client, tokens.refresh_token);
  assert.equal(refused.status, 400);
  assert.equal((await refused.json()).error, "invalid_grant");
});
