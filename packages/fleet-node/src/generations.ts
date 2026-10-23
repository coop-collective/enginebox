import { DatabaseSync, backup } from "node:sqlite";
import {
  mkdir,
  open,
  rename,
  rm,
  stat,
  statfs,
  copyFile,
  readFile,
  lstat,
} from "node:fs/promises";
import { createHash, randomUUID } from "node:crypto";
import { join, resolve } from "node:path";
import {
  canonical,
  CHUNK_BYTES,
  MAX_CHUNK_BYTES,
  MAX_JSON_BYTES,
  PAGE_CHUNKS,
  readBounded,
  requireThat,
  sha256,
  validateChunks,
  validateGeneration,
  type Chunk,
  type Generation,
  type IndexPage,
} from "@enginebox/fleet-protocol";
import { Client } from "./client.ts";
import { Cache } from "./cache.ts";
import { Snapshots } from "./snapshots.ts";
import { syncDirectory } from "./files.ts";

export type Published = { id: string; manifest: Generation; sequence: number };
export type Route = {
  node: string;
  origin: string;
  ready: boolean;
  token: string;
};
export type Routes = { generation: string; expires: number; origins: Route[] };
export function checkSqlite(path: string) {
  const db = new DatabaseSync(path, { readOnly: true });
  try {
    const integrity = db.prepare("PRAGMA integrity_check").all();
    requireThat(
      integrity.length === 1 && Object.values(integrity[0])[0] === "ok",
      "SQLite integrity check failed",
    );
    requireThat(
      db.prepare("PRAGMA foreign_key_check").all().length === 0,
      "SQLite foreign key check failed",
    );
  } finally {
    db.close();
  }
}
export async function hashFile(path: string) {
  const file = await open(path, "r");
  const hash = createHash("sha256");
  try {
    for await (const block of file.createReadStream({ autoClose: false }))
      hash.update(block);
  } finally {
    await file.close();
  }
  return hash.digest("hex");
}
export async function publishSnapshot(
  client: Client,
  project: string,
  source: string,
  state: string,
  parent: string | null,
  metadata: Generation["metadata"] = {},
): Promise<Published> {
  const original = await lstat(source);
  requireThat(
    original.isFile() && !original.isSymbolicLink(),
    "Snapshot source must be a regular file",
  );
  await mkdir(state, { recursive: true, mode: 0o700 });
  const free = await statfs(state);
  requireThat(
    Number(free.bavail) * Number(free.bsize) >
      original.size * 2 + 64 * 1024 * 1024,
    "Insufficient snapshot staging space",
    507,
  );
  const staging = join(state, `publish-${randomUUID()}.sqlite`);
  const db = new DatabaseSync(source, { readOnly: true });
  const deadline = Date.now() + 10 * 60_000;
  const pageSize = Number(
    Object.values(db.prepare("PRAGMA page_size").get()!)[0],
  );
  try {
    await backup(db, staging, {
      progress: ({ totalPages }) => {
        requireThat(Date.now() < deadline, "Snapshot backup timed out", 408);
        requireThat(
          totalPages * pageSize + 64 * 1024 * 1024 <
            Number(free.bavail) * Number(free.bsize),
          "Snapshot grew beyond available staging space",
          507,
        );
      },
    });
  } finally {
    db.close();
  }
  // Keep failed publications for recovery; only successful staging is deleted.
  checkSqlite(staging);
  const file = await open(staging, "r+");
  const size = (await file.stat()).size;
  await file.sync();
  const hash = createHash("sha256");
  const pages: IndexPage[] = [];
  let chunks: Chunk[] = [];
  async function upload(kind: string, digest: string, bytes: Uint8Array) {
    const path = `/v1/data/${project}/${kind}/${digest}`;
    const present = await client.request(path, { method: "HEAD" });
    if (present.status === 404)
      await client.request(path, { method: "PUT", body: bytes as BodyInit });
    else
      requireThat(
        Number(present.headers.get("content-length")) === bytes.length,
        "Existing object has different length",
        409,
      );
  }
  async function flushPage() {
    if (!chunks.length) return;
    const bytes = new TextEncoder().encode(canonical(chunks));
    const digest = await sha256(bytes);
    await upload("pages", digest, bytes);
    pages.push({
      hash: digest,
      bytes: bytes.length,
      count: chunks.length,
      contentBytes: chunks.reduce((n, c) => n + c.bytes, 0),
    });
    chunks = [];
  }
  try {
    let position = 0;
    while (position < size) {
      const buffer = Buffer.alloc(Math.min(CHUNK_BYTES, size - position));
      let filled = 0;
      while (filled < buffer.length) {
        const read = await file.read(
          buffer,
          filled,
          buffer.length - filled,
          position + filled,
        );
        requireThat(read.bytesRead > 0, "Snapshot truncated");
        filled += read.bytesRead;
      }
      hash.update(buffer);
      const digest = createHash("sha256").update(buffer).digest("hex");
      await upload("chunks", digest, buffer);
      chunks.push({ hash: digest, bytes: buffer.length });
      if (chunks.length === PAGE_CHUNKS) await flushPage();
      position += buffer.length;
    }
    await flushPage();
  } finally {
    await file.close();
  }
  const manifest = validateGeneration({
    schema: 1,
    project,
    parent,
    format: "sqlite",
    snapshotHash: hash.digest("hex"),
    snapshotBytes: size,
    pages,
    metadata,
  });
  const published = await client.json<Published>(
    `/v1/projects/${project}/generations`,
    manifest,
  );
  requireThat(
    published.id === (await sha256(canonical(manifest))),
    "Controller returned a different generation",
  );
  await rm(staging);
  return published;
}
export class Generations {
  client: Client;
  cache: Cache;
  root: string;
  routes = new Map<string, Routes>();
  indexes = new Map<string, { published: Published; chunks: Chunk[] }>();
  stats = { originFetches: 0, peerFetches: 0, peerFailures: 0 };
  unavailableOrigins = new Map<string, number>();
  snapshots: Snapshots;
  hydrating = new Map<string, Promise<string>>();
  private hydrationTail: Promise<unknown> = Promise.resolve();
  constructor(
    client: Client,
    cache: Cache,
    root: string,
    snapshotBytes = cache.budget * 4,
  ) {
    this.client = client;
    this.cache = cache;
    this.root = root;
    this.snapshots = new Snapshots(cache, snapshotBytes);
  }
  async index(project: string, id: string) {
    const key = `${project}/${id}`;
    const cached = this.indexes.get(key);
    if (cached) return cached;
    const published = await this.client.json<Published>(
      `/v1/projects/${project}/generations/${id}`,
    );
    const manifest = validateGeneration(published.manifest);
    requireThat(
      published.id === id &&
        manifest.project === project &&
        (await sha256(canonical(manifest))) === id,
      "Manifest identity mismatch",
    );
    const chunks: Chunk[] = [];
    for (const page of manifest.pages) {
      const response = await this.client.request(
        `/v1/data/${project}/pages/${page.hash}`,
      );
      const bytes = await readBounded(response.body, MAX_JSON_BYTES);
      requireThat(
        bytes.length === page.bytes && (await sha256(bytes)) === page.hash,
        "Index page digest mismatch",
      );
      const entries = validateChunks(
        JSON.parse(new TextDecoder().decode(bytes)),
      );
      requireThat(
        entries.length === page.count &&
          entries.reduce((n, c) => n + c.bytes, 0) === page.contentBytes,
        "Index page sizes differ",
      );
      chunks.push(...entries);
    }
    if (this.indexes.size >= 8)
      this.indexes.delete(this.indexes.keys().next().value!);
    const value = { published: { ...published, manifest }, chunks };
    this.indexes.set(key, value);
    return value;
  }
  async chunk(project: string, id: string, chunk: Chunk, usePeers = true) {
    const bytes = await this.cache.get(project, chunk.hash, async () => {
      if (usePeers) {
        const key = `${project}/${id}`;
        let routes = this.routes.get(key);
        if (!routes || routes.expires < Date.now() + 5000) {
          routes = await this.client.json<Routes>(
            `/v1/projects/${project}/routes/${id}`,
          );
          this.routes.set(key, routes);
        }
        for (const route of routes.origins) {
          if ((this.unavailableOrigins.get(route.node) ?? 0) > Date.now())
            continue;
          try {
            const response = await fetch(
              `${route.origin}/v1/chunks/${project}/${chunk.hash}`,
              {
                headers: { authorization: `Bearer ${route.token}` },
                signal: AbortSignal.timeout(2500),
                redirect: "error",
              },
            );
            requireThat(response.ok, "Storage origin unavailable", 502);
            const data = await readBounded(response.body, MAX_CHUNK_BYTES);
            requireThat(
              data.length === chunk.bytes &&
                (await sha256(data)) === chunk.hash,
              "Storage origin returned invalid bytes",
              502,
            );
            this.stats.peerFetches++;
            this.unavailableOrigins.delete(route.node);
            return data;
          } catch {
            this.stats.peerFailures++;
            this.unavailableOrigins.set(route.node, Date.now() + 30_000);
          }
        }
      }
      this.stats.originFetches++;
      const response = await this.client.request(
        `/v1/data/${project}/chunks/${chunk.hash}`,
      );
      return readBounded(response.body, MAX_CHUNK_BYTES);
    });
    requireThat(bytes.length === chunk.bytes, "Chunk length mismatch");
    return bytes;
  }
  async warm(project: string, id: string) {
    const { chunks } = await this.index(project, id);
    const owner = `generation:${project}:${id}`;
    try {
      for (const chunk of chunks) {
        this.cache.pin(project, chunk.hash, owner);
        await this.chunk(project, id, chunk, false);
      }
    } catch (error) {
      this.cache.unpin(owner);
      throw error;
    }
    return chunks.reduce((n, c) => n + c.bytes, 0);
  }
  async hydrate(project: string, id: string) {
    const key = `${project}/${id}`;
    const existing = this.hydrating.get(key);
    if (existing) return existing;
    const work = this.hydrationTail.then(() => {
      this.snapshots.active.add(key);
      return this.hydrateOnce(project, id);
    });
    this.hydrating.set(key, work);
    this.hydrationTail = work.catch(() => {});
    try {
      return await work;
    } finally {
      this.hydrating.delete(key);
      this.snapshots.active.delete(key);
    }
  }
  private async hydrateOnce(project: string, id: string) {
    const { published, chunks } = await this.index(project, id);
    const directory = join(this.root, "generations", project, id);
    await mkdir(directory, { recursive: true, mode: 0o700 });
    const target = join(directory, "store.sqlite");
    try {
      const existing = await lstat(target);
      if (
        existing.isFile() &&
        !existing.isSymbolicLink() &&
        existing.size === published.manifest.snapshotBytes &&
        (await hashFile(target)) === published.manifest.snapshotHash
      ) {
        checkSqlite(target);
        this.snapshots.touch(project, id, existing.size);
        return target;
      }
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    }
    await this.snapshots.cleanPartials(project, id);
    await rm(target, { force: true });
    this.cache.db
      .prepare("DELETE FROM snapshots WHERE project=? AND generation=?")
      .run(project, id);
    await this.snapshots.room(published.manifest.snapshotBytes);
    const free = await statfs(directory);
    requireThat(
      Number(free.bavail) * Number(free.bsize) >
        published.manifest.snapshotBytes + this.cache.reserve,
      "Insufficient hydration space",
      507,
    );
    const path = join(directory, `store-${randomUUID()}.partial`);
    const file = await open(path, "wx", 0o600);
    const hash = createHash("sha256");
    const owner = `hydrate:${project}:${id}`;
    try {
      for (const chunk of chunks) {
        this.cache.pin(project, chunk.hash, owner);
        try {
          const bytes = await this.chunk(project, id, chunk);
          hash.update(bytes);
          await file.writeFile(bytes);
        } finally {
          this.cache.unpin(owner);
        }
      }
      await file.sync();
    } finally {
      await file.close();
      this.cache.unpin(owner);
    }
    requireThat(
      hash.digest("hex") === published.manifest.snapshotHash,
      "Whole snapshot digest mismatch",
    );
    checkSqlite(path);
    await rename(path, target);
    await syncDirectory(directory);
    this.snapshots.touch(project, id, published.manifest.snapshotBytes);
    return target;
  }
}
