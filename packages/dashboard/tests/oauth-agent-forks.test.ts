import assert from "node:assert/strict";
import { generateKeyPairSync } from "node:crypto";
import { beforeEach, test } from "node:test";
import { AGENT_FORK_OPERATIONS } from "../lib/oauth/controller";
import { authorize, listGrants, parseScope } from "../lib/oauth/server";
import { readIdentity, userByEmail } from "../lib/identity/store";
import { nodeProxy } from "../lib/contributors";
import { HttpError } from "../lib/server";
import * as callbackRoute from "../app/v1/gateway/xinf/callback/route";
import * as resourceRoute from "../app/v1/[...path]/route";
import {
  authorizeUrl,
  call,
  consentPost,
  cookie,
  fakeController,
  origin,
  ownerToken,
  pkce,
  registerClient,
  requestId,
  standardParams,
  stateText,
  tokenRequest,
} from "./oauth-fixture";

// Agent forks over OAuth (docs/ENGINE-SESSIONS.md, "Agent forks"): the `agent-forks` scope, room-principal keys, the
// fork routes on the resource proxy, the Xava Inference callback and the node's fork actions.

const ENGINE_OPERATIONS = ["engine_sessions.start", "engine_sessions.get", "engine_sessions.renew", "engine_sessions.end", "engine_sessions.watch"];
const owner = () => userByEmail(readIdentity(), "owner@fixture.example")!.id;
const principalKey = () => (generateKeyPairSync("ed25519").publicKey.export({ format: "jwk" }) as { x: string }).x;
const REDIRECTS = ["https://gamejam.example/cb", "https://gamejam.example/again", "https://account.gamejam.example/cb", "http://127.0.0.1:7777/cb"];

type Registered = { client_id: string; client_secret?: string; redirect_uris: string[]; enginebox_principal_key?: string };
/** Gamejam Account: a confidential client (client_secret_post) with a principal key. */
async function gamejam(key: string | null = principalKey()) {
  const response = await registerClient({
    client_name: "Gamejam Account",
    redirect_uris: REDIRECTS,
    token_endpoint_auth_method: "client_secret_post",
    ...(key ? { enginebox_principal_key: key } : {}),
  });
  assert.equal(response.status, 201);
  return (await response.json()) as Registered;
}
let shared: Registered | null = null;
beforeEach(() => {
  shared = null;
});
async function consent(scope: string | undefined, extra: Record<string, string> = {}, client?: Registered) {
  const app = client ?? (shared ??= await gamejam());
  const { verifier, challenge } = pkce();
  const page = await authorize(new Request(authorizeUrl(standardParams(app, challenge, { scope, ...extra })), { headers: { cookie: cookie() } }));
  assert.equal(page.status, 200);
  const html = await page.text();
  return { client: app, verifier, html, id: requestId(html) };
}
async function exchangeConfidential(client: Registered, location: string | null, verifier: string) {
  const response = await tokenRequest([
    ["grant_type", "authorization_code"],
    ["code", new URL(location!).searchParams.get("code")!],
    ["redirect_uri", client.redirect_uris[0]],
    ["code_verifier", verifier],
    ["client_id", client.client_id],
    ["client_secret", client.client_secret!],
    ["resource", origin],
  ]);
  assert.equal(response.status, 200);
  return response.json();
}
/** Consent to `fields` (beyond request/action) and exchange the code. */
async function grant(scope: string, fields: [string, string][], extra: Record<string, string> = {}, client?: Registered) {
  const { client: app, verifier, id } = await consent(scope, extra, client);
  const ok = await consentPost([["request", id], ["action", "approve"], ...fields]);
  assert.equal(ok.status, 303);
  return { client: app, tokens: await exchangeConfidential(app, ok.headers.get("location"), verifier) };
}
const mints = (controller: ReturnType<typeof fakeController>) => controller.calls.filter((c) => c.path === "/v1/service-tokens").map((c) => c.body as Record<string, unknown>);

test("scope parsing knows agent-forks and normalizes the order", () => {
  assert.equal(parseScope("agent-forks"), "agent-forks");
  assert.equal(parseScope("agent-forks engine-sessions"), "engine-sessions agent-forks");
  assert.equal(parseScope("agent-forks agent-runs agent-forks"), "agent-runs agent-forks");
  assert.equal(parseScope("agent-forks forks"), null);
  assert.equal(parseScope(""), null);
  assert.equal(parseScope(undefined), "agent-runs");
});

test("registration takes an Ed25519 principal key only from a confidential client", async () => {
  const key = principalKey();
  const app = await gamejam(key);
  assert.equal(app.enginebox_principal_key, key);
  const stored = JSON.parse(stateText()).clients.find((c: { client_id: string }) => c.client_id === app.client_id);
  assert.equal(stored.principal_key, key);
  const plain = await gamejam(null);
  assert.equal(plain.enginebox_principal_key, undefined);
  assert.equal(JSON.parse(stateText()).clients.find((c: { client_id: string }) => c.client_id === plain.client_id).principal_key, undefined);
  const scoped = await registerClient({ redirect_uris: REDIRECTS, token_endpoint_auth_method: "client_secret_basic", scope: "agent-forks engine-sessions", enginebox_principal_key: key });
  assert.equal(scoped.status, 201);
  assert.equal((await scoped.json()).scope, "engine-sessions agent-forks");
  // A public client cannot hold a signing key's counterpart secret.
  const publicClient = await registerClient({ redirect_uris: REDIRECTS, token_endpoint_auth_method: "none", enginebox_principal_key: key });
  assert.equal(publicClient.status, 400);
  assert.equal((await publicClient.json()).error, "invalid_client_metadata");
  for (const bad of [key.slice(1), `${key}A`, `${key}=`, `${key.slice(0, 42)}B`, `${key.slice(0, 42)}+`, Buffer.alloc(32).toString("base64"), 42, null, ""]) {
    const refused = await registerClient({ redirect_uris: REDIRECTS, token_endpoint_auth_method: "client_secret_post", enginebox_principal_key: bad });
    assert.equal(refused.status, 400, String(bad));
    assert.equal((await refused.json()).error, "invalid_client_metadata");
  }
});

test("consent offers agent forks only for that scope; an agent-forks-only approval mints the fork grant", async (t) => {
  const controller = fakeController(t);
  for (const scope of ["agent-runs engine-sessions", undefined]) assert.doesNotMatch((await consent(scope)).html, /name="agent_forks"/);
  const { client, verifier, html, id } = await consent("agent-forks");
  for (const project of ["alpha", "beta", "default", "gamma"])
    assert.match(
      html,
      new RegExp(`name="agent_forks" value="${project}"> Let room members run their own agents on forks of your live engine sessions in project <code>${project}</code>`),
    );
  assert.doesNotMatch(html, /name="engine_sessions"|name="pool"|name="workspace"/);
  const nothing = await consentPost([["request", id], ["action", "approve"]]);
  assert.match(await nothing.text(), /at least one project&#39;s agent forks/);
  const unknown = await consentPost([["request", id], ["action", "approve"], ["agent_forks", "delta"]]);
  assert.match(await unknown.text(), /agent forks are not available/);
  const sneaked = await consentPost([["request", id], ["action", "approve"], ["agent_forks", "alpha"], ["engine_sessions", "alpha"]]);
  assert.match(await sneaked.text(), /engine sessions are not available/, "engine sessions were not asked for");
  const ok = await consentPost([["request", id], ["action", "approve"], ["agent_forks", "alpha"]]);
  assert.equal(ok.status, 303);
  const tokens = await exchangeConfidential(client, ok.headers.get("location"), verifier);
  assert.equal(tokens.scope, "agent-forks");
  assert.deepEqual(tokens.enginebox_grants, [{ kind: "agent_forks", project: "alpha", member: owner(), room_principals: true }]);
  // What the controller is asked to mint: fork operations only, the client, its key and its https return origins.
  const [body] = mints(controller);
  assert.deepEqual(body.operations, AGENT_FORK_OPERATIONS, "an agent-forks-only grant holds no agent-run operation");
  assert.deepEqual(body.agent_forks, [
    { project: "alpha", client: client.client_id, principal_key: client.enginebox_principal_key, return_origins: ["https://gamejam.example", "https://account.gamejam.example"] },
  ]);
  assert.equal(body.member, owner());
  assert.equal(body.engine_sessions, undefined);
  assert.equal(body.project, undefined);
  assert.deepEqual(body.scopes, []);
  // Refresh keeps it; the connection lists it.
  const refreshed = await (
    await tokenRequest([
      ["grant_type", "refresh_token"],
      ["refresh_token", tokens.refresh_token],
      ["client_id", client.client_id],
      ["client_secret", client.client_secret!],
    ])
  ).json();
  assert.deepEqual(refreshed.enginebox_grants, tokens.enginebox_grants);
  assert.equal(refreshed.scope, "agent-forks");
  assert.deepEqual(mints(controller)[1].agent_forks, body.agent_forks);
  assert.equal(mints(controller)[1].member, owner());
  const listed = await listGrants(new Request(`${origin}/api/oauth/grants`, { headers: { cookie: cookie() } })).json();
  assert.deepEqual(listed.grants[0].agent_forks, [{ project: "alpha" }]);
  assert.equal(listed.grants[0].scope, "agent-forks");
});

test("agent forks with engine sessions send member once; a requested project restricts the token; no key means no room principals", async (t) => {
  const controller = fakeController(t);
  const both = await grant("engine-sessions agent-forks", [["engine_sessions", "alpha"], ["agent_forks", "alpha"]], { project: "alpha" });
  assert.equal(both.tokens.scope, "engine-sessions agent-forks");
  assert.deepEqual(both.tokens.enginebox_grants, [
    { kind: "engine_sessions", project: "alpha", member: owner() },
    { kind: "agent_forks", project: "alpha", member: owner(), room_principals: true },
  ]);
  const body = mints(controller).at(-1)!;
  assert.deepEqual(body.operations, [...ENGINE_OPERATIONS, ...AGENT_FORK_OPERATIONS]);
  assert.deepEqual(body.engine_sessions, [{ project: "alpha" }]);
  assert.equal(body.member, owner());
  assert.equal(body.project, "alpha");
  // Only alpha is offered when the app asked for alpha.
  const scoped = await consent("agent-forks", { project: "alpha" });
  assert.doesNotMatch(scoped.html, /name="agent_forks" value="beta"/);
  // A client without a principal key: the grant names no key and admits no room principals.
  const plain = await gamejam(null);
  const keyless = await grant("agent-forks", [["agent_forks", "beta"]], {}, plain);
  assert.deepEqual(keyless.tokens.enginebox_grants, [{ kind: "agent_forks", project: "beta", member: owner(), room_principals: false }]);
  assert.deepEqual(mints(controller).at(-1)!.agent_forks, [
    { project: "beta", client: plain.client_id, return_origins: ["https://gamejam.example", "https://account.gamejam.example"] },
  ]);
  // A mixed grant with agent runs keeps every agent-run operation.
  const mixed = await grant("agent-runs agent-forks", [["pool", "workers"], ["agent_forks", "alpha"]]);
  assert.equal(mixed.tokens.scope, "agent-runs agent-forks");
  assert.deepEqual(mints(controller).at(-1)!.operations, ["agent_runs.start", "agent_runs.get", "agent_runs.list", "agent_runs.cancel", ...AGENT_FORK_OPERATIONS]);
});

test("the resource proxy passes agent-fork routes with their queries, principal headers and diff text", async (t) => {
  const controller = fakeController(t);
  const { tokens } = await grant("engine-sessions agent-forks", [["engine_sessions", "alpha"], ["agent_forks", "alpha"]]);
  const at = tokens.access_token as string;
  const fid = "f".repeat(32), rid = "a".repeat(32), sub = "b".repeat(32);
  const base = `v1/projects/alpha/engine-sessions/${"e".repeat(32)}`;
  const principal = {
    "EngineBox-Principal": "gj:room1:usr_42",
    "EngineBox-Principal-Name": "Ada L.",
    "EngineBox-Principal-Role": "builder",
    "EngineBox-Principal-Time": "1793750400",
    "EngineBox-Principal-Nonce": "n0nce-123",
    "EngineBox-Principal-Sig": "c2lnbmF0dXJl",
  };
  const passes: [string, string, unknown?][] = [
    ["GET", `${base}/forks`],
    ["GET", `${base}/forks?all=1`],
    ["POST", `${base}/forks`, { request_id: "r1", prompt: "add a coin", provider: "claude" }],
    ["GET", `${base}/forks/${fid}`],
    ["POST", `${base}/forks/${fid}/runs`, { request_id: "r2", prompt: "again" }],
    ["GET", `${base}/forks/${fid}/runs/${rid}/events`],
    ["GET", `${base}/forks/${fid}/runs/${rid}/events?after=1234567890`],
    ["POST", `${base}/forks/${fid}/runs/${rid}/cancel`, {}],
    ["POST", `${base}/forks/${fid}/submit`, { request_id: "r3", message: "m", expected_head: "h" }],
    ["POST", `${base}/forks/${fid}/rebase`, { request_id: "r4" }],
    ["POST", `${base}/forks/${fid}/close`, {}],
    ["GET", `${base}/submissions`],
    ["GET", `${base}/submissions?state=awaiting_approval`],
    ["POST", `${base}/submissions/${sub}/approve`, { request_id: "r5", expected_fork_head: "h" }],
    ["POST", `${base}/submissions/${sub}/reject`, { reason: "no" }],
    ["GET", `${base}/dev`],
    ["POST", `${base}/dev/revert`, { request_id: "r6", submission: sub }],
    ["POST", `${base}/dev/publish`, { request_id: "r7", branch: "jam/x" }],
    ["POST", "v1/projects/alpha/gateway/connect", { return_to: "https://gamejam.example/room" }],
    ["GET", "v1/projects/alpha/gateway"],
    ["PUT", "v1/projects/alpha/gateway", { monthly_cap_usd: 10 }],
    ["DELETE", "v1/projects/alpha/gateway"],
  ];
  for (const [method, path, body] of passes) {
    const response = await call(path, at, method, body, principal);
    assert.equal(response.status, method === "POST" ? 202 : 200, `${method} ${path}`);
    const echoed = await response.json();
    // The query is forwarded verbatim; the principal reaches the controller.
    assert.equal(`${echoed.path.slice(1)}${echoed.query}`, path);
    assert.deepEqual(echoed.body, body ?? null);
    assert.equal(echoed.principal, "gj:room1:usr_42");
  }
  for (const c of controller.calls.filter((c) => c.path.includes("/forks") || c.path.includes("/gateway") || c.path.includes("/submissions") || c.path.endsWith("/dev") || c.path.includes("/dev/"))) {
    for (const [name, value] of Object.entries(principal)) assert.equal(c.headers.get(name), value, name);
    assert.equal(c.actor, null);
    assert.equal(c.auth, `Bearer ${at}`);
  }
  // DELETE has no body.
  assert.equal(controller.calls.find((c) => c.method === "DELETE")!.body, undefined);
  // The diff is text/x-diff, passed through as text with its query.
  const diff = await call(`${base}/forks/${fid}/diff?path=Assets%2FScenes%2FMain.unity&limit=2097152`, at);
  assert.equal(diff.status, 200);
  assert.equal(diff.headers.get("content-type"), "text/x-diff; charset=utf-8");
  assert.equal(diff.headers.get("cache-control"), "no-store");
  assert.equal(diff.headers.get("x-content-type-options"), "nosniff");
  assert.equal(await diff.text(), "diff --git a/x b/x\n+?path=Assets%2FScenes%2FMain.unity&limit=2097152\n");
  // On other routes the principal headers are dropped.
  assert.equal((await call(`${base}`, at, "GET", undefined, principal)).status, 404, "the fake has no such session; only the headers matter");
  const engine = controller.calls.at(-1)!;
  assert.equal(engine.path, `/${base}`);
  for (const name of Object.keys(principal)) assert.equal(engine.headers.get(name), null, name);
  // Agent-fork bodies may be up to 64 KiB; elsewhere 16 KiB stays.
  assert.equal((await call(`${base}/forks`, at, "POST", { prompt: "x".repeat(40000) })).status, 202);
  assert.equal((await call(`${base}/forks`, at, "POST", { prompt: "x".repeat(70000) })).status, 413);
  assert.equal((await call("v1/projects/alpha/engine-sessions", at, "POST", { prompt: "x".repeat(40000) })).status, 413);
});

test("agent-fork queries, headers and shapes are checked at the edge", async (t) => {
  const controller = fakeController(t);
  const { tokens } = await grant("engine-sessions agent-forks", [["engine_sessions", "alpha"], ["agent_forks", "alpha"]]);
  const at = tokens.access_token as string;
  const fid = "f".repeat(32), rid = "a".repeat(32);
  const base = `v1/projects/alpha/engine-sessions/${"e".repeat(32)}`;
  const before = controller.calls.length;
  for (const path of [
    `${base}/forks?all=2`,
    `${base}/forks?all=1&all=1`,
    `${base}/forks?mine=1`,
    `${base}/forks/${fid}?all=1`,
    `${base}/forks/${fid}/diff?path=`,
    `${base}/forks/${fid}/diff?path=${"a".repeat(301)}`,
    `${base}/forks/${fid}/diff?path=a%0Ab`,
    `${base}/forks/${fid}/diff?limit=0`,
    `${base}/forks/${fid}/diff?limit=2097153`,
    `${base}/forks/${fid}/diff?limit=1&limit=2`,
    `${base}/forks/${fid}/diff?path=a&x=1`,
    `${base}/forks/${fid}/diff?path=${"%41".repeat(180)}`,
    `${base}/forks/${fid}/runs/${rid}/events?after=12345678901`,
    `${base}/forks/${fid}/runs/${rid}/events?after=-1`,
    `${base}/submissions?state=Awaiting`,
    `${base}/submissions?state=${"a".repeat(25)}`,
    `${base}/dev?all=1`,
    "v1/projects/alpha/gateway?x=1",
    "v1/projects/alpha/engine-sessions?all=1",
    "v1/agent-runs?all=1",
  ])
    assert.equal((await call(path, at)).status, 400, path);
  for (const [name, value] of [
    ["EngineBox-Principal", "x".repeat(513)],
    ["EngineBox-Principal-Name", "Ada\u0001"],
    ["EngineBox-Principal-Sig", "a\tb"],
  ])
    assert.equal((await call(`${base}/forks`, at, "GET", undefined, { [name]: value })).status, 400, name);
  assert.equal(controller.calls.length, before, "nothing refused reached the controller");
  for (const [method, path] of [
    ["GET", `${base}/forks/not-a-fork`],
    ["DELETE", `${base}/forks/${fid}`],
    ["PUT", `${base}/forks/${fid}`],
    ["GET", `${base}/forks/${fid}/submit`],
    ["POST", `${base}/forks/${fid}/merge`],
    ["POST", `${base}/forks/${fid}/runs/${rid}`],
    ["POST", `${base}/submissions/${fid}/merge`],
    ["GET", `${base}/dev/revert`],
    ["POST", `${base}/dev`],
    ["PATCH", "v1/projects/alpha/gateway"],
    ["GET", "v1/projects/alpha/gateway/connect"],
    ["DELETE", "v1/projects/alpha/engine-sessions"],
    ["GET", "v1/projects/Alpha/gateway"],
  ] as const)
    assert.equal((await call(path, at, method, method === "POST" || method === "PUT" ? {} : undefined)).status, 404, `${method} ${path}`);
  // The route module exposes PUT and DELETE for the gateway.
  for (const method of ["PUT", "DELETE"] as const) {
    const response = await resourceRoute[method](
      new Request(`${origin}/v1/projects/alpha/gateway`, {
        method,
        headers: { authorization: `Bearer ${at}`, ...(method === "PUT" ? { "content-type": "application/json" } : {}) },
        body: method === "PUT" ? "{}" : undefined,
      }),
      { params: Promise.resolve({ path: ["projects", "alpha", "gateway"] }) },
    );
    assert.equal(response.status, 200, method);
  }
});

test("a token without the agent-forks grant is refused with the agent-forks challenge", async (t) => {
  fakeController(t);
  const { tokens } = await grant("engine-sessions", [["engine_sessions", "alpha"]]);
  const refused = await call(`v1/projects/alpha/engine-sessions/${"e".repeat(32)}/forks`, tokens.access_token);
  assert.equal(refused.status, 403);
  assert.match(refused.headers.get("www-authenticate")!, /error="insufficient_scope".*scope="agent-forks"/);
  const gateway = await call("v1/projects/alpha/gateway", tokens.access_token);
  assert.equal(gateway.status, 403);
  assert.match(gateway.headers.get("www-authenticate")!, /scope="agent-forks"/);
  // Another project of a fork grant is refused by the controller the same way.
  const forks = await grant("agent-forks", [["agent_forks", "alpha"]]);
  const elsewhere = await call("v1/projects/beta/gateway", forks.tokens.access_token);
  assert.equal(elsewhere.status, 403);
  assert.match(elsewhere.headers.get("www-authenticate")!, /scope="agent-forks"/);
  assert.equal((await call("v1/projects/alpha/engine-sessions", forks.tokens.access_token)).status, 403, "no engine-session operation");
});

test("the Xava Inference callback forwards state and code with owner credentials and sends the browser back", async (t) => {
  const seen: { url: string; auth: string | null; actor: string | null; org: string | null; body: unknown }[] = [];
  let answer: Response = Response.json({ return_to: "https://gamejam.example/room/abc?connected=1" });
  t.mock.method(globalThis, "fetch", async (input: string, init: RequestInit = {}) => {
    const headers = new Headers(init.headers);
    seen.push({ url: input, auth: headers.get("authorization"), actor: headers.get("x-enginebox-actor"), org: headers.get("x-enginebox-org"), body: JSON.parse(String(init.body)) });
    return answer.clone();
  });
  const get = (query: string) => callbackRoute.GET(new Request(`${origin}/v1/gateway/xinf/callback${query}`));
  const ok = await get("?state=st_ate-1&code=abc.DEF~1&iss=https%3A%2F%2Fxavainf.com");
  assert.equal(ok.status, 303);
  assert.equal(ok.headers.get("location"), "https://gamejam.example/room/abc?connected=1");
  assert.equal(ok.headers.get("cache-control"), "no-store");
  assert.equal(ok.headers.get("referrer-policy"), "no-referrer");
  assert.deepEqual(seen[0], { url: "http://127.0.0.1:8765/v1/gateway/xinf/callback", auth: `Bearer ${ownerToken}`, actor: null, org: null, body: { state: "st_ate-1", code: "abc.DEF~1" } });
  // A provider error is forwarded too (its description is ignored).
  assert.equal((await get("?state=s1&error=access_denied&error_description=nope")).status, 303);
  assert.deepEqual(seen[1].body, { state: "s1", error: "access_denied" });
  // A return address that is not absolute https is refused.
  for (const target of ["http://gamejam.example/room", "javascript:alert(1)", "/room", "https://user:pw@gamejam.example/"]) {
    answer = Response.json({ return_to: target });
    const refused = await get("?state=s1&code=c");
    assert.equal(refused.status, 502, target);
    assert.equal(refused.headers.get("location"), null);
    assert.match(refused.headers.get("content-type")!, /text\/html/);
  }
  // A controller refusal shows its error on a page.
  answer = Response.json({ error: "This connection request expired. Start again from the room." }, { status: 400 });
  const expired = await get("?state=s1&code=c");
  assert.equal(expired.status, 400);
  assert.match(await expired.text(), /This connection request expired\. Start again from the room\./);
  answer = Response.json({ error: "<script>x</script>" }, { status: 500 });
  const failed = await get("?state=s1&code=c");
  assert.equal(failed.status, 502);
  assert.match(await failed.text(), /&lt;script&gt;/);
  // Malformed callbacks never reach the controller.
  const calls = seen.length;
  for (const query of [
    "",
    "?code=c",
    "?state=&code=c",
    "?state=a%2Fb&code=c",
    `?state=${"s".repeat(129)}&code=c`,
    "?state=s1",
    "?state=s1&code=c&error=access_denied",
    `?state=s1&code=${"c".repeat(513)}`,
    "?state=s1&code=c%0A",
    "?state=s1&error=Access-Denied",
    `?state=s1&error=${"e".repeat(65)}`,
    "?state=s1&state=s2&code=c",
  ]) {
    const refused = await get(query);
    assert.equal(refused.status, 400, query);
    assert.match(refused.headers.get("content-type")!, /text\/html/);
  }
  assert.equal(seen.length, calls);
});

test("the node proxy accepts the fork actions with their body limits", async (t) => {
  const node = `${"a".repeat(32)}.${"b".repeat(64)}`;
  const paths: string[] = [];
  t.mock.method(globalThis, "fetch", async (url: string, init: RequestInit) => {
    assert.equal((init.headers as Record<string, string>).Authorization, `Bearer ${node}`);
    paths.push(url);
    return Response.json({ ok: true });
  });
  const post = (action: string, bytes: number) =>
    nodeProxy(
      new Request(`${origin}/api/node/${action}`, {
        method: "POST",
        headers: { Authorization: `Bearer ${node}`, "Content-Type": "application/json" },
        body: JSON.stringify({ data: "x".repeat(bytes - 11) }),
      }),
      action,
    );
  const tooLarge = (e: unknown) => e instanceof HttpError && e.status === 413;
  for (const [action, limit] of [["fork-report", 1048576], ["fork-events", 300000], ["fork-artifact", 3145728]] as const) {
    assert.equal((await post(action, limit)).status, 200, action);
    await assert.rejects(post(action, limit + 1), tooLarge, action);
  }
  // Other actions keep their limit.
  assert.equal((await post("engine-report", 350000)).status, 200);
  await assert.rejects(post("engine-report", 350001), tooLarge);
  await assert.rejects(post("fork-delete", 20), (e: unknown) => e instanceof HttpError && e.status === 404);
  assert.deepEqual(paths, ["fork-report", "fork-events", "fork-artifact", "engine-report"].map((a) => `http://127.0.0.1:8765/v1/node/${a}`));
});
