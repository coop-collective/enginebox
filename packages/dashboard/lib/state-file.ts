import { randomBytes } from "node:crypto";
import {
  chmodSync,
  closeSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { HttpError } from "./http-error";

/**
 * One private JSON state file in an owner-only directory, written by atomic
 * rename under an exclusive lock file: the same pattern as the OAuth store
 * (`lib/oauth/store.ts`), so the old and candidate dashboards a supervisor runs
 * during an update never lose each other's writes.
 */
export class PrivateStateFile<S> {
  constructor(
    private readonly dir: string,
    private readonly name: string,
    private readonly empty: () => S,
    /** Parses and migrates a stored document; throws HttpError(503) on an unsupported version. */
    private readonly parse: (raw: unknown) => S,
    private readonly label: string,
  ) {}

  private paths() {
    mkdirSync(this.dir, { recursive: true, mode: 0o700 });
    chmodSync(this.dir, 0o700);
    return { file: join(this.dir, `${this.name}.json`), lock: join(this.dir, `${this.name}.lock`) };
  }

  private load(file: string): S {
    let text: string;
    try {
      text = readFileSync(file, "utf8");
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return this.empty();
      throw error;
    }
    return this.parse(JSON.parse(text));
  }

  /** A lock-free snapshot (writes are atomic renames). */
  read(): S {
    return this.load(this.paths().file);
  }

  /** Read-modify-write under the lock. `change` must be synchronous. */
  update<T>(change: (state: S, now: number) => T): T {
    const { file, lock } = this.paths();
    const started = Date.now();
    let fd: number | null = null;
    while (fd === null) {
      try {
        fd = openSync(lock, "wx", 0o600);
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
        try {
          // A crashed holder leaves its lock; nothing legitimately holds it this long.
          if (Date.now() - statSync(lock).mtimeMs > 10_000) unlinkSync(lock);
        } catch {}
        if (Date.now() - started > 5_000) throw new HttpError(503, `${this.label} is busy. Try again.`);
        Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 10);
      }
    }
    try {
      const state = this.load(file);
      const result = change(state, Date.now());
      const temp = `${file}.${process.pid}.${randomBytes(8).toString("hex")}.tmp`;
      writeFileSync(temp, JSON.stringify(state), { mode: 0o600 });
      renameSync(temp, file);
      return result;
    } finally {
      closeSync(fd);
      try {
        unlinkSync(lock);
      } catch {}
    }
  }
}
