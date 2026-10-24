import {
  canonical,
  MACHINE_LEASE_MS,
  MACHINE_MAX_LIFETIME_MS,
  MACHINE_MAX_SIGNALS,
  requireThat,
  type MachineGrantRequest,
  type MachineSession,
  type MachineSignalRequest,
  type MachineSignalEntry,
} from "@enginebox/fleet-protocol";
import type { MachineIceConfiguration } from "./machine-turn.ts";

type Store = {
  get<T>(key: string): T | undefined;
  put(key: string, value: unknown): void;
  remove(key: string): void;
  rows<T>(prefix: string): T[];
};
export type MachineAuthority =
  | { kind: "client"; hash: string }
  | { kind: "host"; nodeId: string; epoch: number }
  | { kind: "owner" };
type Grant = {
  input: MachineGrantRequest;
  epoch: number;
  token: string;
  hash: string;
  createdAt: number;
  expires: number;
  ice: MachineIceConfiguration;
};
type Session = {
  view: MachineSession;
  clientHash: string;
  initialGrantHash: string;
  ice: MachineIceConfiguration;
  deadline: number;
  sequence: number;
  offer: boolean;
  answer: boolean;
  clientIce: number;
  hostIce: number;
  renewedAt: number;
};
type Fence = { epoch: number; revoked: boolean };
const retention = 24 * 60 * 60_000;
const grantRetention = 5 * 60_000;

// Only synchronous SQLite work occurs here: admission, exclusive control and
// operation receipts share the Project DO's durable write boundary.
export class Machines {
  constructor(
    private store: Store,
    private sql: SqlStorage,
  ) {}
  private project() {
    const project = this.store.get<string>("id");
    requireThat(project, "Unknown project", 404);
    return project;
  }
  private fence(nodeId: string, epoch: number) {
    const fence = this.store.get<Fence>(`machine-fence:${nodeId}`);
    requireThat(
      fence && !fence.revoked && fence.epoch === epoch,
      "Machine epoch is stale or revoked",
      409,
    );
  }
  sync(nodeId: string, epoch: number, revoked: boolean) {
    const old = this.store.get<Fence>(`machine-fence:${nodeId}`);
    // A delayed hello callback must never undo a later epoch/revocation.
    if (old && (old.epoch > epoch || old.revoked)) return old;
    this.store.put(`machine-fence:${nodeId}`, { epoch, revoked });
    for (const row of this.store.rows<Session>("machine-session:")) {
      if (
        row.view.nodeId === nodeId &&
        (revoked || row.view.nodeEpoch !== epoch)
      )
        this.close(row, revoked ? "revoked" : "fenced");
    }
    return { epoch, revoked };
  }
  private close(row: Session, reason: NonNullable<MachineSession["reason"]>) {
    if (row.view.state === "open") {
      row.view = { ...row.view, state: "ended", endedAt: Date.now(), reason };
      this.store.put(`machine-session:${row.view.id}`, row);
    }
  }
  private expire(row: Session) {
    if (row.view.state === "open") {
      const fence = this.store.get<Fence>(`machine-fence:${row.view.nodeId}`);
      if (fence?.revoked) this.close(row, "revoked");
      else if (!fence || fence.epoch !== row.view.nodeEpoch)
        this.close(row, "fenced");
      else if (row.view.expires <= Date.now()) this.close(row, "expired");
    }
    return row;
  }
  private gc() {
    const now = Date.now();
    // Bounded retained records; signal payloads are purged with their session.
    for (const row of this.store.rows<Session>("machine-session:")) {
      this.expire(row);
      if (row.view.state === "ended" && row.view.endedAt! < now - retention) {
        this.store.remove(`machine-session:${row.view.id}`);
        for (const prefix of [
          `machine-signal:${row.view.id}:`,
          `machine-op:${row.view.id}:`,
        ])
          this.sql.exec("DELETE FROM records WHERE key LIKE ?", `${prefix}%`);
      }
    }
    this.sql.exec(
      "DELETE FROM records WHERE key LIKE 'machine-grant:%' AND json_extract(value,'$.expires') < ?",
      now - grantRetention,
    );
  }
  grantIce(input: MachineGrantRequest, epoch: number) {
    this.fence(input.nodeId, epoch);
    const row = this.store.get<Session>(`machine-session:${input.sessionId}`);
    if (!row) return null;
    this.expire(row);
    requireThat(
      row.view.state === "open",
      "Session has ended; open a new session",
      409,
    );
    requireThat(
      row.view.nodeId === input.nodeId &&
        row.view.nodeEpoch === epoch &&
        row.view.actorId === input.actorId &&
        row.view.mode === input.mode,
      "Grant differs from the existing session",
      403,
    );
    requireThat(
      row.deadline > Date.now() + 30_000,
      "Relay lifetime reached; open a new session",
      409,
    );
    return row.ice;
  }
  existingGrant(input: MachineGrantRequest, epoch: number) {
    this.fence(input.nodeId, epoch);
    const prior = this.store.get<Grant>(`machine-grant:${input.id}`);
    if (!prior) return null;
    requireThat(
      canonical(prior.input) === canonical(input) && prior.epoch === epoch,
      "Grant operation differs",
      409,
    );
    requireThat(
      prior.expires > Date.now(),
      "Grant expired; request a new grant ID",
      409,
    );
    return this.grantView(prior);
  }
  grant(
    input: MachineGrantRequest,
    epoch: number,
    token: string,
    hash: string,
    ice: MachineIceConfiguration,
  ) {
    this.fence(input.nodeId, epoch);
    this.gc();
    const prior = this.store.get<Grant>(`machine-grant:${input.id}`);
    if (prior) {
      requireThat(
        canonical(prior.input) === canonical(input) && prior.epoch === epoch,
        "Grant operation differs",
        409,
      );
      requireThat(
        prior.expires > Date.now(),
        "Grant expired; request a new grant ID",
        409,
      );
      return this.grantView(prior);
    }
    requireThat(
      this.sql
        .exec<{
          count: number;
        }>("SELECT COUNT(*) AS count FROM records WHERE key LIKE 'machine-grant:%'")
        .one().count < 4096,
      "Grant capacity reached; retry later",
      429,
    );
    requireThat(
      ice.iceExpires > Date.now() + 30_000,
      "Relay credentials expire too soon",
      503,
    );
    const grant: Grant = {
      input,
      epoch,
      token,
      hash,
      createdAt: Date.now(),
      expires: Date.now() + MACHINE_LEASE_MS,
      ice,
    };
    this.store.put(`machine-grant:${input.id}`, grant);
    return this.grantView(grant);
  }
  private grantView(g: Grant) {
    return {
      grant: g.token,
      expires: g.expires,
      actorId: g.input.actorId,
      sessionId: g.input.sessionId,
      nodeId: g.input.nodeId,
      mode: g.input.mode,
    };
  }
  private getGrant(id: string, hash: string) {
    const grant = this.store.get<Grant>(`machine-grant:${id}`);
    requireThat(grant && grant.hash === hash, "Invalid stream grant", 403);
    requireThat(grant.expires > Date.now(), "Stream grant expired", 403);
    this.fence(grant.input.nodeId, grant.epoch);
    return grant;
  }
  open(id: string, grantId: string, grantHash: string, clientHash: string) {
    this.gc();
    const prior = this.store.get<Session>(`machine-session:${id}`);
    // Lost acknowledgements can be recovered after the grant expires, but never
    // create a new lease, reveal ICE to another identity, or resurrect a session.
    if (prior) {
      requireThat(
        prior.clientHash === clientHash && prior.initialGrantHash === grantHash,
        "Session already claimed by another request",
        409,
      );
      this.expire(prior);
      requireThat(prior.view.state === "open", "Session has ended", 409);
      this.fence(prior.view.nodeId, prior.view.nodeEpoch);
      return { session: prior.view, ...prior.ice };
    }
    const grant = this.getGrant(grantId, grantHash);
    requireThat(
      grant.input.sessionId === id,
      "Grant belongs to another session",
      403,
    );
    const sessions = this.store.rows<Session>("machine-session:");
    const active = sessions.filter((s) => s.view.state === "open");
    requireThat(
      sessions.length < 4096 && active.length < 64,
      "Project stream capacity reached",
      429,
    );
    const host = active.filter((s) => s.view.nodeId === grant.input.nodeId);
    requireThat(host.length < 8, "Machine stream capacity reached", 409);
    requireThat(
      grant.input.mode !== "control" ||
        !host.some((s) => s.view.mode === "control"),
      "Machine already has a control lease",
      409,
    );
    const now = Date.now(),
      deadline = Math.min(
        now + MACHINE_MAX_LIFETIME_MS,
        grant.ice.iceExpires - 30_000,
      );
    const row: Session = {
      view: {
        id,
        project: this.project(),
        nodeId: grant.input.nodeId,
        nodeEpoch: grant.epoch,
        actorId: grant.input.actorId,
        mode: grant.input.mode,
        state: "open",
        expires: Math.min(now + MACHINE_LEASE_MS, deadline),
        createdAt: now,
      },
      clientHash,
      initialGrantHash: grantHash,
      ice: grant.ice,
      deadline,
      sequence: 0,
      offer: false,
      answer: false,
      clientIce: 0,
      hostIce: 0,
      renewedAt: grant.createdAt,
    };
    this.store.put(`machine-session:${id}`, row);
    return { session: row.view, ...row.ice };
  }
  private authorized(id: string, auth: MachineAuthority, active = false) {
    const row = this.store.get<Session>(`machine-session:${id}`);
    requireThat(row, "Unknown machine session", 404);
    if (auth.kind === "client")
      requireThat(
        auth.hash === row.clientHash,
        "Stream client authorization failed",
        403,
      );
    if (auth.kind === "host") {
      requireThat(
        auth.nodeId === row.view.nodeId && auth.epoch === row.view.nodeEpoch,
        "Session belongs to another machine epoch",
        403,
      );
      this.fence(auth.nodeId, auth.epoch);
    }
    this.expire(row);
    if (active)
      requireThat(row.view.state === "open", "Machine session has ended", 410);
    return row;
  }
  view(id: string, auth: MachineAuthority) {
    return { session: this.authorized(id, auth).view };
  }
  signals(id: string, auth: MachineAuthority, after: number) {
    const row = this.authorized(id, auth, true);
    const side = auth.kind === "host" ? "client" : "host";
    const all = this.store.rows<MachineSignalEntry>(`machine-signal:${id}:`);
    return {
      session: row.view,
      signals: all
        .filter((s) => s.sequence > after && s.side === side)
        .map(({ side: _side, ...signal }) => signal),
      cursor: row.sequence,
    };
  }
  signal(id: string, auth: MachineAuthority, input: MachineSignalRequest) {
    requireThat(
      auth.kind !== "owner",
      "Owner cannot inject stream signals",
      403,
    );
    const row = this.authorized(id, auth, true),
      side = auth.kind === "host" ? "host" : "client";
    const key = `machine-op:${id}:${side}:${input.id}`;
    const prior = this.store.get<{ input: string; sequence: number }>(key);
    const serialized = canonical(input);
    if (prior) {
      requireThat(prior.input === serialized, "Signal operation differs", 409);
      return { sequence: prior.sequence, session: row.view };
    }
    requireThat(
      row.sequence < MACHINE_MAX_SIGNALS,
      "Signaling mailbox full",
      429,
    );
    if (input.signal.type === "offer") {
      requireThat(
        side === "client" && !row.offer,
        "Only one client offer is allowed",
        409,
      );
      row.offer = true;
    } else if (input.signal.type === "answer") {
      requireThat(
        side === "host" && row.offer && !row.answer,
        "Only one host answer after offer is allowed",
        409,
      );
      row.answer = true;
    } else {
      requireThat(row.offer, "Offer must be sent before ICE", 409);
      const counter = side === "host" ? "hostIce" : "clientIce";
      requireThat(row[counter] < 128, "ICE mailbox full", 429);
      row[counter]++;
    }
    row.sequence++;
    this.store.put(
      `machine-signal:${id}:${String(row.sequence).padStart(3, "0")}`,
      { ...input, sequence: row.sequence, side } satisfies MachineSignalEntry,
    );
    this.store.put(key, { input: serialized, sequence: row.sequence });
    this.store.put(`machine-session:${id}`, row);
    return { sequence: row.sequence, session: row.view };
  }
  renew(
    id: string,
    auth: MachineAuthority,
    grantId: string,
    grantHash: string,
  ) {
    requireThat(
      auth.kind === "client",
      "Only the granted client can renew",
      403,
    );
    const row = this.authorized(id, auth, true),
      grant = this.getGrant(grantId, grantHash);
    requireThat(
      grant.input.sessionId === id &&
        grant.input.nodeId === row.view.nodeId &&
        grant.input.actorId === row.view.actorId &&
        grant.input.mode === row.view.mode &&
        grant.epoch === row.view.nodeEpoch,
      "Renewal grant differs from session",
      403,
    );
    requireThat(
      row.deadline > Date.now() + 5000,
      "Relay lifetime reached; open a new session",
      409,
    );
    // Retrying an old grant cannot extend its original lease window.
    if (grant.createdAt > row.renewedAt) {
      row.view.expires = Math.min(
        grant.createdAt + MACHINE_LEASE_MS,
        row.deadline,
      );
      row.renewedAt = grant.createdAt;
      this.store.put(`machine-session:${id}`, row);
    }
    return { session: row.view, ...row.ice };
  }
  end(id: string, auth: MachineAuthority, _operationId: string) {
    const row = this.authorized(id, auth);
    this.close(
      row,
      auth.kind === "host"
        ? "host"
        : auth.kind === "owner"
          ? "owner"
          : "client",
    );
    return { session: row.view };
  }
  poll(nodeId: string, epoch: number) {
    this.fence(nodeId, epoch);
    this.gc();
    const sessions = this.store
      .rows<Session>("machine-session:")
      .filter(
        (row) =>
          row.view.nodeId === nodeId &&
          row.view.nodeEpoch === epoch &&
          row.view.state === "open",
      );
    // Each session may have separately minted relay credentials. Include its
    // exact ICE envelope so the host never uses another viewer's expired grant.
    return {
      sessions: sessions.map((row) => ({ ...row.view, ...row.ice })),
      now: Date.now(),
    };
  }
}
