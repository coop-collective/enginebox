import assert from "node:assert/strict";
import { test } from "node:test";
import {
  authorize,
  listGrants,
} from "../lib/oauth/server";
import {
  advance,
  authorizeUrl,
  call,
  connect,
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
  stateText,
} from "./oauth-fixture";

// ------------------------------------------------------------ refresh grace window

test("a lost refresh response: retrying with the previous refresh token returns the same pair", async (t) => {
  const { controller, client, tokens } = await connect(t);
  const first = await refreshWith(client, tokens.refresh_token);
  assert.equal(first.status, 200);
  const issued = await first.json();
  const minted = controller.tokens.size;
  // The response above never reached the client; it retries with the token it still holds.
  advance(t, 5_000);
  const retry = await refreshWith(client, tokens.refresh_token);
  assert.equal(retry.status, 200);
  const again = await retry.json();
  assert.equal(again.access_token, issued.access_token, "same access token");
  assert.equal(again.refresh_token, issued.refresh_token, "same refresh token");
  assert.deepEqual(again.enginebox_grants, issued.enginebox_grants);
  assert.ok(again.expires_in <= issued.expires_in);
  assert.equal(controller.tokens.size, minted, "an idempotent retry mints nothing");
  assert.equal((await call("v1/agent-runs", issued.access_token)).status, 200, "the connection is intact");
  // Neither the new refresh token nor the new access token is stored in plaintext.
  const raw = stateText();
  for (const secret of [issued.refresh_token, issued.access_token, issued.access_token.split(".")[1]]) assert.ok(!raw.includes(secret));
  assert.match(raw, /"grace_pair":"v1\./);
  // Another client cannot use the grace window.
  const stranger = await newClient();
  assert.equal((await refreshWith(stranger, tokens.refresh_token)).status, 400);
  // The new refresh token keeps working, and rotating it closes the old window.
  const next = await refreshWith(client, issued.refresh_token);
  assert.equal(next.status, 200);
  const third = await next.json();
  assert.equal((await call("v1/agent-runs", third.access_token)).status, 200);
});

test("past the grace window, the previous refresh token revokes the connection and the stored pair is gone", async (t) => {
  const { controller, client, tokens } = await connect(t);
  const issued = await (await refreshWith(client, tokens.refresh_token)).json();
  advance(t, 31_000);
  // Any state transaction after the window drops the encrypted pair.
  await registerClient({ redirect_uris: ["https://other.example/cb"], token_endpoint_auth_method: "none" });
  assert.ok(!stateText().includes('"grace_pair":"v1.'), "the retrievable pair is deleted at window end");
  const late = await refreshWith(client, tokens.refresh_token);
  assert.equal(late.status, 400);
  assert.equal((await late.json()).error, "invalid_grant");
  assert.equal((await refreshWith(client, issued.refresh_token)).status, 400, "the whole connection is revoked");
  assert.equal((await call("v1/agent-runs", issued.access_token)).status, 401);
  assert.ok([...controller.tokens.values()].every((x) => x.revoked));
});

test("a refresh token two generations old revokes the connection even inside the window", async (t) => {
  const { controller, client, tokens } = await connect(t);
  const second = await (await refreshWith(client, tokens.refresh_token)).json();
  const third = await (await refreshWith(client, second.refresh_token)).json();
  const stale = await refreshWith(client, tokens.refresh_token);
  assert.equal(stale.status, 400);
  assert.equal((await stale.json()).error, "invalid_grant");
  assert.equal((await refreshWith(client, third.refresh_token)).status, 400);
  assert.equal((await call("v1/agent-runs", third.access_token)).status, 401);
  assert.ok([...controller.tokens.values()].every((x) => x.revoked));
});

test("two concurrent refreshes with one token both receive the pair the winner issued", async (t) => {
  const { controller, client, tokens } = await connect(t);
  const [a, b] = await Promise.all([refreshWith(client, tokens.refresh_token), refreshWith(client, tokens.refresh_token)]);
  assert.equal(a.status, 200);
  assert.equal(b.status, 200);
  const [x, y] = [await a.json(), await b.json()];
  assert.equal(x.refresh_token, y.refresh_token);
  assert.equal(x.access_token, y.access_token);
  assert.equal((await call("v1/agent-runs", x.access_token)).status, 200);
  // The loser's freshly minted token is revoked on the controller.
  const live = [...controller.tokens.values()].filter((m) => !m.revoked);
  assert.equal(live.length, 1);
  assert.equal(live[0].secret, x.access_token);
});

test("the grace window is configurable and 0 restores immediate reuse detection", async (t) => {
  process.env.ENGINEBOX_OAUTH_REFRESH_GRACE_SECONDS = "0";
  t.after(() => delete process.env.ENGINEBOX_OAUTH_REFRESH_GRACE_SECONDS);
  const { client, tokens } = await connect(t);
  const issued = await (await refreshWith(client, tokens.refresh_token)).json();
  assert.ok(!stateText().includes('"grace_pair":"v1.'), "nothing retrievable is kept when the window is off");
  assert.equal((await refreshWith(client, tokens.refresh_token)).status, 400);
  assert.equal((await refreshWith(client, issued.refresh_token)).status, 400);
  process.env.ENGINEBOX_OAUTH_REFRESH_GRACE_SECONDS = "301";
  const other = await connect(t);
  assert.equal((await refreshWith(other.client, other.tokens.refresh_token)).status, 503);
});

test("a rotated session secret makes the pair unreadable: the retry fails without revoking", async (t) => {
  const { client, tokens } = await connect(t);
  const issued = await (await refreshWith(client, tokens.refresh_token)).json();
  process.env.ENGINEBOX_SESSION_SECRET = "a-different-session-signing-secret-987654321";
  const retry = await refreshWith(client, tokens.refresh_token);
  assert.equal(retry.status, 400);
  assert.equal((await call("v1/agent-runs", issued.access_token)).status, 200, "not treated as theft");
});

// ------------------------------------------------------------ projects

test("consent groups pools by project; an app may ask for one project and the grant is restricted to it", async (t) => {
  const controller = fakeController(t);
  const client = await newClient();
  // Without a project: every pool, grouped by project.
  const { challenge: c0 } = pkce();
  const open = await (await authorize(new Request(authorizeUrl(standardParams(client, c0)), { headers: { cookie: cookie() } }))).text();
  assert.match(open, /Project <code>alpha<\/code>.*workers/);
  assert.match(open, /Project <code>beta<\/code>.*beta-pool/);
  assert.match(open, /Project <code>default<\/code>.*plain/);
  // An invalid project is refused back to the app.
  const { challenge: c1 } = pkce();
  const bad = await authorize(new Request(authorizeUrl(standardParams(client, c1, { project: "Not_A_Project" })), { headers: { cookie: cookie() } }));
  assert.equal(bad.status, 302);
  assert.equal(new URL(bad.headers.get("location")!).searchParams.get("error"), "invalid_request");
  // With project=alpha: only alpha's pools are offered and accepted.
  const { verifier, challenge } = pkce();
  const page = await (await authorize(new Request(authorizeUrl(standardParams(client, challenge, { project: "alpha" })), { headers: { cookie: cookie() } }))).text();
  assert.match(page, /workers/);
  assert.doesNotMatch(page, /beta-pool/);
  const id = requestId(page);
  const refused = await (await consentPost([["request", id], ["action", "approve"], ["pool", "beta-pool"]])).text();
  assert.match(refused, /no longer available/);
  const approved = await consentPost([["request", id], ["action", "approve"], ["pool", "workers"]]);
  assert.equal(approved.status, 303);
  const code = new URL(approved.headers.get("location")!).searchParams.get("code")!;
  const tokens = await (await exchange(client, code, verifier)).json();
  assert.deepEqual(tokens.enginebox_grants, [{ kind: "pool", pool: "workers", project: "alpha", profile: "coder", profiles: ["coder"] }]);
  const [minted] = [...controller.tokens.values()];
  assert.equal(minted.project, "alpha", "the access token carries the project restriction");
  const refreshed = await (await refreshWith(client, tokens.refresh_token)).json();
  assert.equal(controller.tokens.get(refreshed.access_token.slice(5, 37))!.project, "alpha");
  const listed = await listGrants(new Request(`${origin}/api/oauth/grants`, { headers: { cookie: cookie() } })).json();
  assert.equal(listed.grants[0].project, "alpha");
});
