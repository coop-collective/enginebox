import assert from "node:assert/strict";
import { test } from "node:test";
import { checkoutRows, sourcePreview, workspaceBusy } from "../lib/storage";

test("storage previews require exact commit and approval evidence", () => {
  const preview = { commit: "a".repeat(40), approval: "b".repeat(64), files: ["package.json"], bytes: 100 };
  assert.deepEqual(sourcePreview(preview), preview);
  for (const invalid of [{ ...preview, commit: "HEAD" }, { ...preview, approval: "" }, { ...preview, files: [null] }, { ...preview, bytes: -1 }])
    assert.equal(sourcePreview(invalid), null);
  assert.equal(checkoutRows({ workspaces: [{ name: "a", project: "demo" }] }), null);
});

test("pending storage work only disables its own checkout including unknown outcomes", () => {
  const tasks = { "workspace:a": { operation: "checkpoint", busy: true }, "workspace:c": { operation: "close", busy: false, uncertain: true } };
  assert.equal(workspaceBusy(tasks, "a"), true);
  assert.equal(workspaceBusy(tasks, "b"), false);
  assert.equal(workspaceBusy(tasks, "c"), true);
});
