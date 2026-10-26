import assert from "node:assert/strict";
import { test } from "node:test";
import { invoke, mapError, ReplayGuard, sign, WINDOW_SECONDS } from "../src/gateway.mjs";

const SECRET = "test-only-gateway-secret-0123456789abcdef";
const NOW = 1_800_000_000;
const REMOTE = (ns, name) => `https://acct.artifacts.cloudflare.net/git/${ns}/${name}.git`;
const artifactsError = (code) => Object.assign(new Error(`${code}: detail with art_v1_secret`), { name: "ArtifactsError", code });

/** An in-memory Artifacts binding for one namespace, following the workerd interface. */
function binding(namespace) {
  const repos = new Map();
  const tokens = new Map();
  const calls = [];
  let n = 0;
  const handle = (name) => {
    const repo = repos.get(name);
    return {
      async info() {
        return { ...repo, remote: REMOTE(namespace, name) };
      },
      async createToken(scope, ttl) {
        calls.push(["createToken", name, scope, ttl]);
        const id = `tok${++n}`;
        const plaintext = `art_v1_${String(n).padStart(40, "0")}?expires=${NOW + ttl}`;
        tokens.set(id, { plaintext, repo: name, revoked: false });
        return { id, plaintext, scope, expiresAt: new Date((NOW + ttl) * 1000).toISOString() };
      },
      async revokeToken(tokenOrId) {
        calls.push(["revokeToken", name, tokenOrId.startsWith("art_v1_") ? "<plaintext>" : tokenOrId]);
        for (const [id, t] of tokens) if ((id === tokenOrId || t.plaintext === tokenOrId) && t.repo === name && !t.revoked) return (t.revoked = true);
        return false;
      },
      [Symbol.dispose]() {
        calls.push(["dispose", name]);
      },
    };
  };
  return {
    repos,
    tokens,
    calls,
    async create(name, opts) {
      if (!/^[a-z0-9][a-z0-9._-]*$/.test(name)) throw artifactsError("INVALID_REPO_NAME");
      if (repos.has(name)) throw artifactsError("ALREADY_EXISTS");
      repos.set(name, { id: `repo_${name}`, name, description: opts?.description ?? null, defaultBranch: "main", lastPushAt: null });
      const plaintext = `art_v1_${"c".repeat(40)}?expires=${NOW + 86400}`;
      tokens.set(`init_${name}`, { plaintext, repo: name, revoked: false });
      return { id: `repo_${name}`, name, description: opts?.description ?? null, defaultBranch: "main", remote: REMOTE(namespace, name), token: plaintext };
    },
    async get(name) {
      if (!repos.has(name)) throw artifactsError("NOT_FOUND");
      calls.push(["get", name]);
      return handle(name);
    },
    async list(opts) {
      calls.push(["list", opts]);
      return { repos: [...repos.values()], total: repos.size };
    },
  };
}

/** A Durable Object namespace stand-in running the real ReplayGuard on memory storage. */
function replay() {
  const store = new Map();
  let alarm = null;
  const guard = new ReplayGuard({
    storage: {
      get: async (k) => store.get(k),
      put: async (k, v) => void store.set(k, v),
      getAlarm: async () => alarm,
      setAlarm: async (t) => void (alarm = t),
      list: async ({ prefix }) => new Map([...store].filter(([k]) => k.startsWith(prefix))),
      delete: async (keys) => keys.forEach((k) => store.delete(k)),
    },
  });
  return { store, guard, idFromName: (n) => n, get: () => ({ fetch: (url, init) => guard.fetch(new Request(url, init)) }) };
}

function setup() {
  const alpha = binding("enginebox-alpha");
  const env = {
    GATEWAY_SECRET: SECRET,
    NAMESPACE_PREFIX: "enginebox",
    NAMESPACES: JSON.stringify({ "enginebox-alpha": "ARTIFACTS_0", "other-beta": "ARTIFACTS_1" }),
    ARTIFACTS_0: alpha,
    ARTIFACTS_1: binding("other-beta"),
    REPLAY: replay(),
  };
  return { env, alpha };
}

let counter = 0;
/** The envelope the site Worker forwards over RPC, signed as the controller signs it. */
async function signed(op, body, { secret = SECRET, timestamp = NOW, nonce, tamper, method = "POST" } = {}) {
  const raw = JSON.stringify(body);
  nonce ??= (++counter).toString(16).padStart(32, "0");
  const signature = await sign(secret, method, `/${op}`, String(timestamp), nonce, raw);
  return { method, path: `/${op}`, timestamp: String(timestamp), nonce, signature: `v1=${signature}`, body: tamper ?? raw };
}
const call = async (env, envelope) => invoke(envelope, env, NOW);

test("a signed request is served; expired, future, replayed, tampered or wrongly keyed ones answer 401 without detail", async () => {
  const { env } = setup();
  const ok = await call(env, await signed("repos.list", { namespace: "enginebox-alpha" }));
  assert.equal(ok.status, 200);
  const cases = [
    await signed("repos.list", { namespace: "enginebox-alpha" }, { timestamp: NOW - WINDOW_SECONDS - 1 }),
    await signed("repos.list", { namespace: "enginebox-alpha" }, { timestamp: NOW + WINDOW_SECONDS + 1 }),
    await signed("repos.list", { namespace: "enginebox-alpha" }, { tamper: JSON.stringify({ namespace: "enginebox-alpha", cursor: "x" }) }),
    await signed("repos.list", { namespace: "enginebox-alpha" }, { secret: "another-secret-of-sufficient-length-000" }),
    await signed("repos.list", { namespace: "enginebox-alpha" }, { nonce: "not-a-nonce" }),
    { method: "POST", path: "/repos.list", body: "{}" },
  ];
  for (const request of cases) {
    const result = await call(env, request);
    assert.equal(result.status, 401);
    assert.deepEqual(result.body, { error: "unauthorized" });
  }
  // A path signed for one operation cannot be replayed against another.
  const moved = await signed("repos.list", { namespace: "enginebox-alpha" });
  const swapped = { ...moved, path: "/repos.get" };
  assert.equal((await call(env, swapped)).status, 401);
  // The same nonce is accepted once within the window.
  const nonce = "f".repeat(32);
  assert.equal((await call(env, await signed("repos.list", { namespace: "enginebox-alpha" }, { nonce }))).status, 200);
  const replayed = await call(env, await signed("repos.list", { namespace: "enginebox-alpha" }, { nonce }));
  assert.deepEqual([replayed.status, replayed.body], [401, { error: "unauthorized" }]);
  // Rejected signatures never reach the replay cache.
  assert.equal(env.REPLAY.store.size, 2);
  // The cache forgets nonces after their window.
  const realNow = Date.now;
  Date.now = () => (NOW + WINDOW_SECONDS * 3) * 1000;
  try {
    await env.REPLAY.guard.alarm();
  } finally {
    Date.now = realNow;
  }
  assert.equal(env.REPLAY.store.size, 0);
});

test("only POST envelopes for the five operations, with a string body of at most 16 KiB", async () => {
  const { env } = setup();
  const body = { namespace: "enginebox-alpha" };
  assert.equal((await call(env, await signed("repos.list", body, { method: "GET" }))).status, 405);
  assert.equal((await call(env, await signed("repos.delete", body))).status, 404);
  assert.equal((await call(env, { ...(await signed("repos.list", body)), body: { namespace: "enginebox-alpha" } })).status, 400);
  assert.equal((await call(env, null)).status, 400);
  const big = await signed("repos.create", { namespace: "enginebox-alpha", name: "x", description: "d".repeat(20000) });
  assert.equal((await call(env, big)).status, 413);
});

test("the Worker module has no HTTP surface beyond refusing", async () => {
  const source = await import("node:fs/promises").then((fs) => fs.readFile(new URL("../src/index.mjs", import.meta.url), "utf8"));
  assert.match(source, /class ArtifactsGateway extends WorkerEntrypoint/);
  assert.match(source, /status: 404/);
  assert.doesNotMatch(source, /invoke\(await request/);
});

test("namespaces are restricted to the configured prefix and to provisioned bindings", async () => {
  const { env } = setup();
  const outside = await call(env, await signed("repos.list", { namespace: "other-beta" }));
  assert.deepEqual([outside.status, outside.body], [403, { error: "namespace_not_allowed" }], "a binding outside the prefix is never used");
  const missing = await call(env, await signed("repos.list", { namespace: "enginebox-gamma" }));
  assert.deepEqual([missing.status, missing.body], [404, { error: "namespace_not_provisioned" }]);
  assert.equal((await call(env, await signed("repos.list", { namespace: 7 }))).status, 400);
});

test("repository and token operations return the mapped fields, revoke the creation token and never log a token", async (t) => {
  const { env, alpha } = setup();
  const logs = [];
  t.mock.method(console, "log", (line) => logs.push(line));
  const created = await call(env, await signed("repos.create", { namespace: "enginebox-alpha", name: "game", description: "The game" }));
  assert.equal(created.status, 200);
  assert.deepEqual(created.body, {
    created: true,
    repository: { id: "repo_game", name: "game", description: "The game", default_branch: "main", remote: REMOTE("enginebox-alpha", "game"), last_push_at: null },
  });
  assert.ok(!JSON.stringify(created.body).includes("art_v1_"), "no creation token in the response");
  assert.equal(alpha.tokens.get("init_game").revoked, true, "creation token revoked");
  const again = await call(env, await signed("repos.create", { namespace: "enginebox-alpha", name: "game" }));
  assert.deepEqual([again.status, again.body], [409, { error: "already_exists" }]);
  const got = await call(env, await signed("repos.get", { namespace: "enginebox-alpha", name: "game" }));
  assert.equal(got.body.repository.remote, REMOTE("enginebox-alpha", "game"));
  const missing = await call(env, await signed("repos.get", { namespace: "enginebox-alpha", name: "nope" }));
  assert.deepEqual([missing.status, missing.body], [404, { error: "not_found" }]);
  const listed = await call(env, await signed("repos.list", { namespace: "enginebox-alpha", cursor: "c1" }));
  assert.deepEqual(listed.body.repositories.map((r) => r.name), ["game"]);
  assert.deepEqual(alpha.calls.find((c) => c[0] === "list")[1], { limit: 200, cursor: "c1" });
  const minted = await call(env, await signed("tokens.create", { namespace: "enginebox-alpha", repo: "game", scope: "write", ttl: 720 }));
  assert.equal(minted.status, 200);
  assert.equal(minted.body.token.scope, "write");
  assert.match(minted.body.token.plaintext, /^art_v1_/);
  assert.ok(minted.body.token.id && minted.body.token.expires_at);
  for (const bad of [{ scope: "admin", ttl: 720 }, { scope: "read", ttl: 59 }, { scope: "read", ttl: 90001 }, { scope: "read", ttl: 1.5 }]) {
    assert.equal((await call(env, await signed("tokens.create", { namespace: "enginebox-alpha", repo: "game", ...bad }))).status, 400, JSON.stringify(bad));
  }
  const revoked = await call(env, await signed("tokens.revoke", { namespace: "enginebox-alpha", repo: "game", id: minted.body.token.id }));
  assert.deepEqual(revoked.body, { revoked: true });
  assert.equal(alpha.calls.filter((c) => c[0] === "dispose").length, alpha.calls.filter((c) => c[0] === "get").length, "every repo handle is disposed");
  for (const line of logs) assert.ok(!line.includes("art_v1_") && !line.includes(SECRET), line);
  assert.ok(logs.some((l) => JSON.parse(l).operation === "tokens.create"));
});

test("binding errors map to codes, never to their messages", async () => {
  const cases = [
    ["NOT_FOUND", 404, "not_found"],
    ["ALREADY_EXISTS", 409, "already_exists"],
    ["CREATE_IN_PROGRESS", 409, "in_progress"],
    ["INVALID_TTL", 400, "invalid_input"],
    ["INTERNAL_ERROR", 502, "upstream"],
    [undefined, 502, "upstream"],
  ];
  for (const [code, status, error] of cases) {
    const response = mapError(code ? artifactsError(code) : new Error("boom art_v1_secret"));
    assert.equal(response.status, status);
    const text = JSON.stringify(response.body);
    assert.deepEqual(response.body, { error });
    assert.ok(!text.includes("art_v1_"));
  }
  const { env, alpha } = setup();
  alpha.list = async () => {
    throw artifactsError("INTERNAL_ERROR");
  };
  const failed = await call(env, await signed("repos.list", { namespace: "enginebox-alpha" }));
  assert.deepEqual([failed.status, failed.body], [502, { error: "upstream" }]);
});

test("signatures match the controller's vector", async () => {
  // The same vector is asserted by the controller (repositories.rs, signatures_match_the_gateway_vector).
  const body = JSON.stringify({ namespace: "eb-alpha", name: "game" });
  assert.equal(await sign(SECRET, "POST", "/repos.get", "1800000000", "0".repeat(32), body), "af382d7a1716eee3b28bea32a85f0daa8f803bcec2b0a0643f3c8df94e12dacb");
});
