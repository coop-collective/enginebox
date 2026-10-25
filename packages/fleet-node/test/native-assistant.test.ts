import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer, type RequestListener } from "node:http";
import { DatabaseSync } from "node:sqlite";
import {
  NativeAssistant,
  UncertainNativeRun,
} from "../src/native-assistant.ts";
import type { Assignment } from "@enginebox/fleet-protocol";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

const assignment: Assignment = {
  id: "turn",
  project: "demo",
  kind: "assistant",
  generation: null,
  prompt: "Create an issue for the broken menu",
  timeoutSeconds: 10,
  node: "native",
  epoch: 1,
  session: "session",
  attempt: "attempt",
  expires: Date.now() + 30000,
};
const token = "private-native-controller-fixture";
async function fixture(handler: RequestListener) {
  const server = createServer(handler);
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  assert.ok(address && typeof address !== "string");
  const db = new DatabaseSync(":memory:");
  const adapter = new NativeAssistant(
    {
      endpoint: `http://127.0.0.1:${address.port}`,
      token,
      allowHttp: true,
      bindings: {
        demo: {
          host: "local",
          workspace: "ebox-assistant",
          taskToolsConfigured: true,
        },
      },
    },
    db,
  );
  return {
    adapter,
    db,
    close: async () => {
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
      db.close();
    },
  };
}

test("delegates to the native Codex API, returns only assistant text and never resubmits an attempt", async () => {
  const id = "a".repeat(32);
  let submissions = 0;
  const f = await fixture(async (req, res) => {
    assert.equal(req.headers.authorization, `Bearer ${token}`);
    if (req.method === "POST") {
      submissions++;
      assert.equal(
        req.url,
        "/v1/hosts/local/instances/ebox-assistant/agent-runs",
      );
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString());
      assert.equal(body.provider, "codex");
      assert.equal(body.workdir, ".");
      assert.equal(body.timeout_seconds, 10);
      assert.match(body.prompt, /Create an issue/);
      assert.equal(body.resume_id, undefined);
      res.end(JSON.stringify({ run: { id, status: "queued" } }));
    } else
      res.end(
        JSON.stringify({
          run: {
            id,
            status: "succeeded",
            result: {
              exit_code: 0,
              output: [
                JSON.stringify({
                  type: "item.completed",
                  item: {
                    type: "command_execution",
                    command: "private diagnostic",
                  },
                }),
                JSON.stringify({
                  type: "item.completed",
                  item: {
                    type: "agent_message",
                    text: "Created task menu-fix.",
                  },
                }),
              ].join("\n"),
            },
          },
        }),
      );
  });
  try {
    const result = await f.adapter.execute(
      assignment,
      new AbortController().signal,
    );
    assert.equal(result.text, "Created task menu-fix.");
    assert.equal(result.nativeRunId, id);
    await assert.rejects(
      f.adapter.execute(assignment, new AbortController().signal),
      UncertainNativeRun,
    );
    assert.equal(submissions, 1);
  } finally {
    await f.close();
  }
});

test("a lost native submit acknowledgment is journaled and never replayed", async () => {
  let submissions = 0;
  const f = await fixture(async (req, res) => {
    for await (const _ of req) {
    }
    submissions++;
    res.destroy();
  });
  try {
    await assert.rejects(
      f.adapter.execute(assignment, new AbortController().signal),
      UncertainNativeRun,
    );
    await assert.rejects(
      f.adapter.execute(assignment, new AbortController().signal),
      UncertainNativeRun,
    );
    assert.equal(submissions, 1);
    assert.equal(
      (
        f.db.prepare("SELECT state FROM assistant_runs").get() as {
          state: string;
        }
      ).state,
      "submitting",
    );
  } finally {
    await f.close();
  }
});

test("lease loss confirms native cancellation before the assignment can finish", async () => {
  const abort = new AbortController();
  const id = "a".repeat(32);
  let cancelled = false;
  const f = await fixture(async (req, res) => {
    for await (const _ of req) {
    }
    if (req.url?.endsWith("/cancel")) cancelled = true;
    else if (req.method === "GET") abort.abort();
    res.end(
      JSON.stringify({
        run: { id, status: cancelled ? "cancelled" : "running" },
      }),
    );
  });
  try {
    await assert.rejects(
      f.adapter.execute(assignment, abort.signal),
      (error) =>
        error instanceof Error && !(error instanceof UncertainNativeRun),
    );
    assert.equal(cancelled, true);
    assert.equal(
      (
        f.db.prepare("SELECT state FROM assistant_runs").get() as {
          state: string;
        }
      ).state,
      "settled",
    );
  } finally {
    await f.close();
  }
});

test("code execution prepares source before submission and returns only a successful validator receipt", async () => {
  const directory = await mkdtemp(join(tmpdir(), 'source-lifecycle-'));
  const record = join(directory, 'actions');
  const id = 'b'.repeat(32);
  const f = await fixture(async (req, res) => {
    if (req.method === 'POST') {
      assert.equal(await readFile(record, 'utf8'), 'prepare\n');
      let body = ''; for await (const chunk of req) body += chunk;
      assert.match(JSON.parse(body).prompt, /actual files/);
    }
    res.end(JSON.stringify({ run: { id, status: 'succeeded', result: { exit_code: 0, output: JSON.stringify({ type: 'item.completed', item: { type: 'agent_message', text: 'Edited the source.' } }) } } }));
  });
  f.adapter.config.bindings.demo.mode = 'code';
  f.adapter.config.bindings.demo.sourceLifecycle = { command: [process.execPath, '-e', `const fs=require('node:fs');let s='';process.stdin.on('data',c=>s+=c);process.stdin.on('end',()=>{const r=JSON.parse(s);fs.appendFileSync(${JSON.stringify(record)},r.action+'\\n');process.stdout.write(JSON.stringify({ok:true,summary:'Trusted validation passed.'}));});`] };
  try {
    const result = await f.adapter.execute(assignment, new AbortController().signal);
    assert.equal(await readFile(record, 'utf8'), 'prepare\ncomplete\n');
    assert.equal(result.text, 'Edited the source.\n\nTrusted validation passed.');
  } finally { await f.close(); await rm(directory, { recursive: true, force: true }); }
});

test("offline source preparation settles without submitting a native run", async () => {
  let submissions = 0;
  const f = await fixture((_req, res) => { submissions++; res.end('{}'); });
  f.adapter.config.bindings.demo.mode = 'code';
  f.adapter.config.bindings.demo.sourceLifecycle = { command: [process.execPath, '-e', 'process.stdout.write(JSON.stringify({ok:false,uncertain:false,summary:"Build machine offline"}))'] };
  try {
    await assert.rejects(f.adapter.execute(assignment, new AbortController().signal), /Build machine offline/);
    assert.equal(submissions, 0);
    assert.equal((f.db.prepare('SELECT state FROM assistant_runs').get() as {state:string}).state, 'settled');
  } finally { await f.close(); }
});

test("an explicit native admission refusal settles without cancellation or resubmission", async () => {
  let requests = 0;
  const f = await fixture((_req, res) => { requests++; res.writeHead(400); res.end('{}'); });
  try {
    await assert.rejects(f.adapter.execute(assignment, new AbortController().signal), e => e instanceof Error && !(e instanceof UncertainNativeRun) && /refused/.test(e.message));
    assert.equal(requests, 1);
    assert.equal((f.db.prepare('SELECT state FROM assistant_runs').get() as {state:string}).state, 'settled');
  } finally { await f.close(); }
});
