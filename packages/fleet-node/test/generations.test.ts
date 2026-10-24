import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomBytes } from "node:crypto";
import { DatabaseSync } from "node:sqlite";
import { canonical, sha256 } from "@enginebox/fleet-protocol";
import { Client } from "../src/client.ts";
import { Cache } from "../src/cache.ts";
import {
  Generations,
  hashFile,
  publishSnapshot,
  type Published,
} from "../src/generations.ts";

class ObjectTransport extends Client {
  objects = new Map<string, Uint8Array>();
  published: Published | undefined;
  constructor() {
    super("http://test.invalid", "test-only");
  }
  override async request(
    path: string,
    init: RequestInit = {},
  ): Promise<Response> {
    if (init.method === "PUT") {
      const bytes = new Uint8Array(init.body as Uint8Array);
      assert.equal(path.split("/").at(-1), await sha256(bytes));
      this.objects.set(path, bytes);
      return Response.json({ ok: true });
    }
    const bytes = this.objects.get(path);
    return new Response(init.method === "HEAD" ? null : (bytes as BodyInit), {
      status: bytes ? 200 : 404,
      headers: { "content-length": String(bytes?.length ?? 0) },
    });
  }
  override async json<T>(path: string, body?: unknown): Promise<T> {
    if (body) {
      this.published = {
        id: await sha256(canonical(body)),
        manifest: body as Published["manifest"],
        sequence: 1,
      };
      return this.published as T;
    }
    if (path.includes("/routes/"))
      return {
        generation: this.published!.id,
        expires: Date.now() + 60_000,
        origins: [],
      } as T;
    return this.published as T;
  }
}

test("backs up committed WAL data and restores a snapshot larger than the chunk cache", async () => {
  const root = await mkdtemp(join(tmpdir(), "ebox-generation-"));
  const source = new DatabaseSync(join(root, "source.sqlite"));
  source.exec(
    "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE assets(id INTEGER PRIMARY KEY, model BLOB)",
  );
  const insert = source.prepare("INSERT INTO assets(model) VALUES(?)");
  for (let i = 0; i < 20; i++) insert.run(randomBytes(1024 * 1024));
  const nodeRoot = join(root, "receiver");
  await mkdir(nodeRoot);
  const cache = new Cache(nodeRoot, 12 * 1024 * 1024, 0);
  try {
    const transport = new ObjectTransport();
    const generation = await publishSnapshot(
      transport,
      "demo",
      join(root, "source.sqlite"),
      join(root, "staging"),
      null,
    );
    assert.ok(generation.manifest.snapshotBytes > cache.budget);
    // The writer stays open with committed pages in its WAL throughout publication.
    assert.equal(
      (
        source.prepare("SELECT COUNT(*) AS n FROM assets").get() as {
          n: number;
        }
      ).n,
      20,
    );
    const receiver = new Generations(transport, cache, nodeRoot);
    const path = await receiver.hydrate("demo", generation.id);
    assert.equal(await hashFile(path), generation.manifest.snapshotHash);
    const restored = new DatabaseSync(path, { readOnly: true });
    try {
      assert.equal(
        (
          restored.prepare("SELECT COUNT(*) AS n FROM assets").get() as {
            n: number;
          }
        ).n,
        20,
      );
    } finally {
      restored.close();
    }
    assert.ok(cache.stats.evictedBytes > 0);
    assert.ok(cache.bytes() <= cache.budget);
    const fetched = receiver.stats.originFetches;
    assert.equal(await receiver.hydrate("demo", generation.id), path);
    assert.equal(receiver.stats.originFetches, fetched);
  } finally {
    cache.close();
    source.close();
    await rm(root, { recursive: true, force: true });
  }
});
