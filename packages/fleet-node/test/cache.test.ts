import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { Cache } from "../src/cache.ts";

const hash = (bytes: Uint8Array) =>
  createHash("sha256").update(bytes).digest("hex");
test("coalesces downloads, verifies hits and repairs corrupt cache data", async () => {
  const root = await mkdtemp(join(tmpdir(), "ebox-cache-"));
  const cache = new Cache(root, 1024, 0);
  try {
    const bytes = Buffer.from("the authoritative bytes");
    const digest = hash(bytes);
    let loads = 0;
    const load = async () => {
      loads++;
      await new Promise((r) => setTimeout(r, 10));
      return bytes;
    };
    await Promise.all(
      Array.from({ length: 16 }, () => cache.get("demo", digest, load)),
    );
    assert.equal(loads, 1);
    assert.equal(cache.stats.coalesced, 15);
    await cache.get("demo", digest, load);
    assert.equal(loads, 1);
    await writeFile(cache.path("demo", digest), "corrupt");
    assert.deepEqual(await cache.get("demo", digest, load), bytes);
    assert.equal(loads, 2);
    assert.equal(cache.stats.repairs, 1);
    await assert.rejects(
      cache.get("demo", "a".repeat(64), load),
      /digest mismatch/,
    );
    assert.equal(cache.bytes(), bytes.length);
  } finally {
    cache.close();
    await rm(root, { recursive: true, force: true });
  }
});
test("evicts cold data while retaining pinned chunks, and rejects an all-pinned full cache", async () => {
  const root = await mkdtemp(join(tmpdir(), "ebox-cache-"));
  const cache = new Cache(root, 100, 0);
  try {
    const a = Buffer.alloc(45, 1),
      b = Buffer.alloc(45, 2),
      c = Buffer.alloc(45, 3);
    await cache.get("demo", hash(a), async () => a);
    cache.pin("demo", hash(a), "active");
    await cache.get("demo", hash(b), async () => b);
    await cache.get("demo", hash(c), async () => c);
    assert.deepEqual(await readFile(cache.path("demo", hash(a))), a);
    assert.equal(cache.stats.evictedBytes, 45);
    cache.pin("demo", hash(c), "active");
    await assert.rejects(
      cache.get("demo", hash(b), async () => b),
      /Insufficient unpinned/,
    );
    cache.unpin("active");
    await cache.get("demo", hash(b), async () => b);
    assert.ok(cache.bytes() <= 100);
    assert.throws(() => cache.path("../escape", hash(a)), /Invalid identifier/);
  } finally {
    cache.close();
    await rm(root, { recursive: true, force: true });
  }
});
