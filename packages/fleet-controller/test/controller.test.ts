import { env } from "cloudflare:workers";
import { reset, runInDurableObject } from "cloudflare:test";
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
): Promise<Session> {
  await ok("/v1/projects", { id: "demo" });
  const { token: secret } = await ok("/v1/nodes", {
    id,
    role,
    projects: ["demo"],
    origin: role === "storage" ? "http://storage:8790" : null,
    slots: 1,
    cacheBytes: 16777216,
    ...(assistantProjects ? { assistantProjects } : {}),
  });
  const session = crypto.randomUUID();
  const hello = await ok("/v1/node/hello", { session }, secret);
  return { token: secret, session, epoch: hello.epoch };
}
async function publish(text = "snapshot", parent: string | null = null) {
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
    metadata: {},
  });
}
afterEach(async () => {
  await reset();
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

it("retains assistant reservations across node restart until owner reconciliation", async () => {
  const native = await setup("native-node", "agent", ["demo"]);
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
