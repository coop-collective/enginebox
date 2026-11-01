import assert from "node:assert/strict";
import { createHmac, randomBytes } from "node:crypto";
import { rmSync } from "node:fs";
import { test, beforeEach, afterEach } from "node:test";
import * as sessionRoute from "../app/api/session/route";
import {
  authenticated,
  config,
  cookie,
  HttpError,
  proxy,
  requireOrigin,
} from "../lib/server";
import { memberCookie, memberSessionHeader, useIdentityDir } from "./session-fixture";

const origin = "http://127.0.0.1:3000";
const token = "test-only-backend-token-never-in-browser";
let identityDir = "";
beforeEach(() => {
  identityDir = useIdentityDir();
  process.env.ENGINEBOX_SESSION_SECRET =
    "test-only-session-signing-secret-123456789";
  process.env.ENGINEBOX_API_TOKEN = token;
  process.env.ENGINEBOX_DASHBOARD_ORIGIN = origin;
  process.env.ENGINEBOX_API_URL = "http://127.0.0.1:8765";
});
afterEach(() => rmSync(identityDir, { recursive: true, force: true }));

function request(
  path = "/api/control/v1/fleet",
  method = "GET",
  body?: object,
  authenticatedCookie = true,
) {
  return new Request(`${origin}${path}`, {
    method,
    headers: {
      origin,
      "content-type": "application/json",
      ...(authenticatedCookie ? { cookie: memberCookie() } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  });
}

test("session is signed, expires and never contains the backend token", () => {
  const now = Date.now();
  const header = memberSessionHeader(undefined, now);
  assert.match(header, /HttpOnly; Path=\/; SameSite=Strict; Max-Age=28800/);
  assert.ok(!header.includes(token));
  const req = new Request(origin, {
    headers: { cookie: header.split(";")[0] },
  });
  assert.ok(authenticated(req, now));
  assert.ok(!authenticated(req, now + 28_800_001));
  const tampered = header.split(";")[0].replace("=", "=x");
  assert.ok(
    !authenticated(new Request(origin, { headers: { cookie: tampered } }), now),
  );
});

test("rotating the session secret invalidates an existing session", () => {
  const req = request();
  assert.equal(authenticated(req), true);
  process.env.ENGINEBOX_SESSION_SECRET = "a-different-session-signing-secret-12345678";
  assert.equal(authenticated(req), false);
});

test("operator cookies from the retired password sign-in are rejected", () => {
  const secret = process.env.ENGINEBOX_SESSION_SECRET!;
  for (const fields of [{}, { kind: "operator" }]) {
    const payload = Buffer.from(JSON.stringify({ ...fields, expires: Date.now() + 3_600_000, nonce: randomBytes(24).toString("hex") })).toString("base64url");
    const legacy = createHmac("sha256", secret).update("old-operator-password").update("\0").update(payload).digest("base64url");
    const member = createHmac("sha256", secret).update("enginebox-member-session\0").update(payload).digest("base64url");
    for (const signature of [legacy, member])
      assert.equal(authenticated(new Request(origin, { headers: { cookie: `enginebox_session=${payload}.${signature}` } })), false);
  }
});

test("email sign-in is required: no identity state directory, no dashboard", () => {
  delete process.env.ENGINEBOX_IDENTITY_STATE_DIR;
  assert.throws(config, (error: unknown) => error instanceof HttpError && error.status === 503 && /ENGINEBOX_IDENTITY_STATE_DIR/.test(error.message));
});

test("HTTPS cookies use Secure and logout expires the session", () => {
  process.env.ENGINEBOX_DASHBOARD_ORIGIN = "https://enginebox.example";
  assert.match(memberSessionHeader(), /; Secure$/);
  assert.match(cookie("", 0), /Max-Age=0; Secure$/);
});

test("missing, cross-site and prefix origins are rejected", () => {
  for (const value of [
    "",
    "http://127.0.0.1:3000.attacker.example",
    "https://attacker.example",
    "null",
  ]) {
    assert.throws(
      () =>
        requireOrigin(
          new Request(origin, { headers: value ? { origin: value } : {} }),
        ),
      (error: unknown) => error instanceof HttpError && error.status === 403,
    );
  }
  requireOrigin(new Request(origin, { headers: { origin } }));
});

test("the session route offers no password sign-in", async () => {
  assert.equal("POST" in sessionRoute, false);
  delete process.env.ENGINEBOX_EMAIL_TRANSPORT;
  delete process.env.ENGINEBOX_EMAIL_ENDPOINT;
  const response = await sessionRoute.GET(new Request(origin));
  assert.deepEqual(await response.json(), { authenticated: false, me: null, email_delivery: false });
});

test("unauthenticated proxy requests never reach the backend", async (t) => {
  const fetch = t.mock.method(globalThis, "fetch", () => {
    throw new Error("must not fetch");
  });
  await assert.rejects(
    proxy(request(undefined, "GET", undefined, false), ["v1", "fleet"]),
    (error: unknown) => error instanceof HttpError && error.status === 401,
  );
  assert.equal(fetch.mock.callCount(), 0);
});

test("proxy sends the backend token only from the server and preserves 202 jobs", async (t) => {
  t.mock.method(
    globalThis,
    "fetch",
    async (url: string, options: RequestInit) => {
      assert.equal(url, "http://127.0.0.1:8765/v1/hosts/test-host/instances");
      assert.equal(
        (options.headers as Record<string, string>).Authorization,
        `Bearer ${token}`,
      );
      assert.equal(options.redirect, "error");
      assert.deepEqual(JSON.parse(options.body as string), {
        name: "ebox-test",
        template: "native",
        size: "small",
      });
      return Response.json(
        { job: { id: "job-1", status: "queued" } },
        { status: 202 },
      );
    },
  );
  const response = await proxy(
    request(undefined, "POST", {
      name: "ebox-test",
      template: "native",
      size: "small",
    }),
    ["v1", "hosts", "test-host", "instances"],
  );
  assert.equal(response.status, 202);
  assert.equal(response.headers.get("cache-control"), "no-store");
  assert.ok(!(await response.text()).includes(token));
});

test("proxy rejects cross-origin mutations, traversal and arbitrary endpoints", async (t) => {
  const fetch = t.mock.method(globalThis, "fetch", () => {
    throw new Error("must not fetch");
  });
  const mutation = request(undefined, "POST", { action: "stop" });
  mutation.headers.set("origin", "https://attacker.example");
  await assert.rejects(
    proxy(mutation, [
      "v1",
      "hosts",
      "host",
      "instances",
      "ebox-test",
      "actions",
    ]),
    (error: unknown) => error instanceof HttpError && error.status === 403,
  );
  for (const path of [
    ["v1", "secrets"],
    ["v1", "jobs", "../fleet"],
    ["v1", "jobs", "//attacker.example"],
  ]) {
    await assert.rejects(
      proxy(request(), path),
      (error: unknown) => error instanceof HttpError && error.status === 404,
    );
  }
  assert.equal(fetch.mock.callCount(), 0);
});

test("backend auth errors are useful without returning backend response contents", async (t) => {
  t.mock.method(globalThis, "fetch", async () =>
    Response.json({ secret: token }, { status: 401 }),
  );
  await assert.rejects(
    proxy(request(), ["v1", "fleet"]),
    (error: unknown) =>
      error instanceof HttpError &&
      error.status === 502 &&
      !error.message.includes(token),
  );
});

test("plain HTTP remote URLs and a session secret shared with the API token are refused", () => {
  process.env.ENGINEBOX_API_URL = "http://remote.example:8765";
  assert.throws(config, /HTTPS/);
  process.env.ENGINEBOX_API_URL = "http://127.0.0.1:8765";
  process.env.ENGINEBOX_SESSION_SECRET = token;
  assert.throws(config, /separate/);
});

test("oversized mutation bodies are refused before reaching the backend", async (t) => {
  const fetch = t.mock.method(globalThis, "fetch", () => {
    throw new Error("must not fetch");
  });
  await assert.rejects(
    proxy(request(undefined, "POST", { name: "a".repeat(9000) }), [
      "v1",
      "hosts",
      "host",
      "instances",
    ]),
    (error: unknown) => error instanceof HttpError && error.status === 413,
  );
  assert.equal(fetch.mock.callCount(), 0);
});

test("allocation and template endpoints are allowed without widening other proxy paths", async (t) => {
  const fetch = t.mock.method(
    globalThis,
    "fetch",
    async (_url: string, options: RequestInit) => {
      assert.equal(
        (options.headers as Record<string, string>).Authorization,
        `Bearer ${token}`,
      );
      return Response.json(
        { job: { id: "job-2", status: "queued" } },
        { status: 202 },
      );
    },
  );
  for (const [method, path] of [
    ["GET", "v1/templates"],
    ["POST", "v1/templates"],
    ["POST", "v1/hosts/test-host/templates/native-dev/install"],
    ["POST", "v1/hosts/test-host/allocations"],
  ]) {
    const response = await proxy(
      request(undefined, method, method === "POST" ? {} : undefined),
      path.split("/"),
    );
    assert.equal(response.status, 202);
  }
  assert.equal(fetch.mock.callCount(), 4);
  for (const path of [
    "v1/hosts/test-host/allocations/ebox-test/actions",
    "v1/hosts/test-host/templates/native-dev/delete",
    "v1/templates/arbitrary",
  ])
    await assert.rejects(
      proxy(request(undefined, "POST", {}), path.split("/")),
      (err: unknown) => err instanceof HttpError && err.status === 404,
    );
  assert.equal(fetch.mock.callCount(), 4);
});

test("repository proxy routes list and create only; tokens and backend configuration stay off the dashboard", async (t) => {
  const fetch = t.mock.method(globalThis, "fetch", async () => Response.json({ repositories: [] }, { status: 200 }));
  for (const [method, path] of [
    ["GET", "v1/pools"],
    ["GET", "v1/repositories"],
    ["GET", "v1/repository-backends"],
    ["GET", "v1/projects/alpha/repositories"],
    ["GET", "v1/projects/alpha/repositories/game.v2"],
    ["POST", "v1/projects/alpha/repositories"],
  ]) {
    const response = await proxy(request(undefined, method, method === "POST" ? {} : undefined), path.split("/"));
    assert.equal(response.status, 200, `${method} ${path}`);
  }
  assert.equal(fetch.mock.callCount(), 6);
  for (const [method, path] of [
    ["POST", "v1/projects/alpha/repositories/game/tokens"],
    ["POST", "v1/repository-backends/artifacts-worker"],
    ["GET", "v1/repository-backends/artifacts-worker"],
    ["POST", "v1/repositories"],
    ["GET", "v1/projects/Alpha/repositories"],
    ["GET", "v1/projects/alpha/repositories/game/tokens"],
    ["POST", "v1/pools"],
  ])
    await assert.rejects(
      proxy(request(undefined, method, method === "POST" ? {} : undefined), path.split("/")),
      (err: unknown) => err instanceof HttpError && err.status === 404,
      `${method} ${path}`,
    );
  assert.equal(fetch.mock.callCount(), 6);
});

test("template registration alone accepts larger manifests up to one MiB", async (t) => {
  const fetch = t.mock.method(globalThis, "fetch", async () =>
    Response.json({ ok: true }),
  );
  const manifest = { description: "a".repeat(9000) };
  assert.equal(
    (await proxy(request(undefined, "POST", { manifest }), ["v1", "templates"]))
      .status,
    200,
  );
  await assert.rejects(
    proxy(
      request(undefined, "POST", {
        manifest: { description: "a".repeat(1024 * 1024) },
      }),
      ["v1", "templates"],
    ),
    (err: unknown) => err instanceof HttpError && err.status === 413,
  );
  assert.equal(fetch.mock.callCount(), 1);
});

test("CI template proxy admits only catalog and authenticated bounded mutations", async (t) => {
  const fetch = t.mock.method(globalThis, "fetch", async () => Response.json({ ok: true }));
  for (const [method, path] of [
    ["GET", "v1/ci/templates"], ["POST", "v1/ci/templates"],
    ["POST", "v1/ci/templates/node-linux/enable"], ["POST", "v1/ci/templates/node-linux/disable"],
    ["POST", "v1/ci/templates/node-linux/retry"],
  ]) assert.equal((await proxy(request(undefined, method, method === "POST" ? {} : undefined), path.split("/"))).status, 200);
  assert.equal(fetch.mock.callCount(), 5);
  for (const path of ["v1/ci/templates/node-linux/delete", "v1/ci/templates/node-linux/enable/extra", "v1/ci/runs"])
    await assert.rejects(proxy(request(undefined, "POST", {}), path.split("/")), (err: unknown) => err instanceof HttpError && err.status === 404);
  await assert.rejects(proxy(request(undefined, "POST", { manifest: "x".repeat(65536) }), ["v1", "ci", "templates"]), (err: unknown) => err instanceof HttpError && err.status === 413);
  await assert.rejects(proxy(request(undefined, "GET", undefined, false), ["v1", "ci", "templates"]), (err: unknown) => err instanceof HttpError && err.status === 401);
  assert.equal(fetch.mock.callCount(), 5);
});

test("native account proxy routes stay scoped to known providers and session operations", async (t) => {
  const fetch = t.mock.method(globalThis, "fetch", async () =>
    Response.json({ ok: true }),
  );
  const base = "v1/agents";
  const session = "1234567890abcdef1234567890abcdef";
  for (const [method, path] of [
    ["GET", base],
    ["GET", `${base}/codex/sessions/${session}`],
    ["GET", `${base}/codex/usage`],
    ["GET", `${base}/claude/usage`],
    ["GET", `${base}/cursor/usage`],
    ["POST", `${base}/codex/login`],
    ["POST", `${base}/cursor/login`],
    ["POST", "v1/templates/native-dev/sync"],
    ["POST", `${base}/claude/sessions/${session}/input`],
    ["POST", `${base}/cursor/sessions/${session}/cancel`],
  ])
    assert.equal(
      (
        await proxy(
          request(undefined, method, method === "POST" ? {} : undefined),
          path.split("/"),
        )
      ).status,
      200,
    );
  const accepted = fetch.mock.callCount();
  for (const [method, path] of [
    ["GET", `${base}/codex/login`],
    ["POST", `${base}/codex/usage`],
    ["GET", `${base}/deepseek/usage`],
    ["POST", `${base}/deepseek/login`],
    ["POST", `${base}/codex/sessions/${session}/input`],
    ["POST", `${base}/claude/sessions/not-a-session/cancel`],
    ["POST", `${base}/claude/sessions/${session}/delete`],
    ["POST", `${base}/claude/logout`],
    ["GET", "v1/hosts/test-host/instances/ebox-test/accounts"],
  ])
    await assert.rejects(
      proxy(
        request(undefined, method, method === "POST" ? {} : undefined),
        path.split("/"),
      ),
      (err: unknown) => err instanceof HttpError && err.status === 404,
    );
  const crossOrigin = request(undefined, "POST", { text: "fixture-code" });
  crossOrigin.headers.set("origin", "https://attacker.example");
  await assert.rejects(
    proxy(crossOrigin, `${base}/claude/sessions/${session}/input`.split("/")),
    (err: unknown) => err instanceof HttpError && err.status === 403,
  );
  await assert.rejects(
    proxy(
      request(undefined, "POST", { text: "a".repeat(9000) }),
      `${base}/claude/sessions/${session}/input`.split("/"),
    ),
    (err: unknown) => err instanceof HttpError && err.status === 413,
  );
  assert.equal(fetch.mock.callCount(), accepted);
});

test("storage proxy exposes bounded operations and never a general command or restore endpoint", async (t) => {
  const fetch = t.mock.method(globalThis, "fetch", async () => Response.json({ job: { id: "storage-1", status: "queued" } }, { status: 202 }));
  for (const operation of ["inspect", "list", "preview", "import", "fork", "deps", "checkpoint", "remount", "close", "recover", "github-preview", "github-import", "github-fork", "github-settings", "github-promote", "github-delivery"]) {
    const response = await proxy(request(undefined, "POST", {}), ["v1", "storage", "hosts", "node", operation]);
    assert.equal(response.status, 202);
  }
  assert.equal((await proxy(request(), ["v1", "storage"])).status, 202);
  for (const operation of ["exec", "restore-checkpoint", "delete", "attach", "../inspect"]) {
    await assert.rejects(proxy(request(undefined, "POST", {}), ["v1", "storage", "hosts", "node", operation]),
      (error: unknown) => error instanceof HttpError && error.status === 404);
  }
  await assert.rejects(proxy(request(), ["v1", "storage", "hosts", "node", "fork"]),
    (error: unknown) => error instanceof HttpError && error.status === 404);
  const crossOrigin = request(undefined, "POST", {});
  crossOrigin.headers.set("origin", "https://attacker.example");
  await assert.rejects(proxy(crossOrigin, ["v1", "storage", "hosts", "node", "import"]),
    (error: unknown) => error instanceof HttpError && error.status === 403);
  assert.equal(fetch.mock.callCount(), 17);
});

test("workspace summary, approved source linking and pool runs preserve server-only credentials", async t => {
  const fetch = t.mock.method(globalThis, "fetch", async (_url: string, options: RequestInit) => {
    assert.equal((options.headers as Record<string, string>).Authorization, `Bearer ${token}`);
    return Response.json({ ok: true });
  });
  for (const [method, path, body] of [
    ["GET", "v1/workspaces", undefined],
    ["POST", "v1/projects/app/repositories/github", { name: "main", full_name: "fixture/private" }],
    ["POST", "v1/pools/app-pool/agent-runs", { provider: "codex", prompt: "Inspect source" }],
  ] as const) {
    const response = await proxy(request(`/api/control/${path}`, method, body), path.split("/"));
    assert.equal(response.status, 200);
  }
  assert.equal(fetch.mock.callCount(), 3);
  for (const path of ["v1/workspaces/secret", "v1/projects/app/repositories/github/tokens", "v1/pools/app-pool/agent-runs/arbitrary/cancel"]) {
    await assert.rejects(proxy(request(`/api/control/${path}`, "POST", {}), path.split("/")), (e: unknown) => e instanceof HttpError && e.status === 404);
  }
});
