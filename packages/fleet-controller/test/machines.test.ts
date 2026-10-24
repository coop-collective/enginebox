import { env } from "cloudflare:workers";
import { reset, runInDurableObject, evictDurableObject } from "cloudflare:test";
import { afterEach, expect, it } from "vitest";
import worker from "../src/index.ts";
import { sha256, validateNode } from "@enginebox/fleet-protocol";
// Tests inspect private records without expanding every production RPC method type.
const inspectDO = runInDurableObject as unknown as (
  stub: unknown,
  callback: (instance: any) => void | Promise<void>,
) => Promise<void>;
const owner = "local-tests-only-0123456789abcdef0123456789";
const localEnv = {
  ...env,
  MACHINE_TURN_CONFIG: JSON.stringify({
    local: true,
    iceServers: [
      {
        urls: "turn:127.0.0.1:3478",
        username: "fixture",
        credential: "fixture-only",
      },
    ],
    iceTransportPolicy: "all",
  }),
} as Env;
type Auth = string | { token: string; epoch: number; session: string };
async function req(
  path: string,
  body?: unknown,
  auth: Auth = owner,
  method?: string,
  client?: string,
  bindings: Env = localEnv,
) {
  const headers = new Headers({
    authorization: `Bearer ${typeof auth === "string" ? auth : auth.token}`,
  });
  if (typeof auth !== "string") {
    headers.set("x-enginebox-epoch", String(auth.epoch));
    headers.set("x-enginebox-session", auth.session);
  }
  if (client) headers.set("x-enginebox-stream-client", client);
  return worker.fetch(
    new Request(`http://fleet${path}`, {
      method: method ?? (body === undefined ? "GET" : "POST"),
      headers,
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    }),
    bindings,
  );
}
async function ok(
  path: string,
  body?: unknown,
  auth?: Auth,
  method?: string,
  client?: string,
): Promise<any> {
  const r = await req(path, body, auth, method, client);
  const v = await r.json();
  expect(r.ok, JSON.stringify(v)).toBe(true);
  return v;
}
const P = "/v1/projects/demo";
async function setup(allowControl = true) {
  await ok("/v1/projects", { id: "demo" });
  await ok("/v1/projects", { id: "other" });
  const app = (
    await ok("/v1/nodes", {
      id: "app",
      role: "agent",
      projects: ["demo"],
      origin: null,
      slots: 1,
      cacheBytes: 16777216,
      taskOnly: true,
    })
  ).token;
  const other = (
    await ok("/v1/nodes", {
      id: "other-app",
      role: "agent",
      projects: ["other"],
      origin: null,
      slots: 1,
      cacheBytes: 16777216,
      taskOnly: true,
    })
  ).token;
  const invite = await ok(`${P}/machine-invitations`, {
    id: "laptop",
    label: "My laptop",
    ownerAccountId: "account-owner",
    allowControl,
  });
  const token = `laptop.${"a".repeat(64)}`;
  await ok(
    "/v1/node/enroll",
    { nodeSecretHash: await sha256(token) },
    invite.token,
  );
  const session = crypto.randomUUID();
  const hello = await ok("/v1/node/hello", { session }, token);
  const host = { token, session, epoch: hello.epoch };
  await ok(
    "/v1/node/heartbeat",
    { stats: { webrtc: 1, video: 1, input: 1 } },
    host,
  );
  return { app, other, invite, host };
}
async function grant(
  id = "grant-one",
  sessionId = "session-one",
  mode = "watch",
  actorId = "account-owner",
) {
  return ok(`${P}/machine-session-grants`, {
    id,
    sessionId,
    nodeId: "laptop",
    actorId,
    mode,
  });
}
const client = "b".repeat(64);
async function open(app: string, id = "session-one", mode = "watch") {
  const g = await grant(`grant-${id}`, id, mode);
  return {
    grant: g,
    ...(await ok(
      `${P}/machine-sessions`,
      { id, grant: g.grant, clientSecretHash: await sha256(client) },
      app,
    )),
  };
}
async function changeRecord(key: string, fn: (value: any) => void) {
  await inspectDO(env.PROJECTS.getByName("demo") as any, (instance: any) => {
    const value = instance.get(key);
    fn(value);
    instance.put(key, value);
  });
}
afterEach(async () => {
  await reset();
});

it("registers only invited single-project native identities and survives claim lost-ack/eviction", async () => {
  const { invite, host, app, other } = await setup();
  const repeat = await ok(
    "/v1/node/enroll",
    { nodeSecretHash: await sha256(host.token) },
    invite.token,
  );
  expect(repeat.node.role).toBe("native");
  await evictDurableObject(env.NODES.getByName("laptop"));
  expect(
    (await ok("/v1/node/identity", undefined, host)).native.ownerAccountId,
  ).toBe("account-owner");
  expect(
    (
      await req(
        "/v1/node/enroll",
        { nodeSecretHash: "c".repeat(64) },
        invite.token,
      )
    ).status,
  ).toBe(409);
  expect(
    (
      await req(`${P}/machine-invitations`, {
        id: "laptop",
        label: "My laptop",
        ownerAccountId: "account-owner",
        allowControl: true,
      })
    ).status,
  ).toBe(409);
  expect((await req(`${P}/machines`, undefined, other)).status).toBe(403);
  expect((await ok(`${P}/machines`, undefined, app)).machines[0]).toMatchObject(
    { id: "laptop", online: true, allowControl: true },
  );
  for (const [path, method, body] of [
    ["/v1/projects/demo", "GET", undefined],
    ["/v1/node/poll", "POST", {}],
    ["/v1/node/catalog", "GET", undefined],
    ["/v1/node/finish", "POST", {}],
    [`${P}/tasks`, "GET", undefined],
    [`${P}/artifacts`, "GET", undefined],
    ["/v1/data/demo/chunks/" + "a".repeat(64), "GET", undefined],
    [`${P}/machines`, "GET", undefined],
  ] as const)
    expect((await req(path, body, host, method)).status, path).toBe(403);
});

it("keeps invitation settings/expiry immutable and revokes pending claims", async () => {
  await ok("/v1/projects", { id: "demo" });
  const body = {
    id: "pending",
    label: "Pending",
    ownerAccountId: "account-owner",
    allowControl: false,
  };
  const first = await ok(`${P}/machine-invitations`, body);
  expect(await ok(`${P}/machine-invitations`, body)).toEqual(first);
  expect(
    (await req(`${P}/machine-invitations`, { ...body, allowControl: true }))
      .status,
  ).toBe(409);
  await ok(`${P}/machines/pending/revoke`, { operationId: "revoke-one" });
  expect(
    (
      await req(
        "/v1/node/enroll",
        { nodeSecretHash: "a".repeat(64) },
        first.token,
      )
    ).status,
  ).toBe(401);
  expect((await req(`${P}/machine-invitations`, body)).status).toBe(409);
  const second = await ok(`${P}/machine-invitations`, {
    ...body,
    id: "expired",
  });
  await inspectDO(env.NODES.getByName("expired") as any, (instance: any) => {
    const value = instance.get("invitation");
    value.expires = Date.now() - 1;
    instance.put("invitation", value);
  });
  expect(
    (
      await req(
        "/v1/node/enroll",
        { nodeSecretHash: "a".repeat(64) },
        second.token,
      )
    ).status,
  ).toBe(401);
});

it("rejects native executor flags and direct native enrollment", async () => {
  const spec = {
    id: "native",
    role: "native",
    projects: ["demo"],
    origin: null,
    slots: 1,
    cacheBytes: 16777216,
    native: {
      label: "Host",
      ownerAccountId: "account-owner",
      allowControl: false,
    },
  };
  expect(validateNode(spec, false).native).toEqual(spec.native);
  for (const field of [
    "assistantProjects",
    "buildProjects",
    "mediaProjects",
    "integrationProjects",
  ])
    expect(() => validateNode({ ...spec, [field]: [] }, false)).toThrow();
  expect(() =>
    validateNode({ ...spec, projects: ["demo", "other"] }, false),
  ).toThrow();
  expect(() => validateNode({ ...spec, taskOnly: true }, false)).toThrow();
  expect((await req("/v1/nodes", spec)).status).toBe(400);
});

it("requires Account-issued grants, private client identity and project scope before opening or signaling", async () => {
  const { app, other, host } = await setup();
  expect(
    (
      await req(
        `${P}/machine-session-grants`,
        {
          id: "grant",
          sessionId: "one",
          nodeId: "laptop",
          actorId: "spoofed",
          mode: "control",
        },
        app,
      )
    ).status,
  ).toBe(403);
  const opened = await open(app);
  expect(opened.session).toMatchObject({
    actorId: "account-owner",
    nodeEpoch: host.epoch,
    mode: "watch",
  });
  const body = {
    id: "session-one",
    grant: opened.grant.grant,
    clientSecretHash: await sha256(client),
  };
  expect((await ok(`${P}/machine-sessions`, body, app)).session).toEqual(
    opened.session,
  );
  expect(
    (
      await req(
        `${P}/machine-sessions`,
        { ...body, clientSecretHash: "c".repeat(64) },
        app,
      )
    ).status,
  ).toBe(409);
  expect(
    (await req(`${P}/machine-sessions/session-one`, undefined, app)).status,
  ).toBe(403);
  expect(
    (
      await req(
        `${P}/machine-sessions/session-one`,
        undefined,
        other,
        undefined,
        client,
      )
    ).status,
  ).toBe(403);
  expect(
    (
      await ok(
        `${P}/machine-sessions/session-one`,
        undefined,
        app,
        undefined,
        client,
      )
    ).session.actorId,
  ).toBe("account-owner");
  expect(
    (await req(`${P}/machine-sessions/session-one/signals`, undefined, owner))
      .status,
  ).toBe(403);
  expect(
    (
      await req(
        `${P}/machine-sessions/session-one/signals`,
        { id: "bad", signal: { type: "frame", bytes: "abc" } },
        app,
        undefined,
        client,
      )
    ).status,
  ).toBe(400);
});

it("delivers bounded raw SDP/ICE metadata with replay-safe operations and private per-session ICE", async () => {
  const { app, host } = await setup();
  await open(app);
  const path = `${P}/machine-sessions/session-one/signals`;
  const offer = {
    id: "offer",
    signal: { type: "offer", sdp: "v=0\r\ns=fixture-offer\r\n" },
  };
  const sent = await ok(path, offer, app, undefined, client);
  expect(sent.sequence).toBe(1);
  expect((await ok(path, offer, app, undefined, client)).sequence).toBe(1);
  expect(
    (
      await req(
        path,
        { ...offer, signal: { type: "offer", sdp: "v=0\nchanged" } },
        app,
        undefined,
        client,
      )
    ).status,
  ).toBe(409);
  const hostInbox = await ok(path + "?after=0", undefined, host);
  expect(hostInbox.signals).toEqual([{ ...offer, sequence: 1 }]);
  await ok(
    path,
    {
      id: "answer",
      signal: { type: "answer", sdp: "v=0\r\ns=fixture-answer\r\n" },
    },
    host,
  );
  await ok(path, { id: "ice", signal: { type: "ice", candidate: null } }, host);
  const clientInbox = await ok(
    path + "?after=0",
    undefined,
    app,
    undefined,
    client,
  );
  expect(clientInbox.signals.map((s: any) => s.signal.type)).toEqual([
    "answer",
    "ice",
  ]);
  expect(clientInbox.cursor).toBe(3);
  expect(
    (await ok(path + "?after=3", undefined, app, undefined, client)).signals,
  ).toEqual([]);
  const poll = await ok("/v1/node/machine-sessions/poll", {}, host);
  expect(poll.sessions[0].iceServers[0].credential).toBe("fixture-only");
  expect(poll.sessions[0].iceExpires).toBeGreaterThan(Date.now());
  expect(
    JSON.stringify(await ok(`${P}/machines`, undefined, app)),
  ).not.toContain("fixture-only");
  expect(
    (
      await req(
        path,
        {
          id: "oversize",
          signal: {
            type: "ice",
            candidate: {
              candidate: "x".repeat(4097),
              sdpMid: null,
              sdpMLineIndex: null,
            },
          },
        },
        host,
      )
    ).status,
  ).toBe(400);
  await changeRecord("machine-session:session-one", (v) => {
    v.hostIce = 128;
  });
  expect(
    (
      await req(
        path,
        { id: "full", signal: { type: "ice", candidate: null } },
        host,
      )
    ).status,
  ).toBe(429);
});

it("allows only one control lease, retains authority across eviction, and releases it on end", async () => {
  const { app } = await setup();
  await open(app, "control-one", "control");
  await evictDurableObject(env.PROJECTS.getByName("demo"));
  const g = await grant("control-two-grant", "control-two", "control");
  expect(
    (
      await req(
        `${P}/machine-sessions`,
        {
          id: "control-two",
          grant: g.grant,
          clientSecretHash: await sha256(client),
        },
        app,
      )
    ).status,
  ).toBe(409);
  await ok(
    `${P}/machine-sessions/control-one/end`,
    { id: "end-one" },
    app,
    undefined,
    client,
  );
  await ok(
    `${P}/machine-sessions`,
    {
      id: "control-two",
      grant: g.grant,
      clientSecretHash: await sha256(client),
    },
    app,
  );
  expect(
    (
      await ok(
        `${P}/machine-sessions/control-one/end`,
        { id: "end-one" },
        app,
        undefined,
        client,
      )
    ).session.state,
  ).toBe("ended");
});

it("fences prior process UUIDs and all old sessions without allowing hello replay to steal epoch", async () => {
  const { app, host } = await setup();
  await open(app);
  const nextSession = crypto.randomUUID();
  const newer = await ok(
    "/v1/node/hello",
    { session: nextSession },
    host.token,
  );
  expect(newer.epoch).toBe(host.epoch + 1);
  expect(
    (await req("/v1/node/hello", { session: host.session }, host.token)).status,
  ).toBe(409);
  expect((await req("/v1/node/machine-sessions/poll", {}, host)).status).toBe(
    409,
  );
  expect(
    (
      await req(
        `${P}/machine-sessions/session-one/signals`,
        undefined,
        app,
        undefined,
        client,
      )
    ).status,
  ).toBe(410);
  expect(
    (
      await ok(
        `${P}/machine-sessions/session-one`,
        undefined,
        app,
        undefined,
        client,
      )
    ).session.reason,
  ).toBe("fenced");
  await inspectDO(
    env.PROJECTS.getByName("demo") as any,
    async (instance: any) => {
      await instance.syncMachine("laptop", host.epoch, false);
      expect(instance.get("machine-fence:laptop").epoch).toBe(newer.epoch);
    },
  );
});

it("revokes active sessions through both project and global owner paths", async () => {
  const { app, host } = await setup();
  await open(app);
  await ok("/v1/nodes/laptop/revoke", {});
  expect((await req("/v1/node/machine-sessions/poll", {}, host)).status).toBe(
    401,
  );
  expect(
    (
      await ok(
        `${P}/machine-sessions/session-one`,
        undefined,
        app,
        undefined,
        client,
      )
    ).session.reason,
  ).toBe("revoked");
  expect(
    (
      await req(
        `${P}/machine-sessions/session-one/renew`,
        { grant: (await Promise.resolve({ grant: "fake" })).grant },
        app,
        undefined,
        client,
      )
    ).status,
  ).toBe(403);
  expect(
    (
      await req(`${P}/machine-session-grants`, {
        id: "new-grant",
        sessionId: "new-session",
        nodeId: "laptop",
        actorId: "account-owner",
        mode: "watch",
      })
    ).status,
  ).toBe(409);
});

it("does not revive expired sessions or renew with another actor and caps lease at initial relay lifetime", async () => {
  const { app } = await setup();
  const first = await open(app);
  const different = await grant(
    "different-actor",
    "different-session",
    "watch",
    "account-other",
  );
  expect(
    (
      await req(
        `${P}/machine-sessions/session-one/renew`,
        { grant: different.grant },
        app,
        undefined,
        client,
      )
    ).status,
  ).toBe(403);
  const fresh = await grant("renew-one");
  await changeRecord("machine-session:session-one", (v) => {
    v.deadline = Date.now() + 10000;
    v.renewedAt = 0;
  });
  const renewed = await ok(
    `${P}/machine-sessions/session-one/renew`,
    { grant: fresh.grant },
    app,
    undefined,
    client,
  );
  expect(renewed.session.expires).toBeLessThanOrEqual(Date.now() + 10000);
  expect(
    (
      await ok(
        `${P}/machine-sessions/session-one/renew`,
        { grant: fresh.grant },
        app,
        undefined,
        client,
      )
    ).session.expires,
  ).toBe(renewed.session.expires);
  await changeRecord("machine-session:session-one", (v) => {
    v.view.expires = Date.now() - 1;
  });
  expect(
    (
      await req(
        `${P}/machine-sessions/session-one/renew`,
        { grant: fresh.grant },
        app,
        undefined,
        client,
      )
    ).status,
  ).toBe(410);
  expect(
    (
      await req(
        `${P}/machine-sessions`,
        {
          id: "session-one",
          grant: first.grant.grant,
          clientSecretHash: await sha256(client),
        },
        app,
      )
    ).status,
  ).toBe(409);
});

it("fails closed without configured TURN while registration/status remain available and denies disabled control", async () => {
  const { app } = await setup(false);
  expect(
    (
      await req(
        `${P}/machine-session-grants`,
        {
          id: "none",
          sessionId: "no-relay",
          nodeId: "laptop",
          actorId: "account-owner",
          mode: "watch",
        },
        owner,
        undefined,
        undefined,
        env as Env,
      )
    ).status,
  ).toBe(503);
  expect(
    (
      await req(`${P}/machine-session-grants`, {
        id: "control",
        sessionId: "no-control",
        nodeId: "laptop",
        actorId: "account-owner",
        mode: "control",
      })
    ).status,
  ).toBe(403);
  expect((await ok(`${P}/machines`, undefined, app)).machines[0].online).toBe(
    true,
  );
});

it("serializes competing identity claims and control admissions under actual concurrent requests", async () => {
  await ok("/v1/projects", { id: "demo" });
  const invitation = await ok(`${P}/machine-invitations`, {
    id: "race",
    label: "Race",
    ownerAccountId: "account-owner",
    allowControl: true,
  });
  const claims = await Promise.all(
    ["a", "b"].map((char) =>
      req(
        "/v1/node/enroll",
        { nodeSecretHash: char.repeat(64) },
        invitation.token,
      ),
    ),
  );
  expect(claims.map((r) => r.status).sort()).toEqual([200, 409]);
  const { app } = await setup();
  const [a, b] = await Promise.all([
    grant("race-a", "race-a", "control"),
    grant("race-b", "race-b", "control"),
  ]);
  const admitted = await Promise.all(
    [a, b].map((g) =>
      req(
        `${P}/machine-sessions`,
        { id: g.sessionId, grant: g.grant, clientSecretHash: "a".repeat(64) },
        app,
      ),
    ),
  );
  expect(admitted.map((r) => r.status).sort()).toEqual([201, 409]);
});

it("rejects grants from another project and caps live peers while keeping independent watch/control permissions", async () => {
  const { app, other, host } = await setup();
  const scoped = await grant();
  expect(
    (
      await req(
        "/v1/projects/other/machine-sessions",
        {
          id: "session-one",
          grant: scoped.grant,
          clientSecretHash: "a".repeat(64),
        },
        other,
      )
    ).status,
  ).toBe(403);
  for (let i = 0; i < 8; i++) await open(app, `watch-${i}`);
  const excess = await grant("excess", "excess");
  expect(
    (
      await req(
        `${P}/machine-sessions`,
        { id: "excess", grant: excess.grant, clientSecretHash: "a".repeat(64) },
        app,
      )
    ).status,
  ).toBe(409);
  await inspectDO(env.PROJECTS.getByName("demo") as any, (instance: any) => {
    const row = instance.get("machine-session:watch-0");
    row.view.expires = Date.now() - 1;
    instance.put("machine-session:watch-0", row);
  });
  expect(
    (await ok("/v1/node/machine-sessions/poll", {}, host)).sessions.length,
  ).toBe(7);
  await ok(
    `${P}/machine-sessions`,
    { id: "excess", grant: excess.grant, clientSecretHash: "a".repeat(64) },
    app,
  );
});

it("garbage collects expired renewal grants and reuses original session relay credentials", async () => {
  const { app } = await setup();
  const opened = await open(app);
  await inspectDO(env.PROJECTS.getByName("demo") as any, (instance: any) => {
    for (let i = 0; i < 4096; i++)
      instance.put(`machine-grant:expired-${i}`, {
        expires: Date.now() - 6 * 60_000,
      });
  });
  // No relay binding: renewal must reuse the session's original private ICE.
  const response = await req(
    `${P}/machine-session-grants`,
    {
      id: "renew-gc",
      sessionId: "session-one",
      nodeId: "laptop",
      actorId: "account-owner",
      mode: "watch",
    },
    owner,
    undefined,
    undefined,
    env as Env,
  );
  expect(response.status).toBe(201);
  const renewedGrant: any = await response.json();
  const renewed = await ok(
    `${P}/machine-sessions/session-one/renew`,
    { grant: renewedGrant.grant },
    app,
    undefined,
    client,
  );
  expect(renewed.iceExpires).toBe(opened.iceExpires);
  await inspectDO(env.PROJECTS.getByName("demo") as any, (instance: any) => {
    expect(instance.rows("machine-grant:").length).toBe(2);
  });
});
