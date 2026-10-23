import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, access, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomUUID } from "node:crypto";
import { Cache } from "../src/cache.ts";
import { Snapshots } from "../src/snapshots.ts";

test("recovers accounting and reclaims inactive snapshots without deleting live work", async () => {
  const root = await mkdtemp(join(tmpdir(), "ebox-snapshots-"));
  const cache = new Cache(root, 1024, 0);
  const snapshots = new Snapshots(cache, 100);
  const a = "a".repeat(64),
    b = "b".repeat(64);
  try {
    for (const id of [a, b]) {
      await mkdir(snapshots.path("demo", id), { recursive: true });
      await writeFile(
        join(snapshots.path("demo", id), "store.sqlite"),
        Buffer.alloc(40),
      );
    }
    const partial = join(
      snapshots.path("demo", a),
      `store-${randomUUID()}.partial`,
    );
    await writeFile(partial, "interrupted");
    const live = join(root, "live-workspace.sqlite");
    await writeFile(live, "unpublished");
    await snapshots.recover();
    await assert.rejects(access(partial));
    snapshots.active.add(`demo/${a}`);
    await snapshots.room(40);
    await access(join(snapshots.path("demo", a), "store.sqlite"));
    await assert.rejects(
      access(join(snapshots.path("demo", b), "store.sqlite")),
    );
    await access(live);
    await assert.rejects(snapshots.room(80), /Insufficient inactive/);
  } finally {
    cache.close();
    await rm(root, { recursive: true, force: true });
  }
});
