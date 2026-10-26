/** Shared fixture for the OAuth tests: environment, a fake controller and flow helpers. */
import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, type TestContext } from "node:test";
import { sessionCookie } from "../lib/server";
import {
  authorize,
  authorizePost,
  oauthErrorResponse,
  register,
  resourceProxy,
  token,
} from "../lib/oauth/server";

export const origin = "https://enginebox.example";
export const password = "test-only-dashboard-password";
export const ownerToken = "test-only-owner-token-never-leaves-server";
export let dir = "";

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

export type FakeToken = {
  id: string;
  secret: string;
  scopes: { host_id: string; name: string }[];
  operations: string[];
  revoked: boolean;
  label: string;
  pools?: string[];
  profiles?: string[];
  callback_secret?: string;
  project?: string;
  repositories?: { project: string; access: "read" | "write" }[];
};
export type FakePool = { name: string; description: string; profile: string | null; members: { host_id: string; name: string }[]; project?: string };
export function fakeController(t: TestContext, opts: { failRevoke?: boolean; fleetDown?: boolean } = {}) {
  const pools = new Map<string, FakePool>([
    ["workers", { name: "workers", project: "alpha", description: "Coding agents", profile: "coder", members: [{ host_id: "worker", name: "ebox-w1" }, { host_id: "worker", name: "ebox-w2" }] }],
    // Listed without a project, as an older controller would: the default project.
    ["plain", { name: "plain", description: "", profile: null, members: [] }],
    ["beta-pool", { name: "beta-pool", project: "beta", description: "", profile: null, members: [] }],
  ]);
  const profiles = ["review", "build", "coder"];
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
      if (method === "GET" && url.pathname === "/v1/pools") return reply({ pools: [...pools.values()] });
      // Repositories exist in a project that has no pool.
      if (method === "GET" && url.pathname === "/v1/repositories") return reply({ repositories: [{ project: "gamma", name: "tools" }] });
      if (method === "GET" && url.pathname === "/v1/profiles") return reply({ profiles: profiles.map((name) => ({ name })) });
      if (method === "GET" && url.pathname === "/v1/fleet" && opts.fleetDown) return reply({ error: "host unreachable" }, 500);
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
        // The controller validates grants at mint: unknown pools or profiles are a 400.
        if ((body.pools ?? []).some((p: string) => !pools.has(p)) || (body.profiles ?? []).some((p: string) => !profiles.includes(p)))
          return reply({ error: "pools must name at most 256 existing pools" }, 400);
        if (body.project && (body.pools ?? []).some((p: string) => (pools.get(p)!.project ?? "default") !== body.project))
          return reply({ error: "Every granted pool must belong to the token's project" }, 400);
        if (body.project && (body.repositories ?? []).some((r: { project: string }) => r.project !== body.project))
          return reply({ error: "Every repository grant must name the token's project" }, 400);
        const id = randomBytes(16).toString("hex");
        const secret = `ebst_${id}.${randomBytes(32).toString("hex")}`;
        tokens.set(id, {
          id,
          secret,
          scopes: body.scopes,
          operations: body.operations,
          revoked: false,
          label: body.label,
          pools: body.pools,
          profiles: body.profiles,
          callback_secret: body.callback_secret,
          project: body.project,
          repositories: body.repositories,
        });
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
    if (parts[1] === "projects" && parts[3] === "repositories") {
      const access = (held.repositories ?? []).find((r) => r.project === parts[2])?.access;
      const refused = () => reply({ error: "This service token is not granted that operation on that project." }, 403);
      if (!access) return refused();
      const repository = { project: parts[2], name: parts[4] ?? body?.name, backend: "artifacts", remote: `https://acct.artifacts.cloudflare.net/git/eb-${parts[2]}/${parts[4] ?? body?.name}.git`, default_branch: "main" };
      if (method === "GET" && parts.length === 4) return reply({ project: parts[2], repositories: [] });
      if (method === "GET") return reply({ repository });
      if (parts.length === 4) return access === "write" ? reply({ repository }, 201) : refused();
      if (body?.access === "write" && access !== "write") return refused();
      return reply({ token: "art_v1_fake", expires_at: "2026-10-26T01:00:00Z", remote: repository.remote, access: body?.access }, 201);
    }
    if (parts[1] === "pools") {
      if (!(held.pools ?? []).includes(parts[2])) return reply({ error: "This service token is not granted that pool." }, 403);
      const placed = { host_id: null, name: null, pool: parts[2] };
      if (method === "POST" && parts.length === 4 && body?.prompt === "queue full")
        return Response.json({ error: "The run queue is full.", retry_after_seconds: 9 }, { status: 429, headers: { "Retry-After": "9" } });
      if (method === "POST" && parts.length === 4) return reply({ run: { id: "2".repeat(32), status: "queued", ...placed } }, 202);
      return reply({ run: { id: parts[4], status: parts[5] === "cancel" ? "cancelled" : "queued", ...placed } }, parts[5] === "cancel" ? 202 : 200);
    }
    const [, , host, , name] = parts;
    if (parts[5] !== "agent-runs" || !sees(host, name))
      return reply({ error: "This service token is not granted that operation on that workspace." }, 403);
    return reply({ run: { id: "1".repeat(32), host_id: host, name } }, method === "POST" ? 202 : 200);
  });
  return { tokens, calls, pools };
}

// ------------------------------------------------------------ helpers

export const cookie = () => sessionCookie().split(";")[0];
export const b64 = (bytes: Buffer) => bytes.toString("base64url");
export function pkce() {
  const verifier = b64(randomBytes(32));
  return { verifier, challenge: b64(createHash("sha256").update(verifier).digest()) };
}
export async function registerClient(metadata: Record<string, unknown>) {
  return register(new Request(`${origin}/oauth/register`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(metadata) })).catch(oauthErrorResponse);
}
export async function newClient(kind: "public" | "basic" = "public", redirect = "http://127.0.0.1:7777/callback") {
  const response = await registerClient({
    client_name: "Example App",
    redirect_uris: [redirect],
    token_endpoint_auth_method: kind === "public" ? "none" : "client_secret_basic",
  });
  assert.equal(response.status, 201);
  return (await response.json()) as { client_id: string; client_secret?: string; redirect_uris: string[] };
}
export function authorizeUrl(params: Record<string, string | undefined>) {
  const url = new URL(`${origin}/oauth/authorize`);
  for (const [k, v] of Object.entries(params)) if (v !== undefined) url.searchParams.set(k, v);
  return url.href;
}
export function standardParams(client: { client_id: string; redirect_uris: string[] }, challenge: string, extra: Record<string, string | undefined> = {}) {
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
export function form(values: [string, string][]) {
  return new URLSearchParams(values).toString();
}
export function consentPost(fields: [string, string][], headers: Record<string, string> = { origin, cookie: cookie() }) {
  return authorizePost(
    new Request(`${origin}/oauth/authorize`, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded", ...headers },
      body: form(fields),
    }),
  );
}
export const requestId = (html: string) => /name="request" value="([^"]+)"/.exec(html)![1];

/** Run sign-in and consent; returns the redirect Location. */
export async function approve(
  client: { client_id: string; redirect_uris: string[] },
  challenge: string,
  workspaces = ["worker/ebox-a"],
  profiles: Record<string, string> = {},
  pools: string[] = [],
) {
  const page = await authorize(new Request(authorizeUrl(standardParams(client, challenge)), { headers: { cookie: cookie() } }));
  assert.equal(page.status, 200);
  const html = await page.text();
  assert.match(html, /Example App/);
  const fields: [string, string][] = [["request", requestId(html)], ["action", "approve"]];
  for (const w of workspaces) fields.push(["workspace", w]);
  for (const [w, p] of Object.entries(profiles)) fields.push([`profiles:${w}`, p]);
  for (const p of pools) fields.push(["pool", p]);
  const response = await consentPost(fields);
  assert.equal(response.status, 303);
  return new URL(response.headers.get("location")!);
}
export function tokenRequest(fields: [string, string][], headers: Record<string, string> = {}) {
  return token(
    new Request(`${origin}/oauth/token`, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded", ...headers },
      body: form(fields),
    }),
  ).catch(oauthErrorResponse);
}
export async function exchange(client: { client_id: string; redirect_uris: string[] }, code: string, verifier: string) {
  return tokenRequest([
    ["grant_type", "authorization_code"],
    ["code", code],
    ["redirect_uri", client.redirect_uris[0]],
    ["code_verifier", verifier],
    ["client_id", client.client_id],
    ["resource", origin],
  ]);
}
export async function connect(t: TestContext, workspaces = ["worker/ebox-a"], pools: string[] = []) {
  const controller = fakeController(t);
  const client = await newClient();
  const { verifier, challenge } = pkce();
  const location = await approve(client, challenge, workspaces, {}, pools);
  const response = await exchange(client, location.searchParams.get("code")!, verifier);
  assert.equal(response.status, 200);
  const tokens = (await response.json()) as {
    access_token: string;
    refresh_token: string;
    expires_in: number;
    enginebox_grants: unknown;
    enginebox_callback_secret?: string;
  };
  return { controller, client, tokens, code: location.searchParams.get("code")!, verifier };
}
export function refreshWith(client: { client_id: string }, refreshToken: string) {
  return tokenRequest([
    ["grant_type", "refresh_token"],
    ["refresh_token", refreshToken],
    ["client_id", client.client_id],
  ]);
}
export function call(path: string, accessToken?: string, method = "GET", body?: unknown) {
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

/** Move the dashboard's clock forward (Date.now only; network timeouts keep real time). */
export function advance(t: TestContext, ms: number) {
  const real = Date.now.bind(Date);
  const offset = ms + (Date.now() - real());
  t.mock.method(Date, "now", () => real() + offset);
}
export const stateText = () => readFileSync(join(dir, "oauth.json"), "utf8");

