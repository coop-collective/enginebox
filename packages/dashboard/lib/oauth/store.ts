import { createHash, randomBytes } from "node:crypto";
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
import { isAbsolute, join } from "node:path";
import { HttpError } from "../server";

export type AuthMethod = "none" | "client_secret_basic" | "client_secret_post";
export type WorkspaceGrant = { host: string; instance: string; profiles: string[] };

export type Client = {
  client_id: string;
  /** SHA-256 of the client secret; null for public clients. */
  secret_hash: string | null;
  client_name: string;
  redirect_uris: string[];
  token_endpoint_auth_method: AuthMethod;
  grant_types: string[];
  created_at: number;
};
/** An authorization request waiting for sign-in and consent. */
export type Pending = {
  id_hash: string;
  client_id: string;
  redirect_uri: string;
  state: string | null;
  code_challenge: string;
  scope: string;
  resource: string;
  expires_at: number;
};
export type Code = {
  hash: string;
  client_id: string;
  redirect_uri: string;
  code_challenge: string;
  scope: string;
  resource: string;
  user_id: string;
  workspaces: WorkspaceGrant[];
  expires_at: number;
  used_at: number | null;
  /** The grant this code produced, so a replay can revoke it. */
  grant_id: string | null;
  replayed: boolean;
};
/** One client's consented access for one user: a refresh-token family. */
export type Grant = {
  id: string;
  user_id: string;
  client_id: string;
  scope: string;
  resource: string;
  workspaces: WorkspaceGrant[];
  created_at: number;
  refreshed_at: number;
  revoked_at: number | null;
  revoke_reason: string | null;
  refresh_hash: string | null;
  refresh_expires_at: number;
  used_refresh_hashes: string[];
  access_token_id: string | null;
  access_token_hash: string | null;
  access_expires_at: number;
};
export type State = {
  version: 1;
  clients: Client[];
  pending: Pending[];
  codes: Code[];
  grants: Grant[];
  /** Controller service-token IDs still to revoke (retried until the controller confirms). */
  pending_revocations: string[];
};

export const sha256 = (value: string) => createHash("sha256").update(value).digest("hex");
export const randomToken = (bytes = 32) => randomBytes(bytes).toString("base64url");
export const randomId = () => randomBytes(16).toString("hex");

export function stateDir(): string | null {
  const dir = process.env.ENGINEBOX_OAUTH_STATE_DIR ?? "";
  if (!dir) return null;
  if (!isAbsolute(dir)) throw new HttpError(503, "ENGINEBOX_OAUTH_STATE_DIR must be an absolute path.");
  return dir;
}
function paths() {
  const dir = stateDir();
  if (!dir) throw new HttpError(404, "Not found.");
  mkdirSync(dir, { recursive: true, mode: 0o700 });
  chmodSync(dir, 0o700);
  return { dir, file: join(dir, "oauth.json"), lock: join(dir, "oauth.lock") };
}
const empty = (): State => ({ version: 1, clients: [], pending: [], codes: [], grants: [], pending_revocations: [] });

function load(file: string): State {
  let text: string;
  try {
    text = readFileSync(file, "utf8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return empty();
    throw error;
  }
  const state = JSON.parse(text) as State;
  if (state.version !== 1) throw new HttpError(503, "Unsupported OAuth state version.");
  return state;
}

function sleep(ms: number) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

/**
 * Read-modify-write under an exclusive lock file, so the old and candidate
 * dashboards a supervisor runs during an update never lose each other's writes.
 * `change` must be synchronous: no network call happens while the lock is held.
 */
export function withState<T>(change: (state: State, now: number) => T): T {
  const { file, lock } = paths();
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
      if (Date.now() - started > 5_000) throw new HttpError(503, "OAuth state is busy. Try again.");
      sleep(10);
    }
  }
  try {
    const state = load(file);
    const now = Date.now();
    prune(state, now);
    const result = change(state, now);
    const temp = `${file}.${process.pid}.${randomId()}.tmp`;
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
/** A lock-free snapshot (writes are atomic renames). Callers check expiry themselves. */
export function readState(): State {
  return load(paths().file);
}

const DAY = 24 * 60 * 60 * 1000;
function prune(state: State, now: number) {
  state.pending = state.pending.filter((p) => p.expires_at > now);
  // Used codes stay until well past expiry so a replay is recognised and its grant revoked.
  state.codes = state.codes.filter((c) => c.expires_at + 10 * 60 * 1000 > now);
  state.grants = state.grants.filter(
    (g) => !(g.revoked_at !== null && g.revoked_at + 30 * DAY < now) && !(g.revoked_at === null && g.refresh_expires_at + 30 * DAY < now),
  );
  for (const grant of state.grants) {
    if (grant.revoked_at === null && grant.refresh_expires_at <= now) {
      grant.revoked_at = now;
      grant.revoke_reason = "expired";
      grant.refresh_hash = null;
    }
  }
}
