import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { runNode } from "../src/main.ts";

test("a restarted node fences an uncertain native lease before advertising spare capacity", async () => {
  const state = await mkdtemp(join(tmpdir(), "enginebox-native-recovery-"));
  const heartbeats: Record<string, number>[] = [];
  const calls: string[] = [];
  const server = createServer(async (request, response) => {
    calls.push(request.url!);
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const input = chunks.length ? JSON.parse(Buffer.concat(chunks).toString()) : {};
    let result: unknown = {};
    if (request.url === "/v1/node/hello")
      result = {
        epoch: 2,
        session: input.session,
        reservations: [{
          id: "unfinished", project: "demo", kind: "assistant", generation: null,
          prompt: "Create a task", timeoutSeconds: 300, node: "native", epoch: 1,
          session: "prior-session", attempt: "prior-attempt", expires: Date.now() + 30_000,
        }],
      };
    else if (request.url === "/v1/node/heartbeat") heartbeats.push(input.stats);
    else if (request.url === "/v1/node/catalog") result = [];
    else if (request.url === "/v1/node/poll") result = {assignment: null};
    response.setHeader("content-type", "application/json");
    response.end(JSON.stringify(result));
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  assert.ok(address && typeof address !== "string");
  const controller = `http://127.0.0.1:${address.port}`;
  let node;
  try {
    node = await runNode({
      id: "native", role: "agent", projects: ["demo"], assistantProjects: ["demo"],
      origin: null, slots: 2, cacheBytes: 16 * 1024 * 1024, state, port: 0,
      controller, token: "test-project-scoped-token",
      nativeAssistant: {
        endpoint: controller, token: "test-private-native-token", allowHttp: true,
        bindings: { demo: {host: "worker", workspace: "ebox-task", taskToolsConfigured: true} },
      },
    });
    const deadline = Date.now() + 10_000;
    while (heartbeats.length < 2 && Date.now() < deadline)
      await new Promise((resolve) => setTimeout(resolve, 25));
    assert.ok(heartbeats.length >= 2, "node must remain connected and report its blocked state");
    assert.ok(heartbeats.every((stats) => stats.uncertainNativeJobs === 1));
    assert.equal(calls.includes("/v1/node/poll"), false, "spare slots cannot bypass reconciliation");
    assert.equal(calls.includes("/v1/node/reconcile"), false, "native work cannot be reconciled as read-only");
    assert.ok(calls.every((path) => path.startsWith("/v1/node/")), "no native request may be replayed");
  } finally {
    await node?.stop();
    server.closeAllConnections();
    await new Promise<void>((resolve) => server.close(() => resolve()));
    await rm(state, {recursive: true, force: true});
  }
});
