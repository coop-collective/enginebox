import { env } from "cloudflare:workers";
import { reset, runInDurableObject } from "cloudflare:test";
import { afterEach, expect, it } from "vitest";
import {
  canonical,
  validateEngineBuild,
  type Generation,
} from "@enginebox/fleet-protocol";
afterEach(() => reset());
it("DO queue, assignment, completion and acceptance retain the exact build artifact", async () => {
  const project = env.PROJECTS.getByName("demo"),
    node = env.NODES.getByName("builder");
  const generation = "a".repeat(64),
    tree = "b".repeat(64),
    recipe = "c".repeat(64),
    artifact = "d".repeat(64);
  await project.create("demo");
  const manifest: Generation = {
    schema: 1,
    project: "demo",
    parent: null,
    format: "sqlite",
    snapshotHash: "e".repeat(64),
    snapshotBytes: 1,
    pages: [{ hash: "f".repeat(64), bytes: 100, count: 1, contentBytes: 1 }],
    metadata: { integration: "integration-a", treeHash: tree },
  };
  await project.publish(generation, manifest);
  await project.configureChecks([
    { name: "playable", kind: "engine-build", recipe, timeoutSeconds: 30 },
  ]);
  await project.stage({
    id: "integration-a",
    project: "demo",
    base: null,
    generation,
    changes: [{ id: "change-a", generation }],
  });
  // A completed integration and registered artifact are fixture inputs here;
  // native LayerFS/build/publication are exercised by their separate live tests.
  await runInDurableObject(project, (instance) =>
    instance.put("job:integration-a", {
      spec: { id: "integration-a", project: "demo", kind: "integration" },
      status: "completed",
      assignment: { node: "builder" },
      result: canonical({ generation, treeHash: tree }),
    }),
  );
  const spec = validateEngineBuild({
    id: "build-a",
    project: "demo",
    kind: "engine-build",
    generation,
    timeoutSeconds: 30,
    build: {
      candidate: "integration-a",
      integration: "integration-a",
      sourceTree: tree,
      recipe,
    },
    check: {
      candidate: "integration-a",
      name: "playable",
      recipe,
      scope: "full",
    },
  });
  expect(
    await runInDurableObject(project, async (instance) => {
      try {
        await instance.enqueue({
          ...spec,
          build: { ...spec.build, sourceTree: "f".repeat(64) },
        });
        return null;
      } catch (error) {
        return String(error);
      }
    }),
  ).toMatch(/frozen integration/);
  await project.enqueue(spec);
  await node.create(
    {
      id: "builder",
      role: "agent",
      projects: ["demo"],
      origin: null,
      slots: 1,
      cacheBytes: 16777216,
      buildProjects: ["demo"],
    },
    "private-hash",
  );
  const hello = await node.hello("session");
  const assignment = await node.reserve(spec, hello.epoch, "session");
  await project.assign(assignment);
  await node.start(assignment);
  const result = canonical({
    generation,
    sourceTree: tree,
    recipe,
    artifact,
    check: spec.check,
  });
  expect(
    await runInDurableObject(project, async (instance) => {
      try {
        await instance.finish(assignment, result, true);
        return null;
      } catch (error) {
        return String(error);
      }
    }),
  ).toMatch(/published artifact/);
  const build = {
    id: artifact,
    project: "demo",
    generation,
    sourceTree: tree,
    recipe,
    candidate: "integration-a",
    createdBy: "node:builder",
  };
  await runInDurableObject(project, (instance) =>
    instance.put(`artifact:${artifact}`, build),
  );
  await project.finish(assignment, result, true);
  await runInDurableObject(project, (instance) =>
    instance.put(`artifact:${artifact}`, {
      ...build,
      sourceTree: "f".repeat(64),
    }),
  );
  expect(
    await runInDurableObject(project, async (instance) => {
      try {
        await instance.accept(generation, null, "integration-a");
        return null;
      } catch (error) {
        return String(error);
      }
    }),
  ).toMatch(/published artifact/);
  await runInDurableObject(project, (instance) =>
    instance.put(`artifact:${artifact}`, build),
  );
  const accepted = await project.accept(generation, null, "integration-a");
  expect(accepted.head).toBe(generation);
  const completed = await project.job("build-a");
  expect(JSON.parse(completed!.result!).artifact).toBe(artifact);
});
