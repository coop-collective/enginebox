// Test-only native executor fixture. The fleet/Worker/MCP paths are real;
// no provider inference, private native workspace, or LayerFS mount is exercised.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { runNode } from "../../fleet-node/src/main.ts";

const exec = promisify(execFile);
const proofScript = fileURLToPath(
  new URL("../../fleet-node/test/task-mcp-proof.ts", import.meta.url),
);
const requestText =
  "Create an idea titled MCP task proof, set its description, and move it to In Progress using the task tools.";

async function nativeFixture(config, project) {
  const token = randomBytes(32).toString("hex");
  const runId = randomBytes(16).toString("hex");
  const path = "/v1/hosts/smoke-host/instances/smoke-workspace/agent-runs";
  const abort = new AbortController();
  const state = {
    submissions: 0,
    polls: 0,
    status: "queued",
    proof: null,
    failure: null,
    output: "",
  };
  let executing;
  const server = createServer(async (req, res) => {
    res.setHeader("content-type", "application/json");
    try {
      assert.equal(req.headers.authorization, `Bearer ${token}`);
      if (req.method === "POST" && req.url === path) {
        state.submissions++;
        assert.equal(state.submissions, 1, "Native job must submit only once");
        const chunks = [];
        for await (const chunk of req) chunks.push(chunk);
        const body = JSON.parse(Buffer.concat(chunks).toString());
        assert.deepEqual(Object.keys(body).sort(), [
          "prompt",
          "provider",
          "timeout_seconds",
          "workdir",
        ]);
        assert.equal(body.provider, "codex");
        assert.equal(body.workdir, ".");
        assert.equal(body.timeout_seconds, 300);
        assert.ok(body.prompt.endsWith(requestText));
        assert.ok(body.prompt.includes(`Enginebox project ${project}`));
        assert.ok(!body.prompt.includes(token));
        assert.ok(!body.prompt.includes(config));
        // A native API acknowledgment precedes execution. Mutations are made
        // only by an official MCP client spawning the actual scoped server.
        executing = exec(process.execPath, [proofScript, config, project], {
          timeout: 45_000,
          maxBuffer: 1024 * 1024,
          windowsHide: true,
          signal: abort.signal,
        })
          .then(({ stdout }) => {
            state.proof = JSON.parse(stdout);
            assert.equal(state.proof.ok, true);
            state.output = JSON.stringify({
              type: "item.completed",
              item: {
                type: "agent_message",
                text: `Fixture executor created and updated task ${state.proof.task}; it is in progress at revision ${state.proof.revision}.`,
              },
            });
            state.status = "succeeded";
          })
          .catch((error) => {
            if (!abort.signal.aborted) state.failure = error;
            state.status = abort.signal.aborted ? "cancelled" : "failed";
          });
        res.end(JSON.stringify({ run: { id: runId, status: "queued" } }));
        return;
      }
      if (req.method === "POST" && req.url === `${path}/${runId}/cancel`) {
        abort.abort();
        await executing;
        state.status = "cancelled";
      } else {
        assert.equal(req.method, "GET");
        assert.equal(req.url, `${path}/${runId}`);
        state.polls++;
      }
      res.end(
        JSON.stringify({
          run: {
            id: runId,
            status: state.status,
            result: {
              exit_code: state.status === "succeeded" ? 0 : 1,
              timed_out: false,
              output: state.output,
            },
          },
        }),
      );
    } catch (error) {
      state.failure = error;
      res.writeHead(500);
      res.end(
        JSON.stringify({ error: "Native test fixture rejected request" }),
      );
    }
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  return {
    token,
    endpoint: `http://127.0.0.1:${server.address().port}`,
    runId,
    state,
    async close() {
      abort.abort();
      await executing;
      server.closeAllConnections();
      await new Promise((resolve) => server.close(resolve));
    },
  };
}

export async function assistantDispatchSmoke({
  controller,
  state,
  api,
  until,
}) {
  // This project and node exist only in the smoke's unique Compose namespace.
  const project = "assistant-smoke";
  const base = `/v1/projects/${project}`;
  const directory = await mkdtemp(join(state, "assistant-proof-"));
  let fixture;
  let node;
  try {
    await api("/v1/projects", { id: project });
    assert.equal((await api(`${base}/assistant`)).available, false);
    const spec = {
      id: "assistant-proof",
      role: "agent",
      projects: [project],
      assistantProjects: [project],
      origin: null,
      slots: 1,
      cacheBytes: 16 * 1024 * 1024,
    };
    const { token } = await api("/v1/nodes", spec);
    const config = join(directory, "task-mcp.json");
    await writeFile(config, JSON.stringify({ controller, token, project }), {
      mode: 0o600,
      flag: "wx",
    });
    fixture = await nativeFixture(config, project);
    node = await runNode({
      ...spec,
      controller,
      token,
      state: join(directory, "node-metadata"),
      port: 0,
      nativeAssistant: {
        endpoint: fixture.endpoint,
        token: fixture.token,
        allowHttp: true,
        bindings: {
          [project]: {
            host: "smoke-host",
            workspace: "smoke-workspace",
            taskToolsConfigured: true,
          },
        },
      },
    });
    await until(
      async () => (await api(`${base}/assistant`, undefined, token)).available,
      "assistant-capable node heartbeat",
      30_000,
    );
    const id = `assistant-${randomBytes(8).toString("hex")}`;
    const input = { id, prompt: requestText };
    const queued = await api(`${base}/assistant`, input, token);
    assert.equal(queued.id, id);
    assert.ok(["queued", "assigned"].includes(queued.status));
    const answer = await until(
      async () => {
        if (fixture.state.failure) throw fixture.state.failure;
        const answer = await api(`${base}/assistant/${id}`, undefined, token);
        assert.notEqual(answer.status, "failed", answer.error);
        return answer.status === "completed" ? answer : null;
      },
      "assistant dispatch and scoped MCP mutations",
      60_000,
    );
    const job = await api(`${base}/jobs/${id}`);
    assert.equal(job.spec.kind, "assistant");
    assert.equal(job.spec.generation, null);
    assert.equal(job.assignment.node, spec.id);
    assert.ok(job.assignment.attempt);
    assert.equal(JSON.parse(job.result).nativeRunId, fixture.runId);
    assert.equal(fixture.state.submissions, 1);
    assert.ok(fixture.state.polls > 0);
    const proof = fixture.state.proof;
    assert.ok(proof?.ok);
    const task = await api(`${base}/tasks/${proof.task}`, undefined, token);
    assert.equal(task.status, "in_progress");
    assert.equal(task.revision, 3);
    assert.equal(task.createdBy, `node:${spec.id}`);
    assert.equal(task.updatedBy, `node:${spec.id}`);
    assert.equal(task.description, "Changed through the official MCP client");
    assert.match(answer.text, new RegExp(proof.task));
    assert.ok(!answer.text.includes(token));
    assert.ok(!answer.text.includes(fixture.token));
    assert.deepEqual(
      (await api(`${base}/tasks/activity`, undefined, token)).events.map(
        (event) => [event.taskId, event.action],
      ),
      [
        [task.id, "created"],
        [task.id, "updated"],
        [task.id, "moved"],
      ],
    );
    assert.equal(
      (await api(`${base}/assistant`, input, token)).status,
      "completed",
    );
    assert.equal(fixture.state.submissions, 1);
    assert.equal((await api(base)).generations.length, 0);
    assert.equal(
      node.cache.bytes(),
      0,
      "Metadata assistant must not hydrate a Store",
    );
    return {
      job: id,
      node: spec.id,
      task: task.id,
      revision: task.revision,
      liveInference: false,
    };
  } finally {
    try {
      await node?.stop();
    } finally {
      await fixture?.close();
      await rm(directory, { recursive: true, force: true });
    }
  }
}
