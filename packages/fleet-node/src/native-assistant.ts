import { DatabaseSync } from "node:sqlite";
import {
  identifier,
  requireThat,
  readBounded,
  type Assignment,
} from "@enginebox/fleet-protocol";

export type NativeAssistantConfig = {
  endpoint: string;
  token: string;
  allowHttp?: boolean;
  bindings: Record<
    string,
    {
      host: string;
      workspace: string;
      workdir?: string;
      taskToolsConfigured: true;
    }
  >;
};
export class UncertainNativeRun extends Error {}
export class NativeAssistant {
  config: NativeAssistantConfig;
  db: DatabaseSync;
  constructor(config: NativeAssistantConfig, db: DatabaseSync) {
    const url = new URL(config.endpoint);
    requireThat(
      (url.protocol === "https:" ||
        (config.allowHttp === true && url.protocol === "http:")) &&
        !url.username &&
        !url.password &&
        url.pathname === "/" &&
        !url.search &&
        !url.hash,
      "Invalid native controller endpoint",
    );
    requireThat(
      typeof config.token === "string" &&
        config.token.length >= 16 &&
        !/[\r\n]/.test(config.token),
      "Native controller credential required",
    );
    for (const [project, binding] of Object.entries(config.bindings)) {
      identifier(project);
      identifier(binding.host);
      identifier(binding.workspace);
      requireThat(
        binding.taskToolsConfigured === true,
        "Qualify scoped task MCP tools in the native workspace before enabling its assistant",
      );
      const dir = binding.workdir ?? ".";
      requireThat(
        dir.length > 0 &&
          dir.length <= 512 &&
          !dir.startsWith("/") &&
          !dir.includes("\\") &&
          !dir.split("/").includes("..") &&
          !/[\x00-\x1f]/.test(dir),
        "Invalid private assistant workdir",
      );
    }
    this.config = { ...config, endpoint: url.origin };
    this.db = db;
    db.exec(
      "CREATE TABLE IF NOT EXISTS assistant_runs(attempt TEXT PRIMARY KEY, path TEXT, state TEXT NOT NULL)",
    );
  }
  async call(path: string, body?: unknown) {
    const response = await fetch(this.config.endpoint + path, {
      method: body === undefined ? "GET" : "POST",
      headers: {
        authorization: `Bearer ${this.config.token}`,
        "content-type": "application/json",
      },
      body: body === undefined ? undefined : JSON.stringify(body),
      redirect: "error",
      signal: AbortSignal.timeout(10_000),
    });
    // Never include native controller bodies, credentials or private URLs in an app error.
    requireThat(
      response.ok,
      `Native controller refused the operation (${response.status})`,
      response.status,
    );
    const bytes = await readBounded(response.body, 2 * 1024 * 1024);
    return JSON.parse(new TextDecoder().decode(bytes)) as {
      run: {
        id: string;
        status: string;
        execution_uncertain?: boolean;
        result?: { output?: string; exit_code?: number; timed_out?: boolean };
      };
    };
  }
  async execute(assignment: Assignment, signal: AbortSignal) {
    requireThat(
      assignment.kind === "assistant",
      "Assistant assignment required",
    );
    const binding = this.config.bindings[assignment.project];
    requireThat(binding, "No native assistant workspace for this project", 409);
    requireThat(!signal.aborted, "Assistant lease expired", 409);
    if (
      this.db
        .prepare("SELECT attempt FROM assistant_runs WHERE attempt=?")
        .get(assignment.attempt)
    )
      throw new UncertainNativeRun(
        "Prior native attempt requires owner reconciliation; it will not be submitted again",
      );
    const base = `/v1/hosts/${binding.host}/instances/${binding.workspace}/agent-runs`;
    this.db
      .prepare("INSERT INTO assistant_runs VALUES(?,NULL,'submitting')")
      .run(assignment.attempt);
    let path: string | undefined;
    let settled = false;
    try {
      const submitted = await this.call(base, {
        provider: "codex",
        workdir: binding.workdir ?? ".",
        timeout_seconds: assignment.timeoutSeconds,
        prompt: `You are managing Enginebox project ${assignment.project}. Use the configured enginebox task MCP tools to read, create, update and move its tasks. Do not claim changes without successful tool results. You cannot approve or ship work; those require owner review and accepted candidate evidence. Do not modify provider credentials or operate on another project.\n\nUser request:\n${assignment.prompt}`,
      });
      requireThat(
        /^[a-f0-9]{32}$/.test(submitted.run?.id),
        "Native controller did not return a run identity",
      );
      path = `${base}/${submitted.run.id}`;
      this.db
        .prepare(
          "UPDATE assistant_runs SET path=?,state='running' WHERE attempt=?",
        )
        .run(path, assignment.attempt);
      const deadline = Date.now() + (assignment.timeoutSeconds + 90) * 1000;
      while (Date.now() < deadline && !signal.aborted) {
        const { run } = await this.call(path);
        requireThat(
          run?.id === submitted.run.id,
          "Native run identity changed",
        );
        if (!["queued", "running"].includes(run.status)) {
          if (run.execution_uncertain)
            throw new UncertainNativeRun(
              "Native runtime still holds an uncertain executor",
            );
          requireThat(
            ["succeeded", "failed", "cancelled"].includes(run.status),
            "Unknown native execution status",
          );
          settled = true;
          this.db
            .prepare(
              "UPDATE assistant_runs SET state='settled' WHERE attempt=?",
            )
            .run(assignment.attempt);
          requireThat(
            run.status === "succeeded" &&
              run.result?.exit_code === 0 &&
              !run.result.timed_out,
            "Native Codex run failed; inspect the native run record",
            409,
          );
          return {
            provider: "codex",
            nativeRunId: run.id,
            text: nativeText(run.result.output ?? ""),
          };
        }
        await new Promise((resolve) => setTimeout(resolve, 250));
      }
      throw new Error("Assistant lease or execution deadline expired");
    } catch (error) {
      if (!settled && path) {
        try {
          await this.call(`${path}/cancel`, {});
          const deadline = Date.now() + 15_000;
          while (Date.now() < deadline) {
            const { run } = await this.call(path);
            if (
              run?.id === path.split("/").at(-1) &&
              ["succeeded", "failed", "cancelled"].includes(run.status) &&
              !run.execution_uncertain
            ) {
              settled = true;
              this.db
                .prepare(
                  "UPDATE assistant_runs SET state='settled' WHERE attempt=?",
                )
                .run(assignment.attempt);
              break;
            }
            await new Promise((resolve) => setTimeout(resolve, 250));
          }
        } catch {
          /* Keep the durable reservation until an owner confirms stop. */
        }
      }
      if (!settled)
        throw new UncertainNativeRun(
          "Native submission or cancellation is uncertain; inspect the native runtime, reconcile this assignment and restart the node",
        );
      throw error;
    }
  }
}

export function nativeText(output: string) {
  const chunks: string[] = [];
  for (const line of output.split("\n")) {
    try {
      const event = JSON.parse(line);
      if (
        event.type === "item.completed" &&
        event.item?.type === "agent_message" &&
        typeof event.item.text === "string"
      )
        chunks.push(event.item.text);
    } catch {
      /* Native diagnostic lines are not assistant messages. */
    }
  }
  return (
    chunks.join("\n").slice(-32_000) ||
    "Codex completed without a text response. Check the task board for recorded changes."
  );
}
