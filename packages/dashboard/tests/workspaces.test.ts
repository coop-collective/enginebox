import { test } from "node:test";
import assert from "node:assert/strict";
import { duration, sourceBytes, mergeWorkspaces } from "../lib/workspaces";
test("unknown measurement differs from a measured zero", () => {
  assert.equal(duration(null), "Not measured");
  assert.equal(sourceBytes(undefined), "Not measured");
  assert.equal(duration(0), "0 ms");
  assert.equal(sourceBytes(0), "0.00 MiB");
  assert.equal(duration(-1), "Not measured");
});
test("environment-only sources become durable workspace identities, without conflating environment IDs", () => {
  const all = mergeWorkspaces(null, { environments: [
    { id: "runtime-1", source: { kind: "layerfs", project: "app" }, status: "running", healthy: true },
    { id: "runtime-2", source: { kind: "layerfs", project: "app" }, status: "running", healthy: true },
    { id: "unknown", source: { kind: "git" }, status: "running", healthy: true },
  ] });
  assert.deepEqual(all.map(w => w.id), ["app"]);
  assert.equal(all[0].repositories.length, 0);
});
