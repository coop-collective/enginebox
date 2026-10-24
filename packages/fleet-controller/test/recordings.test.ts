import { env } from "cloudflare:workers";
import {
  reset,
  evictDurableObject,
  runInDurableObject,
  runDurableObjectAlarm,
} from "cloudflare:test";
import { afterEach, expect, it } from "vitest";
import worker from "../src/index.ts";
import {
  canonical,
  sha256,
  type Assignment,
  type BuildManifest,
  type Recording,
} from "@enginebox/fleet-protocol";

const owner = "local-tests-only-0123456789abcdef0123456789";
type Session = { token: string; epoch: number; session: string };
async function request(
  path: string,
  body?: unknown,
  auth: string | Session = owner,
  method?: string,
  extra: Record<string, string> = {},
) {
  const headers = new Headers({
    authorization: `Bearer ${typeof auth === "string" ? auth : auth.token}`,
    ...extra,
  });
  if (typeof auth !== "string") {
    headers.set("x-enginebox-session", auth.session);
    headers.set("x-enginebox-epoch", String(auth.epoch));
  }
  return worker.fetch(
    new Request(`http://fleet/v1/${path}`, {
      method: method ?? (body === undefined ? "GET" : "POST"),
      headers,
      body:
        body === undefined
          ? undefined
          : typeof body === "string"
            ? body
            : canonical(body),
    }),
    env as Env,
  );
}
async function ok<T = any>(
  path: string,
  body?: unknown,
  auth?: string | Session,
  method?: string,
  headers?: Record<string, string>,
): Promise<T> {
  const response = await request(path, body, auth, method, headers),
    value = await response.json();
  expect(response.ok, JSON.stringify(value)).toBe(true);
  return value as T;
}
async function node(
  id: string,
  media = false,
  taskOnly = false,
): Promise<Session> {
  const response = await ok("nodes", {
    id,
    role: "agent",
    projects: ["demo"],
    origin: null,
    slots: 1,
    cacheBytes: 16777216,
    ...(media ? { mediaProjects: ["demo"], buildProjects: ["demo"] } : {}),
    ...(taskOnly ? { taskOnly: true } : {}),
  });
  const session = crypto.randomUUID();
  if (taskOnly) return { token: response.token, epoch: 0, session };
  const hello = await ok("node/hello", { session }, response.token);
  return { token: response.token, epoch: hello.epoch, session };
}
async function object(text: string) {
  const hash = await sha256(text);
  await ok(`data/demo/chunks/${hash}`, text, owner, "PUT");
  const page = canonical([{ hash, bytes: text.length }]),
    pageHash = await sha256(page);
  await ok(`data/demo/pages/${pageHash}`, page, owner, "PUT");
  return {
    sha256: hash,
    bytes: text.length,
    pages: [
      {
        hash: pageHash,
        bytes: page.length,
        count: 1,
        contentBytes: text.length,
      },
    ],
  };
}
async function fixture(batch = false) {
  await ok("projects", { id: "demo" });
  const source = await object("sqlite fixture"),
    tree = "a".repeat(64),
    recipe = "b".repeat(64);
  const generation = await ok("projects/demo/generations", {
    schema: 1,
    project: "demo",
    parent: null,
    format: "sqlite",
    snapshotHash: source.sha256,
    snapshotBytes: source.bytes,
    pages: source.pages,
    metadata: { treeHash: tree, integration: "candidate-one" },
  });
  await ok("projects/demo/checks", {
    checks: [
      { name: "verify", kind: "verify", recipe },
      { name: "playable", kind: "engine-build", recipe, timeoutSeconds: 30 },
    ],
  });
  await ok("projects/demo/tasks", {
    id: "task-one",
    title: "Test a change",
    description: "Frozen requirements",
    type: "idea",
    review: "required",
  });
  if (batch)
    await ok("projects/demo/tasks", {
      id: "task-two",
      title: "Second change",
      description: "",
      type: "idea",
      review: "required",
    });
  const candidate = {
    id: "candidate-one",
    project: "demo",
    base: null,
    generation: generation.id,
    changes: [
      { id: "task-one", generation: generation.id },
      ...(batch ? [{ id: "task-two", generation: generation.id }] : []),
    ],
  };
  await ok("projects/demo/candidates", candidate);
  const task = await ok("projects/demo/tasks/task-one/move", {
    operationId: "test-task",
    expectedRevision: 1,
    status: "testing",
    generation: generation.id,
    candidate: candidate.id,
  });
  if (batch)
    await ok("projects/demo/tasks/task-two/move", {
      operationId: "test-two",
      expectedRevision: 1,
      status: "testing",
      generation: generation.id,
      candidate: candidate.id,
    });
  const file = await object("<html>Frozen playable artifact</html>");
  await ok(`projects/demo/artifact-files/${file.sha256}`, file);
  const files = [
    {
      path: "index.html",
      sha256: file.sha256,
      bytes: file.bytes,
      executable: false,
    },
  ];
  await ok(
    `projects/demo/artifact-file-pages/${await sha256(canonical(files))}`,
    files,
  );
  const manifest: BuildManifest = {
    schema: 1,
    kind: "engine-build",
    project: "demo",
    generation: generation.id,
    sourceTree: tree,
    recipe,
    toolchain: {
      engine: "godot",
      version: "4.7.2",
      target: "web",
      architecture: "wasm32",
      fingerprint: "c".repeat(64),
    },
    entrypoint: "index.html",
    files,
    totalBytes: file.bytes,
  };
  const app = await node("app", false, true),
    executor = await node("media-worker", true),
    ordinary = await node("ordinary");
  const build = await ok(
    "projects/demo/artifacts",
    { candidate: candidate.id, manifest },
    executor,
  );
  // The native integration is a fixture input; this test exercises real
  // controller build-result validation, not an engine execution claim.
  await runInDurableObject(env.PROJECTS.getByName("demo"), (instance) =>
    instance.put("job:candidate-one", {
      spec: { id: "candidate-one", kind: "integration", project: "demo" },
      status: "completed",
      assignment: { node: "media-worker" },
      result: canonical({ generation: generation.id, treeHash: tree }),
    }),
  );
  await ok("projects/demo/jobs", {
    id: "build-web",
    project: "demo",
    generation: generation.id,
    kind: "engine-build",
    timeoutSeconds: 30,
    build: {
      candidate: candidate.id,
      integration: candidate.id,
      sourceTree: tree,
      recipe,
    },
    check: { candidate: candidate.id, name: "playable", recipe, scope: "full" },
  });
  const buildAssignment = (await ok("node/poll", {}, executor)).assignment;
  await ok(
    "node/finish",
    {
      assignment: buildAssignment,
      ok: true,
      result: {
        artifact: build.id,
        generation: generation.id,
        sourceTree: tree,
        recipe,
        check: buildAssignment.check,
      },
    },
    executor,
  );
  await ok(
    "projects/demo/test-sessions",
    {
      id: "test-session",
      taskId: task.id,
      expectedRevision: task.revision,
      tester: "account-one",
    },
    app,
  );
  const recording = await ok(
    "projects/demo/recordings",
    {
      id: "recording-one",
      testSessionId: "test-session",
      mimeType: "video/webm;codecs=vp8,opus",
      captureOrigin: "browser-webrtc",
      gameAudio: true,
      microphone: false,
    },
    app,
  );
  return {
    generation,
    candidate,
    task,
    build,
    app,
    executor,
    ordinary,
    recording,
    manifest,
  };
}
afterEach(async () => {
  await reset();
});

it("streams immutable artifact ranges and rejects mismatched source and task-only publication", async () => {
  const f = await fixture();
  const path = `projects/demo/artifacts/${f.build.id}/files/index.html`;
  const range = await request(path, undefined, f.app, "GET", {
    range: "bytes=6-11",
  });
  expect(range.status).toBe(206);
  expect(await range.text()).toBe("Frozen");
  expect(
    (await request(path, undefined, f.app, "GET", { range: "bytes=-0" }))
      .status,
  ).toBe(416);
  expect(
    (
      await request(
        "projects/demo/artifacts",
        { candidate: f.candidate.id, manifest: f.manifest },
        f.app,
      )
    ).status,
  ).toBe(403);
  expect(
    (
      await request("projects/demo/artifacts", {
        candidate: f.candidate.id,
        manifest: { ...f.manifest, sourceTree: "d".repeat(64) },
      })
    ).status,
  ).toBe(409);
  await evictDurableObject(env.PROJECTS.getByName("demo"));
  expect(
    (await ok(path.replace("/files/index.html", ""), undefined, f.app)).id,
  ).toBe(f.build.id);
});

it("binds resumable recordings to frozen tasks, reserves bytes once, and rejects forged media evidence", async () => {
  const f = await fixture(),
    path = "projects/demo/recordings/recording-one";
  await ok(
    "projects/demo/playtest-policy",
    {
      required: true,
      requireAudio: true,
      minDurationMs: 1000,
      retentionDays: 30,
    },
    owner,
    "PUT",
  );
  await greenCheck(f);
  const approved = await ok("projects/demo/tasks/task-one/approve", {
    operationId: "approve",
    expectedRevision: f.task.revision,
  });
  expect(approved.status).toBe("testing");
  expect(approved.reviewReceipt.candidate).toBe(f.candidate.id);
  expect(
    (
      await request("projects/demo/accept", {
        generation: f.generation.id,
        expected: null,
        candidate: f.candidate.id,
      })
    ).status,
  ).toBe(409);
  const chunk = "encoded media fixture",
    hash = await sha256(chunk),
    headers = { "x-chunk-sha256": hash, "x-recording-segment": "0" };
  await ok(`${path}/chunks/0`, chunk, f.app, "PUT", headers);
  await ok(`${path}/chunks/0`, chunk, f.app, "PUT", headers);
  expect((await ok(path, undefined, f.app)).bytes).toBe(chunk.length);
  expect(
    (await request(`${path}/chunks/0`, "bad", f.app, "PUT", headers)).status,
  ).toBe(409);
  expect(
    (await request(`${path}/output/0`, chunk, f.app, "PUT", headers)).status,
  ).toBe(403);
  await evictDurableObject(env.PROJECTS.getByName("demo"));
  const finish = {
    operationId: "finish-one",
    outcome: "pass",
    feedback: "Works",
    segments: [{ index: 0, chunks: [0], durationMs: 3000, offsetMs: 0 }],
    durationMs: 3000,
    bytes: chunk.length,
  };
  expect((await ok(`${path}/finish`, finish, f.app)).state).toBe("finalizing");
  await ok(`${path}/finish`, finish, f.app);
  expect((await ok("node/poll", {}, f.ordinary)).assignment).toBeNull();
  const { assignment } = await ok<{ assignment: Assignment }>(
    "node/poll",
    {},
    f.executor,
  );
  expect(assignment.kind).toBe("media-finalize");
  const media = {
    sha256: hash,
    bytes: chunk.length,
    chunks: [{ sequence: 0, sha256: hash, bytes: chunk.length }],
    durationMs: 3000,
    width: 640,
    height: 360,
    hasAudio: true,
    mimeType: "video/webm",
  };
  expect(
    (
      await request(
        "node/finish",
        { assignment, ok: true, result: { recording: "recording-one", media } },
        f.executor,
      )
    ).status,
  ).toBe(409);
  await ok(`${path}/output/0`, chunk, f.executor, "PUT", {
    "x-chunk-sha256": hash,
    "x-enginebox-attempt": assignment.attempt,
  });
  await ok(
    "node/finish",
    { assignment, ok: true, result: { recording: "recording-one", media } },
    f.executor,
  );
  expect((await ok(path, undefined, f.app)).state).toBe("ready");
  expect((await ok("projects/demo")).head).toBe(f.generation.id);
  expect(
    (await ok("projects/demo/tasks/task-one", undefined, f.app)).status,
  ).toBe("shipped");
  const playback = await request(`${path}/media`, undefined, f.app, "GET", {
    range: "bytes=0-6",
  });
  expect(playback.status).toBe(206);
  expect(await playback.text()).toBe("encoded");
  const authority = env.PROJECTS.getByName("demo");
  await runInDurableObject(authority, (instance) => {
    const value = instance.get<Recording>("recording:recording-one")!;
    value.expiresAt = Date.now() - 1;
    instance.put("recording:recording-one", value);
  });
  await runDurableObjectAlarm(authority);
  expect((await ok(path, undefined, f.app)).state).toBe("expired");
  expect(
    await env.STORE.head("projects/demo/recordings/recording-one/output/0"),
  ).toBeNull();
  expect(
    await env.STORE.head(`projects/demo/chunks/${f.manifest.files[0].sha256}`),
  ).not.toBeNull();
});

it("keeps aborted empty captures terminal and rejects stale requirements", async () => {
  const f = await fixture();
  const partial = await ok(
    "projects/demo/recordings/recording-one/finish",
    {
      operationId: "abandon",
      outcome: "partial",
      feedback: "Permission denied",
      segments: [],
      durationMs: 0,
      bytes: 0,
    },
    f.app,
  );
  expect(partial.state).toBe("partial");
  expect(partial.media).toBeNull();
  expect((await ok("node/poll", {}, f.executor)).assignment).toBeNull();
  await ok("projects/demo/tasks/task-one/update", {
    operationId: "changed",
    expectedRevision: f.task.revision,
    patch: { title: "Changed requirements" },
  });
  const contexts = await ok("projects/demo/test-contexts", undefined, f.app);
  expect(contexts.contexts).toEqual([]);
  expect(
    (
      await request(
        "projects/demo/test-sessions",
        {
          id: "stale-test",
          taskId: f.task.id,
          expectedRevision: f.task.revision,
          tester: "account-one",
        },
        f.app,
      )
    ).status,
  ).toBe(409);
});

async function greenCheck(f: Awaited<ReturnType<typeof fixture>>) {
  await ok("projects/demo/jobs", {
    id: "verify-full",
    project: "demo",
    generation: f.generation.id,
    kind: "verify",
    check: {
      candidate: f.candidate.id,
      name: "verify",
      recipe: f.manifest.recipe,
      scope: "full",
    },
  });
  const { assignment } = await ok("node/poll", {}, f.ordinary);
  await ok(
    "node/finish",
    {
      assignment,
      ok: true,
      result: { check: assignment.check, verified: true },
    },
    f.ordinary,
  );
}
it("Enginebox collects batch reviews before promotion and ships after the final review", async () => {
  const f = await fixture(true);
  await greenCheck(f);
  expect(
    (
      await request(
        "projects/demo/tasks/task-one/approve",
        { operationId: "forge", expectedRevision: f.task.revision },
        f.app,
      )
    ).status,
  ).toBe(403);
  const first = await ok("projects/demo/tasks/task-one/approve", {
    operationId: "approve-one",
    expectedRevision: f.task.revision,
  });
  expect(first.status).toBe("testing");
  expect((await ok("projects/demo")).head).toBeNull();
  const second = await ok("projects/demo/tasks/task-two/approve", {
    operationId: "approve-two",
    expectedRevision: 2,
  });
  expect(second.status).toBe("shipped");
  expect((await ok("projects/demo")).head).toBe(f.generation.id);
  expect(
    (
      await ok("projects/demo/tasks/task-one/approve", {
        operationId: "approve-one",
        expectedRevision: f.task.revision,
      })
    ).status,
  ).toBe("shipped");
});

it("refuses a published alternate recipe as evidence for the protected release artifact", async () => {
  const f = await fixture();
  const alternate = await ok("projects/demo/artifacts", {
    candidate: f.candidate.id,
    manifest: { ...f.manifest, recipe: "e".repeat(64) },
  });
  expect(
    (
      await request(
        "projects/demo/test-sessions",
        {
          id: "alternate",
          taskId: f.task.id,
          expectedRevision: f.task.revision,
          tester: "account-one",
          artifact: alternate.id,
        },
        f.app,
      )
    ).status,
  ).toBe(409);
  const contexts = await ok("projects/demo/test-contexts", undefined, f.app);
  expect(contexts.contexts.map((context: any) => context.artifact)).toEqual([
    f.build.id,
  ]);
});

it("confirmed media interruption leaves a terminal recording rather than endless finalization", async () => {
  const f = await fixture(),
    chunk = "input",
    digest = await sha256(chunk),
    path = "projects/demo/recordings/recording-one";
  await ok(`${path}/chunks/0`, chunk, f.app, "PUT", {
    "x-chunk-sha256": digest,
    "x-recording-segment": "0",
  });
  await ok(
    `${path}/finish`,
    {
      operationId: "finish",
      outcome: "pass",
      feedback: "",
      segments: [{ index: 0, chunks: [0], durationMs: 1000, offsetMs: 0 }],
      durationMs: 1000,
      bytes: chunk.length,
    },
    f.app,
  );
  const { assignment } = await ok("node/poll", {}, f.executor);
  expect(assignment.kind).toBe("media-finalize");
  await ok("nodes/media-worker/reconcile", { assignment, stopped: true });
  const failed = await ok(path, undefined, f.app);
  expect(failed.state).toBe("failed");
  expect(failed.media).toBeNull();
});

it("acknowledges an exited worker when later promotion fails, preserving a visible retryable error", async () => {
  const f = await fixture(),
    input = {
      operationId: "approve-before-check",
      expectedRevision: f.task.revision,
    };
  expect((await ok("projects/demo/tasks/task-one/approve", input)).status).toBe(
    "testing",
  );
  // Simulate lost artifact metadata after a valid build, so the subsequent
  // check completes but its promotion callback must refuse acceptance.
  await runInDurableObject(env.PROJECTS.getByName("demo"), (instance) =>
    instance.remove(`artifact:${f.build.id}`),
  );
  await greenCheck(f);
  const job = await ok("projects/demo/jobs/verify-full");
  expect(job.status).toBe("completed");
  expect(job.followupError).toMatch(/published artifact/);
  expect((await ok("projects/demo")).head).toBeNull();
  expect((await ok("node/poll", {}, f.ordinary)).assignment).toBeNull();
  await runInDurableObject(env.PROJECTS.getByName("demo"), (instance) =>
    instance.put(`artifact:${f.build.id}`, f.build),
  );
  expect((await ok("projects/demo/tasks/task-one/approve", input)).status).toBe(
    "shipped",
  );
});

it("releases a failed media executor after retention without resurrecting expired evidence", async () => {
  const f = await fixture(),
    chunk = "input",
    digest = await sha256(chunk),
    path = "projects/demo/recordings/recording-one";
  await ok(`${path}/chunks/0`, chunk, f.app, "PUT", {
    "x-chunk-sha256": digest,
    "x-recording-segment": "0",
  });
  await ok(
    `${path}/finish`,
    {
      operationId: "finish",
      outcome: "pass",
      feedback: "",
      segments: [{ index: 0, chunks: [0], durationMs: 1000, offsetMs: 0 }],
      durationMs: 1000,
      bytes: chunk.length,
    },
    f.app,
  );
  const { assignment } = await ok("node/poll", {}, f.executor),
    authority = env.PROJECTS.getByName("demo");
  await runInDurableObject(authority, (instance) => {
    const value = instance.get<Recording>("recording:recording-one")!;
    value.expiresAt = 0;
    instance.put("recording:recording-one", value);
  });
  await runDurableObjectAlarm(authority);
  expect(
    (
      await ok(
        "node/finish",
        { assignment, ok: false, result: { error: "Expired media input" } },
        f.executor,
      )
    ).status,
  ).toBe("failed");
  expect((await ok(path, undefined, f.app)).state).toBe("expired");
  expect((await ok("node/poll", {}, f.executor)).assignment).toBeNull();
});
