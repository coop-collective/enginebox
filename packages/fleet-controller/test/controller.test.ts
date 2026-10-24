import { env } from "cloudflare:workers";
import { reset, runInDurableObject, evictDurableObject } from "cloudflare:test";
import { afterEach, expect, it } from "vitest";
import worker from "../src/index.ts";
import { canonical, sha256 } from "@enginebox/fleet-protocol";

const token = "local-tests-only-0123456789abcdef0123456789";
type Session = { token: string; epoch: number; session: string };
async function request(
  path: string,
  body?: unknown,
  auth: string | Session = token,
  method?: string,
) {
  const headers = new Headers({
    authorization: `Bearer ${typeof auth === "string" ? auth : auth.token}`,
  });
  if (typeof auth !== "string") {
    headers.set("x-enginebox-epoch", String(auth.epoch));
    headers.set("x-enginebox-session", auth.session);
  }
  return worker.fetch(
    new Request(`http://fleet${path}`, {
      method: method ?? (body === undefined ? "GET" : "POST"),
      headers,
      body:
        body === undefined
          ? undefined
          : typeof body === "string"
            ? body
            : JSON.stringify(body),
    }),
    env as Env,
  );
}
async function ok(
  path: string,
  body?: unknown,
  auth?: string | Session,
  method?: string,
): Promise<any> {
  const response = await request(path, body, auth, method);
  const data = await response.json();
  expect(response.ok, JSON.stringify(data)).toBe(true);
  return data;
}
async function setup(
  id = "agent-a",
  role = "agent",
  assistantProjects?: string[],
  slots = 1,
): Promise<Session> {
  await ok("/v1/projects", { id: "demo" });
  const { token: secret } = await ok("/v1/nodes", {
    id,
    role,
    projects: ["demo"],
    origin: role === "storage" ? "http://storage:8790" : null,
    slots,
    cacheBytes: 16777216,
    ...(assistantProjects ? { assistantProjects } : {}),
  });
  const session = crypto.randomUUID();
  const hello = await ok("/v1/node/hello", { session }, secret);
  return { token: secret, session, epoch: hello.epoch };
}
async function publish(text = "snapshot", parent: string | null = null, metadata: Record<string,string> = {}) {
  await ok("/v1/projects", { id: "demo" });
  const hash = await sha256(text);
  await ok(`/v1/data/demo/chunks/${hash}`, text, token, "PUT");
  const page = canonical([{ hash, bytes: text.length }]);
  const pageHash = await sha256(page);
  await ok(`/v1/data/demo/pages/${pageHash}`, page, token, "PUT");
  return ok("/v1/projects/demo/generations", {
    schema: 1,
    project: "demo",
    parent,
    format: "sqlite",
    snapshotHash: hash,
    snapshotBytes: text.length,
    pages: [
      {
        hash: pageHash,
        bytes: page.length,
        count: 1,
        contentBytes: text.length,
      },
    ],
    metadata,
  });
}
afterEach(async () => {
  await reset();
});

it("keeps assistant prompts and results out of storage inventory, catalog and raw job reads", async () => {
  const native = await setup("native-node", "agent", ["demo"]);
  const storage = await setup("storage-node", "storage");
  const generation = await publish();
  const prompt = "Private community request to create a task";
  const answer = "Private assistant response about task content";
  await ok(
    "/v1/projects/demo/assistant",
    { id: "private-turn", prompt },
    native,
  );
  const { assignment } = await ok("/v1/node/poll", {}, native);
  await ok(
    "/v1/node/finish",
    { assignment, ok: true, result: { text: answer } },
    native,
  );
  await ok("/v1/projects/demo/jobs", {
    id: "store-check",
    project: "demo",
    generation: generation.id,
    kind: "verify",
  });
  const inventory = await ok("/v1/projects/demo", undefined, storage);
  const catalog = await ok("/v1/node/catalog", undefined, storage);
  for (const value of [inventory, catalog[0]]) {
    expect(value.jobs.map((job: any) => job.spec.id)).toEqual(["store-check"]);
    expect(value.generations.map((g: any) => g.id)).toContain(generation.id);
    expect(JSON.stringify(value)).not.toContain(prompt);
    expect(JSON.stringify(value)).not.toContain(answer);
  }
  expect(
    (
      await ok(
        `/v1/projects/demo/generations/${generation.id}`,
        undefined,
        storage,
      )
    ).id,
  ).toBe(generation.id);
  for (const path of [
    "/v1/projects/demo/assistant/private-turn",
    "/v1/projects/demo/jobs/private-turn",
    "/v1/projects/demo/jobs/store-check",
  ])
    expect((await request(path, undefined, storage)).status).toBe(403);
  for (const actor of [token, native]) {
    const job = await ok(
      "/v1/projects/demo/jobs/private-turn",
      undefined,
      actor,
    );
    expect(job.spec.prompt).toBe(prompt);
    expect(JSON.parse(job.result).text).toBe(answer);
    expect(
      JSON.stringify(await ok("/v1/projects/demo", undefined, actor)),
    ).toContain(prompt);
  }
  await ok("/v1/nodes/storage-node/revoke", {});
  for (const path of ["/v1/projects/demo", "/v1/node/catalog"])
    expect((await request(path, undefined, storage)).status).toBe(401);
});

it("dispatches assistant metadata jobs only to capable nodes and preserves request identity", async () => {
  const app = await setup();
  expect(
    (await ok("/v1/projects/demo/assistant", undefined, app)).available,
  ).toBe(false);
  expect(
    (
      await request(
        "/v1/projects/demo/assistant",
        { id: "turn", prompt: "Create an issue" },
        app,
      )
    ).status,
  ).toBe(503);
  const native = await setup("native-node", "agent", ["demo"]);
  expect(
    (await ok("/v1/projects/demo/assistant", undefined, app)).available,
  ).toBe(true);
  const spec = { id: "turn", prompt: "Create an issue" };
  await ok("/v1/projects/demo/assistant", spec, app);
  await ok("/v1/projects/demo/assistant", spec, app);
  expect(
    (
      await request(
        "/v1/projects/demo/assistant",
        { ...spec, prompt: "Different instruction" },
        app,
      )
    ).status,
  ).toBe(409);
  expect((await ok("/v1/node/poll", {}, app)).assignment).toBeNull();
  const { assignment } = await ok("/v1/node/poll", {}, native);
  expect(assignment.kind).toBe("assistant");
  expect(assignment.generation).toBeNull();
  await ok(
    "/v1/node/finish",
    {
      assignment,
      ok: true,
      result: { text: "Created issue task-one", nativeRunId: "a".repeat(32) },
    },
    native,
  );
  expect(await ok("/v1/projects/demo/assistant/turn", undefined, app)).toEqual({
    id: "turn",
    status: "completed",
    text: "Created issue task-one",
  });
  const storage = await setup("storage", "storage");
  expect(
    (await request("/v1/projects/demo/assistant", undefined, storage)).status,
  ).toBe(403);
  expect(
    (await request("/v1/projects/another/assistant", undefined, app)).status,
  ).toBe(403);
});

it("fences restarted native reservations despite spare slots and an optimistic heartbeat", async () => {
  const native = await setup("native-node", "agent", ["demo"], 2);
  await ok(
    "/v1/projects/demo/assistant",
    { id: "turn", prompt: "Review the backlog" },
    native,
  );
  const { assignment } = await ok("/v1/node/poll", {}, native);
  const session = crypto.randomUUID();
  const hello = await ok("/v1/node/hello", { session }, native.token);
  const restarted = { ...native, session, epoch: hello.epoch };
  expect(
    (await ok("/v1/projects/demo/assistant", undefined, restarted)).available,
  ).toBe(false);
  await ok("/v1/projects/demo/jobs", {
    id: "another-turn",
    project: "demo",
    generation: null,
    kind: "assistant",
    prompt: "Another queued request",
    timeoutSeconds: 30,
  });
  expect(
    (
      await request(
        "/v1/node/reconcile",
        { attempt: assignment.attempt },
        restarted,
      )
    ).status,
  ).toBe(409);
  expect(
    (await ok("/v1/node/poll", {}, restarted)).reconciliationRequired,
  ).toBe(true);
  await ok(
    "/v1/node/heartbeat",
    { stats: { uncertainNativeJobs: 0 } },
    restarted,
  );
  expect(
    (await ok("/v1/projects/demo/assistant/turn", undefined, restarted))
      .blocked,
  ).toBe(true);
  expect(
    (await ok("/v1/projects/demo/assistant", undefined, restarted)).available,
  ).toBe(false);
  expect((await ok("/v1/node/poll", {}, restarted)).assignment).toBeNull();
  expect((await ok("/v1/projects/demo/jobs/another-turn")).status).toBe(
    "queued",
  );
  await ok("/v1/nodes/native-node/reconcile", { assignment, stopped: true });
  expect(
    (await ok("/v1/projects/demo/assistant/turn", undefined, restarted)).status,
  ).toBe("failed");
});

it("scopes node credentials and revokes access", async () => {
  const node = await setup();
  expect((await request("/v1/nodes", undefined, node)).status).toBe(403);
  expect((await request("/v1/projects/another", undefined, node)).status).toBe(
    403,
  );
  await ok("/v1/nodes/agent-a/revoke", {});
  expect((await request("/v1/node/catalog", undefined, node)).status).toBe(401);
  expect((await request("/v1/projects", undefined, "short")).status).toBe(401);
});
it("limits task-only credentials to metadata tools and rejects executor impersonation", async () => {
  const native = await setup("native-node", "agent", ["demo"]);
  const spec = {
    id: "task-client",
    role: "agent",
    projects: ["demo"],
    taskOnly: true,
    origin: null,
    slots: 1,
    cacheBytes: 16777216,
  };
  const { token: service } = await ok("/v1/nodes", spec);
  expect(await ok("/v1/node/identity", undefined, service)).toEqual({
    id: spec.id,
    role: "agent",
    projects: ["demo"],
    taskOnly: true,
  });
  const task = await ok(
    "/v1/projects/demo/tasks",
    {
      id: "scoped-task",
      title: "Scoped task",
      type: "idea",
    },
    service,
  );
  expect(task.createdBy).toBe("node:task-client");
  await ok(
    "/v1/projects/demo/tasks/scoped-task/update",
    {
      operationId: "edit",
      expectedRevision: 1,
      patch: { description: "Created through scoped service" },
    },
    service,
  );
  await ok(
    "/v1/projects/demo/tasks/scoped-task/move",
    {
      operationId: "start",
      expectedRevision: 2,
      status: "in_progress",
    },
    service,
  );
  expect(
    (await ok("/v1/projects/demo/tasks", undefined, service)).tasks,
  ).toHaveLength(1);
  expect(
    (await ok("/v1/projects/demo/tasks/activity", undefined, service)).events,
  ).toHaveLength(3);
  expect(
    (await ok("/v1/projects/demo/activity", undefined, service)).nodes.map(
      (n: any) => n.id,
    ),
  ).toEqual(["native-node"]);
  await ok(
    "/v1/projects/demo/assistant",
    { id: "service-turn", prompt: "List tasks" },
    service,
  );
  const { assignment } = await ok("/v1/node/poll", {}, native);
  expect(assignment.id).toBe("service-turn");
  const denied: [string, unknown?, string?][] = [
    ["/v1/node/hello", { session: "stolen-executor" }],
    ["/v1/node/poll", {}],
    ["/v1/node/finish", { assignment, ok: true, result: { check: "forged" } }],
    ["/v1/node/catalog"],
    ["/v1/projects/demo"],
    ["/v1/projects/demo/jobs/service-turn"],
    ["/v1/projects/demo/jobs", { id: "forged-check" }],
    ["/v1/projects/demo/generations", {}],
    ["/v1/projects/demo/checks", {}],
    ["/v1/projects/demo/candidates", {}],
    ["/v1/projects/demo/accept", {}],
    [`/v1/data/demo/chunks/${"a".repeat(64)}`, "untrusted", "PUT"],
    [
      "/v1/projects/demo/tasks/scoped-task/move",
      { operationId: "ship", expectedRevision: 3, status: "shipped" },
    ],
    ["/v1/projects/other/tasks"],
  ];
  for (const [path, body, method] of denied)
    expect((await request(path, body, service, method)).status, path).toBe(403);
  expect((await ok("/v1/projects/demo/jobs/service-turn")).status).toBe(
    "assigned",
  );
  await ok("/v1/projects", { id: "other" });
  expect(
    (
      await request("/v1/nodes", {
        ...spec,
        id: "wide-service",
        projects: ["demo", "other"],
      })
    ).status,
  ).toBe(400);
  await ok("/v1/nodes/task-client/revoke", {});
  expect(
    (await request("/v1/projects/demo/tasks", undefined, service)).status,
  ).toBe(401);
});
it("requires verified chunks before pages and verified pages before publication", async () => {
  await ok("/v1/projects", { id: "demo" });
  const missing = "a".repeat(64);
  const page = canonical([{ hash: missing, bytes: 4 }]);
  expect(
    (await request(`/v1/data/demo/chunks/${missing}`, "bad", token, "PUT"))
      .status,
  ).toBe(400);
  expect(
    (
      await request(
        `/v1/data/demo/pages/${await sha256(page)}`,
        page,
        token,
        "PUT",
      )
    ).status,
  ).toBe(409);
  const generation = await publish();
  const modified = {
    ...generation.manifest,
    pages: [{ ...generation.manifest.pages[0], hash: missing }],
  };
  expect(
    (await request("/v1/projects/demo/generations", modified)).status,
  ).toBe(409);
  expect(
    (
      await request(
        `/v1/data/demo/pages/${await sha256("{")}`,
        "{",
        token,
        "PUT",
      )
    ).status,
  ).toBe(400);
});
it("records acceptance in R2 before advancing the head and rejects competing changes", async () => {
  const first = await publish();
  const other = await publish("other");
  const accepted = await ok("/v1/projects/demo/accept", {
    generation: first.id,
    expected: null,
  });
  const receipt = await env.STORE.get(
    `projects/demo/decisions/${accepted.decision}.json`,
  );
  expect(await receipt!.json()).toMatchObject({
    generation: first.id,
    previous: null,
  });
  expect(
    (
      await request("/v1/projects/demo/accept", {
        generation: other.id,
        expected: null,
      })
    ).status,
  ).toBe(409);
  expect(
    await ok("/v1/projects/demo/accept", {
      generation: first.id,
      expected: null,
    }),
  ).toEqual(accepted);
});
it("reserves capacity, retries the same attempt, and rejects stale session results", async () => {
  const node = await setup();
  const generation = await publish();
  const spec = {
    id: "task-one",
    project: "demo",
    generation: generation.id,
    kind: "verify",
  };
  await ok("/v1/projects/demo/jobs", spec);
  expect(
    (await request("/v1/projects/demo/jobs", { ...spec, kind: "hydrate" }))
      .status,
  ).toBe(409);
  const { assignment } = await ok("/v1/node/poll", {}, node);
  expect((await ok("/v1/node/poll", {}, node)).assignment.attempt).toBe(
    assignment.attempt,
  );
  const newSession = crypto.randomUUID();
  await ok("/v1/node/hello", { session: newSession }, node.token);
  expect(
    (
      await request(
        "/v1/node/finish",
        { assignment, ok: true, result: {} },
        node,
      )
    ).status,
  ).toBe(403);
  expect((await ok("/v1/projects/demo")).jobs[0].status).toBe("assigned");
});
it("only advertises live storage and prefers verified generation readiness", async () => {
  const storage = await setup("storage", "storage");
  const generation = await publish();
  await ok("/v1/node/heartbeat", { stats: {} }, storage);
  let routes = await ok(`/v1/projects/demo/routes/${generation.id}`);
  expect(routes.origins[0].ready).toBe(false);
  await ok(
    "/v1/node/ready",
    { project: "demo", generation: generation.id, ready: true },
    storage,
  );
  routes = await ok(`/v1/projects/demo/routes/${generation.id}`);
  expect(routes.origins[0].ready).toBe(true);
  const payload = JSON.parse(atob(routes.origins[0].token.split(".")[0]));
  expect(payload).toMatchObject({
    project: "demo",
    generation: generation.id,
    epoch: storage.epoch,
  });
  await ok("/v1/nodes/storage/revoke", {});
  expect(
    (await ok(`/v1/projects/demo/routes/${generation.id}`)).origins,
  ).toEqual([]);
});

it("requires a new generation to incorporate the current accepted parent", async () => {
  const first = await publish();
  const stale = await publish("stale-base");
  await ok("/v1/projects/demo/accept", {
    generation: first.id,
    expected: null,
  });
  expect(
    (
      await request("/v1/projects/demo/accept", {
        generation: stale.id,
        expected: first.id,
      })
    ).status,
  ).toBe(409);
  const integrated = await publish("integrated", first.id);
  expect(
    (
      await ok("/v1/projects/demo/accept", {
        generation: integrated.id,
        expected: first.id,
      })
    ).head,
  ).toBe(integrated.id);
});

it("reconciles interrupted verification while keeping native commands blocked for owner inspection", async () => {
  let node = await setup();
  const generation = await publish();
  await ok("/v1/projects/demo/jobs", {
    id: "read",
    project: "demo",
    generation: generation.id,
    kind: "verify",
  });
  const first = (await ok("/v1/node/poll", {}, node)).assignment;
  let session = crypto.randomUUID();
  let hello = await ok("/v1/node/hello", { session }, node.token);
  node = { ...node, session, epoch: hello.epoch };
  await ok("/v1/node/reconcile", { attempt: first.attempt }, node);
  expect((await ok("/v1/projects/demo/jobs/read")).status).toBe("failed");
  await ok("/v1/projects/demo/jobs", {
    id: "native",
    project: "demo",
    generation: generation.id,
    kind: "layerfs-exec",
    argv: ["true"],
    timeoutSeconds: 10,
  });
  const native = (await ok("/v1/node/poll", {}, node)).assignment;
  session = crypto.randomUUID();
  hello = await ok("/v1/node/hello", { session }, node.token);
  node = { ...node, session, epoch: hello.epoch };
  expect(
    (await request("/v1/node/reconcile", { attempt: native.attempt }, node))
      .status,
  ).toBe(409);
  expect((await ok("/v1/node/poll", {}, node)).reconciliationRequired).toBe(
    true,
  );
  await ok("/v1/nodes/agent-a/reconcile", {
    assignment: native,
    stopped: true,
  });
  expect((await ok("/v1/projects/demo/jobs/native")).status).toBe("failed");
  expect((await ok("/v1/node/poll", {}, node)).assignment).toBe(null);
});

it("does not let two nodes execute the same queued job or change a recorded outcome", async () => {
  const a = await setup();
  const b = await setup("agent-b");
  const generation = await publish();
  await ok("/v1/projects/demo/jobs", {
    id: "single",
    project: "demo",
    generation: generation.id,
    kind: "verify",
  });
  const responses = await Promise.all([
    request("/v1/node/poll", {}, a),
    request("/v1/node/poll", {}, b),
  ]);
  const payloads = await Promise.all(
    responses.map((r) => r.json() as Promise<any>),
  );
  const assignments = payloads
    .filter((p) => p.assignment)
    .map((p) => p.assignment);
  expect(assignments).toHaveLength(1);
  const assignment = assignments[0];
  const owner = assignment.node === "agent-a" ? a : b;
  await ok(
    "/v1/node/finish",
    { assignment, ok: true, result: { verified: true } },
    owner,
  );
  expect(
    (
      await request(
        "/v1/node/finish",
        { assignment, ok: false, result: { verified: true } },
        owner,
      )
    ).status,
  ).toBe(409);
  await ok(
    "/v1/node/finish",
    { assignment, ok: true, result: { verified: true } },
    owner,
  );
});

it("validates a 100-change candidate once and refuses selected checks as promotion evidence", async () => {
  const node = await setup();
  const generation = await publish();
  const recipe = await sha256("full-store-integrity-v1");
  await ok("/v1/projects/demo/checks", {
    checks: [{ name: "integrity", recipe, kind: "verify" }],
  });
  const candidate = {
    id: "batch",
    project: "demo",
    base: null,
    generation: generation.id,
    changes: Array.from({ length: 100 }, (_, i) => ({
      id: `task-${i}`,
      generation: generation.id,
    })),
  };
  await ok("/v1/projects/demo/candidates", candidate);
  const promotion = {
    generation: generation.id,
    expected: null,
    candidate: "batch",
  };
  expect((await request("/v1/projects/demo/accept", promotion)).status).toBe(
    409,
  );
  const selected = {
    candidate: "batch",
    name: "integrity",
    recipe,
    scope: "selected",
  };
  await ok("/v1/projects/demo/jobs", {
    id: "selected",
    project: "demo",
    generation: generation.id,
    kind: "verify",
    check: selected,
  });
  let assignment = (await ok("/v1/node/poll", {}, node)).assignment;
  expect(
    (
      await request(
        "/v1/node/finish",
        {
          assignment,
          ok: true,
          result: { check: { ...selected, scope: "full" } },
        },
        node,
      )
    ).status,
  ).toBe(409);
  await ok(
    "/v1/node/finish",
    { assignment, ok: true, result: { check: selected } },
    node,
  );
  expect((await request("/v1/projects/demo/accept", promotion)).status).toBe(
    409,
  );
  const full = { ...selected, scope: "full" };
  await ok("/v1/projects/demo/jobs", {
    id: "full",
    project: "demo",
    generation: generation.id,
    kind: "verify",
    check: full,
  });
  assignment = (await ok("/v1/node/poll", {}, node)).assignment;
  await ok(
    "/v1/node/finish",
    { assignment, ok: true, result: { check: full } },
    node,
  );
  const accepted = await ok("/v1/projects/demo/accept", promotion);
  const receipt = await (await env.STORE.get(
    `projects/demo/decisions/${accepted.decision}.json`,
  ))!.json<any>();
  expect(receipt.validation.candidate.changes).toHaveLength(100);
  expect(receipt.validation.jobs).toEqual({ integrity: "full" });
});

it("invalidates candidate evidence when the protected recipe changes", async () => {
  await setup();
  const generation = await publish();
  const recipe = await sha256("recipe-one");
  await ok("/v1/projects/demo/checks", {
    checks: [
      {
        name: "test",
        recipe,
        kind: "layerfs-exec",
        argv: ["true"],
        timeoutSeconds: 10,
      },
    ],
  });
  await ok("/v1/projects/demo/candidates", {
    id: "candidate",
    project: "demo",
    base: null,
    generation: generation.id,
    changes: [{ id: "task", generation: generation.id }],
  });
  expect(
    (
      await request("/v1/projects/demo/jobs", {
        id: "wrong-executor",
        project: "demo",
        generation: generation.id,
        kind: "verify",
        check: { candidate: "candidate", name: "test", recipe, scope: "full" },
      })
    ).status,
  ).toBe(409);
  await ok("/v1/projects/demo/checks", {
    checks: [
      {
        name: "test",
        recipe: await sha256("recipe-two"),
        kind: "layerfs-exec",
        argv: ["true"],
        timeoutSeconds: 10,
      },
    ],
  });
  expect(
    (
      await request("/v1/projects/demo/accept", {
        generation: generation.id,
        expected: null,
        candidate: "candidate",
      })
    ).status,
  ).toBe(409);
});

it("preserves frozen validation while recovering an interrupted acceptance", async () => {
  const node = await setup();
  const generation = await publish();
  const recipe = await sha256("recovery-integrity-v1");
  const checks = [{ name: "integrity", recipe, kind: "verify" }];
  await ok("/v1/projects/demo/checks", { checks });
  const candidate = {
    id: "recovery-batch",
    project: "demo",
    base: null,
    generation: generation.id,
    changes: [{ id: "change", generation: generation.id }],
  };
  await ok("/v1/projects/demo/candidates", candidate);
  const job = {
    id: "full",
    project: "demo",
    generation: generation.id,
    kind: "verify",
    check: {
      candidate: candidate.id,
      name: "integrity",
      recipe,
      scope: "full",
    },
  };
  await ok("/v1/projects/demo/jobs", job);
  const { assignment } = await ok("/v1/node/poll", {}, node);
  await ok(
    "/v1/node/finish",
    { assignment, ok: true, result: { check: job.check } },
    node,
  );
  // Model a crash after the durable intent and before the R2 receipt/head write.
  const pending = {
    id: crypto.randomUUID(),
    project: "demo",
    previous: null,
    generation: generation.id,
    created: Date.now(),
    validation: { candidate, checks, jobs: { integrity: "full" } },
  };
  await runInDurableObject(env.PROJECTS.getByName("demo"), (instance) => {
    instance.put("pendingDecision", pending);
  });
  expect(
    (await request("/v1/projects/demo/jobs", { ...job, id: "replacement" }))
      .status,
  ).toBe(409);
  expect(
    (await request("/v1/projects/demo/checks", { checks: [] })).status,
  ).toBe(409);
  const accepted = await ok("/v1/projects/demo/accept", {
    generation: generation.id,
    expected: null,
    candidate: candidate.id,
  });
  expect(accepted.decision).toBe(pending.id);
  expect(
    await (await env.STORE.get(
      `projects/demo/decisions/${pending.id}.json`,
    ))!.json(),
  ).toEqual(pending);
});

it("persists scoped task creation, chat provenance, idempotency and optimistic edits", async () => {
  const node = await setup();
  const input = {
    id: "chat-task",
    title: "A useful idea",
    type: "idea",
    source: {
      kind: "chat",
      conversation: "room:one",
      messageIds: ["message-2", "message-1"],
    },
  };
  let task = await ok("/v1/projects/demo/tasks", input, node);
  expect(task.createdBy).toBe("node:agent-a");
  expect(task.source.messageIds).toEqual(["message-1", "message-2"]);
  expect((await ok("/v1/projects/demo/tasks", input, node)).revision).toBe(1);
  expect(
    (
      await request(
        "/v1/projects/demo/tasks",
        { ...input, title: "Different request" },
        node,
      )
    ).status,
  ).toBe(409);
  expect(
    (
      await request(
        "/v1/projects/demo/tasks",
        { ...input, id: "duplicate-source" },
        node,
      )
    ).status,
  ).toBe(409);
  const update = {
    operationId: "edit",
    expectedRevision: 1,
    patch: { title: "Revised idea" },
  };
  task = await ok("/v1/projects/demo/tasks/chat-task/update", update, node);
  expect(task.revision).toBe(2);
  expect(
    await ok("/v1/projects/demo/tasks/chat-task/update", update, node),
  ).toEqual(task);
  expect(
    (
      await request(
        "/v1/projects/demo/tasks/chat-task/update",
        { ...update, operationId: "stale" },
        node,
      )
    ).status,
  ).toBe(409);
  expect(
    (
      await request(
        "/v1/projects/demo/tasks/chat-task/update",
        { ...update, patch: { title: "Reuse operation" } },
        node,
      )
    ).status,
  ).toBe(409);
  await evictDurableObject(env.PROJECTS.getByName("demo"));
  expect(
    await ok("/v1/projects/demo/tasks/chat-task", undefined, node),
  ).toEqual(task);
  expect((await ok("/v1/projects/demo/tasks", input, node)).revision).toBe(2);
  const events = await ok(
    "/v1/projects/demo/tasks/activity?limit=1",
    undefined,
    node,
  );
  expect(events.events).toHaveLength(1);
  expect(events.nextCursor).toBe(1);
  expect(
    (await ok("/v1/projects/demo/tasks/activity?cursor=1", undefined, node))
      .events[0].action,
  ).toBe("updated");
});

it("denies task policy escalation, storage-node access, cross-project access and spoofed fields", async () => {
  const agent = await setup();
  const storage = await setup("storage", "storage");
  expect(
    (
      await request(
        "/v1/projects/demo/tasks",
        { id: "auto", title: "Automatic", type: "idea", review: "automatic" },
        agent,
      )
    ).status,
  ).toBe(403);
  expect(
    (await request("/v1/projects/demo/tasks", undefined, storage)).status,
  ).toBe(403);
  await ok("/v1/projects", { id: "other" });
  expect(
    (await request("/v1/projects/other/tasks", undefined, agent)).status,
  ).toBe(403);
  expect(
    (
      await request(
        "/v1/projects/demo/tasks",
        { id: "spoof", title: "Bad actor", type: "idea", createdBy: "owner" },
        agent,
      )
    ).status,
  ).toBe(400);
  expect(
    (
      await request(
        "/v1/projects/demo/tasks",
        { id: "activity", title: "Reserved", type: "idea" },
        agent,
      )
    ).status,
  ).toBe(400);
  await ok(
    "/v1/projects/demo/tasks",
    { id: "one", title: "Task one", type: "issue" },
    agent,
  );
  await ok(
    "/v1/projects/demo/tasks",
    { id: "two", title: "Task two", type: "idea" },
    agent,
  );
  const first = await ok("/v1/projects/demo/tasks?limit=1", undefined, agent);
  expect(first.tasks[0].id).toBe("one");
  expect(first.nextCursor).toBe("one");
  expect(
    (await ok("/v1/projects/demo/tasks?limit=1&cursor=one", undefined, agent))
      .tasks[0].id,
  ).toBe("two");
  expect(
    (
      await request(
        "/v1/projects/demo/tasks/one/move",
        { operationId: "ship", expectedRevision: 1, status: "shipped" },
        agent,
      )
    ).status,
  ).toBe(403);
  expect(
    (
      await request("/v1/projects/demo/tasks/one/move", {
        operationId: "ship",
        expectedRevision: 1,
        status: "shipped",
      })
    ).status,
  ).toBe(409);
});

async function acceptedTask(
  review: "required" | "automatic",
  editWhileTesting = false,
  beforeAccept?: (context: {
    node: Session;
    source: any;
    integrated: any;
  }) => Promise<void>,
) {
  const node = await setup();
  await ok("/v1/projects/demo/tasks", {
    id: "feature",
    title: "Feature",
    type: "idea",
    review,
  });
  const source = await publish("feature result");
  const integrated = await publish("integrated result");
  const recipe = await sha256("task-check");
  await ok("/v1/projects/demo/checks", {
    checks: [{ name: "integrity", recipe, kind: "verify" }],
  });
  await ok("/v1/projects/demo/candidates", {
    id: "batch",
    project: "demo",
    base: null,
    generation: integrated.id,
    changes: [{ id: "feature", generation: source.id }],
  });
  await ok(
    "/v1/projects/demo/tasks/feature/move",
    {
      operationId: "testing",
      expectedRevision: 1,
      status: "testing",
      generation: source.id,
      candidate: "batch",
    },
    node,
  );
  const check = {
    candidate: "batch",
    name: "integrity",
    recipe,
    scope: "full",
  };
  await ok("/v1/projects/demo/jobs", {
    id: "task-check",
    project: "demo",
    generation: integrated.id,
    kind: "verify",
    check,
  });
  const { assignment } = await ok("/v1/node/poll", {}, node);
  await ok(
    "/v1/node/finish",
    { assignment, ok: true, result: { check } },
    node,
  );
  if (editWhileTesting)
    await ok(
      "/v1/projects/demo/tasks/feature/update",
      {
        operationId: "changed-request",
        expectedRevision: 2,
        patch: { description: "New acceptance requirement" },
      },
      node,
    );
  if (beforeAccept) await beforeAccept({ node, source, integrated });
  const promotion = {
    generation: integrated.id,
    expected: null,
    candidate: "batch",
  };
  let acceptance;
  if (editWhileTesting)
    expect((await request("/v1/projects/demo/accept", promotion)).status).toBe(
      409,
    );
  else acceptance = await ok("/v1/projects/demo/accept", promotion);
  return { node, source, integrated, acceptance };
}

it("cannot reattach a stale candidate or auto-ship changed task requirements", async () => {
  const { node, source, integrated } = await acceptedTask("automatic", true);
  const candidate = await ok("/v1/projects/demo/candidates/batch");
  expect(
    (await ok("/v1/projects/demo/candidates", candidate.spec)).taskRequirements,
  ).toEqual(candidate.taskRequirements);
  expect(
    (
      await request(
        "/v1/projects/demo/tasks/feature/move",
        {
          operationId: "reattach-old",
          expectedRevision: 3,
          status: "testing",
          generation: source.id,
          candidate: "batch",
        },
        node,
      )
    ).status,
  ).toBe(409);
  expect(
    (await ok("/v1/projects/demo/tasks/feature", undefined, node)).status,
  ).toBe("in_progress");
  // Omitting the candidate must not let acceptance infer stale evidence.
  await ok(
    "/v1/projects/demo/tasks/feature/move",
    {
      operationId: "generation-only",
      expectedRevision: 3,
      status: "testing",
      generation: source.id,
    },
    node,
  );
  expect(
    (
      await request("/v1/projects/demo/accept", {
        generation: integrated.id,
        expected: null,
        candidate: "batch",
      })
    ).status,
  ).toBe(409);
  expect(
    (
      await request("/v1/projects/demo/tasks/feature/move", {
        operationId: "approve-stale",
        expectedRevision: 4,
        status: "shipped",
        candidate: "batch",
      })
    ).status,
  ).toBe(409);
  expect(await ok("/v1/projects/demo/tasks/feature")).toMatchObject({
    status: "testing",
    shippedGeneration: null,
  });
});

it("freezes task requirements through a pending acceptance without freezing status or assignment", async () => {
  const { integrated } = await acceptedTask(
    "automatic",
    false,
    async ({ node, integrated }) => {
      const candidate = await ok("/v1/projects/demo/candidates/batch");
      await runInDurableObject(env.PROJECTS.getByName("demo"), (instance) => {
        instance.put("pendingDecision", {
          id: crypto.randomUUID(),
          project: "demo",
          previous: null,
          generation: integrated.id,
          created: Date.now(),
          validation: {
            candidate: candidate.spec,
            checks: candidate.checks,
            jobs: candidate.fullJobs,
            taskRequirements: candidate.taskRequirements,
          },
        });
      });
      expect(
        (
          await request(
            "/v1/projects/demo/tasks/feature/update",
            {
              operationId: "change-during-accept",
              expectedRevision: 2,
              patch: { title: "Different requirements" },
            },
            node,
          )
        ).status,
      ).toBe(409);
      await ok(
        "/v1/projects/demo/tasks/feature/update",
        {
          operationId: "assign-during-accept",
          expectedRevision: 2,
          patch: { assignee: "agent-a" },
        },
        node,
      );
    },
  );
  expect(await ok("/v1/projects/demo/tasks/feature")).toMatchObject({
    status: "shipped",
    revision: 4,
    assignee: "agent-a",
    shippedGeneration: integrated.id,
  });
});

it("ships only accepted candidate membership after explicit owner review and records the decision", async () => {
  const { node, integrated, source, acceptance } =
    await acceptedTask("required");
  expect(
    (await ok("/v1/projects/demo/tasks/feature", undefined, node)).status,
  ).toBe("testing");
  expect(
    (
      await request(
        "/v1/projects/demo/tasks/feature/move",
        { operationId: "self-ship", expectedRevision: 2, status: "shipped" },
        node,
      )
    ).status,
  ).toBe(403);
  await ok(
    "/v1/projects/demo/tasks",
    { id: "unrelated", title: "Unrelated task", type: "issue" },
    node,
  );
  expect(
    (
      await request(
        "/v1/projects/demo/tasks/unrelated/move",
        {
          operationId: "wrong-membership",
          expectedRevision: 1,
          status: "testing",
          generation: source.id,
          candidate: "batch",
        },
        node,
      )
    ).status,
  ).toBe(409);
  await ok(
    "/v1/projects/demo/tasks/unrelated/move",
    {
      operationId: "testing",
      expectedRevision: 1,
      status: "testing",
      generation: integrated.id,
    },
    node,
  );
  expect(
    (
      await request("/v1/projects/demo/tasks/unrelated/move", {
        operationId: "ship",
        expectedRevision: 2,
        status: "shipped",
      })
    ).status,
  ).toBe(409);
  const ship = {
    operationId: "approve",
    expectedRevision: 2,
    status: "shipped",
  };
  const task = await ok("/v1/projects/demo/tasks/feature/move", ship);
  expect(task.shippedGeneration).toBe(integrated.id);
  expect(task.updatedBy).toBe("owner");
  expect(await ok("/v1/projects/demo/tasks/feature/move", ship)).toEqual(task);
  expect(
    (
      await request("/v1/projects/demo/tasks/feature/update", {
        operationId: "rewrite",
        expectedRevision: 3,
        patch: { title: "Different task" },
      })
    ).status,
  ).toBe(409);
  const { events } = await ok("/v1/projects/demo/tasks/activity");
  expect(events.find((event: any) => event.action === "shipped")).toMatchObject(
    { actor: "owner", taskId: "feature", decision: acceptance.decision },
  );
});

it("auto-ships only owner-configured tasks with accepted evidence and invalidates evidence on changed requests", async () => {
  const { integrated } = await acceptedTask("automatic");
  expect(await ok("/v1/projects/demo/tasks/feature")).toMatchObject({
    status: "shipped",
    shippedGeneration: integrated.id,
    updatedBy: "policy:automatic",
  });
  await reset();
  await acceptedTask("automatic", true);
  expect(await ok("/v1/projects/demo/tasks/feature")).toMatchObject({
    status: "in_progress",
    resultGeneration: null,
    candidate: null,
    shippedGeneration: null,
  });
});

it("reports actual project activity without exposing other project membership or node origins", async () => {
  const node = await setup();
  await ok("/v1/projects", { id: "other" });
  await ok("/v1/nodes", {
    id: "private-node",
    role: "agent",
    projects: ["other"],
    origin: null,
    slots: 1,
    cacheBytes: 16777216,
  });
  const activity = await ok("/v1/projects/demo/activity", undefined, node);
  expect(activity.jobs).toEqual([]);
  expect(activity.nodes.map((node: any) => node.id)).toEqual(["agent-a"]);
  expect(activity.nodes[0]).not.toHaveProperty("projects");
  expect(activity.nodes[0]).not.toHaveProperty("origin");
});

it("serializes competing task edits and preserves one operation receipt", async () => {
  const node = await setup();
  await ok(
    "/v1/projects/demo/tasks",
    { id: "race", title: "Original", type: "idea" },
    node,
  );
  const mutation = {
    operationId: "one",
    expectedRevision: 1,
    patch: { title: "First" },
  };
  const responses = await Promise.all([
    request("/v1/projects/demo/tasks/race/update", mutation, node),
    request(
      "/v1/projects/demo/tasks/race/update",
      { ...mutation, operationId: "two", patch: { title: "Second" } },
      node,
    ),
  ]);
  expect(responses.map((response) => response.status).sort()).toEqual([
    200, 409,
  ]);
  const current = await ok("/v1/projects/demo/tasks/race", undefined, node);
  expect(current.revision).toBe(2);
  expect(
    (await ok("/v1/projects/demo/tasks/activity", undefined, node)).events,
  ).toHaveLength(2);
  expect((await request("/v1/node/identity")).status).toBe(403);
  expect(await ok("/v1/node/identity", undefined, node)).toMatchObject({
    id: "agent-a",
    role: "agent",
    projects: ["demo"],
  });
});

async function integrationFixture() {
  await ok("/v1/projects",{id:"demo"});
  const baseRef="32"+"a".repeat(64),baseRoot="b".repeat(64),ref="12"+"c".repeat(64),root="d".repeat(64);
  const base=await publish("base",null,{projectName:"source",layerfsRef:baseRef,layerfsRoot:baseRoot});
  await ok("/v1/projects/demo/accept",{generation:base.id,expected:null});
  const change=await publish("change",base.id,{projectName:"source",layerfsRef:ref,layerfsRoot:root});
  await ok("/v1/projects/demo/checks",{checks:[{name:"full",kind:"verify",recipe:"f".repeat(64)}]});
  await ok("/v1/projects/demo/tasks",{id:"feature",title:"Frozen requirements",type:"idea"});
  const spec={id:"feature",project:"demo",base:base.id,generation:change.id,layerfsProject:"source",baseRef,baseRoot,ref,root};
  return {base,change,spec};
}
it("freezes exact-base integration submissions, fences capabilities and stages only verified output provenance",async()=>{
  const {base,change,spec}=await integrationFixture();
  await ok("/v1/projects/demo/submissions",spec);
  expect((await request("/v1/projects/demo/submissions",{...spec,root:"e".repeat(64)})).status).toBe(409);
  const frozen=await ok("/v1/projects/demo/integrations",{id:"batch-merge",changes:["feature"]});
  expect((await ok("/v1/projects/demo/integrations",{id:"batch-merge",changes:["feature"]})).spec).toEqual(frozen.spec);
  const ordinary=await setup(); expect((await ok("/v1/node/poll",{},ordinary)).assignment).toBeNull();
  const identity=await ok("/v1/nodes",{id:"merger",role:"agent",projects:["demo"],origin:null,slots:1,cacheBytes:16777216,integrationProjects:["demo"]});
  const session=crypto.randomUUID(); const hello=await ok("/v1/node/hello",{session},identity.token);
  const actor={token:identity.token,epoch:hello.epoch,session};
  const {assignment}=await ok("/v1/node/poll",{},actor);
  expect(assignment.integration.changes).toEqual([{id:"feature",submission:"feature",generation:change.id,ref:spec.ref,root:spec.root}]);
  expect((await request("/v1/node/finish",{assignment,ok:true,result:{generation:change.id,ref:spec.ref,root:spec.root,treeHash:"e".repeat(64)}},actor)).status).toBe(409);
  const result={ref:"12"+"1".repeat(64),root:"2".repeat(64),treeHash:"3".repeat(64)};
  const output=await publish("merged",base.id,{projectName:"source",layerfsRef:result.ref,layerfsRoot:result.root,treeHash:result.treeHash,integration:"batch-merge"});
  await ok("/v1/node/finish",{assignment,ok:true,result:{...result,generation:output.id}},actor);
  await ok("/v1/node/finish",{assignment,ok:true,result:{...result,generation:output.id}},actor);
  expect((await ok("/v1/projects/demo/candidates/batch-merge")).spec).toEqual({id:"batch-merge",project:"demo",base:base.id,generation:output.id,changes:[{id:"feature",generation:change.id}]});
  expect((await ok("/v1/projects/demo")).head).toBe(base.id);
  expect((await request("/v1/projects/demo/accept",{generation:output.id,expected:base.id,candidate:"batch-merge"})).status).toBe(409);
  const task=await ok("/v1/projects/demo/tasks/feature");
  expect(task).toMatchObject({status:"testing",candidate:"batch-merge",resultGeneration:change.id});
  const inventory=await ok("/v1/projects/demo");
  expect(inventory.jobs.filter((job:any)=>job.spec.check?.candidate==="batch-merge")).toHaveLength(1);
  const check=(await ok("/v1/node/poll",{},ordinary)).assignment;
  expect(check).toMatchObject({kind:"verify",generation:output.id,check:{candidate:"batch-merge",scope:"full"}});
  await ok("/v1/node/finish",{assignment:check,ok:true,result:{check:check.check}},ordinary);
  expect((await ok("/v1/projects/demo")).head).toBe(base.id);
  expect((await ok("/v1/projects/demo/tasks/feature/approve",{operationId:"review-integrated",expectedRevision:task.revision})).status).toBe("shipped");
  expect((await ok("/v1/projects/demo")).head).toBe(output.id);
});
it("task-only credentials cannot submit or execute integration and arbitrary jobs cannot invent a frozen batch",async()=>{
  const {spec}=await integrationFixture();
  const scoped=await ok("/v1/nodes",{id:"task-client",role:"agent",projects:["demo"],origin:null,slots:1,cacheBytes:16777216,taskOnly:true});
  expect((await request("/v1/projects/demo/submissions",spec,scoped.token)).status).toBe(403);
  expect((await request("/v1/projects/demo/integrations",{id:"merge",changes:["feature"]},scoped.token)).status).toBe(403);
  expect((await request("/v1/nodes",{id:"bad-scope",role:"agent",projects:["demo"],origin:null,slots:1,cacheBytes:16777216,taskOnly:true,integrationProjects:["demo"]})).status).toBe(400);
  expect((await request("/v1/projects/demo/jobs",{id:"invented",project:"demo",kind:"integration",generation:spec.base,integration:{id:"invented",layerfsProject:spec.layerfsProject,baseRef:spec.baseRef,baseRoot:spec.baseRoot,changes:[{id:spec.id,submission:spec.id,generation:spec.generation,ref:spec.ref,root:spec.root}],taskRequirements:{}}})).status).toBe(409);
});

it("invalidates queued integration after requirement edits while ordinary task moves retain semantic binding",async()=>{
 const {spec}=await integrationFixture();
 await ok("/v1/projects/demo/submissions",spec);
 await ok("/v1/projects/demo/tasks/feature/move",{operationId:"start",expectedRevision:1,status:"in_progress"});
 const frozen=await ok("/v1/projects/demo/integrations",{id:"before-edit",changes:["feature"]});
 expect(frozen.spec.integration.taskRequirements.feature).toMatch(/^[a-f0-9]{64}$/);
 await ok("/v1/projects/demo/tasks/feature/update",{operationId:"new-requirements",expectedRevision:2,patch:{title:"Different requirements"}});
 expect((await request("/v1/projects/demo/integrations",{id:"after-edit",changes:["feature"]})).status).toBe(409);
 const output=await publish("stale-merged",spec.base,{projectName:"source",layerfsRef:spec.ref,layerfsRoot:spec.root,treeHash:"3".repeat(64),integration:"before-edit"});
 await runInDurableObject(env.PROJECTS.getByName("demo"),async(instance)=>{
  const assignment={...frozen.spec,node:"test-node",epoch:1,session:crypto.randomUUID(),expires:Date.now()+30_000,attempt:crypto.randomUUID()};
  await instance.assign(assignment);
  await expect(instance.finish(assignment,canonical({generation:output.id,ref:spec.ref,root:spec.root,treeHash:"3".repeat(64)}),true)).rejects.toThrow("Task requirements changed");
  expect(await instance.candidate("before-edit")).toBeNull();
 });
});
