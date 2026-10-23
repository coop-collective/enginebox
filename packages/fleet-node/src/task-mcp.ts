import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { McpServer } from "@modelcontextprotocol/server";
import { serveStdio } from "@modelcontextprotocol/server/stdio";
import { z } from "zod";
import {
  identifier,
  integer,
  record,
  requireThat,
  taskStatus,
  validateTaskCreate,
  validateTaskUpdate,
  validateTaskMove,
  type TaskStatus,
} from "@enginebox/fleet-protocol";
import { Client } from "./client.ts";

export type TaskToolConfig = {
  controller: string;
  token: string;
  project: string;
};
export function taskToolConfig(
  value: unknown,
  projectOverride?: string,
): TaskToolConfig {
  record(value);
  requireThat(typeof value.controller === "string", "Controller URL required");
  const url = new URL(value.controller);
  requireThat(
    ["http:", "https:"].includes(url.protocol) &&
      !url.username &&
      !url.password &&
      url.pathname === "/" &&
      !url.search &&
      !url.hash,
    "Controller must be an HTTP(S) origin",
  );
  requireThat(
    typeof value.token === "string" &&
      /^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?\.[a-f0-9]{64}$/.test(value.token),
    "Task MCP requires a scoped node token; owner credentials are forbidden",
    403,
  );
  const project = projectOverride ?? value.project;
  identifier(project);
  if (value.projects !== undefined)
    requireThat(
      Array.isArray(value.projects) && value.projects.includes(project),
      "Project not present in node configuration",
      403,
    );
  return { controller: url.origin, token: value.token, project };
}
export class TaskTools {
  readonly client: Client;
  readonly base: string;
  private project: string;
  private scope?: Promise<void>;
  constructor(config: TaskToolConfig) {
    const valid = taskToolConfig(config);
    this.client = new Client(valid.controller, valid.token);
    this.base = `/v1/projects/${valid.project}`;
    this.project = valid.project;
  }
  private async request(path: string, body?: unknown) {
    this.scope ??= this.client
      .json<{ role: string; projects: string[] }>("/v1/node/identity")
      .then((identity) => {
        requireThat(
          identity.role === "agent" && identity.projects.includes(this.project),
          "Task MCP requires an agent credential scoped to this project",
          403,
        );
      })
      .catch((error) => {
        this.scope = undefined;
        throw error;
      });
    await this.scope;
    return this.client.json(path, body);
  }
  list(input: { cursor?: string; limit?: number; status?: TaskStatus } = {}) {
    const query = new URLSearchParams();
    if (input.cursor) {
      identifier(input.cursor);
      query.set("cursor", input.cursor);
    }
    if (input.limit !== undefined) {
      integer(input.limit, 1, 100);
      query.set("limit", String(input.limit));
    }
    if (input.status !== undefined) {
      taskStatus(input.status);
      query.set("status", input.status);
    }
    return this.request(`${this.base}/tasks?${query}`);
  }
  get(id: string) {
    identifier(id);
    return this.request(`${this.base}/tasks/${id}`);
  }
  create(input: unknown) {
    return this.request(`${this.base}/tasks`, validateTaskCreate(input));
  }
  update(id: string, input: unknown) {
    identifier(id);
    return this.request(
      `${this.base}/tasks/${id}/update`,
      validateTaskUpdate(input),
    );
  }
  move(id: string, input: unknown) {
    identifier(id);
    const move = validateTaskMove(input);
    requireThat(
      move.status !== "shipped",
      "Codex cannot approve shipping",
      403,
    );
    return this.request(`${this.base}/tasks/${id}/move`, move);
  }
  activity(input: { cursor?: number; limit?: number } = {}) {
    const cursor = input.cursor ?? 0,
      limit = input.limit ?? 50;
    integer(cursor, 0, Number.MAX_SAFE_INTEGER);
    integer(limit, 1, 100);
    return this.request(
      `${this.base}/tasks/activity?cursor=${cursor}&limit=${limit}`,
    );
  }
  agents() {
    return this.request(`${this.base}/activity`);
  }
}
const id = z.string().regex(/^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$/);
const hash = z.string().regex(/^[a-f0-9]{64}$/);
const source = z
  .object({
    kind: z.literal("chat"),
    conversation: z.string().min(1).max(128),
    messageIds: z.array(z.string().min(1).max(128)).min(1).max(32),
  })
  .strict();
async function result(call: () => Promise<unknown>) {
  try {
    return {
      content: [{ type: "text" as const, text: JSON.stringify(await call()) }],
    };
  } catch (error) {
    return {
      isError: true,
      content: [{ type: "text" as const, text: (error as Error).message }],
    };
  }
}
export function taskMcpServer(config: TaskToolConfig) {
  const tools = new TaskTools(config);
  const server = new McpServer(
    { name: "enginebox-tasks", version: "0.1.0" },
    { capabilities: { tools: {} } },
  );
  server.registerTool(
    "enginebox_tasks_list",
    {
      description:
        "List project tasks and revisions. A task board move does not execute or merge code.",
      inputSchema: z
        .object({
          cursor: id.optional(),
          limit: z.number().int().min(1).max(100).optional(),
          status: z
            .enum(["backlog", "in_progress", "testing", "shipped"])
            .optional(),
        })
        .strict(),
      annotations: { readOnlyHint: true },
    },
    (input) => result(() => tools.list(input)),
  );
  server.registerTool(
    "enginebox_tasks_get",
    {
      description:
        "Read one task before changing it. Use its revision in the mutation.",
      inputSchema: z.object({ id }).strict(),
      annotations: { readOnlyHint: true },
    },
    (input) => result(() => tools.get(input.id)),
  );
  server.registerTool(
    "enginebox_tasks_create",
    {
      description:
        "Create an idea or issue with required owner review. Reuse the same ID and content when retrying.",
      inputSchema: z
        .object({
          id,
          title: z.string().min(1).max(200),
          description: z.string().max(16000).optional(),
          type: z.enum(["idea", "issue"]),
          source: source.optional(),
        })
        .strict(),
    },
    (input) => result(() => tools.create(input)),
  );
  server.registerTool(
    "enginebox_tasks_update",
    {
      description:
        "Edit task text/type/assignment using expectedRevision and a unique operationId. Changing content clears previous result evidence.",
      inputSchema: z
        .object({
          id,
          operationId: id,
          expectedRevision: z.number().int().positive(),
          patch: z
            .object({
              title: z.string().min(1).max(200).optional(),
              description: z.string().max(16000).optional(),
              type: z.enum(["idea", "issue"]).optional(),
              assignee: id.nullable().optional(),
            })
            .strict(),
        })
        .strict(),
    },
    (input) => {
      const { id: taskId, ...operation } = input;
      return result(() => tools.update(taskId, operation));
    },
  );
  server.registerTool(
    "enginebox_tasks_move",
    {
      description:
        "Move a task into backlog, in progress, or testing. Optional published generation/candidate identifies review evidence. Codex cannot approve or ship its own work.",
      inputSchema: z
        .object({
          id,
          operationId: id,
          expectedRevision: z.number().int().positive(),
          status: z.enum(["backlog", "in_progress", "testing"]),
          generation: hash.optional(),
          candidate: id.optional(),
        })
        .strict(),
    },
    (input) => {
      const { id: taskId, ...operation } = input;
      return result(() => tools.move(taskId, operation));
    },
  );
  server.registerTool(
    "enginebox_tasks_activity",
    {
      description: "Read persisted task audit events after a sequence number.",
      inputSchema: z
        .object({
          cursor: z.number().int().nonnegative().optional(),
          limit: z.number().int().min(1).max(100).optional(),
        })
        .strict(),
      annotations: { readOnlyHint: true },
    },
    (input) => result(() => tools.activity(input)),
  );
  server.registerTool(
    "enginebox_agents_activity",
    {
      description:
        "Read actual project node and durable job activity. This does not start an agent.",
      inputSchema: z.object({}).strict(),
      annotations: { readOnlyHint: true },
    },
    () => result(() => tools.agents()),
  );
  return server;
}
async function main() {
  requireThat(process.argv[2], "Usage: task-mcp.ts PRIVATE_CONFIG [PROJECT]");
  const config = taskToolConfig(
    JSON.parse(await readFile(process.argv[2], "utf8")),
    process.argv[3],
  );
  serveStdio(() => taskMcpServer(config), {
    onerror: (error) => console.error(JSON.stringify({ error: error.message })),
  });
}
if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
)
  main().catch((error) => {
    console.error(JSON.stringify({ error: error.message }));
    process.exitCode = 1;
  });
