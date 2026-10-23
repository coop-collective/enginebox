import { lstat, readdir, rm, statfs } from "node:fs/promises";
import { join } from "node:path";
import { digest, identifier, requireThat } from "@enginebox/fleet-protocol";
import type { Cache } from "./cache.ts";

// This index owns only immutable hydrated snapshots. Live LayerFS Stores and
// unpublished writable workspaces must never enter this eviction namespace.
export class Snapshots {
  cache: Cache;
  budget: number;
  active = new Set<string>();
  constructor(cache: Cache, budget: number) {
    this.cache = cache;
    this.budget = budget;
    cache.db.exec(
      "CREATE TABLE IF NOT EXISTS snapshots(project TEXT, generation TEXT, bytes INTEGER, used INTEGER, PRIMARY KEY(project,generation))",
    );
  }
  path(project: string, generation: string) {
    identifier(project);
    digest(generation);
    return join(this.cache.root, "generations", project, generation);
  }
  touch(project: string, generation: string, bytes: number) {
    this.cache.db
      .prepare("INSERT OR REPLACE INTO snapshots VALUES(?,?,?,?)")
      .run(project, generation, bytes, Date.now());
  }
  async recover() {
    const root = join(this.cache.root, "generations");
    let projects;
    try {
      projects = await readdir(root, { withFileTypes: true });
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return;
      throw error;
    }
    for (const project of projects) {
      if (
        !project.isDirectory() ||
        !/^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$/.test(project.name)
      )
        continue;
      for (const generation of await readdir(join(root, project.name), {
        withFileTypes: true,
      })) {
        if (
          !generation.isDirectory() ||
          !/^[a-f0-9]{64}$/.test(generation.name)
        )
          continue;
        await this.cleanPartials(project.name, generation.name);
        try {
          const info = await lstat(
            join(this.path(project.name, generation.name), "store.sqlite"),
          );
          if (info.isFile() && !info.isSymbolicLink())
            this.cache.db
              .prepare("INSERT OR IGNORE INTO snapshots VALUES(?,?,?,?)")
              .run(project.name, generation.name, info.size, Date.now());
        } catch (error) {
          if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
        }
      }
    }
  }
  async room(incoming: number) {
    requireThat(
      incoming <= this.budget,
      "Snapshot exceeds configured snapshotBytes budget",
      507,
    );
    const entries = this.cache.db
      .prepare("SELECT * FROM snapshots ORDER BY used")
      .all() as { project: string; generation: string; bytes: number }[];
    let used = entries.reduce((sum, entry) => sum + entry.bytes, 0);
    for (const entry of entries) {
      const space = await statfs(this.cache.root);
      if (
        used + incoming <= this.budget &&
        Number(space.bavail) * Number(space.bsize) >
          incoming + this.cache.reserve
      )
        break;
      if (this.active.has(`${entry.project}/${entry.generation}`)) continue;
      await rm(this.path(entry.project, entry.generation), {
        recursive: true,
        force: true,
      });
      this.cache.db
        .prepare("DELETE FROM snapshots WHERE project=? AND generation=?")
        .run(entry.project, entry.generation);
      used -= entry.bytes;
    }
    const space = await statfs(this.cache.root);
    requireThat(
      used + incoming <= this.budget &&
        Number(space.bavail) * Number(space.bsize) >
          incoming + this.cache.reserve,
      "Insufficient inactive snapshot capacity",
      507,
    );
  }
  async cleanPartials(project: string, generation: string) {
    const path = this.path(project, generation);
    for (const name of await readdir(path)) {
      if (!/^store-[a-f0-9-]{36}\.partial(?:-wal|-shm|-journal)?$/.test(name))
        continue;
      const target = join(path, name);
      const info = await lstat(target);
      if (info.isFile()) await rm(target);
    }
  }
}
