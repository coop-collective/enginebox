import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { taskToolConfig, TaskTools } from "../src/task-mcp.ts";

const token = `agent-a.${"a".repeat(64)}`;
test("MCP configuration rejects owner credentials, ambiguous origins and scope escapes", () => {
  assert.throws(
    () =>
      taskToolConfig({
        controller: "https://fleet.example",
        token: "a".repeat(64),
        project: "demo",
      }),
    /owner credentials/,
  );
  assert.throws(
    () =>
      taskToolConfig({
        controller: "https://user:secret@fleet.example",
        token,
        project: "demo",
      }),
    /HTTP\(S\) origin/,
  );
  assert.throws(
    () =>
      taskToolConfig({
        controller: "https://fleet.example/path",
        token,
        project: "demo",
      }),
    /HTTP\(S\) origin/,
  );
  assert.throws(
    () =>
      taskToolConfig(
        { controller: "https://fleet.example", token, projects: ["demo"] },
        "private",
      ),
    /Project not present/,
  );
  const tools = new TaskTools({
    controller: "https://fleet.example",
    token,
    project: "demo",
  });
  assert.throws(() => tools.get("../private"), /Invalid identifier/);
  assert.throws(
    () =>
      tools.move("task", {
        operationId: "self-approve",
        expectedRevision: 1,
        status: "shipped",
      }),
    /Codex cannot approve/,
  );
});

test("MCP verifies server-side agent identity before sending task requests", async () => {
  const seen: string[] = [];
  let identity: unknown;
  const server = createServer((request, response) => {
    seen.push(request.url!);
    assert.equal(request.headers.authorization, `Bearer ${token}`);
    response.setHeader("content-type", "application/json");
    response.end(JSON.stringify(identity));
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  try {
    const address = server.address();
    assert.ok(address && typeof address !== "string");
    for (const invalid of [
      {role: "owner", projects: ["demo"]},
      {role: "agent", projects: ["demo"]},
      {role: "agent", projects: ["demo", "private"], taskOnly: true},
      {role: "agent", projects: ["private"], taskOnly: true},
    ]) {
      identity = invalid;
      const tools = new TaskTools({
        controller: `http://127.0.0.1:${address.port}`, token, project: "demo",
      });
      await assert.rejects(tools.list(), /requires a task-only agent credential/);
    }
    assert.deepEqual(seen, Array(4).fill("/v1/node/identity"));
  } finally {
    server.closeAllConnections();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
});
