import { test } from "node:test";
import assert from "node:assert/strict";
import { DatabaseSync } from "node:sqlite";
import {
  canonical,
  ProtocolError,
  type Assignment,
} from "@enginebox/fleet-protocol";
import { Client } from "../src/client.ts";
import { UncertainNativeRun } from "../src/native-assistant.ts";
import {
  deliverFinishedResult,
  type FinishedOutcome,
} from "../src/result-delivery.ts";

const assignment: Assignment = {
  id: "merge",
  project: "demo",
  kind: "integration",
  generation: "a".repeat(64),
  integration: {
    id: "merge",
    layerfsProject: "demo",
    baseRef: `12${"b".repeat(64)}`,
    baseRoot: "c".repeat(64),
    changes: [],
    taskRequirements: {},
  },
  node: "worker",
  epoch: 1,
  session: "session",
  attempt: "attempt",
  expires: Date.now() + 30000,
};
const success: FinishedOutcome = {
  ok: true,
  result: { generation: "d".repeat(64), treeHash: "e".repeat(64) },
};
function journal(value = success, state = "finished") {
  const db = new DatabaseSync(":memory:");
  db.exec(
    "CREATE TABLE operations(attempt TEXT PRIMARY KEY,state TEXT NOT NULL,assignment TEXT NOT NULL,result TEXT)",
  );
  db.prepare("INSERT INTO operations VALUES(?,?,?,?)").run(
    assignment.attempt,
    state,
    canonical(assignment),
    canonical(value),
  );
  return db;
}
function saved(db: DatabaseSync): FinishedOutcome {
  return JSON.parse(
    (db.prepare("SELECT result FROM operations").get() as { result: string })
      .result,
  );
}
function job(status = "assigned", result?: FinishedOutcome) {
  return {
    spec: { id: assignment.id, project: assignment.project },
    status,
    assignment,
    ...(result && { result: canonical(result.result) }),
  };
}
class FixtureClient extends Client {
  calls: { path: string; body?: unknown }[] = [];
  handler: (path: string, body: unknown) => unknown;
  constructor(handler: (path: string, body: unknown) => unknown) {
    super("http://127.0.0.1:1", "private-test-token");
    this.handler = handler;
  }
  override async json<T = unknown>(path: string, body?: unknown): Promise<T> {
    this.calls.push({ path, body });
    return (await this.handler(path, body)) as T;
  }
}

for (const status of [400, 409])
  test(`validation ${status} journals failure before releasing the same assigned attempt`, async () => {
    const db = journal();
    let posts = 0;
    const client = new FixtureClient((path, body) => {
      if (path.endsWith("/jobs/merge")) return job();
      const receipt = body as FinishedOutcome;
      if (++posts === 1) {
        assert.deepEqual(receipt.result, success.result);
        throw new ProtocolError(status, "Candidate accepted base changed");
      }
      assert.equal(receipt.ok, false);
      assert.deepEqual(saved(db), { ok: receipt.ok, result: receipt.result });
      assert.match(String(receipt.result.error), /new job must validate/);
      return job("failed", receipt);
    });
    try {
      const result = await deliverFinishedResult(client, assignment, db);
      assert.equal(result.ok, false);
      assert.equal(posts, 2);
      assert.deepEqual(
        client.calls.map((c) => c.path),
        ["/v1/node/finish", "/v1/projects/demo/jobs/merge", "/v1/node/finish"],
      );
    } finally {
      db.close();
    }
  });

for (const terminal of ["completed", "failed"])
  test(`post-commit conflict preserves authoritative ${terminal} outcome`, async () => {
    const db = journal();
    const authoritative = {
      ok: terminal === "completed",
      result: { receipt: "durable-controller-outcome" },
    };
    let posts = 0;
    const client = new FixtureClient((path, body) => {
      if (path.endsWith("/jobs/merge")) return job(terminal, authoritative);
      if (++posts === 1)
        throw new ProtocolError(
          409,
          "Promotion callback conflicted after commit",
        );
      assert.deepEqual(saved(db), authoritative);
      assert.deepEqual(body, { assignment, ...authoritative });
      return job(terminal, authoritative);
    });
    try {
      assert.deepEqual(
        await deliverFinishedResult(client, assignment, db),
        authoritative,
      );
      assert.equal(posts, 2);
    } finally {
      db.close();
    }
  });

test("lost failure acknowledgement retries the persisted failure without replaying success", async () => {
  const db = journal();
  let posts = 0;
  let failure: FinishedOutcome | undefined;
  const client = new FixtureClient((path, body) => {
    if (path.endsWith("/jobs/merge")) return job();
    posts++;
    if (posts === 1) throw new ProtocolError(409, "Task requirements changed");
    const receipt = body as FinishedOutcome;
    assert.equal(receipt.ok, false);
    if (posts === 2) {
      failure = saved(db);
      throw new Error("Connection reset after controller commit");
    }
    assert.deepEqual(body, { assignment, ...failure });
    return job("failed", receipt);
  });
  try {
    await assert.rejects(
      deliverFinishedResult(client, assignment, db),
      /Connection reset/,
    );
    assert.deepEqual(
      await deliverFinishedResult(client, assignment, db),
      failure,
    );
    assert.equal(posts, 3);
  } finally {
    db.close();
  }
});

for (const error of [
  new Error("Network failure"),
  new ProtocolError(403, "Invalid session"),
  new ProtocolError(500, "Storage unavailable"),
])
  test(`${error.message} preserves the original receipt without a validation readback`, async () => {
    const db = journal();
    const client = new FixtureClient(() => {
      throw error;
    });
    try {
      await assert.rejects(
        deliverFinishedResult(client, assignment, db),
        (e) => e === error,
      );
      assert.deepEqual(saved(db), success);
      assert.equal(client.calls.length, 1);
    } finally {
      db.close();
    }
  });

for (const changed of [
  { attempt: "other" },
  { node: "other" },
  { epoch: 2 },
  { session: "other" },
])
  test(`another ${Object.keys(changed)[0]} cannot replace this executor's receipt`, async () => {
    const db = journal();
    const client = new FixtureClient((path) => {
      if (path.endsWith("/jobs/merge"))
        return { ...job(), assignment: { ...assignment, ...changed } };
      throw new ProtocolError(409, "Result mismatch");
    });
    try {
      await assert.rejects(
        deliverFinishedResult(client, assignment, db),
        /different attempt/,
      );
      assert.deepEqual(saved(db), success);
      assert.equal(client.calls.length, 2);
    } finally {
      db.close();
    }
  });

test("a non-assigned nonterminal job cannot be failed by an old receipt", async () => {
  const db = journal();
  const client = new FixtureClient((path) => {
    if (path.endsWith("/jobs/merge")) return job("queued");
    throw new ProtocolError(409, "Result mismatch");
  });
  try {
    await assert.rejects(
      deliverFinishedResult(client, assignment, db),
      /not assigned/,
    );
    assert.deepEqual(saved(db), success);
    assert.equal(client.calls.length, 2);
  } finally {
    db.close();
  }
});

test("an uncertain executor never enters result delivery", async () => {
  const db = journal(success, "running");
  const client = new FixtureClient(() => {
    throw new Error("Must not call controller");
  });
  try {
    await assert.rejects(
      deliverFinishedResult(client, assignment, db),
      UncertainNativeRun,
    );
    assert.equal(client.calls.length, 0);
    assert.equal(
      (db.prepare("SELECT state FROM operations").get() as { state: string })
        .state,
      "running",
    );
  } finally {
    db.close();
  }
});

test("rejected known failure remains an exact retry receipt", async () => {
  const failed = { ok: false, result: { error: "Exited with code 1" } };
  const db = journal(failed);
  const client = new FixtureClient((path) => {
    if (path.endsWith("/jobs/merge")) return job();
    throw new ProtocolError(409, "Recording already expired");
  });
  try {
    await assert.rejects(
      deliverFinishedResult(client, assignment, db),
      /Recording already expired/,
    );
    assert.deepEqual(saved(db), failed);
    assert.equal(client.calls.length, 2);
  } finally {
    db.close();
  }
});
