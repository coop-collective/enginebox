#!/usr/bin/env node
// End-to-end check of the connected-app contract: an OAuth client (shaped like
// Ideator's EngineBox dispatcher) connects to an Engine Box dashboard, starts
// pool runs, polls them to completion, verifies a signed callback, refreshes,
// revokes, and confirms the revoked token is refused.
//
// It runs against any Engine Box dashboard with OAuth on. Locally (macOS, no
// Incus) use the dev controller with a stand-in executor:
//
//   cargo build -p enginebox-controller --example dev_fake_executor
//   ENGINEBOX_API_TOKEN=$OWNER ENGINEBOX_DEV_CALLBACK_SINK=http://127.0.0.1:18790/callback \
//     target/debug/examples/dev_fake_executor --config dev-fleet.toml --state-dir $DIR/state --listen 127.0.0.1:18765
//   (dashboard) ENGINEBOX_API_URL=http://127.0.0.1:18765 ENGINEBOX_OAUTH_STATE_DIR=$DIR/oauth \
//     ENGINEBOX_DASHBOARD_ORIGIN=http://127.0.0.1:13000 ... vite --port 13000
//   cd packages/dashboard && EB_BASE=http://127.0.0.1:13000 EB_PASSWORD=... \
//     EB_API=http://127.0.0.1:18765 EB_OWNER_TOKEN=$OWNER EB_CREATE_POOL=1 \
//     EB_CALLBACK_URL=https://callbacks.example.com/enginebox \
//     IDEATOR_CLIENT=/path/to/ideator/packages/api/src/enginebox/client.ts \
//     node --import tsx ../../scripts/e2e-connected-app.mjs
//
// Against a real Engine Box: EB_BASE=https://<dashboard origin>, EB_POOL=<an
// existing pool>. Without EB_PASSWORD the script prints the authorization URL;
// open it in a browser on this computer, sign in and consent, and the loopback
// redirect completes the flow. EB_CALLBACK_URL must then be a public HTTPS URL
// you can watch (the script cannot receive it); leave it unset to skip that step.
//
// Environment:
//   EB_BASE            dashboard origin (the OAuth issuer and protected resource)   required
//   EB_POOL            pool to grant and run on (default "e2e")
//   EB_PASSWORD        operator password: simulate the owner's sign-in and consent
//   EB_API, EB_OWNER_TOKEN, EB_CREATE_POOL=1, EB_POOL_HOST, EB_POOL_WORKSPACE
//                      create the pool on the controller first (dev only)
//   EB_CALLBACK_URL    callback URL put on one run; EB_SINK_PORT (default 18790) is the
//                      loopback listener for the OAuth redirect and dev callback deliveries
//   EB_PROVIDER        provider for runs (default "claude")
//   IDEATOR_CLIENT     path to Ideator's src/enginebox/client.ts; its startRun/getRun/
//                      cancelRun/listRuns are used for the run calls (needs --import tsx)
//
// No token, code, verifier or secret is ever printed.
import { createHash, createHmac, randomBytes } from "node:crypto";
import { createServer } from "node:http";

const env = process.env;
const BASE = (env.EB_BASE ?? "").replace(/\/+$/, "");
const POOL = env.EB_POOL ?? "e2e";
const PROVIDER = env.EB_PROVIDER ?? "claude";
const SINK_PORT = Number(env.EB_SINK_PORT ?? 18790);
const REDIRECT_URI = `http://127.0.0.1:${SINK_PORT}/api/enginebox/oauth/callback`;
if (!BASE) throw new Error("Set EB_BASE to the Engine Box dashboard origin.");

const results = [];
function step(name, ok, detail = "") {
  results.push({ name, ok });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
  if (!ok) process.exitCode = 1;
}
function must(name, ok, detail = "") {
  step(name, ok, detail);
  if (!ok) throw new Error(`stopping: ${name}`);
}
const b64url = (buf) => Buffer.from(buf).toString("base64url");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ------------------------------------------------------------- loopback sink
// Receives the OAuth redirect (interactive mode) and dev callback deliveries.
const callbacks = [];
let redirectWaiter = null;
const sink = createServer((req, res) => {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    const url = new URL(req.url, `http://127.0.0.1:${SINK_PORT}`);
    if (req.method === "GET" && url.pathname === "/api/enginebox/oauth/callback") {
      res.writeHead(200, { "content-type": "text/plain" }).end("Engine Box connected; return to the terminal.");
      redirectWaiter?.(url);
      return;
    }
    if (req.method === "POST") {
      callbacks.push({ headers: req.headers, body: Buffer.concat(chunks) });
      res.writeHead(204).end();
      return;
    }
    res.writeHead(404).end();
  });
});
await new Promise((r) => sink.listen(SINK_PORT, "127.0.0.1", r));

// ------------------------------------------------------------- run client
// Ideator's own client when available, else the same requests inline.
let client;
if (env.IDEATOR_CLIENT) {
  client = await import(env.IDEATOR_CLIENT);
  console.log("using Ideator's client.ts for run calls");
} else {
  const base = (t) => `${t.base_url}/v1/${t.pool ? `pools/${encodeURIComponent(t.pool)}` : `hosts/${encodeURIComponent(t.host)}/instances/${encodeURIComponent(t.instance)}`}/agent-runs`;
  const send = async (t, method, url, body) => {
    const res = await fetch(url, {
      method,
      headers: { authorization: `Bearer ${t.token}`, accept: "application/json", ...(body ? { "content-type": "application/json" } : {}) },
      body: body ? JSON.stringify(body) : undefined,
    });
    const json = await res.json().catch(() => null);
    if (!res.ok) throw Object.assign(new Error(`EngineBox ${res.status}: ${json?.error ?? ""}`), { status: res.status });
    return json;
  };
  client = {
    startRun: async (_f, t, i) => (await send(t, "POST", base(t), { provider: i.provider, prompt: i.prompt, workdir: i.workdir, timeout_seconds: i.timeout_seconds, ...(i.resume_id ? { resume_id: i.resume_id } : {}), ...(i.env ? { env: i.env } : {}), idempotency_key: i.idempotency_key })).run,
    getRun: async (_f, t, id) => (await send(t, "GET", `${base(t)}/${id}`)).run,
    cancelRun: async (_f, t, id) => (await send(t, "POST", `${base(t)}/${id}/cancel`, {})).run,
    listRuns: async (_f, t) => (await send(t, "GET", `${t.base_url}/v1/agent-runs`)).runs ?? [],
  };
  console.log("using the inline client (set IDEATOR_CLIENT to use Ideator's)");
}
const fetcher = (input, init) => fetch(input, init);

async function getJson(url, init = {}) {
  const res = await fetch(url, { ...init, headers: { accept: "application/json", ...(init.headers ?? {}) }, redirect: "manual" });
  const text = await res.text();
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {}
  return { res, json, text };
}

try {
  // ----------------------------------------------------------- 0. pool (dev)
  if (env.EB_CREATE_POOL === "1") {
    const host = env.EB_POOL_HOST ?? "devhost";
    const workspace = env.EB_POOL_WORKSPACE ?? "ebox-e2e-1";
    const r = await getJson(`${env.EB_API}/v1/pools`, {
      method: "POST",
      headers: { authorization: `Bearer ${env.EB_OWNER_TOKEN}`, "content-type": "application/json" },
      body: JSON.stringify({ name: POOL, description: "Connected-app e2e", workspaces: [{ host_id: host, name: workspace }] }),
    });
    must("owner creates the pool on the controller", r.res.status === 200 || r.res.status === 201, `HTTP ${r.res.status}`);
  }

  // ----------------------------------------------------------- 1. discovery
  const anon = await fetch(`${BASE}/v1/agent-runs`);
  step("an unauthenticated run call is 401 with resource metadata", anon.status === 401 && /resource_metadata="[^"]+oauth-protected-resource"/.test(anon.headers.get("www-authenticate") ?? ""));
  const prm = await getJson(`${BASE}/.well-known/oauth-protected-resource`);
  must("protected-resource metadata (RFC 9728) names this resource", prm.res.status === 200 && prm.json?.resource?.replace(/\/+$/, "") === BASE);
  const issuer = prm.json.authorization_servers?.[0]?.replace(/\/+$/, "");
  const issuerUrl = new URL(issuer);
  const metaUrl = `${issuerUrl.origin}/.well-known/oauth-authorization-server${issuerUrl.pathname === "/" ? "" : issuerUrl.pathname}`;
  const as = await getJson(metaUrl);
  must(
    "authorization-server metadata (RFC 8414) has every endpoint and S256",
    as.res.status === 200 && as.json?.issuer?.replace(/\/+$/, "") === issuer && ["authorization_endpoint", "token_endpoint", "registration_endpoint", "revocation_endpoint"].every((k) => as.json[k]) && as.json.code_challenge_methods_supported?.includes("S256"),
  );
  const meta = as.json;

  // ----------------------------------------------------------- 2. registration (Ideator's exact body)
  const reg = await getJson(meta.registration_endpoint, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ client_name: "Ideator", redirect_uris: [REDIRECT_URI], grant_types: ["authorization_code", "refresh_token"], response_types: ["code"], token_endpoint_auth_method: "client_secret_basic", scope: "agent-runs" }),
  });
  must("dynamic client registration (RFC 7591) issues a confidential client", reg.res.status === 201 && typeof reg.json?.client_id === "string" && typeof reg.json?.client_secret === "string");
  const clientId = reg.json.client_id;
  const clientSecret = reg.json.client_secret;
  const basic = `Basic ${btoa(`${encodeURIComponent(clientId)}:${encodeURIComponent(clientSecret)}`)}`;
  const postForm = (url, params) => getJson(url, { method: "POST", headers: { "content-type": "application/x-www-form-urlencoded", authorization: basic }, body: new URLSearchParams(params).toString() });

  // ----------------------------------------------------------- 3. authorize + consent
  const state = b64url(randomBytes(32));
  const verifier = b64url(randomBytes(48));
  const challenge = b64url(createHash("sha256").update(verifier).digest());
  const authUrl = new URL(meta.authorization_endpoint);
  for (const [k, v] of Object.entries({ response_type: "code", client_id: clientId, redirect_uri: REDIRECT_URI, scope: "agent-runs", state, code_challenge: challenge, code_challenge_method: "S256", resource: BASE }))
    authUrl.searchParams.set(k, v);
  let redirect;
  if (env.EB_PASSWORD) {
    // Simulate the owner's browser: interstitial, sign-in, consent.
    const first = await getJson(authUrl.href);
    const pendingPath = /url=([^"]+)"/.exec(first.text)?.[1]?.replace(/&amp;/g, "&");
    must("authorize starts with a same-origin hop that hides any existing session", first.res.status === 200 && !!pendingPath);
    const signInPage = await getJson(`${issuerUrl.origin}${pendingPath}`);
    const requestId = /name="request" value="([^"]+)"/.exec(signInPage.text)?.[1];
    must("the owner is asked to sign in", /Operator password|Sign in/.test(signInPage.text) && !!requestId);
    const formPost = (fields, cookie) =>
      fetch(meta.authorization_endpoint, {
        method: "POST",
        redirect: "manual",
        headers: { "content-type": "application/x-www-form-urlencoded", origin: issuerUrl.origin, ...(cookie ? { cookie } : {}) },
        body: new URLSearchParams(fields).toString(),
      });
    const signedIn = await formPost([["request", requestId], ["action", "sign_in"], ["password", env.EB_PASSWORD]]);
    const cookie = (signedIn.headers.get("set-cookie") ?? "").split(";")[0];
    const consent = await signedIn.text();
    must("sign-in sets a session and shows the consent screen", !!cookie && /Connect “Ideator”/.test(consent));
    step("consent offers the pool", consent.includes(`name="pool" value="${POOL}"`), POOL);
    const approved = await formPost([["request", requestId], ["action", "approve"], ["pool", POOL]], cookie);
    must("approving redirects to the registered URI", approved.status === 303 && (approved.headers.get("location") ?? "").startsWith(REDIRECT_URI));
    redirect = new URL(approved.headers.get("location"));
  } else {
    console.log(`\nOpen this URL in a browser on this computer, sign in, tick pool "${POOL}" and allow:\n\n  ${authUrl.href}\n`);
    redirect = await new Promise((resolve) => (redirectWaiter = resolve));
  }
  must("the redirect carries the same state and the issuer (RFC 9207)", redirect.searchParams.get("state") === state && redirect.searchParams.get("iss")?.replace(/\/+$/, "") === issuer && !!redirect.searchParams.get("code"));

  // ----------------------------------------------------------- 4. token
  const tok = await postForm(meta.token_endpoint, { grant_type: "authorization_code", code: redirect.searchParams.get("code"), redirect_uri: REDIRECT_URI, code_verifier: verifier, resource: BASE });
  must("the code exchange returns access and refresh tokens", tok.res.status === 200 && typeof tok.json?.access_token === "string" && typeof tok.json?.refresh_token === "string" && tok.json.expires_in > 0);
  const grants = tok.json.enginebox_grants ?? [];
  const poolGrant = grants.find((g) => g.kind === "pool" && g.pool === POOL);
  step("enginebox_grants lists the pool with kind, profile and profiles", !!poolGrant && "profile" in poolGrant && Array.isArray(poolGrant.profiles), JSON.stringify(grants.map(({ kind, pool, host, instance, profile }) => ({ kind, pool, host, instance, profile }))));
  // Ideator's parseGrants keeps an entry with a string `pool` (or host + ebox- instance).
  step("Ideator's grant parser would keep it", grants.some((g) => typeof g.pool === "string" && /^[A-Za-z0-9][A-Za-z0-9_.-]{0,62}$/.test(g.pool)));
  const callbackSecret = tok.json.enginebox_callback_secret;
  step("the code exchange returns the connection's callback key", typeof callbackSecret === "string" && callbackSecret.length >= 32);
  let access = tok.json.access_token;
  let refresh = tok.json.refresh_token;
  const target = () => ({ base_url: BASE, token: access, pool: POOL });

  // ----------------------------------------------------------- 5. pool runs
  const runEnv = { HQ_URL: "https://ideator.example", HQ_TOKEN: `hqa_${b64url(randomBytes(24))}`, HQ_TASK_ID: "task_e2e", HQ_MACHINE: "enginebox:org_e2e", HQ_ORG: "org_e2e" };
  const key = `task_e2e:${Date.now()}`;
  const prompt = `[ideator ${key}] Run Ideator (HQ) task task_e2e: connected-app e2e`;
  const startInput = { provider: PROVIDER, prompt, workdir: ".", timeout_seconds: 600, env: runEnv, idempotency_key: key };
  const run1 = await client.startRun(fetcher, target(), startInput);
  step("a pool start with env and idempotency_key is accepted as queued", run1?.status === "queued" && !!run1.id, `status ${run1?.status}`);
  const replay = await client.startRun(fetcher, target(), startInput);
  step("repeating the idempotency_key returns the same run", replay?.id === run1.id);

  // A second run with a callback, queued behind the first on the one-workspace pool.
  let run2 = null;
  if (env.EB_CALLBACK_URL) {
    const r = await getJson(`${BASE}/v1/pools/${POOL}/agent-runs`, {
      method: "POST",
      headers: { authorization: `Bearer ${access}`, "content-type": "application/json" },
      body: JSON.stringify({ provider: PROVIDER, prompt: `[ideator ${key}:cb] callback run`, workdir: ".", timeout_seconds: 600, idempotency_key: `${key}:cb`, callback: { url: env.EB_CALLBACK_URL } }),
    });
    run2 = r.json?.run;
    step("a pool start with a callback is accepted", r.res.status === 202 && !!run2?.id, `HTTP ${r.res.status}${r.json?.error ? `: ${r.json.error}` : ""}`);
  }

  // Cancel a run while it is still queued behind the others (unplaced).
  const run3 = await client.startRun(fetcher, target(), { ...startInput, prompt: `[ideator ${key}:x] cancel me`, idempotency_key: `${key}:x` });
  const cancelled = await client.cancelRun(fetcher, target(), run3.id);
  let c3 = cancelled;
  for (let i = 0; i < 40 && !["cancelled", "failed", "succeeded"].includes(c3?.status); i++) {
    await sleep(250);
    c3 = await client.getRun(fetcher, target(), run3.id);
  }
  step("cancelling a queued pool run ends it as cancelled", c3?.status === "cancelled" && !c3?.host_id, `status ${c3?.status}`);

  const seen = new Set([run1.status]);
  let done = null;
  for (let i = 0; i < 120 && !done; i++) {
    const r = await client.getRun(fetcher, target(), run1.id);
    seen.add(r.status);
    if (["succeeded", "failed", "cancelled"].includes(r.status)) done = r;
    else await sleep(500);
  }
  step("the run moves queued -> running -> succeeded", done?.status === "succeeded" && seen.has("running"), [...seen].join(" -> "));
  step("the finished run names its workspace, session and result", !!done?.host_id && !!done?.name && /^[0-9a-f-]{36}$/.test(done?.native_session_id ?? "") && done?.result?.exit_code === 0 && done?.result?.timed_out === false, `${done?.host_id}/${done?.name}`);
  step("env values never come back (names only)", JSON.stringify(done).includes("HQ_TOKEN") && !JSON.stringify(done).includes(runEnv.HQ_TOKEN));
  const listed = await client.listRuns(fetcher, target());
  step("the run list finds the run by its prompt preview (Ideator's lost-answer recovery)", listed.some((r) => r.id === run1.id && typeof r.task === "string" && r.task.includes(`[ideator ${key}]`)));

  if (run2) {
    let r2 = null;
    for (let i = 0; i < 120; i++) {
      r2 = await client.getRun(fetcher, target(), run2.id);
      if (["succeeded", "failed", "cancelled"].includes(r2.status) && r2.callback?.state && r2.callback.state !== "pending" && r2.callback.state !== "delivering") break;
      await sleep(500);
    }
    step("the callback run finished and recorded delivery", r2?.status === "succeeded" && r2?.callback?.state === "delivered", `callback ${JSON.stringify(r2?.callback ?? null)}`);
    const delivery = callbacks.find((c) => {
      try {
        return JSON.parse(c.body.toString()).run_id === run2.id;
      } catch {
        return false;
      }
    });
    if (delivery) {
      const sig = /^t=(\d+),v1=([0-9a-f]{64})$/.exec(delivery.headers["x-enginebox-signature"] ?? "");
      const expected = sig && createHmac("sha256", callbackSecret).update(`${sig[1]}.`).update(delivery.body).digest("hex");
      const payload = JSON.parse(delivery.body.toString());
      step("the callback is signed with the connection's key", !!sig && expected === sig[2]);
      step("the callback payload names the run, pool and status, and no output", payload.event === "agent_run.finished" && payload.pool === POOL && payload.status === "succeeded" && !("result" in payload) && !("output" in payload));
    } else step("the callback reached the loopback sink", false, "only the dev controller redirects deliveries to EB_SINK_PORT");
  }

  // Resume: through the pool (pinned to the session's workspace) works; the workspace route needs a workspace grant.
  const resumeInput = { ...startInput, prompt: `[ideator ${key}:r1] resume`, idempotency_key: `${key}:r1`, resume_id: done?.native_session_id };
  const viaPool = await client.startRun(fetcher, target(), resumeInput);
  let rr = viaPool;
  for (let i = 0; i < 120 && !["succeeded", "failed", "cancelled"].includes(rr?.status); i++) {
    await sleep(500);
    rr = await client.getRun(fetcher, target(), viaPool.id);
  }
  step("a resume started on the pool lands on the session's workspace", rr?.status === "succeeded" && rr?.name === done?.name && rr?.native_session_id === done?.native_session_id, `${rr?.host_id}/${rr?.name}`);
  const viaWorkspace = await fetch(`${BASE}/v1/hosts/${done?.host_id}/instances/${done?.name}/agent-runs`, {
    method: "POST",
    headers: { authorization: `Bearer ${access}`, "content-type": "application/json" },
    body: JSON.stringify({ ...resumeInput, idempotency_key: `${key}:r2` }),
  });
  step("a resume on the workspace route is 403 for a pool-only grant (resume via the pool instead)", viaWorkspace.status === 403, `HTTP ${viaWorkspace.status}`);

  // ----------------------------------------------------------- 6. refresh
  const before = access;
  const ref = await postForm(meta.token_endpoint, { grant_type: "refresh_token", refresh_token: refresh, resource: BASE });
  must("refresh rotates both tokens and repeats the grants", ref.res.status === 200 && ref.json.access_token !== before && ref.json.refresh_token !== refresh && JSON.stringify(ref.json.enginebox_grants) === JSON.stringify(grants));
  step("refresh does not repeat the callback key", ref.json.enginebox_callback_secret === undefined);
  access = ref.json.access_token;
  refresh = ref.json.refresh_token;
  const old = await fetch(`${BASE}/v1/pools/${POOL}/agent-runs/${run1.id}`, { headers: { authorization: `Bearer ${before}` } });
  step("the previous access token is refused after refresh", old.status === 401);
  const fresh = await client.getRun(fetcher, target(), run1.id);
  step("the new access token reads the run", fresh?.id === run1.id);

  // ----------------------------------------------------------- 7. revoke (Ideator's order: refresh, then access)
  for (const [token, hint] of [[refresh, "refresh_token"], [access, "access_token"]]) {
    const r = await postForm(meta.revocation_endpoint, { token, token_type_hint: hint });
    step(`revoking the ${hint} answers 200`, r.res.status === 200);
  }
  const after = await fetch(`${BASE}/v1/pools/${POOL}/agent-runs/${run1.id}`, { headers: { authorization: `Bearer ${access}` } });
  step("after revocation the access token is 401 invalid_token", after.status === 401 && /error="invalid_token"/.test(after.headers.get("www-authenticate") ?? ""));
  const reuse = await postForm(meta.token_endpoint, { grant_type: "refresh_token", refresh_token: refresh, resource: BASE });
  step("after revocation the refresh token is invalid_grant", reuse.res.status === 400 && reuse.json?.error === "invalid_grant");
} catch (error) {
  console.log(`ERROR ${error?.message ?? error}`);
  process.exitCode = 1;
} finally {
  sink.close();
  const failed = results.filter((r) => !r.ok).length;
  console.log(`\n${results.length - failed}/${results.length} checks passed`);
}
