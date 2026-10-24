// Executed by the isolated Compose smoke against the real Worker/DO service.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { Client as McpClient } from "@modelcontextprotocol/client";
import { StdioClientTransport } from "@modelcontextprotocol/client/stdio";
import { TaskTools, taskToolConfig } from "../src/task-mcp.ts";

const [configPath, project = "demo"] = process.argv.slice(2);
assert.ok(configPath, "A private scoped node configuration is required");
const config = taskToolConfig(
  JSON.parse(await readFile(configPath, "utf8")),
  project,
);
const id = `mcp-${crypto.randomUUID()}`;
const client = new McpClient({
  name: "enginebox-task-proof",
  version: "0.1.0",
});
const transport = new StdioClientTransport({
  command: process.execPath,
  args: [
    fileURLToPath(new URL("../src/task-mcp.ts", import.meta.url)),
    configPath,
    project,
  ],
  stderr: "pipe",
});
transport.stderr?.on("data", () => {});
async function call(name: string, args: Record<string, unknown>) {
  const result = await client.callTool({ name, arguments: args });
  assert.ok(!result.isError, JSON.stringify(result));
  const content = result.content as { type: string; text?: string }[];
  return JSON.parse(content.find((item) => item.type === "text")!.text!);
}
try {
  await client.connect(transport);
  const names = (await client.listTools()).tools.map((tool) => tool.name);
  for (const name of ["list", "get", "create", "update", "move", "activity"])
    assert.ok(names.includes(`enginebox_tasks_${name}`));
  const input = {
    id,
    title: "MCP task proof",
    type: "idea",
    source: { kind: "chat", conversation: "smoke", messageIds: [id] },
  };
  const task = await call("enginebox_tasks_create", input);
  assert.equal(task.status, "backlog");
  assert.equal(task.createdBy, "node:" + config.token.split(".")[0]);
  assert.equal((await call("enginebox_tasks_create", input)).revision, 1);
  const edited = await call("enginebox_tasks_update", {
    id,
    operationId: "edit",
    expectedRevision: task.revision,
    patch: { description: "Changed through the official MCP client" },
  });
  const moved = await call("enginebox_tasks_move", {
    id,
    operationId: "start",
    expectedRevision: edited.revision,
    status: "in_progress",
  });
  assert.equal(
    (await call("enginebox_tasks_get", { id })).revision,
    moved.revision,
  );
  assert.ok(
    (await call("enginebox_tasks_list", { status: "in_progress" })).tasks.some(
      (task: { id: string }) => task.id === id,
    ),
  );
  assert.ok(
    (await call("enginebox_tasks_activity", {})).events.some(
      (event: { taskId: string; action: string }) =>
        event.taskId === id && event.action === "moved",
    ),
  );
  const observed = (await new TaskTools(config).get(id)) as {
    status: string;
    revision: number;
  };
  assert.equal(observed.status, "in_progress");
  assert.equal(observed.revision, 3);
  assert.throws(
    () =>
      new TaskTools(config).move(id, {
        operationId: "ship",
        expectedRevision: 3,
        status: "shipped",
      }),
    /Codex cannot approve/,
  );
  console.log(
    JSON.stringify({
      ok: true,
      task: id,
      status: observed.status,
      revision: observed.revision,
      tools: names,
    }),
  );
} finally {
  await client.close();
}
