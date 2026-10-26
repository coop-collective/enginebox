import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import {
  authorizationServerMetadata,
  authorize,
  listGrants,
  oauthErrorResponse,
  protectedResourceMetadata,
  resourceProxy,
  revoke,
  revokeGrantByOwner,
} from "../lib/oauth/server";
import {
  advance,
  approve,
  authorizeUrl,
  call,
  connect,
  consentPost,
  cookie,
  dir,
  exchange,
  fakeController,
  form,
  newClient,
  origin,
  ownerToken,
  password,
  pkce,
  refreshWith,
  registerClient,
  requestId,
  standardParams,
  tokenRequest,
} from "./oauth-fixture";

// ------------------------------------------------------------ tests

test("discovery documents describe the resource and the authorization server", async () => {
  const prm = await protectedResourceMetadata().json();
  assert.deepEqual(prm.authorization_servers, [origin]);
  assert.equal(prm.resource, origin);
  assert.deepEqual(prm.scopes_supported, ["agent-runs"]);
  const as = await authorizationServerMetadata().json();
  assert.equal(as.issuer, origin);
  assert.equal(as.authorization_endpoint, `${origin}/oauth/authorize`);
  assert.equal(as.token_endpoint, `${origin}/oauth/token`);
  assert.equal(as.registration_endpoint, `${origin}/oauth/register`);
  assert.equal(as.revocation_endpoint, `${origin}/oauth/revoke`);
  assert.deepEqual(as.code_challenge_methods_supported, ["S256"]);
  assert.deepEqual(as.grant_types_supported, ["authorization_code", "refresh_token"]);
  assert.deepEqual(as.response_types_supported, ["code"]);
  delete process.env.ENGINEBOX_OAUTH_STATE_DIR;
  assert.equal(oauthErrorResponse((() => { try { protectedResourceMetadata(); } catch (e) { return e; } })()).status, 404);
});

test("dynamic registration validates redirect URIs and metadata, and stores only a secret digest", async () => {
  for (const redirect_uris of [
    undefined,
    [],
    ["http://example.com/cb"],
    ["https://example.com/cb#frag"],
    ["https://user:pw@example.com/cb"],
    ["myapp://callback"],
    ["/relative"],
    ["javascript:alert(1)"],
  ]) {
    const response = await registerClient({ client_name: "x", redirect_uris });
    assert.equal(response.status, 400, JSON.stringify(redirect_uris));
    assert.equal((await response.json()).error, "invalid_redirect_uri");
  }
  for (const metadata of [
    { redirect_uris: ["https://a.example/cb"], token_endpoint_auth_method: "private_key_jwt" },
    { redirect_uris: ["https://a.example/cb"], grant_types: ["client_credentials"] },
    { redirect_uris: ["https://a.example/cb"], response_types: ["token"] },
    { redirect_uris: ["https://a.example/cb"], scope: "admin" },
    { redirect_uris: ["https://a.example/cb"], client_name: "bad\nname" },
  ]) {
    const response = await registerClient(metadata);
    assert.equal(response.status, 400, JSON.stringify(metadata));
    assert.equal((await response.json()).error, "invalid_client_metadata");
  }
  for (const uri of ["http://localhost:3000/cb", "http://127.0.0.1/cb", "http://[::1]:9/cb", "https://app.example/oauth/callback?x=1"]) {
    assert.equal((await registerClient({ redirect_uris: [uri], token_endpoint_auth_method: "none" })).status, 201, uri);
  }
  const confidential = await newClient("basic", "https://app.example/cb");
  assert.match(confidential.client_secret!, /^ebcs_/);
  const stored = readFileSync(join(dir, "oauth.json"), "utf8");
  assert.ok(!stored.includes(confidential.client_secret!));
  const publicClient = await newClient("public");
  assert.equal(publicClient.client_secret, undefined);
});

test("PKCE is required (S256 only) and a wrong verifier burns the code", async (t) => {
  fakeController(t);
  const client = await newClient();
  const { verifier, challenge } = pkce();
  for (const extra of [{ code_challenge: undefined }, { code_challenge_method: "plain" }, { code_challenge_method: undefined }, { code_challenge: "short" }]) {
    const response = await authorize(new Request(authorizeUrl(standardParams(client, challenge, extra)), { headers: { cookie: cookie() } }));
    assert.equal(response.status, 302);
    const location = new URL(response.headers.get("location")!);
    assert.equal(location.searchParams.get("error"), "invalid_request");
    assert.equal(location.searchParams.get("state"), "opaque-state-123");
  }
  const location = await approve(client, challenge);
  const code = location.searchParams.get("code")!;
  const wrong = await exchange(client, code, pkce().verifier);
  assert.equal(wrong.status, 400);
  assert.equal((await wrong.json()).error, "invalid_grant");
  const retry = await exchange(client, code, verifier);
  assert.equal(retry.status, 400);
  assert.equal((await retry.json()).error, "invalid_grant");
});

test("state passes through; unregistered redirects never redirect; mismatches fail", async (t) => {
  fakeController(t);
  const client = await newClient();
  const { verifier, challenge } = pkce();
  for (const extra of [{ redirect_uri: "http://127.0.0.1:7777/other" }, { redirect_uri: "http://127.0.0.1:7777/callback/" }, { client_id: "ebc_unknown" }, { redirect_uri: undefined }]) {
    const response = await authorize(new Request(authorizeUrl(standardParams(client, challenge, extra)), { headers: { cookie: cookie() } }));
    assert.equal(response.status, 400);
    assert.equal(response.headers.get("location"), null);
  }
  const target = await authorize(new Request(authorizeUrl(standardParams(client, challenge, { resource: "https://elsewhere.example" })), { headers: { cookie: cookie() } }));
  assert.equal(new URL(target.headers.get("location")!).searchParams.get("error"), "invalid_target");
  const scope = await authorize(new Request(authorizeUrl(standardParams(client, challenge, { scope: "agent-runs admin" })), { headers: { cookie: cookie() } }));
  assert.equal(new URL(scope.headers.get("location")!).searchParams.get("error"), "invalid_scope");

  const location = await approve(client, challenge);
  assert.equal(location.origin + location.pathname, client.redirect_uris[0]);
  assert.equal(location.searchParams.get("state"), "opaque-state-123");
  assert.equal(location.searchParams.get("iss"), origin);
  const mismatch = await tokenRequest([
    ["grant_type", "authorization_code"],
    ["code", location.searchParams.get("code")!],
    ["redirect_uri", "http://127.0.0.1:7777/other"],
    ["code_verifier", verifier],
    ["client_id", client.client_id],
  ]);
  assert.equal((await mismatch.json()).error, "invalid_grant");

  // Denial returns access_denied with the same state.
  const page = await authorize(new Request(authorizeUrl(standardParams(client, challenge)), { headers: { cookie: cookie() } }));
  const denied = await consentPost([["request", requestId(await page.text())], ["action", "deny"]]);
  const deniedAt = new URL(denied.headers.get("location")!);
  assert.equal(deniedAt.searchParams.get("error"), "access_denied");
  assert.equal(deniedAt.searchParams.get("state"), "opaque-state-123");
});

test("consent needs sign-in, a same-origin form and a real workspace; profiles are kept as names", async (t) => {
  fakeController(t);
  const client = await newClient();
  const { verifier, challenge } = pkce();
  const first = await authorize(new Request(authorizeUrl(standardParams(client, challenge))));
  const interstitial = await first.text();
  assert.match(interstitial, /http-equiv="refresh"/);
  const resume = /url=([^"]+)"/.exec(interstitial)![1].replace(/&amp;/g, "&");
  const signIn = await authorize(new Request(`${origin}${resume}`));
  const signInHtml = await signIn.text();
  assert.match(signInHtml, /Operator password/);
  const id = requestId(signInHtml);
  const unsigned = await consentPost([["request", id], ["action", "approve"], ["workspace", "worker/ebox-a"]], { origin });
  assert.match(await unsigned.text(), /Sign in again/);
  const badPassword = await consentPost([["request", id], ["action", "sign_in"], ["password", "wrong-password-value"]], { origin });
  assert.match(await badPassword.text(), /not accepted/);
  const signedIn = await consentPost([["request", id], ["action", "sign_in"], ["password", password]], { origin });
  assert.ok(signedIn.headers.get("set-cookie")?.includes("enginebox_session="));
  const consent = await signedIn.text();
  assert.match(consent, /alpha/);
  assert.ok(!consent.includes("ebox-pool") && !consent.includes("worker/base"), "reserves and unmanaged bases are not offered");
  const crossSite = await consentPost([["request", id], ["action", "approve"], ["workspace", "worker/ebox-a"]], { origin: "https://attacker.example", cookie: cookie() });
  assert.equal(crossSite.status, 403);
  const unknown = await consentPost([["request", id], ["action", "approve"], ["workspace", "worker/ebox-pool"]]);
  assert.match(await unknown.text(), /no longer available/);
  const badProfile = await consentPost([["request", id], ["action", "approve"], ["workspace", "worker/ebox-a"], ["profiles:worker/ebox-a", "ok, ../bad"]]);
  assert.match(await badProfile.text(), /Profile names/);
  const unknownProfile = await consentPost([["request", id], ["action", "approve"], ["workspace", "worker/ebox-a"], ["profiles:worker/ebox-a", "review, nope"]]);
  assert.match(await unknownProfile.text(), /Unknown profile: nope/);
  const ok = await consentPost([["request", id], ["action", "approve"], ["workspace", "worker/ebox-a"], ["profiles:worker/ebox-a", "review, build"]], { cookie: cookie(), "sec-fetch-site": "same-origin" });
  assert.equal(ok.status, 303);
  const issued = await exchange(client, new URL(ok.headers.get("location")!).searchParams.get("code")!, verifier);
  const body = await issued.json();
  assert.deepEqual(body.enginebox_grants, [{ kind: "workspace", host: "worker", instance: "ebox-a", profiles: ["review", "build"] }]);
  assert.equal(body.token_type, "Bearer");
  assert.equal(body.scope, "agent-runs");
  assert.ok(body.expires_in > 3500 && body.expires_in <= 3600);
  assert.equal(issued.headers.get("cache-control"), "no-store");
  // The request was consumed.
  const again = await consentPost([["request", id], ["action", "approve"], ["workspace", "worker/ebox-a"]]);
  assert.match(await again.text(), /expired or was already used/);
});

test("access tokens are scoped service tokens minted on the controller for the consented workspaces", async (t) => {
  const { controller, tokens } = await connect(t);
  const minted = [...controller.tokens.values()];
  assert.equal(minted.length, 1);
  assert.equal(minted[0].secret, tokens.access_token);
  assert.deepEqual(minted[0].scopes, [{ host_id: "worker", name: "ebox-a" }]);
  assert.deepEqual(minted[0].operations, ["agent_runs.start", "agent_runs.get", "agent_runs.list", "agent_runs.cancel"]);
  assert.match(minted[0].label, /^oauth grant [a-f0-9]{32}$/);
  const stored = readFileSync(join(dir, "oauth.json"), "utf8");
  for (const secret of [tokens.access_token, tokens.refresh_token]) assert.ok(!stored.includes(secret), "state holds only digests");
});

test("a reused authorization code fails and revokes the tokens it produced", async (t) => {
  const { controller, client, tokens, code, verifier } = await connect(t);
  const reuse = await exchange(client, code, verifier);
  assert.equal(reuse.status, 400);
  assert.equal((await reuse.json()).error, "invalid_grant");
  assert.ok([...controller.tokens.values()].every((x) => x.revoked));
  assert.equal((await call("v1/agent-runs", tokens.access_token)).status, 401);
  assert.equal((await refreshWith(client, tokens.refresh_token)).status, 400);
});

test("refresh rotates both tokens; reusing a rotated refresh token revokes the family", async (t) => {
  const { controller, client, tokens } = await connect(t);
  const first = await refreshWith(client, tokens.refresh_token);
  assert.equal(first.status, 200);
  const rotated = await first.json();
  assert.notEqual(rotated.refresh_token, tokens.refresh_token);
  assert.notEqual(rotated.access_token, tokens.access_token);
  assert.equal(controller.tokens.get(tokens.access_token.slice(5, 37))!.revoked, true, "the previous access token is revoked");
  assert.equal((await call("v1/agent-runs", tokens.access_token)).status, 401);
  assert.equal((await call("v1/agent-runs", rotated.access_token)).status, 200);

  // Past the refresh grace window, presenting the rotated token is reuse.
  advance(t, 31_000);
  const reuse = await refreshWith(client, tokens.refresh_token);
  assert.equal(reuse.status, 400);
  assert.equal((await reuse.json()).error, "invalid_grant");
  assert.equal((await refreshWith(client, rotated.refresh_token)).status, 400, "the whole family is revoked");
  assert.equal((await call("v1/agent-runs", rotated.access_token)).status, 401);
  assert.ok([...controller.tokens.values()].every((x) => x.revoked));

  // Another client cannot use this client's refresh token.
  const other = await connect(t);
  const stranger = await newClient();
  assert.equal((await refreshWith(stranger, other.tokens.refresh_token)).status, 400);
});

test("confidential clients must authenticate with their registered method", async (t) => {
  fakeController(t);
  const client = await newClient("basic", "https://app.example/cb");
  const { verifier, challenge } = pkce();
  const location = await approve(client, challenge);
  const fields: [string, string][] = [
    ["grant_type", "authorization_code"],
    ["code", location.searchParams.get("code")!],
    ["redirect_uri", client.redirect_uris[0]],
    ["code_verifier", verifier],
  ];
  const wrong = await tokenRequest(fields, { authorization: `Basic ${Buffer.from(`${client.client_id}:ebcs_wrong`).toString("base64")}` });
  assert.equal(wrong.status, 401);
  assert.match(wrong.headers.get("www-authenticate")!, /^Basic/);
  const post = await tokenRequest([...fields, ["client_id", client.client_id], ["client_secret", client.client_secret!]]);
  assert.equal(post.status, 401, "client_secret_post is refused for a basic client");
  const ok = await tokenRequest(fields, { authorization: `Basic ${Buffer.from(`${client.client_id}:${client.client_secret}`).toString("base64")}` });
  assert.equal(ok.status, 200);
});

test("revocation: a client revokes its refresh token, the owner disconnects a grant, both kill the live access token", async (t) => {
  const { controller, client, tokens } = await connect(t);
  const unknown = await revoke(new Request(`${origin}/oauth/revoke`, { method: "POST", headers: { "content-type": "application/x-www-form-urlencoded" }, body: form([["token", "ebrt_nothing"], ["client_id", client.client_id]]) }));
  assert.equal(unknown.status, 200);
  const revoked = await revoke(new Request(`${origin}/oauth/revoke`, { method: "POST", headers: { "content-type": "application/x-www-form-urlencoded" }, body: form([["token", tokens.refresh_token], ["token_type_hint", "refresh_token"], ["client_id", client.client_id]]) }));
  assert.equal(revoked.status, 200);
  assert.ok([...controller.tokens.values()].every((x) => x.revoked));
  assert.equal((await call("v1/agent-runs", tokens.access_token)).status, 401);
  assert.equal((await refreshWith(client, tokens.refresh_token)).status, 400);

  const second = await connect(t);
  const owner = new Request(`${origin}/api/oauth/grants`, { headers: { cookie: cookie() } });
  const listed = await listGrants(owner).json();
  assert.equal(listed.grants.length, 1);
  assert.equal(listed.grants[0].client_name, "Example App");
  assert.deepEqual(listed.grants[0].redirect_hosts, ["127.0.0.1:7777"]);
  assert.ok(!JSON.stringify(listed).includes(second.tokens.access_token));
  await assert.rejects(revokeGrantByOwner(new Request(`${origin}/api/oauth/grants/x/revoke`, { method: "POST", headers: { cookie: cookie(), origin: "https://attacker.example" } }), listed.grants[0].id));
  await revokeGrantByOwner(new Request(`${origin}/api/oauth/grants/x/revoke`, { method: "POST", headers: { cookie: cookie(), origin } }), listed.grants[0].id);
  assert.ok([...second.controller.tokens.values()].every((x) => x.revoked));
  assert.equal((await call("v1/agent-runs", second.tokens.access_token)).status, 401);
  assert.equal((await refreshWith(second.client, second.tokens.refresh_token)).status, 400);
  assert.equal((await listGrants(owner).json()).grants.length, 0);
});

test("the public resource path: WWW-Authenticate challenges, and a token cannot reach workspaces it was not granted", async (t) => {
  const { controller, tokens } = await connect(t);
  const anonymous = await call("v1/agent-runs");
  assert.equal(anonymous.status, 401);
  assert.equal(anonymous.headers.get("www-authenticate"), `Bearer resource_metadata="${origin}/.well-known/oauth-protected-resource"`);
  const granted = await call("v1/hosts/worker/instances/ebox-a/agent-runs", tokens.access_token, "POST", { provider: "codex", prompt: "hi" });
  assert.equal(granted.status, 202);
  const run = await call(`v1/hosts/worker/instances/ebox-a/agent-runs/${"1".repeat(32)}`, tokens.access_token);
  assert.equal(run.status, 200);
  const other = await call("v1/hosts/worker/instances/ebox-b/agent-runs", tokens.access_token, "POST", { provider: "codex", prompt: "hi" });
  assert.equal(other.status, 403);
  assert.match(other.headers.get("www-authenticate")!, /error="insufficient_scope"/);
  const cancel = await call(`v1/hosts/worker/instances/ebox-b/agent-runs/${"1".repeat(32)}/cancel`, tokens.access_token, "POST", {});
  assert.equal(cancel.status, 403);
  for (const path of ["v1/fleet", "v1/service-tokens", "v1/templates", "v1/hosts/worker/instances/ebox-a/actions"])
    assert.equal((await call(path, tokens.access_token)).status, 404, path);
  // The upstream call carries the scoped token, never the owner token.
  const forwarded = controller.calls.filter((c) => c.path.includes("agent-runs"));
  assert.ok(forwarded.length > 0 && forwarded.every((c) => c.auth === `Bearer ${tokens.access_token}`));
  // A service token that no live grant holds (e.g. minted by hand) is refused at the edge.
  const handMade = `ebst_${"a".repeat(32)}.${"b".repeat(64)}`;
  const refused = await call("v1/agent-runs", handMade);
  assert.equal(refused.status, 401);
  assert.match(refused.headers.get("www-authenticate")!, /error="invalid_token"/);
  // Cookies do not authenticate this path.
  const cookieOnly = await resourceProxy(new Request(`${origin}/v1/agent-runs`, { headers: { cookie: cookie() } }), ["v1", "agent-runs"]);
  assert.equal(cookieOnly.status, 401);
});

test("no token, code or secret reaches the logs, even when controller revocation fails", async (t) => {
  const lines: string[] = [];
  for (const level of ["log", "info", "warn", "error", "debug"] as const)
    t.mock.method(console, level, (...args: unknown[]) => lines.push(args.map(String).join(" ")));
  const controller = fakeController(t, { failRevoke: true });
  const client = await newClient("basic", "https://app.example/cb");
  const { verifier, challenge } = pkce();
  const location = await approve(client, challenge);
  const code = location.searchParams.get("code")!;
  const basic = { authorization: `Basic ${Buffer.from(`${client.client_id}:${client.client_secret}`).toString("base64")}` };
  const issued = await (await tokenRequest([["grant_type", "authorization_code"], ["code", code], ["redirect_uri", client.redirect_uris[0]], ["code_verifier", verifier]], basic)).json();
  const rotated = await (await tokenRequest([["grant_type", "refresh_token"], ["refresh_token", issued.refresh_token]], basic)).json();
  advance(t, 31_000);
  await tokenRequest([["grant_type", "refresh_token"], ["refresh_token", issued.refresh_token]], basic);
  await call("v1/agent-runs", rotated.access_token);
  assert.ok(lines.some((l) => l.includes("oauth_revocation_retry")), "the failed revocation is logged");
  // The edge refuses the token at once even though the controller has not confirmed revocation.
  assert.equal((await call("v1/agent-runs", rotated.access_token)).status, 401);
  assert.ok(readFileSync(join(dir, "oauth.json"), "utf8").includes("pending_revocations"));
  const secrets = [client.client_secret!, code, verifier, issued.access_token, issued.refresh_token, rotated.access_token, rotated.refresh_token, ownerToken, password];
  for (const minted of controller.tokens.values()) secrets.push(minted.secret, minted.secret.split(".")[1]);
  const text = lines.join("\n");
  for (const secret of secrets) assert.ok(!text.includes(secret), "a credential appeared in logs");
});

// ------------------------------------------------------------ pools

test("consent offers owner-managed pools; the token response lists workspaces and pools with a kind", async (t) => {
  const { controller, client, tokens } = await connect(t, ["worker/ebox-a"], ["workers", "plain"]);
  assert.deepEqual(tokens.enginebox_grants, [
    { kind: "workspace", host: "worker", instance: "ebox-a", profiles: [] },
    { kind: "pool", pool: "workers", project: "alpha", profile: "coder", profiles: ["coder"] },
    { kind: "pool", pool: "plain", project: "default", profile: null, profiles: [] },
  ]);
  // The access token carries the controller's pool grant and the grant's callback key.
  const [minted] = [...controller.tokens.values()];
  assert.deepEqual(minted.pools, ["workers", "plain"]);
  assert.deepEqual(minted.scopes, [{ host_id: "worker", name: "ebox-a" }]);
  assert.equal(minted.profiles, undefined, "no workspace profiles were named");
  assert.match(tokens.enginebox_callback_secret!, /^[A-Za-z0-9_-]{43}$/);
  assert.equal(minted.callback_secret, tokens.enginebox_callback_secret);
  // Refresh keeps the grants and the callback key, and never repeats the key.
  const refreshed = await (await refreshWith(client, tokens.refresh_token)).json();
  assert.deepEqual(refreshed.enginebox_grants, tokens.enginebox_grants);
  assert.equal(refreshed.enginebox_callback_secret, undefined);
  const renewed = controller.tokens.get(refreshed.access_token.slice(5, 37))!;
  assert.deepEqual(renewed.pools, ["workers", "plain"]);
  assert.equal(renewed.callback_secret, tokens.enginebox_callback_secret);
  // The owner sees the pools on the connection.
  const listed = await listGrants(new Request(`${origin}/api/oauth/grants`, { headers: { cookie: cookie() } })).json();
  assert.deepEqual(listed.grants[0].pools, [
    { pool: "workers", profile: "coder", project: "alpha" },
    { pool: "plain", profile: null, project: "default" },
  ]);
  assert.equal(listed.grants[0].project, null);
  assert.equal(minted.project, undefined, "an unrestricted grant sends no project");
});

test("a pool-only grant needs no workspace, and consent still works when workspace inventory is unavailable", async (t) => {
  fakeController(t, { fleetDown: true });
  const client = await newClient();
  const { verifier, challenge } = pkce();
  const page = await authorize(new Request(authorizeUrl(standardParams(client, challenge)), { headers: { cookie: cookie() } }));
  assert.equal(page.status, 200);
  const html = await page.text();
  assert.match(html, /name="pool" value="workers"/);
  assert.match(html, /runs use profile <code>coder<\/code>/);
  const id = requestId(html);
  const none = await consentPost([["request", id], ["action", "approve"]]);
  assert.match(await none.text(), /Select at least one workspace or pool/);
  const gone = await consentPost([["request", id], ["action", "approve"], ["pool", "deleted"]]);
  assert.match(await gone.text(), /pool is no longer available/);
  const ok = await consentPost([["request", id], ["action", "approve"], ["pool", "workers"]]);
  assert.equal(ok.status, 303);
  const issued = await (await exchange(client, new URL(ok.headers.get("location")!).searchParams.get("code")!, verifier)).json();
  assert.deepEqual(issued.enginebox_grants, [{ kind: "pool", pool: "workers", project: "alpha", profile: "coder", profiles: ["coder"] }]);
});

test("the pool routes reach the controller only for granted pools", async (t) => {
  const { controller, tokens } = await connect(t, ["worker/ebox-a"], ["workers"]);
  const started = await call("v1/pools/workers/agent-runs", tokens.access_token, "POST", { provider: "claude", prompt: "hi", idempotency_key: "task-1:1" });
  assert.equal(started.status, 202);
  const run = (await started.json()).run;
  assert.equal(run.status, "queued");
  assert.equal(run.host_id, null);
  assert.equal((await call(`v1/pools/workers/agent-runs/${run.id}`, tokens.access_token)).status, 200);
  assert.equal((await call(`v1/pools/workers/agent-runs/${run.id}/cancel`, tokens.access_token, "POST", {})).status, 202);
  const full = await call("v1/pools/workers/agent-runs", tokens.access_token, "POST", { provider: "claude", prompt: "queue full" });
  assert.equal(full.status, 429);
  assert.equal(full.headers.get("retry-after"), "9", "Retry-After reaches the client");
  const other = await call("v1/pools/plain/agent-runs", tokens.access_token, "POST", { provider: "claude", prompt: "hi" });
  assert.equal(other.status, 403);
  assert.match(other.headers.get("www-authenticate")!, /error="insufficient_scope"/);
  // Shapes the controller does not serve to service tokens stay unknown at the edge.
  for (const [method, path] of [
    ["GET", "v1/pools"],
    ["GET", "v1/pools/workers"],
    ["POST", "v1/pools/workers/delete"],
    ["GET", "v1/pools/workers/agent-runs"],
    ["POST", "v1/pools/Workers/agent-runs"],
    ["POST", "v1/pools/workers/agent-runs/x/delete"],
  ] as const)
    assert.equal((await call(path, tokens.access_token, method, method === "POST" ? {} : undefined)).status, 404, `${method} ${path}`);
  const forwarded = controller.calls.filter((c) => c.path.startsWith("/v1/pools/"));
  assert.ok(forwarded.length === 5 && forwarded.every((c) => c.auth === `Bearer ${tokens.access_token}`));
});

test("a refresh after a consented pool was deleted ends the connection with invalid_grant", async (t) => {
  const { controller, client, tokens } = await connect(t, [], ["workers"]);
  controller.pools.delete("workers");
  const refused = await refreshWith(client, tokens.refresh_token);
  assert.equal(refused.status, 400);
  assert.equal((await refused.json()).error, "invalid_grant");
  assert.equal((await call("v1/pools/workers/agent-runs/x", tokens.access_token)).status, 401, "the live access token is revoked too");
  assert.ok([...controller.tokens.values()].every((x) => x.revoked));
});


// ------------------------------------------------------------ repositories

test("consent offers repository access per project; the grant, token response and access token carry it", async (t) => {
  const controller = fakeController(t);
  const client = await newClient();
  const { verifier, challenge } = pkce();
  const page = await authorize(new Request(authorizeUrl(standardParams(client, challenge)), { headers: { cookie: cookie() } }));
  const html = await page.text();
  // Projects with pools (alpha, beta, default) and with repositories only (gamma).
  for (const project of ["alpha", "beta", "default", "gamma"]) {
    assert.match(html, new RegExp(`Manage repositories in project <code>${project}</code> <select name="repositories:${project}"`));
  }
  const id = requestId(html);
  const bad = await consentPost([["request", id], ["action", "approve"], ["repositories:alpha", "admin"]]);
  assert.match(await bad.text(), /Choose read or write access/);
  const unknown = await consentPost([["request", id], ["action", "approve"], ["repositories:delta", "read"]]);
  assert.match(await unknown.text(), /selected project is not available/);
  const ok = await consentPost([
    ["request", id],
    ["action", "approve"],
    ["repositories:alpha", "write"],
    ["repositories:gamma", "read"],
    ["repositories:beta", ""],
  ]);
  assert.equal(ok.status, 303);
  const response = await exchange(client, new URL(ok.headers.get("location")!).searchParams.get("code")!, verifier);
  const tokens = await response.json();
  assert.deepEqual(tokens.enginebox_grants, [
    { kind: "repositories", project: "alpha", access: "write" },
    { kind: "repositories", project: "gamma", access: "read" },
  ]);
  const [minted] = [...controller.tokens.values()];
  assert.deepEqual(minted.repositories, [
    { project: "alpha", access: "write" },
    { project: "gamma", access: "read" },
  ]);
  assert.deepEqual(minted.scopes, []);
  assert.equal(minted.pools, undefined);
  assert.equal(minted.project, undefined);
  // The repository routes reach the controller, which enforces the grant.
  assert.equal((await call("v1/projects/alpha/repositories", tokens.access_token)).status, 200);
  assert.equal((await call("v1/projects/alpha/repositories", tokens.access_token, "POST", { name: "game" })).status, 201);
  assert.equal((await call("v1/projects/alpha/repositories/game", tokens.access_token)).status, 200);
  const writeToken = await call("v1/projects/alpha/repositories/game/tokens", tokens.access_token, "POST", { access: "write", ttl_seconds: 600 });
  assert.equal(writeToken.status, 201);
  assert.equal((await call("v1/projects/gamma/repositories/tools/tokens", tokens.access_token, "POST", { access: "read" })).status, 201);
  const readOnly = await call("v1/projects/gamma/repositories/tools/tokens", tokens.access_token, "POST", { access: "write" });
  assert.equal(readOnly.status, 403);
  assert.match(readOnly.headers.get("www-authenticate")!, /error="insufficient_scope"/);
  assert.equal((await call("v1/projects/gamma/repositories", tokens.access_token, "POST", { name: "x" })).status, 403);
  assert.equal((await call("v1/projects/beta/repositories", tokens.access_token)).status, 403);
  // Owner-only and unserved shapes stay unknown at the edge.
  for (const [method, path] of [
    ["GET", "v1/repositories"],
    ["GET", "v1/repository-backends"],
    ["POST", "v1/repository-backends/artifacts-worker"],
    ["POST", "v1/projects/alpha/repositories/game/delete"],
    ["GET", "v1/projects/Alpha/repositories"],
    ["GET", "v1/projects/alpha/repositories/game/tokens"],
    ["GET", "v1/projects/alpha/repositories/Game"],
  ] as const)
    assert.equal((await call(path, tokens.access_token, method, method === "POST" ? {} : undefined)).status, 404, `${method} ${path}`);
  // Refresh keeps the grant; the owner sees it on the connection.
  const refreshed = await (await refreshWith(client, tokens.refresh_token)).json();
  assert.deepEqual(refreshed.enginebox_grants, tokens.enginebox_grants);
  assert.deepEqual(controller.tokens.get(refreshed.access_token.slice(5, 37))!.repositories, minted.repositories);
  const listed = await listGrants(new Request(`${origin}/api/oauth/grants`, { headers: { cookie: cookie() } })).json();
  assert.deepEqual(listed.grants[0].repositories, [
    { project: "alpha", access: "write" },
    { project: "gamma", access: "read" },
  ]);
});

test("an app that asks for one project is offered only that project's repositories, and the token is restricted to it", async (t) => {
  const controller = fakeController(t);
  const client = await newClient();
  const { verifier, challenge } = pkce();
  const page = await (await authorize(new Request(authorizeUrl(standardParams(client, challenge, { project: "alpha" })), { headers: { cookie: cookie() } }))).text();
  assert.match(page, /name="repositories:alpha"/);
  assert.doesNotMatch(page, /name="repositories:gamma"/);
  const id = requestId(page);
  const outside = await consentPost([["request", id], ["action", "approve"], ["repositories:gamma", "read"]]);
  assert.match(await outside.text(), /selected project is not available/);
  const ok = await consentPost([["request", id], ["action", "approve"], ["repositories:alpha", "read"]]);
  assert.equal(ok.status, 303);
  const tokens = await (await exchange(client, new URL(ok.headers.get("location")!).searchParams.get("code")!, verifier)).json();
  assert.deepEqual(tokens.enginebox_grants, [{ kind: "repositories", project: "alpha", access: "read" }]);
  const [minted] = [...controller.tokens.values()];
  assert.equal(minted.project, "alpha", "a repository-only grant still carries the project restriction");
});
