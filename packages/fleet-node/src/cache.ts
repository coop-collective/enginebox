import { DatabaseSync } from "node:sqlite";
import {
  mkdir,
  open,
  readFile,
  rename,
  rm,
  statfs,
  lstat,
  readdir,
} from "node:fs/promises";
import { join } from "node:path";
import { createHash, randomUUID } from "node:crypto";
import {
  digest,
  identifier,
  MAX_CHUNK_BYTES,
  requireThat,
} from "@enginebox/fleet-protocol";
import { syncDirectory } from "./files.ts";

export type CacheStats = {
  hits: number;
  misses: number;
  repairs: number;
  downloadedBytes: number;
  evictedBytes: number;
  coalesced: number;
};
export class Cache {
  root: string;
  budget: number;
  reserve: number;
  db: DatabaseSync;
  inflight = new Map<string, Promise<Uint8Array>>();
  pins = new Map<string, number>();
  stats: CacheStats = {
    hits: 0,
    misses: 0,
    repairs: 0,
    downloadedBytes: 0,
    evictedBytes: 0,
    coalesced: 0,
  };
  private admission: Promise<void> = Promise.resolve();
  constructor(root: string, budget: number, reserve = 64 * 1024 * 1024) {
    this.root = root;
    this.budget = budget;
    this.reserve = reserve;
    this.db = new DatabaseSync(join(root, "cache.sqlite"));
    this.db.exec(
      "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS chunks(project TEXT, hash TEXT, bytes INTEGER, used INTEGER, PRIMARY KEY(project,hash)); CREATE TABLE IF NOT EXISTS pins(project TEXT, hash TEXT, owner TEXT, PRIMARY KEY(project,hash,owner));",
    );
  }
  path(project: string, hash: string) {
    identifier(project);
    digest(hash);
    return join(this.root, "chunks", project, hash.slice(0, 2), hash);
  }
  bytes() {
    return Number(
      (
        this.db
          .prepare("SELECT COALESCE(SUM(bytes),0) AS bytes FROM chunks")
          .get() as { bytes: number }
      ).bytes,
    );
  }
  async cached(project: string, hash: string): Promise<Uint8Array | null> {
    const path = this.path(project, hash);
    try {
      const info = await lstat(path);
      requireThat(
        info.isFile() && !info.isSymbolicLink() && info.size <= MAX_CHUNK_BYTES,
        "Invalid cache object",
      );
      const bytes = await readFile(path);
      if (createHash("sha256").update(bytes).digest("hex") !== hash)
        throw new Error("corrupt chunk");
      this.db
        .prepare(
          "INSERT INTO chunks VALUES(?,?,?,?) ON CONFLICT(project,hash) DO UPDATE SET used=excluded.used,bytes=excluded.bytes",
        )
        .run(project, hash, bytes.length, Date.now());
      return bytes;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT")
        this.stats.repairs++;
      await rm(path, { force: true });
      this.db
        .prepare("DELETE FROM chunks WHERE project=? AND hash=?")
        .run(project, hash);
      return null;
    }
  }
  async get(
    project: string,
    hash: string,
    load: () => Promise<Uint8Array>,
  ): Promise<Uint8Array> {
    const key = `${project}/${hash}`;
    const existing = this.inflight.get(key);
    if (existing) {
      this.stats.coalesced++;
      return existing;
    }
    const work = (async () => {
      const cached = await this.cached(project, hash);
      if (cached) {
        this.stats.hits++;
        return cached;
      }
      this.stats.misses++;
      const bytes = await load();
      requireThat(
        bytes.length > 0 &&
          bytes.length <= MAX_CHUNK_BYTES &&
          createHash("sha256").update(bytes).digest("hex") === hash,
        "Downloaded chunk digest mismatch",
        502,
      );
      // Serialize accounting and installation; downloads remain concurrent.
      const previous = this.admission;
      let release!: () => void;
      this.admission = new Promise((resolve) => {
        release = resolve;
      });
      await previous;
      try {
        await this.makeRoom(bytes.length);
        const path = this.path(project, hash);
        await mkdir(join(path, ".."), { recursive: true, mode: 0o700 });
        const tmp = `${path}.${randomUUID()}.partial`;
        const file = await open(tmp, "wx", 0o600);
        try {
          await file.writeFile(bytes);
          await file.sync();
        } finally {
          await file.close();
        }
        try {
          await rename(tmp, path);
          await syncDirectory(join(path, ".."));
        } catch (error) {
          await rm(tmp, { force: true });
          throw error;
        }
        this.db
          .prepare("INSERT OR REPLACE INTO chunks VALUES(?,?,?,?)")
          .run(project, hash, bytes.length, Date.now());
        this.stats.downloadedBytes += bytes.length;
      } finally {
        release();
      }
      return bytes;
    })();
    this.inflight.set(key, work);
    try {
      return await work;
    } finally {
      this.inflight.delete(key);
    }
  }
  pin(project: string, hash: string, owner: string) {
    this.db
      .prepare("INSERT OR IGNORE INTO pins VALUES(?,?,?)")
      .run(project, hash, owner);
  }
  unpin(owner: string) {
    this.db.prepare("DELETE FROM pins WHERE owner=?").run(owner);
  }
  clearPins() {
    this.db.exec("DELETE FROM pins");
  }
  async cleanPartials() {
    const path = join(this.root, "chunks");
    await mkdir(path, { recursive: true, mode: 0o700 });
    for (const entry of await readdir(path, {
      recursive: true,
      withFileTypes: true,
    })) {
      if (
        entry.isFile() &&
        /^[a-f0-9]{64}\.[a-f0-9-]{36}\.partial$/.test(entry.name)
      )
        await rm(join(entry.parentPath, entry.name));
    }
  }
  async makeRoom(incoming: number) {
    requireThat(incoming <= this.budget, "Chunk exceeds cache budget", 507);
    const info = await statfs(this.root);
    let free = Number(info.bavail) * Number(info.bsize);
    let used = this.bytes();
    const mustReclaim =
      used + incoming > this.budget * 0.8 || free - incoming < this.reserve;
    if (mustReclaim) {
      const candidates = this.db
        .prepare(
          "SELECT c.project,c.hash,c.bytes FROM chunks c WHERE NOT EXISTS(SELECT 1 FROM pins p WHERE p.project=c.project AND p.hash=c.hash) ORDER BY c.used",
        )
        .all() as { project: string; hash: string; bytes: number }[];
      for (const entry of candidates) {
        if (
          used + incoming <= this.budget * 0.65 &&
          free - incoming >= this.reserve
        )
          break;
        if (this.inflight.has(`${entry.project}/${entry.hash}`)) continue;
        await rm(this.path(entry.project, entry.hash), { force: true });
        this.db
          .prepare("DELETE FROM chunks WHERE project=? AND hash=?")
          .run(entry.project, entry.hash);
        used -= entry.bytes;
        this.stats.evictedBytes += entry.bytes;
        const current = await statfs(this.root);
        free = Number(current.bavail) * Number(current.bsize);
      }
    }
    requireThat(
      used + incoming <= this.budget && free - incoming >= this.reserve,
      "Insufficient unpinned cache capacity",
      507,
    );
  }
  close() {
    this.db.close();
  }
}
