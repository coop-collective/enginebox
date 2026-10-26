// Engine Box Artifacts gateway core: verifies a signed envelope and runs one
// of five operations on the Artifacts bindings (one per provisioned
// namespace). The Worker (src/index.mjs) exposes it only over RPC to the site
// Worker, which forwards the controller's request unchanged and holds no
// secret. No Cloudflare API token exists anywhere.
//
// Authentication: every envelope carries what the controller sent as
//   X-EngineBox-Timestamp: <unix seconds>
//   X-EngineBox-Nonce:     <32 lowercase hex>
//   X-EngineBox-Signature: v1=<hex HMAC-SHA256(secret, canonical)>
// where canonical = "v1\n" + method + "\n" + path + "\n" + timestamp + "\n" +
// nonce + "\n" + hex(SHA-256(body)). The timestamp must be within ±300 s and a
// nonce is accepted once (a Durable Object remembers nonces for the window).
// Failures answer 401 without detail. Tokens are never logged.

export const WINDOW_SECONDS = 300;
export const MAX_BODY = 16384;
const NONCE = /^[a-f0-9]{32}$/;
const NAME = /^[a-z0-9][a-z0-9._-]{0,62}$/;
const OPERATIONS = new Set(["/repos.create", "/repos.get", "/repos.list", "/tokens.create", "/tokens.revoke"]);
const encoder = new TextEncoder();

/** An RPC result: the HTTP status and JSON body the site Worker returns. */
export function reply(body, status = 200) {
  return { status, body };
}
const unauthorized = () => reply({ error: "unauthorized" }, 401);
const notFound = () => reply({ error: "not_found" }, 404);
const invalid = () => reply({ error: "invalid_input" }, 400);

function hex(buffer) {
  return [...new Uint8Array(buffer)].map((b) => b.toString(16).padStart(2, "0")).join("");
}
export async function sha256Hex(bytes) {
  return hex(await crypto.subtle.digest("SHA-256", bytes));
}
export function canonical(method, path, timestamp, nonce, bodyHash) {
  return `v1\n${method}\n${path}\n${timestamp}\n${nonce}\n${bodyHash}`;
}
export async function sign(secret, method, path, timestamp, nonce, body) {
  const key = await crypto.subtle.importKey("raw", encoder.encode(secret), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  const bytes = typeof body === "string" ? encoder.encode(body) : body;
  const message = canonical(method, path, timestamp, nonce, await sha256Hex(bytes));
  return hex(await crypto.subtle.sign("HMAC", key, encoder.encode(message)));
}
/** Constant-time comparison of two equal-length lowercase hex strings. */
export function equalHex(a, b) {
  if (typeof a !== "string" || typeof b !== "string" || a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

/** The configured namespace → binding map, restricted to the configured prefix. */
export function namespaces(env) {
  const prefix = env.NAMESPACE_PREFIX;
  if (typeof prefix !== "string" || !/^[a-z0-9][a-z0-9-]{0,13}$/.test(prefix)) return new Map();
  let map;
  try {
    map = JSON.parse(env.NAMESPACES ?? "{}");
  } catch {
    return new Map();
  }
  const out = new Map();
  for (const [namespace, binding] of Object.entries(map)) {
    if (namespace.startsWith(`${prefix}-`) && env[binding] && typeof env[binding].get === "function") out.set(namespace, env[binding]);
  }
  return out;
}

async function verify(envelope, env, body, now) {
  const secret = env.GATEWAY_SECRET;
  if (typeof secret !== "string" || secret.length < 32) return false;
  const { method, path, timestamp, nonce, signature } = envelope;
  if (typeof timestamp !== "string" || typeof nonce !== "string" || typeof signature !== "string") return false;
  if (!/^\d{1,12}$/.test(timestamp) || !NONCE.test(nonce) || !signature.startsWith("v1=")) return false;
  if (Math.abs(now - Number(timestamp)) > WINDOW_SECONDS) return false;
  const expected = await sign(secret, method, path, timestamp, nonce, body);
  if (!equalHex(expected, signature.slice(3))) return false;
  // Only a correctly signed request reaches the replay cache.
  return claimNonce(env, nonce, Number(timestamp) + WINDOW_SECONDS * 2);
}

async function claimNonce(env, nonce, expires) {
  const stub = env.REPLAY.get(env.REPLAY.idFromName("nonces"));
  const response = await stub.fetch("https://replay.invalid/claim", {
    method: "POST",
    body: JSON.stringify({ nonce, expires }),
  });
  return response.status === 200;
}

/** Map an Artifacts binding error to a status and a code; never its message. */
export function mapError(error) {
  const code = typeof error?.code === "string" ? error.code : "";
  if (code === "NOT_FOUND") return reply({ error: "not_found" }, 404);
  if (code === "ALREADY_EXISTS") return reply({ error: "already_exists" }, 409);
  if (["CREATE_IN_PROGRESS", "IMPORT_IN_PROGRESS", "FORK_IN_PROGRESS"].includes(code)) return reply({ error: "in_progress" }, 409);
  if (["INVALID_INPUT", "INVALID_REPO_NAME", "INVALID_TTL"].includes(code)) return reply({ error: "invalid_input" }, 400);
  return reply({ error: "upstream" }, 502);
}

function repository(info) {
  return {
    id: info.id ?? null,
    name: info.name,
    description: info.description ?? null,
    default_branch: info.defaultBranch ?? "main",
    remote: info.remote ?? null,
    last_push_at: info.lastPushAt ?? null,
  };
}

async function withRepo(artifacts, name, use) {
  const repo = await artifacts.get(name);
  try {
    return await use(repo);
  } finally {
    repo?.[Symbol.dispose]?.();
  }
}

const string = (value, pattern) => typeof value === "string" && pattern.test(value);

async function operate(path, input, artifacts) {
  switch (path) {
    case "/repos.create": {
      if (!string(input.name, NAME) || (input.description !== undefined && (typeof input.description !== "string" || input.description.length > 512))) return invalid();
      const created = await artifacts.create(input.name, input.description ? { description: input.description } : undefined);
      // The creation token is never returned; revoke it straight away.
      try {
        await withRepo(artifacts, created.name, (repo) => repo.revokeToken(created.token));
      } catch {}
      return reply({ repository: repository(created), created: true });
    }
    case "/repos.get": {
      if (!string(input.name, NAME)) return invalid();
      return reply({ repository: repository(await withRepo(artifacts, input.name, (repo) => repo.info())) });
    }
    case "/repos.list": {
      if (input.cursor !== undefined && (typeof input.cursor !== "string" || input.cursor.length > 512)) return invalid();
      const page = await artifacts.list({ limit: 200, ...(input.cursor ? { cursor: input.cursor } : {}) });
      return reply({ repositories: (page.repos ?? []).map(repository), cursor: page.cursor ?? null });
    }
    case "/tokens.create": {
      const ttl = input.ttl;
      if (!string(input.repo, NAME) || !["read", "write"].includes(input.scope) || !Number.isInteger(ttl) || ttl < 60 || ttl > 90000) return invalid();
      const token = await withRepo(artifacts, input.repo, (repo) => repo.createToken(input.scope, ttl));
      return reply({ token: { id: token.id ?? null, plaintext: token.plaintext, scope: token.scope, expires_at: token.expiresAt } });
    }
    case "/tokens.revoke": {
      if (!string(input.repo, NAME) || !string(input.id, /^[A-Za-z0-9_-]{1,128}$/)) return invalid();
      const revoked = await withRepo(artifacts, input.repo, (repo) => repo.revokeToken(input.id));
      return reply({ revoked: revoked === true });
    }
  }
  return notFound();
}

/**
 * Verify and run one signed envelope `{method, path, timestamp, nonce,
 * signature, body}` (body: the exact request text). Returns `{status, body}`.
 * `now` (unix seconds) is injectable for tests.
 */
export async function invoke(envelope, env, now = Math.floor(Date.now() / 1000)) {
  if (!envelope || typeof envelope !== "object") return invalid();
  if (envelope.method !== "POST") return reply({ error: "method_not_allowed" }, 405);
  if (typeof envelope.path !== "string" || !OPERATIONS.has(envelope.path)) return notFound();
  if (typeof envelope.body !== "string") return invalid();
  const body = encoder.encode(envelope.body);
  if (body.byteLength > MAX_BODY) return reply({ error: "too_large" }, 413);
  if (!(await verify(envelope, env, body, now))) return unauthorized();
  let input;
  try {
    input = JSON.parse(envelope.body);
  } catch {
    return invalid();
  }
  if (!input || typeof input !== "object" || Array.isArray(input) || typeof input.namespace !== "string") return invalid();
  const artifacts = namespaces(env).get(input.namespace);
  if (!artifacts) {
    const prefix = env.NAMESPACE_PREFIX ?? "";
    return input.namespace.startsWith(`${prefix}-`) ? reply({ error: "namespace_not_provisioned" }, 404) : reply({ error: "namespace_not_allowed" }, 403);
  }
  let result;
  try {
    result = await operate(envelope.path, input, artifacts);
  } catch (error) {
    result = mapError(error);
  }
  // Operation, namespace and status only: never a token, body or error message.
  console.log(JSON.stringify({ event: "artifacts_gateway", operation: envelope.path.slice(1), namespace: input.namespace, status: result.status }));
  return result;
}

/** Remembers each accepted nonce until its window has passed. */
export class ReplayGuard {
  constructor(ctx) {
    this.ctx = ctx;
  }
  async fetch(request) {
    const { nonce, expires } = await request.json();
    if (typeof nonce !== "string" || !NONCE.test(nonce) || !Number.isInteger(expires)) return new Response(null, { status: 400 });
    const key = `n:${nonce}`;
    if ((await this.ctx.storage.get(key)) !== undefined) return new Response(null, { status: 409 });
    await this.ctx.storage.put(key, expires);
    if ((await this.ctx.storage.getAlarm()) === null) await this.ctx.storage.setAlarm(Date.now() + WINDOW_SECONDS * 2000);
    return new Response(null, { status: 200 });
  }
  async alarm() {
    const now = Math.floor(Date.now() / 1000);
    const entries = await this.ctx.storage.list({ prefix: "n:" });
    const stale = [...entries].filter(([, expires]) => expires <= now).map(([key]) => key);
    for (let i = 0; i < stale.length; i += 128) await this.ctx.storage.delete(stale.slice(i, i + 128));
    if (entries.size > stale.length) await this.ctx.storage.setAlarm(Date.now() + WINDOW_SECONDS * 2000);
  }
}
