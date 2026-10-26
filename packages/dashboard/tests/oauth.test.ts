import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, test, type TestContext } from "node:test";
import { sessionCookie } from "../lib/server";
import {
  authorizationServerMetadata,
  authorize,
  authorizePost,
  listGrants,
  oauthErrorResponse,
  protectedResourceMetadata,
  register,
  resourceProxy,
  revoke,
  revokeGrantByOwner,
  token,
} from "../lib/oauth/server";

const origin = "https://enginebox.example";
const password = "test-only-dashboard-password";
const ownerToken = "test-only-owner-token-never-leaves-server";
let dir = "";

beforeEach(() => {
  process.env.ENGINEBOX_DASHBOARD_PASSWORD = password;
  process.env.ENGINEBOX_SESSION_SECRET = "test-only-session-signing-secret-123456789";
  process.env.ENGINEBOX_API_TOKEN = ownerToken;
  process.env.ENGINEBOX_DASHBOARD_ORIGIN = origin;
  process.env.ENGINEBOX_API_URL = "http://127.0.0.1:8765";
  dir = mkdtempSync(join(tmpdir(), "enginebox-oauth-"));
  process.env.ENGINEBOX_OAUTH_STATE_DIR = dir;
});
afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

// ------------------------------------------------------------ fake controller

type FakeToken = { id: string; secret: string; scopes: { host_id: string; name: string }[]; operations: string[]; revoked: boolean; label: string };
function fakeController(t: TestContext, opts: { failRevoke?: boolean } = {}) {
  const tokens = new Map<string, FakeToken>();
  const calls: { method: string; path: string; auth: string; body?: unknown }[] = [];
  const reply = (body: unknown, status = 200) => Response.json(body, { status });
  t.mock.method(globalThis, "fetch", async (input: string, init: RequestInit = {}) => {
    const url = new URL(input);
    assert.equal(url.origin, "http://127.0.0.1:8765", "only the loopback controller is called");
    const auth = new Headers(init.headers).get("authorization") ?? "";
    const method = init.method ?? "GET";
    const body = init.body ? JSON.parse(String(init.body)) : undefined;
    calls.push({ method, path: url.pathname, auth, body });
    const parts = url.pathname.split("/").filter(Boolean);
    if (auth === `Bearer ${ownerToken}`) {
      if (method === "GET" && url.pathname === "/v1/fleet")
        return reply({
          hosts: [
            {
              id: "worker",
              label: "Worker",
              online: true,
              instances: [
                { name: "ebox-a", workspace_name: "alpha", managed: true },
                { name: "ebox-b", workspace_name: "beta", managed: true },
                { name: "ebox-pool", managed: true, warm_pool_state: "ready" },
                { name: "base", managed: false },
              ],
            },
          ],
          templates: [],
          sizes: {},
        });
      if (method === "POST" && url.pathname === "/v1/service-tokens") {
        const id = randomBytes(16).toString("hex");
        const secret = `ebst_${id}.${randomBytes(32).toString("hex")}`;
        tokens.set(id, { id, secret, scopes: body.scopes, operations: body.operations, revoked: false, label: body.label });
        return reply({ token: { id, expires_at: body.expires_at }, secret, secret_shown_once: true }, 201);
      }
      if (method === "POST" && parts[1] === "service-tokens" && parts[3] === "revoke") {
        if (opts.failRevoke) throw new TypeError("fetch failed");
        const found = tokens.get(parts[2]);
        if (!found) return reply({ error: "Unknown service token" }, 400);
        found.revoked = true;
        return reply({ token: { id: found.id } });
      }
      return reply({ error: "Unknown endpoint." }, 404);
    }
    // A service token: the controller's own scope enforcement, in miniature.
    const presented = auth.replace(/^Bearer /, "");
    const held = [...tokens.values()].find((x) => x.secret === presented && !x.revoked);
    if (!held) return reply({ error: "Service token is unknown, expired or revoked." }, 401);
    const sees = (h: string, n: string) => held.scopes.some((s) => s.host_id === h && s.name === n);
    if (url.pathname === "/v1/agent-runs") return reply({ runs: [] });
    const [, , host, , name] = parts;
    if (parts[5] !== "agent-runs" || !sees(host, name))
      return reply({ error: "This service token is not granted that operation on that workspace." }, 403);
    return reply({ run: { id: "1".repeat(32), host_id: host, name } }, method === "POST" ? 202 : 200);
  });
  return { tokens, calls };
}

// ------------------------------------------------------------ helpers

const cookie = () => sessionCookie().split(";")[0];
const b64 = (bytes: Buffer) => bytes.toString("base64url");
function pkce() {
  const verifier = b64(randomBytes(32));
  return { verifier, challenge: b64(createHash("sha256").update(verifier).digest()) };
}
async function registerClient(metadata: Record<string, unknown>) {
  return register(new Request(`${origin}/oauth/register`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(metadata) })).catch(oauthErrorResponse);
}
async function newClient(kind: "public" | "basic" = "public", redirect = "http://127.0.0.1:7777/callback") {
  const response = await registerClient({
    client_name: "Example App",
    redirect_uris: [redirect],
    token_endpoint_auth_method: kind === "public" ? "none" : "client_secret_basic",
  });
  assert.equal(response.status, 201);
  return (await response.json()) as { client_id: string; client_secret?: string; redirect_uris: string[] };
}
function authorizeUrl(params: Record<string, string | undefined>) {
  const url = new URL(`${origin}/oauth/authorize`);
  for (const [k, v] of Object.entries(params)) if (v !== undefined) url.searchParams.set(k, v);
  return url.href;
}
function standardParams(client: { client_id: string; redirect_uris: string[] }, challenge: string, extra: Record<string, string | undefined> = {}) {
  return {
    response_type: "code",
    client_id: client.client_id,
    redirect_uri: client.redirect_uris[0],
    code_challenge: challenge,
    code_challenge_method: "S256",
    scope: "agent-runs",
    resource: origin,
    state: "opaque-state-123",
    ...extra,
  };
}
function form(values: [string, string][]) {
  return new URLSearchParams(values).toString();
}
function consentPost(fields: [string, string][], headers: Record<string, string> = { origin, cookie: cookie() }) {
  return authorizePost(
    new Request(`${origin}/oauth/authorize`, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded", ...headers },
      body: form(fields),
    }),
  );
}
const requestId = (html: string) => /name="request" value="([^"]+)"/.exec(html)![1];

/** Run sign-in and consent; returns the redirect Location. */
async function approve(client: { client_id: string; redirect_uris: string[] }, challenge: string, workspaces = ["worker/ebox-a"], profiles: Record<string, string> = {}) {
  const page = await authorize(new Request(authorizeUrl(standardParams(client, challenge)), { headers: { cookie: cookie() } }));
  assert.equal(page.status, 200);
  const html = await page.text();
  assert.match(html, /Example App/);
  const fields: [string, string][] = [["request", requestId(html)], ["action", "approve"]];
  for (const w of workspaces) fields.push(["workspace", w]);
  for (const [w, p] of Object.entries(profiles)) fields.push([`profiles:${w}`, p]);
  const response = await consentPost(fields);
  assert.equal(response.status, 303);
  return new URL(response.headers.get("location")!);
}
function tokenRequest(fields: [string, string][], headers: Record<string, string> = {}) {
  return token(
    new Request(`${origin}/oauth/token`, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded", ...headers },
      body: form(fields),
    }),
  ).catch(oauthErrorResponse);
}
async function exchange(client: { client_id: string; redirect_uris: string[] }, code: string, verifier: string) {
  return tokenRequest([
    ["grant_type", "authorization_code"],
    ["code", code],
    ["redirect_uri", client.redirect_uris[0]],
    ["code_verifier", verifier],
    ["client_id", client.client_id],
    ["resource", origin],
  ]);
}
async function connect(t: TestContext, workspaces = ["worker/ebox-a"]) {
  const controller = fakeController(t);
  const client = await newClient();
  const { verifier, challenge } = pkce();
  const location = await approve(client, challenge, workspaces);
  const response = await exchange(client, location.searchParams.get("code")!, verifier);
  assert.equal(response.status, 200);
  const tokens = (await response.json()) as { access_token: string; refresh_token: string; expires_in: number; enginebox_grants: unknown };
  return { controller, client, tokens, code: location.searchParams.get("code")!, verifier };
}
function refreshWith(client: { client_id: string }, refreshToken: string) {
  return tokenRequest([
    ["grant_type", "refresh_token"],
    ["refresh_token", refreshToken],
    ["client_id", client.client_id],
  ]);
}
function call(path: string, accessToken?: string, method = "GET", body?: unknown) {
  const segments = path.split("/").filter(Boolean);
  return resourceProxy(
    new Request(`${origin}/${path}`, {
      method,
      headers: { ...(accessToken ? { authorization: `Bearer ${accessToken}` } : {}), ...(body ? { "content-type": "application/json" } : {}) },
      body: body ? JSON.stringify(body) : undefined,
    }),
    segments,
  );
}

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
  const ok = await consentPost([["request", id], ["action", "approve"], ["workspace", "worker/ebox-a"], ["profiles:worker/ebox-a", "review, build"]], { cookie: cookie(), "sec-fetch-site": "same-origin" });
  assert.equal(ok.status, 303);
  const issued = await exchange(client, new URL(ok.headers.get("location")!).searchParams.get("code")!, verifier);
  const body = await issued.json();
  assert.deepEqual(body.enginebox_grants, [{ host: "worker", instance: "ebox-a", profiles: ["review", "build"] }]);
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
