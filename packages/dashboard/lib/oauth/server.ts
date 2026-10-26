import { createCipheriv, createDecipheriv, hkdfSync, randomBytes, timingSafeEqual } from "node:crypto";
import { config, HttpError, json, requireOrigin, requireSession } from "../server";
import { DEFAULT_PROJECT, GrantRefused, grantablePools, grantableWorkspaces, mintAccessToken, PROJECT, profileNames, revokeAccessToken, type Minted, type Pool, type Workspace } from "./controller";
import { users } from "./identity";
import { consentPage, errorPage, page, signInPage } from "./pages";
import {
  randomId,
  randomToken,
  readState,
  sha256,
  stateDir,
  withState,
  type AuthMethod,
  type Client,
  type Grant,
  type PoolGrant,
  type State,
  type WorkspaceGrant,
} from "./store";

export const SCOPE = "agent-runs";
export const ACCESS_TOKEN_SECONDS = 60 * 60;
export const CODE_SECONDS = 60;
const PENDING_SECONDS = 10 * 60;
export const REFRESH_SECONDS = 30 * 24 * 60 * 60;
/** Default refresh grace window; ENGINEBOX_OAUTH_REFRESH_GRACE_SECONDS sets 0 (off) to 300. */
export const REFRESH_GRACE_SECONDS = 30;
const MAX_CLIENTS = 200;
const MAX_PENDING = 200;
const MAX_GRANT_WORKSPACES = 64;
/** Workspaces and pools together, so the token response stays bounded. */
const MAX_GRANT_TARGETS = 64;
const REFRESH_PREFIX = "ebrt_";
const CODE_PREFIX = "ebac_";
const CLIENT_SECRET_PREFIX = "ebcs_";

export class OAuthError extends Error {
  constructor(
    public error: string,
    public description: string,
    public status = 400,
    public headers: Record<string, string> = {},
  ) {
    super(description);
  }
}

const noStore = { "Cache-Control": "no-store", Pragma: "no-cache", "X-Content-Type-Options": "nosniff" };
function oauthJson(body: unknown, status = 200, headers: Record<string, string> = {}) {
  return Response.json(body, { status, headers: { ...noStore, ...headers } });
}
export function oauthErrorResponse(error: unknown) {
  if (error instanceof OAuthError)
    return oauthJson({ error: error.error, error_description: error.description }, error.status, error.headers);
  if (error instanceof HttpError) {
    if (error.status === 404) return oauthJson({ error: "not_found" }, 404);
    return oauthJson({ error: error.status >= 500 ? "temporarily_unavailable" : "invalid_request", error_description: error.message }, error.status);
  }
  return oauthJson({ error: "server_error", error_description: "The request could not be completed." }, 500);
}

/** OAuth is off unless a private state directory is configured. */
function requireEnabled() {
  if (!stateDir()) throw new HttpError(404, "Not found.");
  return config().origin;
}
const issuer = () => config().origin;
/** The protected resource's identifier: the public dashboard origin. */
const resourceId = () => config().origin;
export const resourceMetadataUrl = () => `${config().origin}/.well-known/oauth-protected-resource`;

// ---------------------------------------------------------------- discovery

export function protectedResourceMetadata() {
  const origin = requireEnabled();
  return oauthJson({
    resource: resourceId(),
    authorization_servers: [origin],
    scopes_supported: [SCOPE],
    bearer_methods_supported: ["header"],
    resource_name: "Engine Box agent runs",
  });
}

export function authorizationServerMetadata() {
  const origin = requireEnabled();
  const auth = ["none", "client_secret_basic", "client_secret_post"];
  return oauthJson({
    issuer: origin,
    authorization_endpoint: `${origin}/oauth/authorize`,
    token_endpoint: `${origin}/oauth/token`,
    registration_endpoint: `${origin}/oauth/register`,
    revocation_endpoint: `${origin}/oauth/revoke`,
    scopes_supported: [SCOPE],
    response_types_supported: ["code"],
    response_modes_supported: ["query"],
    grant_types_supported: ["authorization_code", "refresh_token"],
    code_challenge_methods_supported: ["S256"],
    token_endpoint_auth_methods_supported: auth,
    revocation_endpoint_auth_methods_supported: auth,
    authorization_response_iss_parameter_supported: true,
  });
}

// ---------------------------------------------------------------- bodies

async function readText(request: Request, maxBytes: number) {
  if (Number(request.headers.get("content-length") ?? 0) > maxBytes) throw new OAuthError("invalid_request", "Request is too large.", 413);
  const reader = request.body?.getReader();
  if (!reader) return "";
  const chunks: Uint8Array[] = [];
  let bytes = 0;
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    bytes += value.byteLength;
    if (bytes > maxBytes) {
      await reader.cancel();
      throw new OAuthError("invalid_request", "Request is too large.", 413);
    }
    chunks.push(value);
  }
  return Buffer.concat(chunks).toString("utf8");
}
async function readForm(request: Request, { repeats = false } = {}) {
  if (!request.headers.get("content-type")?.toLowerCase().startsWith("application/x-www-form-urlencoded"))
    throw new OAuthError("invalid_request", "Send application/x-www-form-urlencoded.");
  const form = new URLSearchParams(await readText(request, 16384));
  if (!repeats) {
    const keys = [...form.keys()];
    if (new Set(keys).size !== keys.length) throw new OAuthError("invalid_request", "A parameter was repeated.");
  }
  return form;
}

// ---------------------------------------------------------------- registration (RFC 7591)

const LOOPBACK = new Set(["localhost", "127.0.0.1", "[::1]"]);
export function validRedirectUri(uri: unknown): uri is string {
  if (typeof uri !== "string" || uri.length > 512 || uri.includes("#") || /[\s\u0000-\u001f]/.test(uri)) return false;
  let url: URL;
  try {
    url = new URL(uri);
  } catch {
    return false;
  }
  if (url.username || url.password || !url.hostname) return false;
  if (url.protocol === "https:") return true;
  return url.protocol === "http:" && LOOPBACK.has(url.hostname);
}
const printable = (value: string) => !/[\u0000-\u001f\u007f]/.test(value);

let registrations: number[] = [];
export async function register(request: Request) {
  requireEnabled();
  if (!request.headers.get("content-type")?.toLowerCase().startsWith("application/json"))
    throw new OAuthError("invalid_client_metadata", "Send application/json.");
  let body: Record<string, unknown>;
  try {
    body = JSON.parse(await readText(request, 8192));
  } catch (error) {
    if (error instanceof OAuthError) throw error;
    throw new OAuthError("invalid_client_metadata", "Body must be a JSON object.");
  }
  if (!body || typeof body !== "object" || Array.isArray(body)) throw new OAuthError("invalid_client_metadata", "Body must be a JSON object.");
  const redirects = body.redirect_uris;
  if (!Array.isArray(redirects) || redirects.length < 1 || redirects.length > 10)
    throw new OAuthError("invalid_redirect_uri", "redirect_uris must list 1-10 URIs.");
  for (const uri of redirects)
    if (!validRedirectUri(uri))
      throw new OAuthError("invalid_redirect_uri", "Each redirect URI must be an absolute https URI (http only for localhost, 127.0.0.1 or [::1]) without a fragment or credentials.");
  const method = (body.token_endpoint_auth_method ?? "client_secret_basic") as AuthMethod;
  if (!["none", "client_secret_basic", "client_secret_post"].includes(method))
    throw new OAuthError("invalid_client_metadata", "Unsupported token_endpoint_auth_method.");
  const grantTypes = body.grant_types ?? ["authorization_code"];
  if (!Array.isArray(grantTypes) || !grantTypes.includes("authorization_code") || grantTypes.some((g) => g !== "authorization_code" && g !== "refresh_token"))
    throw new OAuthError("invalid_client_metadata", "grant_types must include authorization_code and may add refresh_token.");
  const finalGrantTypes = [...new Set<string>([...(grantTypes as string[]), "refresh_token"])];
  const responseTypes = body.response_types ?? ["code"];
  if (!Array.isArray(responseTypes) || responseTypes.length !== 1 || responseTypes[0] !== "code")
    throw new OAuthError("invalid_client_metadata", "response_types must be [\"code\"].");
  if (body.scope !== undefined && (typeof body.scope !== "string" || body.scope.split(" ").some((s) => s !== SCOPE)))
    throw new OAuthError("invalid_client_metadata", `The only scope is ${SCOPE}.`);
  const name = body.client_name === undefined ? "Unnamed app" : body.client_name;
  if (typeof name !== "string" || !name.trim() || name.length > 100 || !printable(name))
    throw new OAuthError("invalid_client_metadata", "client_name must be 1-100 printable characters.");

  const now = Date.now();
  registrations = registrations.filter((t) => t > now - 10 * 60 * 1000);
  if (registrations.length >= 30) throw new OAuthError("temporarily_unavailable", "Too many registrations. Try again later.", 429);
  registrations.push(now);

  const secret = method === "none" ? null : `${CLIENT_SECRET_PREFIX}${randomToken()}`;
  const client: Client = {
    client_id: `ebc_${randomId()}`,
    secret_hash: secret ? sha256(secret) : null,
    client_name: name.trim(),
    redirect_uris: redirects as string[],
    token_endpoint_auth_method: method,
    grant_types: finalGrantTypes,
    created_at: now,
  };
  withState((state) => {
    if (state.clients.length >= MAX_CLIENTS) {
      // Make room by dropping the oldest clients that never obtained a live grant.
      const live = new Set(state.grants.filter((g) => g.revoked_at === null).map((g) => g.client_id));
      const idle = state.clients.filter((c) => !live.has(c.client_id) && c.created_at < now - 24 * 60 * 60 * 1000).sort((a, b) => a.created_at - b.created_at);
      const drop = new Set(idle.slice(0, state.clients.length - MAX_CLIENTS + 1).map((c) => c.client_id));
      state.clients = state.clients.filter((c) => !drop.has(c.client_id));
      if (state.clients.length >= MAX_CLIENTS) throw new OAuthError("temporarily_unavailable", "Client registration is full.", 503);
    }
    state.clients.push(client);
  });
  return oauthJson(
    {
      client_id: client.client_id,
      ...(secret ? { client_secret: secret, client_secret_expires_at: 0 } : {}),
      client_id_issued_at: Math.floor(now / 1000),
      client_name: client.client_name,
      redirect_uris: client.redirect_uris,
      token_endpoint_auth_method: method,
      grant_types: client.grant_types,
      response_types: ["code"],
      scope: SCOPE,
    },
    201,
  );
}

// ---------------------------------------------------------------- authorization endpoint

function redirectTo(redirectUri: string, params: Record<string, string | null>, status = 302) {
  const url = new URL(redirectUri);
  for (const [key, value] of Object.entries(params)) if (value !== null) url.searchParams.set(key, value);
  url.searchParams.set("iss", issuer());
  return new Response(null, { status, headers: { Location: url.href, "Cache-Control": "no-store", "Referrer-Policy": "no-referrer" } });
}

const sameResource = (value: string) => value === resourceId() || value === `${resourceId()}/`;

function render(
  request: Request,
  pendingId: string,
  client: Client,
  redirectUri: string,
  extra: { error?: string; setCookie?: string; project?: string | null } = {},
) {
  return (async () => {
    const listed = await grantable(extra.project);
    if (!listed) return errorPage("Engine Box could not list your workspaces or pools. Check that the fleet service is running.", 503);
    const response = consentPage({
      request: pendingId,
      clientName: client.client_name,
      clientId: client.client_id,
      redirectUri,
      workspaces: listed.workspaces,
      pools: listed.pools,
      project: extra.project,
      error: extra.error,
    });
    if (extra.setCookie) response.headers.append("Set-Cookie", extra.setCookie);
    return response;
  })();
}

/**
 * What the owner can grant. Either list may fail on its own (a host whose
 * inventory cannot be read still leaves its pools grantable); null when both do.
 */
async function grantable(project?: string | null): Promise<{ workspaces: Workspace[]; pools: Pool[] } | null> {
  const [workspaces, pools] = await Promise.allSettled([grantableWorkspaces(), grantablePools()]);
  if (workspaces.status === "rejected" && pools.status === "rejected") return null;
  const listed = pools.status === "fulfilled" ? pools.value : [];
  return {
    workspaces: workspaces.status === "fulfilled" ? workspaces.value : [],
    // An app that asked for one project's pools is offered only those.
    pools: project ? listed.filter((p) => p.project === project) : listed,
  };
}

function findPending(state: State, id: string | null) {
  if (!id || id.length > 100) return null;
  const pending = state.pending.find((p) => p.id_hash === sha256(id) && p.expires_at > Date.now());
  const client = pending && state.clients.find((c) => c.client_id === pending.client_id);
  return pending && client ? { pending, client } : null;
}

export async function authorize(request: Request) {
  requireEnabled();
  const url = new URL(request.url);
  const params = url.searchParams;
  const resumeId = params.get("request");
  if (resumeId !== null) {
    const found = findPending(readState(), resumeId);
    if (!found) return errorPage("This authorization request has expired or was already used.");
    if (users().current(request)) return render(request, resumeId, found.client, found.pending.redirect_uri, { project: found.pending.project });
    return signInPage(users(), resumeId, found.client.client_name);
  }
  const keys = [...params.keys()];
  if (new Set(keys).size !== keys.length && (params.getAll("client_id").length > 1 || params.getAll("redirect_uri").length > 1))
    return errorPage("The request repeated client_id or redirect_uri.");
  const clientId = params.get("client_id") ?? "";
  const redirectUri = params.get("redirect_uri") ?? "";
  const state0 = readState();
  const client = state0.clients.find((c) => c.client_id === clientId);
  // Never redirect anywhere until the client and its exact redirect URI are known.
  if (!client) return errorPage("The app is not registered with this Engine Box.");
  if (!client.redirect_uris.includes(redirectUri)) return errorPage("The redirect URI does not match one the app registered.");
  const state = params.get("state");
  const fail = (error: string, description: string) => redirectTo(redirectUri, { error, error_description: description, state });
  if (new Set(keys).size !== keys.length) return fail("invalid_request", "A parameter was repeated.");
  if (state !== null && (state.length > 2048 || !printable(state))) return fail("invalid_request", "state is too long or not printable.");
  if (params.get("response_type") !== "code") return fail("unsupported_response_type", "Only response_type=code is supported.");
  const challenge = params.get("code_challenge") ?? "";
  if (params.get("code_challenge_method") !== "S256" || !/^[A-Za-z0-9_-]{43}$/.test(challenge))
    return fail("invalid_request", "PKCE is required: send a code_challenge with code_challenge_method=S256.");
  const scope = params.get("scope") ?? SCOPE;
  if (scope.split(" ").some((s) => s !== SCOPE)) return fail("invalid_scope", `The only scope is ${SCOPE}.`);
  const resource = params.get("resource") ?? "";
  if (!sameResource(resource)) return fail("invalid_target", `resource must be ${resourceId()}.`);
  // Optional: the app asks for one project's pools; the grant is then restricted to that project.
  const project = params.get("project");
  if (project !== null && !PROJECT.test(project)) return fail("invalid_request", "project must be a project ID.");
  const id = randomToken(24);
  withState((s, now) => {
    if (s.pending.length >= MAX_PENDING) s.pending.splice(0, s.pending.length - MAX_PENDING + 1);
    s.pending.push({
      id_hash: sha256(id),
      client_id: client.client_id,
      redirect_uri: redirectUri,
      state,
      code_challenge: challenge,
      scope: SCOPE,
      resource: resourceId(),
      expires_at: now + PENDING_SECONDS * 1000,
      project,
    });
  });
  if (users().current(request)) return render(request, id, client, redirectUri, { project });
  // The session cookie is SameSite=Strict, so it is not sent on the app's
  // cross-site redirect here. One same-origin navigation reveals whether the
  // owner is already signed in before asking for a password.
  return page("Continue to Engine Box", `<h1>Continuing to Engine Box…</h1><p><a href="/oauth/authorize?request=${id}">Continue</a></p>`, {
    refresh: `/oauth/authorize?request=${id}`,
  });
}

/** Same-origin form posts: an exact Origin, or (when a browser withholds it) Fetch metadata. */
function requireSameOriginForm(request: Request) {
  const origin = request.headers.get("origin");
  if (origin && origin !== "null") return requireOrigin(request);
  if (request.headers.get("sec-fetch-site") !== "same-origin") throw new HttpError(403, "Request origin is not allowed.");
}

const PROFILE = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;

export async function authorizePost(request: Request) {
  requireEnabled();
  try {
    requireSameOriginForm(request);
  } catch {
    return errorPage("This form must be submitted from Engine Box.", 403);
  }
  let form: URLSearchParams;
  try {
    form = await readForm(request, { repeats: true });
  } catch {
    return errorPage("The form could not be read.");
  }
  const id = form.get("request");
  const found = findPending(readState(), id);
  if (!found || !id) return errorPage("This authorization request has expired or was already used.");
  const { pending, client } = found;
  const project = pending.project ?? null;
  const action = form.get("action");
  if (action === "sign_in") {
    try {
      const { setCookie } = users().signIn(form);
      return render(request, id, client, pending.redirect_uri, { setCookie, project });
    } catch (error) {
      return signInPage(users(), id, client.client_name, error instanceof HttpError && error.status < 500 ? error.message : "Sign-in failed.");
    }
  }
  const user = users().current(request);
  if (!user) return signInPage(users(), id, client.client_name, "Sign in again to continue.");
  const consume = () =>
    withState((state) => {
      const index = state.pending.findIndex((p) => p.id_hash === pending.id_hash);
      if (index < 0) return false;
      state.pending.splice(index, 1);
      return true;
    });
  if (action === "deny") {
    if (!consume()) return errorPage("This authorization request has expired or was already used.");
    return redirectTo(pending.redirect_uri, { error: "access_denied", error_description: "The user denied access.", state: pending.state }, 303);
  }
  if (action !== "approve") return errorPage("Unknown action.");

  const chosen = [...new Set(form.getAll("workspace"))];
  const chosenPools = [...new Set(form.getAll("pool"))];
  if (!chosen.length && !chosenPools.length) return render(request, id, client, pending.redirect_uri, { project, error: "Select at least one workspace or pool." });
  if (chosen.length > MAX_GRANT_WORKSPACES || chosen.length + chosenPools.length > MAX_GRANT_TARGETS)
    return render(request, id, client, pending.redirect_uri, { project, error: "Select fewer workspaces and pools." });
  const available = await grantable(project);
  if (!available) return errorPage("Engine Box could not list your workspaces or pools. Check that the fleet service is running.", 503);
  const workspaces: WorkspaceGrant[] = [];
  for (const value of chosen) {
    const workspace = available.workspaces.find((w) => `${w.host}/${w.instance}` === value);
    if (!workspace) return render(request, id, client, pending.redirect_uri, { project, error: "A selected workspace is no longer available." });
    const profiles = [...new Set((form.get(`profiles:${value}`) ?? "").split(",").map((p) => p.trim()).filter(Boolean))];
    if (profiles.length > 16 || profiles.some((p) => !PROFILE.test(p)))
      return render(request, id, client, pending.redirect_uri, { project, error: "Profile names use letters, digits, dot, dash or underscore (at most 16 per workspace)." });
    workspaces.push({ host: workspace.host, instance: workspace.instance, profiles });
  }
  const pools: PoolGrant[] = [];
  for (const name of chosenPools) {
    const pool = available.pools.find((p) => p.name === name);
    if (!pool) return render(request, id, client, pending.redirect_uri, { project, error: "A selected pool is no longer available." });
    pools.push({ pool: pool.name, profile: pool.profile, project: pool.project });
  }
  // The controller refuses to mint a token naming an unknown profile; say so here instead.
  const named = [...new Set(workspaces.flatMap((w) => w.profiles))];
  if (named.length) {
    let known: string[];
    try {
      known = await profileNames();
    } catch {
      return errorPage("Engine Box could not list your profiles. Check that the fleet service is running.", 503);
    }
    const unknown = named.filter((p) => !known.includes(p));
    if (unknown.length) return render(request, id, client, pending.redirect_uri, { project, error: `Unknown profile: ${unknown.join(", ")}.` });
  }
  const code = `${CODE_PREFIX}${randomToken()}`;
  const ok = withState((state, now) => {
    const index = state.pending.findIndex((p) => p.id_hash === pending.id_hash);
    if (index < 0) return false;
    state.pending.splice(index, 1);
    state.codes.push({
      hash: sha256(code),
      client_id: client.client_id,
      redirect_uri: pending.redirect_uri,
      code_challenge: pending.code_challenge,
      scope: pending.scope,
      resource: pending.resource,
      user_id: user.id,
      workspaces,
      pools,
      project,
      expires_at: now + CODE_SECONDS * 1000,
      used_at: null,
      grant_id: null,
      replayed: false,
    });
    return true;
  });
  if (!ok) return errorPage("This authorization request has expired or was already used.");
  return redirectTo(pending.redirect_uri, { code, state: pending.state }, 303);
}

// ---------------------------------------------------------------- client authentication

function equalHash(a: string, b: string) {
  return a.length === b.length && timingSafeEqual(Buffer.from(a), Buffer.from(b));
}
function authenticateClient(request: Request, form: URLSearchParams, state: State): Client {
  const header = request.headers.get("authorization");
  let id = form.get("client_id");
  let secret = form.get("client_secret");
  let basic = false;
  const invalid = (description: string) =>
    new OAuthError("invalid_client", description, 401, basic ? { "WWW-Authenticate": 'Basic realm="enginebox"' } : {});
  if (header) {
    const match = /^Basic ([A-Za-z0-9+/=]+)$/i.exec(header);
    if (!match) throw invalid("Unsupported client authentication.");
    basic = true;
    const decoded = Buffer.from(match[1], "base64").toString("utf8");
    const colon = decoded.indexOf(":");
    if (colon < 0 || secret !== null) throw invalid("Malformed client credentials.");
    const decode = (v: string) => decodeURIComponent(v.replace(/\+/g, " "));
    let basicId: string;
    try {
      basicId = decode(decoded.slice(0, colon));
      secret = decode(decoded.slice(colon + 1));
    } catch {
      throw invalid("Malformed client credentials.");
    }
    if (id !== null && id !== basicId) throw invalid("client_id does not match the credentials.");
    id = basicId;
  }
  const client = id ? state.clients.find((c) => c.client_id === id) : undefined;
  if (!client) throw invalid("Unknown client.");
  const method = client.token_endpoint_auth_method;
  if (method === "none") {
    if (secret !== null || basic) throw invalid("This public client has no secret.");
    return client;
  }
  if ((method === "client_secret_basic") !== basic || !secret || !client.secret_hash || !equalHash(sha256(secret), client.secret_hash))
    throw invalid("Client authentication failed.");
  return client;
}

// ---------------------------------------------------------------- token endpoint

/**
 * The `enginebox_grants` extension: one entry per consented workspace
 * (`{kind: "workspace", host, instance, profiles}`, the original shape plus
 * `kind`) and per pool (`{kind: "pool", pool, profile, profiles}`, where
 * `profile` is the pool's own profile and `profiles` repeats it as a list, and
 * `project` is the project the pool belonged to at consent).
 */
export function grantsView(grant: Pick<Grant, "workspaces" | "pools">) {
  return [
    ...grant.workspaces.map((w) => ({ kind: "workspace" as const, host: w.host, instance: w.instance, profiles: w.profiles })),
    ...(grant.pools ?? []).map((p) => ({
      kind: "pool" as const,
      pool: p.pool,
      project: p.project ?? DEFAULT_PROJECT,
      profile: p.profile,
      profiles: p.profile ? [p.profile] : [],
    })),
  ];
}
function tokenResponse(minted: Minted, refresh: string, grant: Pick<Grant, "workspaces" | "pools">, callbackSecret?: string) {
  return oauthJson({
    access_token: minted.secret,
    token_type: "Bearer",
    expires_in: Math.max(1, Math.floor((minted.expires_at - Date.now()) / 1000)),
    refresh_token: refresh,
    scope: SCOPE,
    enginebox_grants: grantsView(grant),
    // Only at the code exchange: the key that signs this connection's run callbacks.
    ...(callbackSecret ? { enginebox_callback_secret: callbackSecret } : {}),
  });
}

/** Mark a grant revoked inside a state transaction; its access token joins the revocation queue. */
function revokeGrantIn(state: State, grant: Grant, reason: string, now: number) {
  if (grant.revoked_at === null) {
    grant.revoked_at = now;
    grant.revoke_reason = reason;
  }
  grant.refresh_hash = null;
  if (grant.access_token_id) state.pending_revocations.push(grant.access_token_id);
  grant.access_token_id = null;
  grant.access_token_hash = null;
  grant.grace_pair = null;
  grant.previous_refresh_hash = null;
}

// ---------------------------------------------------------------- refresh grace window

/**
 * The grace window in seconds: ENGINEBOX_OAUTH_REFRESH_GRACE_SECONDS (0 turns it
 * off, at most 300), else REFRESH_GRACE_SECONDS.
 */
export function refreshGraceSeconds() {
  const raw = process.env.ENGINEBOX_OAUTH_REFRESH_GRACE_SECONDS;
  if (raw === undefined || raw === "") return REFRESH_GRACE_SECONDS;
  const n = Number(raw);
  if (!Number.isInteger(n) || n < 0 || n > 300) throw new HttpError(503, "ENGINEBOX_OAUTH_REFRESH_GRACE_SECONDS must be an integer from 0 to 300.");
  return n;
}
type GracePair = { access_token: string; access_token_id: string; access_expires_at: number; refresh_token: string };
/** AES-256-GCM key for grace pairs, derived from the dashboard's session secret (never stored). */
function graceKey() {
  return Buffer.from(hkdfSync("sha256", config().secret, "enginebox-oauth", "refresh-grace-v1", 32));
}
function sealGrace(grantId: string, previousHash: string, pair: GracePair) {
  const iv = randomBytes(12);
  const cipher = createCipheriv("aes-256-gcm", graceKey(), iv);
  cipher.setAAD(Buffer.from(`${grantId}:${previousHash}`));
  const sealed = Buffer.concat([cipher.update(JSON.stringify(pair), "utf8"), cipher.final()]);
  return `v1.${Buffer.concat([iv, cipher.getAuthTag(), sealed]).toString("base64url")}`;
}
function openGrace(grantId: string, previousHash: string, blob: string): GracePair | null {
  try {
    if (!blob.startsWith("v1.")) return null;
    const raw = Buffer.from(blob.slice(3), "base64url");
    const decipher = createDecipheriv("aes-256-gcm", graceKey(), raw.subarray(0, 12));
    decipher.setAAD(Buffer.from(`${grantId}:${previousHash}`));
    decipher.setAuthTag(raw.subarray(12, 28));
    const pair = JSON.parse(Buffer.concat([decipher.update(raw.subarray(28)), decipher.final()]).toString("utf8")) as GracePair;
    return typeof pair.access_token === "string" && typeof pair.refresh_token === "string" ? pair : null;
  } catch {
    // A rotated session secret or a damaged record: the pair is unrecoverable.
    return null;
  }
}
/**
 * The pair issued for `hash` when it is the grant's immediately-previous refresh
 * token and its grace window is open; "closed" when it is not; "lost" when it is
 * but the stored pair cannot be decrypted.
 */
function graceReplay(grant: Grant, hash: string, now: number): GracePair | "closed" | "lost" {
  if (grant.revoked_at !== null || !grant.previous_refresh_hash || !grant.grace_pair || !((grant.grace_until ?? 0) > now)) return "closed";
  if (!equalHash(grant.previous_refresh_hash, hash)) return "closed";
  return openGrace(grant.id, hash, grant.grace_pair) ?? "lost";
}
function graceResponse(pair: GracePair, grant: Pick<Grant, "workspaces" | "pools">) {
  return tokenResponse({ id: pair.access_token_id, secret: pair.access_token, expires_at: pair.access_expires_at }, pair.refresh_token, grant);
}

/** Revoke queued controller tokens; failures stay queued and are retried on the next call. */
export async function flushRevocations() {
  const ids = [...new Set(readState().pending_revocations)];
  const done: string[] = [];
  for (const id of ids) if (await revokeAccessToken(id)) done.push(id);
  if (ids.length && done.length < ids.length) console.warn(JSON.stringify({ event: "oauth_revocation_retry", pending: ids.length - done.length }));
  if (done.length) withState((state) => (state.pending_revocations = state.pending_revocations.filter((id) => !done.includes(id))));
}

function parseRefresh(token: string | null) {
  const match = token ? /^ebrt_([a-f0-9]{32})\.([A-Za-z0-9_-]{43})$/.exec(token) : null;
  return match ? { grantId: match[1], hash: sha256(token!) } : null;
}

export async function token(request: Request) {
  requireEnabled();
  const form = await readForm(request);
  const grantType = form.get("grant_type");
  const resource = form.get("resource");
  if (resource !== null && !sameResource(resource)) throw new OAuthError("invalid_target", `resource must be ${resourceId()}.`);
  const scope = form.get("scope");
  if (scope !== null && scope.split(" ").some((s) => s !== SCOPE)) throw new OAuthError("invalid_scope", `The only scope is ${SCOPE}.`);
  try {
    if (grantType === "authorization_code") return await exchangeCode(request, form);
    if (grantType === "refresh_token") return await refresh(request, form);
    throw new OAuthError("unsupported_grant_type", "Use authorization_code or refresh_token.");
  } finally {
    await flushRevocations().catch(() => {});
  }
}

async function exchangeCode(request: Request, form: URLSearchParams) {
  const code = form.get("code") ?? "";
  const verifier = form.get("code_verifier") ?? "";
  const redirectUri = form.get("redirect_uri");
  const grantId = randomId();
  const claimed = withState((state, now) => {
    const client = authenticateClient(request, form, state);
    const record = state.codes.find((c) => c.hash === sha256(code));
    if (!record || record.client_id !== client.client_id) throw new OAuthError("invalid_grant", "The authorization code is invalid.");
    if (record.used_at !== null) {
      // A replayed code revokes everything it produced (RFC 6749 §4.1.2).
      record.replayed = true;
      const grant = state.grants.find((g) => g.id === record.grant_id);
      if (grant) revokeGrantIn(state, grant, "authorization code replayed", now);
      return { replay: true as const };
    }
    // Any presentation burns the code, so a failed attempt cannot be retried.
    record.used_at = now;
    if (record.expires_at <= now) return { failure: "The authorization code has expired." };
    if (redirectUri !== record.redirect_uri) return { failure: "redirect_uri does not match the authorization request." };
    if (!/^[A-Za-z0-9._~-]{43,128}$/.test(verifier) || sha256b64(verifier) !== record.code_challenge)
      return { failure: "PKCE verification failed." };
    record.grant_id = grantId;
    return { replay: false as const, record: structuredClone(record), client };
  });
  if ("failure" in claimed) throw new OAuthError("invalid_grant", claimed.failure!);
  if (claimed.replay) throw new OAuthError("invalid_grant", "The authorization code was already used; its tokens are revoked.");
  const { record, client } = claimed;
  const pools = record.pools ?? [];
  const callbackSecret = randomToken(32);
  let minted: Minted;
  try {
    minted = await mintAccessToken(grantId, { workspaces: record.workspaces, pools, callback_secret: callbackSecret, project: record.project }, ACCESS_TOKEN_SECONDS);
  } catch (error) {
    if (error instanceof GrantRefused) throw new OAuthError("invalid_grant", error.message);
    throw error;
  }
  const refreshToken = `${REFRESH_PREFIX}${grantId}.${randomToken()}`;
  const stored = withState((state, now) => {
    const current = state.codes.find((c) => c.hash === record.hash);
    if (!current || current.replayed) {
      state.pending_revocations.push(minted.id);
      return false;
    }
    state.grants.push({
      id: grantId,
      user_id: record.user_id,
      client_id: client.client_id,
      scope: record.scope,
      resource: record.resource,
      workspaces: record.workspaces,
      pools,
      callback_secret: callbackSecret,
      project: record.project ?? null,
      created_at: now,
      refreshed_at: now,
      revoked_at: null,
      revoke_reason: null,
      refresh_hash: sha256(refreshToken),
      refresh_expires_at: now + REFRESH_SECONDS * 1000,
      used_refresh_hashes: [],
      access_token_id: minted.id,
      access_token_hash: sha256(minted.secret),
      access_expires_at: minted.expires_at,
    });
    return true;
  });
  if (!stored) throw new OAuthError("invalid_grant", "The authorization code was already used; its tokens are revoked.");
  return tokenResponse(minted, refreshToken, { workspaces: record.workspaces, pools }, callbackSecret);
}

function sha256b64(value: string) {
  return Buffer.from(sha256(value), "hex").toString("base64url");
}

async function refresh(request: Request, form: URLSearchParams) {
  const parsed = parseRefresh(form.get("refresh_token"));
  const checked = withState((state, now) => {
    const client = authenticateClient(request, form, state);
    if (!client.grant_types.includes("refresh_token")) throw new OAuthError("unauthorized_client", "This client may not refresh.");
    const grant = parsed && state.grants.find((g) => g.id === parsed.grantId);
    if (!parsed || !grant || grant.client_id !== client.client_id) throw new OAuthError("invalid_grant", "The refresh token is invalid.");
    if (grant.used_refresh_hashes.includes(parsed.hash)) {
      // The immediately-previous token, within its grace window, from the same
      // client (checked above): the response to that refresh was probably lost.
      // Answer with the same pair instead of treating it as theft.
      const replay = graceReplay(grant, parsed.hash, now);
      if (replay === "lost") throw new OAuthError("invalid_grant", "The refresh token was already used.");
      if (replay !== "closed") return { reuse: false as const, replay, grant: structuredClone(grant) };
      // Reuse of a rotated refresh token: assume theft and end the whole family.
      revokeGrantIn(state, grant, "refresh token reused", now);
      return { reuse: true as const };
    }
    if (grant.revoked_at !== null || grant.refresh_hash !== parsed.hash) throw new OAuthError("invalid_grant", "The refresh token is invalid.");
    return { reuse: false as const, replay: null, grant: structuredClone(grant) };
  });
  if (checked.reuse) throw new OAuthError("invalid_grant", "The refresh token was already used; this connection is revoked.");
  if (checked.replay) return graceResponse(checked.replay, checked.grant);
  const grant = checked.grant;
  const graceSeconds = refreshGraceSeconds();
  let minted: Minted;
  try {
    minted = await mintAccessToken(
      grant.id,
      { workspaces: grant.workspaces, pools: grant.pools ?? [], callback_secret: grant.callback_secret, project: grant.project },
      ACCESS_TOKEN_SECONDS,
    );
  } catch (error) {
    if (!(error instanceof GrantRefused)) throw error;
    // What the owner consented to is gone: end the connection so the app asks again.
    withState((state, now) => {
      const current = state.grants.find((g) => g.id === grant.id);
      if (current) revokeGrantIn(state, current, "consented workspaces or pools no longer available", now);
    });
    throw new OAuthError("invalid_grant", error.message);
  }
  const next = `${REFRESH_PREFIX}${grant.id}.${randomToken()}`;
  const outcome = withState((state, now) => {
    const current = state.grants.find((g) => g.id === grant.id);
    if (!current || current.revoked_at !== null) {
      state.pending_revocations.push(minted.id);
      return "revoked";
    }
    if (current.refresh_hash !== parsed!.hash) {
      state.pending_revocations.push(minted.id);
      // A concurrent refresh with the same token already rotated it: within the
      // grace window, answer with the pair that refresh issued.
      const replay = graceReplay(current, parsed!.hash, now);
      if (replay !== "closed" && replay !== "lost") return replay;
      if (replay === "lost") return "lost";
      revokeGrantIn(state, current, "refresh token reused", now);
      return "reused";
    }
    if (current.access_token_id) state.pending_revocations.push(current.access_token_id);
    current.used_refresh_hashes = [...current.used_refresh_hashes, parsed!.hash].slice(-100);
    if (graceSeconds > 0) {
      current.previous_refresh_hash = parsed!.hash;
      current.grace_until = now + graceSeconds * 1000;
      current.grace_pair = sealGrace(current.id, parsed!.hash, {
        access_token: minted.secret,
        access_token_id: minted.id,
        access_expires_at: minted.expires_at,
        refresh_token: next,
      });
    } else {
      current.previous_refresh_hash = null;
      current.grace_pair = null;
    }
    current.refresh_hash = sha256(next);
    current.refresh_expires_at = now + REFRESH_SECONDS * 1000;
    current.refreshed_at = now;
    current.access_token_id = minted.id;
    current.access_token_hash = sha256(minted.secret);
    current.access_expires_at = minted.expires_at;
    return "ok";
  });
  if (outcome === "reused") throw new OAuthError("invalid_grant", "The refresh token was already used; this connection is revoked.");
  if (outcome === "revoked") throw new OAuthError("invalid_grant", "The refresh token is invalid.");
  if (outcome === "lost") throw new OAuthError("invalid_grant", "The refresh token was already used.");
  if (outcome !== "ok") return graceResponse(outcome, grant);
  return tokenResponse(minted, next, grant);
}

// ---------------------------------------------------------------- revocation (RFC 7009)

export async function revoke(request: Request) {
  requireEnabled();
  const form = await readForm(request);
  const presented = form.get("token");
  if (!presented) throw new OAuthError("invalid_request", "token is required.");
  withState((state, now) => {
    const client = authenticateClient(request, form, state);
    const refresh = parseRefresh(presented);
    if (refresh) {
      const grant = state.grants.find((g) => g.id === refresh.grantId && g.client_id === client.client_id);
      if (grant && (grant.refresh_hash === refresh.hash || grant.used_refresh_hashes.includes(refresh.hash)))
        revokeGrantIn(state, grant, "revoked by client", now);
      return;
    }
    const hash = sha256(presented);
    const grant = state.grants.find((g) => g.client_id === client.client_id && g.access_token_hash === hash);
    if (grant?.access_token_id) {
      state.pending_revocations.push(grant.access_token_id);
      grant.access_token_id = null;
      grant.access_token_hash = null;
    }
  });
  await flushRevocations().catch(() => {});
  // Unknown or already-revoked tokens also answer 200 (RFC 7009 §2.2).
  return oauthJson({}, 200);
}

// ---------------------------------------------------------------- the protected resource

const SEGMENT = "[A-Za-z0-9][A-Za-z0-9_-]{0,99}";
/** The controller's pool names. */
const POOL = "[a-z][a-z0-9-]{0,31}";
export const RESOURCE_ROUTES: [string, RegExp][] = [
  ["GET", /^v1\/agent-runs$/],
  ["GET", new RegExp(`^v1/hosts/${SEGMENT}/instances/${SEGMENT}/agent-runs/${SEGMENT}$`)],
  ["POST", new RegExp(`^v1/hosts/${SEGMENT}/instances/${SEGMENT}/agent-runs$`)],
  ["POST", new RegExp(`^v1/hosts/${SEGMENT}/instances/${SEGMENT}/agent-runs/${SEGMENT}/cancel$`)],
  ["POST", new RegExp(`^v1/pools/${POOL}/agent-runs$`)],
  ["GET", new RegExp(`^v1/pools/${POOL}/agent-runs/${SEGMENT}$`)],
  ["POST", new RegExp(`^v1/pools/${POOL}/agent-runs/${SEGMENT}/cancel$`)],
];

function challenge(error?: string, description?: string) {
  const parts = [`resource_metadata="${resourceMetadataUrl()}"`];
  if (error) parts.unshift(`error="${error}"`);
  if (description) parts.push(`error_description="${description}"`);
  if (error === "insufficient_scope") parts.push(`scope="${SCOPE}"`);
  return `Bearer ${parts.join(", ")}`;
}
function unauthorized(error?: string, description?: string) {
  return json({ error: description ?? "Authentication required." }, 401, { "WWW-Authenticate": challenge(error, description) });
}

/**
 * Agent-run routes on the public origin. The bearer must be the current access
 * token of a live grant; the controller then enforces the token's workspace and
 * operation scope. Cookies are ignored here, so this path cannot be driven by
 * the dashboard session.
 */
export async function resourceProxy(request: Request, segments: string[]) {
  requireEnabled();
  const path = segments.join("/");
  if (
    segments.some((s) => s.includes("/") || s.includes("..")) ||
    !RESOURCE_ROUTES.some(([method, pattern]) => method === request.method && pattern.test(path))
  )
    return json({ error: "Unknown operation." }, 404);
  const header = request.headers.get("authorization") ?? "";
  const match = /^Bearer (ebst_([a-f0-9]{32})\.[A-Za-z0-9_-]{1,128})$/.exec(header);
  if (!header) return unauthorized();
  if (!match) return unauthorized("invalid_token", "The access token is malformed.");
  const [, accessToken, tokenId] = match;
  const now = Date.now();
  const live = readState().grants.some(
    (g) => g.revoked_at === null && g.access_token_id === tokenId && g.access_token_hash !== null && equalHash(g.access_token_hash, sha256(accessToken)) && g.access_expires_at > now,
  );
  if (!live) return unauthorized("invalid_token", "The access token is unknown, expired or revoked.");
  if (new URL(request.url).search) return json({ error: "Query parameters are not supported." }, 400);
  let body: string | undefined;
  if (request.method === "POST") {
    if (!request.headers.get("content-type")?.toLowerCase().startsWith("application/json"))
      return json({ error: "Send an application/json request." }, 415);
    try {
      body = await readText(request, 16384);
    } catch {
      return json({ error: "Request is too large." }, 413);
    }
  }
  const { apiUrl } = config();
  let response: Response;
  try {
    response = await fetch(`${apiUrl}/${path}`, {
      method: request.method,
      headers: { Authorization: `Bearer ${accessToken}`, "Content-Type": "application/json" },
      body,
      cache: "no-store",
      redirect: "error",
      signal: AbortSignal.timeout(30_000),
    });
  } catch {
    return json({ error: "The fleet service is unavailable." }, 502);
  }
  let result: unknown;
  try {
    result = await response.json();
  } catch {
    return json({ error: "The fleet service returned an invalid response." }, 502);
  }
  if (response.status === 401) return unauthorized("invalid_token", "The access token is unknown, expired or revoked.");
  if (response.status === 403)
    return json(result, 403, { "WWW-Authenticate": challenge("insufficient_scope", "The token is not granted that operation on that workspace or pool.") });
  // A full run queue answers 429 with Retry-After; clients need it to wait rather than back off blindly.
  const retryAfter = response.headers.get("retry-after");
  return json(result, response.status, retryAfter && /^\d{1,6}$/.test(retryAfter) ? { "Retry-After": retryAfter } : {});
}

// ---------------------------------------------------------------- owner grant management

export function listGrants(request: Request) {
  requireSession(request);
  if (!stateDir()) return json({ enabled: false, grants: [] });
  const user = users().current(request);
  const state = readState();
  const now = Date.now();
  const grants = state.grants
    .filter((g) => g.revoked_at === null && g.refresh_expires_at > now && g.user_id === user?.id)
    .map((g) => {
      const client = state.clients.find((c) => c.client_id === g.client_id);
      return {
        id: g.id,
        client_id: g.client_id,
        client_name: client?.client_name ?? "Unknown app",
        redirect_hosts: [...new Set((client?.redirect_uris ?? []).map((u) => new URL(u).host))],
        scope: g.scope,
        workspaces: grantsView({ workspaces: g.workspaces }),
        pools: (g.pools ?? []).map((p) => ({ pool: p.pool, profile: p.profile, project: p.project ?? DEFAULT_PROJECT })),
        project: g.project ?? null,
        created_at: new Date(g.created_at).toISOString(),
        refreshed_at: new Date(g.refreshed_at).toISOString(),
        access_token_active: g.access_token_id !== null && g.access_expires_at > now,
        refresh_expires_at: new Date(g.refresh_expires_at).toISOString(),
      };
    });
  return json({ enabled: true, grants });
}

export async function revokeGrantByOwner(request: Request, id: string) {
  requireSession(request);
  requireOrigin(request);
  requireEnabled();
  if (!/^[a-f0-9]{32}$/.test(id)) throw new HttpError(404, "Unknown connection.");
  const user = users().current(request);
  const found = withState((state, now) => {
    const grant = state.grants.find((g) => g.id === id && g.user_id === user?.id && g.revoked_at === null);
    if (!grant) return false;
    revokeGrantIn(state, grant, "revoked by owner", now);
    return true;
  });
  if (!found) throw new HttpError(404, "Unknown connection.");
  await flushRevocations();
  return json({ revoked: true });
}
