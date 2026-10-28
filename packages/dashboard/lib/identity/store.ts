import { createHmac, randomBytes } from "node:crypto";
import { isAbsolute } from "node:path";
import { HttpError } from "../http-error";
import { PrivateStateFile } from "../state-file";

/**
 * Users, organizations, memberships, invites, email sign-in challenges and
 * member sessions. One private file, `identity.json`, in
 * ENGINEBOX_IDENTITY_STATE_DIR (production: `<controller state>/identity`, next
 * to the OAuth state). Unset, the dashboard keeps its single-operator
 * behaviour and every org route answers 404.
 *
 * Tenancy (docs/ORGS.md):
 * - The **platform org** (`default`, shown as "Default Org" on a self-hosted
 *   install) owns the install's nodes and pools. Everything that existed before
 *   orgs belongs to it.
 * - The **instance owner** operates the install: the break-glass operator
 *   session, and the user named by ENGINEBOX_OWNER_EMAIL (or claimed by the
 *   operator). On a self-hosted install the instance owner owns the platform org.
 * - Every user gets a **personal org** when their account is created, and joins
 *   other orgs by invite. The platform org never sees a personal org's resources.
 * - `mode` (ENGINEBOX_TENANCY_MODE): `self_hosted` (default) or `managed`. In
 *   managed mode tenants never see or join the platform org; its capacity
 *   reaches them only through capacity grants held by the controller.
 *
 * Secrets are never stored: invite tokens, sign-in codes, magic-link tokens and
 * session IDs are kept only as HMAC-SHA256 digests keyed by
 * ENGINEBOX_SESSION_SECRET (domain-separated per purpose), and rate-limit keys
 * are digests of the email or client address.
 */
export const IDENTITY_SCHEMA = 1;
export type Role = "owner" | "admin" | "member";
export const ROLES: Role[] = ["owner", "admin", "member"];
/** The platform org. Matches the controller's default `org_id`. */
export const DEFAULT_ORG = "default";
export type Mode = "self_hosted" | "managed";
export type OrgKind = "platform" | "personal" | "team";

export type Org = { id: string; name: string; kind: OrgKind; created_at: number };
export type User = {
  id: string;
  /** Normalized (trimmed, lowercase). Unique. */
  email: string;
  name: string;
  personal_org_id: string;
  /** Operator of the install (owns the platform org on a self-hosted install). */
  instance_owner: boolean;
  created_at: number;
  last_sign_in_at: number | null;
  /** How the account was created. */
  created_by: "env" | "operator" | "invite" | "import" | "signup";
};
export type Member = {
  id: string;
  org_id: string;
  user_id: string;
  role: Role;
  created_at: number;
  /** Removed memberships keep their record (audit, grant ownership) but grant nothing. */
  removed_at: number | null;
  joined_by: "env" | "operator" | "invite" | "import" | "personal";
};
export type Invite = {
  id: string;
  org_id: string;
  email: string;
  role: Role;
  token_hash: string;
  created_by: string;
  created_at: number;
  expires_at: number;
  used_at: number | null;
  used_by: string | null;
  revoked_at: number | null;
};
/** One emailed sign-in: a 6-digit code and a magic link, both single use. */
export type Challenge = {
  id_hash: string;
  email: string;
  /**
   * `sign_in`: an existing user. `sign_up`: open sign-up (managed mode, or
   * ENGINEBOX_OPEN_SIGNUP) creates the user and their personal org on success.
   * `claim_owner`: the break-glass operator proving the instance owner's email.
   */
  purpose: "sign_in" | "sign_up" | "claim_owner";
  code_hash: string;
  link_hash: string;
  attempts: number;
  created_at: number;
  expires_at: number;
  used_at: number | null;
};
export type Session = {
  id_hash: string;
  user_id: string;
  /** The org the session acts in (switchable among the user's memberships). */
  org_id: string;
  created_at: number;
  expires_at: number;
  revoked_at: number | null;
};
export type IdentityState = {
  version: 1;
  orgs: Org[];
  users: User[];
  members: Member[];
  invites: Invite[];
  challenges: Challenge[];
  sessions: Session[];
  /** Rate-limit buckets: digest key → hit times (ms). */
  rates: Record<string, number[]>;
};

export const MINUTE = 60 * 1000;
export const HOUR = 60 * MINUTE;
export const DAY = 24 * HOUR;
export const MAX_USERS = 5000;
export const MAX_MEMBERS = 500;
export const MAX_INVITES = 500;
const MAX_CHALLENGES = 2000;
const MAX_SESSIONS = 10_000;
const MAX_RATE_KEYS = 10_000;

export const EMAIL = /^[^\s@<>()",;:\\[\]]{1,64}@[A-Za-z0-9](?:[A-Za-z0-9-]{0,62}[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,62}[A-Za-z0-9])?)+$/;
/** Normalize an email address, or null when it is not a plausible single address. */
export function normalizeEmail(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const email = value.trim().toLowerCase();
  return email.length <= 254 && EMAIL.test(email) ? email : null;
}

export function mode(): Mode {
  return process.env.ENGINEBOX_TENANCY_MODE === "managed" ? "managed" : "self_hosted";
}
/** Self-service sign-up: always in managed mode; on a self-hosted install only with ENGINEBOX_OPEN_SIGNUP=1. */
export function openSignup() {
  return mode() === "managed" || process.env.ENGINEBOX_OPEN_SIGNUP === "1";
}

export function identityDir(): string | null {
  const dir = process.env.ENGINEBOX_IDENTITY_STATE_DIR ?? "";
  if (!dir) return null;
  if (!isAbsolute(dir)) throw new HttpError(503, "ENGINEBOX_IDENTITY_STATE_DIR must be an absolute path.");
  return dir;
}
/** True when orgs and email sign-in are configured. */
export function identityEnabled() {
  return identityDir() !== null;
}
export function requireIdentity(): string {
  const dir = identityDir();
  if (!dir) throw new HttpError(404, "Organizations are not enabled on this Engine Box.");
  return dir;
}

const secret = () => {
  const value = process.env.ENGINEBOX_SESSION_SECRET ?? "";
  if (value.length < 32) throw new HttpError(503, "Dashboard credentials are not configured. Check the server environment.");
  return value;
};
/** Keyed digest for one purpose; nothing reversible is stored. */
export function digest(purpose: string, value: string) {
  return createHmac("sha256", secret()).update(`enginebox-identity/${purpose}\0`).update(value).digest("hex");
}
export const randomToken = (bytes = 32) => randomBytes(bytes).toString("base64url");
export const newId = (prefix: string) => `${prefix}_${randomBytes(12).toString("hex")}`;

const empty = (): IdentityState => ({ version: 1, orgs: [], users: [], members: [], invites: [], challenges: [], sessions: [], rates: {} });
function parse(raw: unknown): IdentityState {
  const state = raw as Partial<IdentityState>;
  if (!state || state.version !== IDENTITY_SCHEMA) throw new HttpError(503, "Unsupported identity state version.");
  return { ...empty(), ...state } as IdentityState;
}

export const platformName = () =>
  (process.env.ENGINEBOX_ORG_NAME ?? "").trim().slice(0, 80) || (mode() === "managed" ? "Platform" : "Default Org");

/**
 * Create a user and their personal org (every account gets one), owned by the
 * user. Returns the user; the caller adds any further memberships.
 */
export function createUser(state: IdentityState, email: string, createdBy: User["created_by"], now: number): User {
  if (state.users.length >= MAX_USERS) throw new HttpError(409, "This Engine Box has too many accounts.");
  const org: Org = { id: newId("org"), name: `${email.split("@")[0]}'s workspace`.slice(0, 80), kind: "personal", created_at: now };
  const user: User = { id: newId("usr"), email, name: "", personal_org_id: org.id, instance_owner: false, created_at: now, last_sign_in_at: null, created_by: createdBy };
  state.orgs.push(org);
  state.users.push(user);
  state.members.push({ id: newId("mem"), org_id: org.id, user_id: user.id, role: "owner", created_at: now, removed_at: null, joined_by: "personal" });
  return user;
}

/** Add (or restore) a membership. */
export function addMember(state: IdentityState, orgId: string, userId: string, role: Role, joinedBy: Member["joined_by"], now: number): Member {
  const existing = state.members.find((m) => m.org_id === orgId && m.user_id === userId);
  if (existing) {
    if (existing.removed_at !== null || roleRank[role] > roleRank[existing.role]) existing.role = role;
    existing.removed_at = null;
    return existing;
  }
  if (state.members.filter((m) => m.org_id === orgId && m.removed_at === null).length >= MAX_MEMBERS)
    throw new HttpError(409, "This organization has too many members.");
  const member: Member = { id: newId("mem"), org_id: orgId, user_id: userId, role, created_at: now, removed_at: null, joined_by: joinedBy };
  state.members.push(member);
  return member;
}
export const roleRank: Record<Role, number> = { member: 0, admin: 1, owner: 2 };

/** Make `user` the instance owner and an owner of the platform org (the provider's org on a managed install). */
export function makeInstanceOwner(state: IdentityState, user: User, joinedBy: "env" | "operator", now: number) {
  user.instance_owner = true;
  addMember(state, DEFAULT_ORG, user.id, "owner", joinedBy, now);
}

/**
 * Migration and bootstrap, applied on every read and write (idempotent): the
 * platform org always exists, and ENGINEBOX_OWNER_EMAIL, when set, is the
 * instance owner if there is none yet. Everything that existed before orgs
 * (nodes, pools, profiles, repositories, OAuth grants, service tokens,
 * controller agent accounts) belongs to the platform org.
 */
export function bootstrap(state: IdentityState, now: number) {
  if (!state.orgs.some((o) => o.id === DEFAULT_ORG)) state.orgs.unshift({ id: DEFAULT_ORG, name: platformName(), kind: "platform", created_at: now });
  const ownerEmail = normalizeEmail(process.env.ENGINEBOX_OWNER_EMAIL);
  if (ownerEmail && !state.users.some((u) => u.instance_owner)) {
    const user = state.users.find((u) => u.email === ownerEmail) ?? createUser(state, ownerEmail, "env", now);
    makeInstanceOwner(state, user, "env", now);
  }
}

function prune(state: IdentityState, now: number) {
  state.challenges = state.challenges.filter((c) => c.expires_at + HOUR > now).slice(-MAX_CHALLENGES);
  state.sessions = state.sessions.filter((s) => s.expires_at > now && (s.revoked_at === null || s.revoked_at + DAY > now)).slice(-MAX_SESSIONS);
  // Used, revoked and expired invites stay a while so the admin sees what happened.
  state.invites = state.invites.filter((i) => Math.max(i.expires_at, i.used_at ?? 0, i.revoked_at ?? 0) + 30 * DAY > now);
  const rates: Record<string, number[]> = {};
  for (const [key, hits] of Object.entries(state.rates)) {
    const live = hits.filter((t) => t > now - DAY);
    if (live.length) rates[key] = live;
  }
  const keys = Object.keys(rates);
  if (keys.length > MAX_RATE_KEYS) for (const key of keys.slice(0, keys.length - MAX_RATE_KEYS)) delete rates[key];
  state.rates = rates;
}

let file: PrivateStateFile<IdentityState> | null = null;
let fileDir = "";
function store() {
  const dir = requireIdentity();
  if (!file || fileDir !== dir) {
    file = new PrivateStateFile(dir, "identity", empty, parse, "Identity state");
    fileDir = dir;
  }
  return file;
}

/** Read-modify-write the identity state under its lock. `change` must be synchronous. */
export function withIdentity<T>(change: (state: IdentityState, now: number) => T): T {
  return store().update((state, now) => {
    bootstrap(state, now);
    prune(state, now);
    return change(state, now);
  });
}
/** A lock-free snapshot, bootstrapped in memory. Callers check expiry themselves. */
export function readIdentity(): IdentityState {
  const state = store().read();
  bootstrap(state, Date.now());
  return state;
}

/**
 * Sliding-window limit: records one hit for `key` and throws 429 when `limit`
 * hits already fall inside `windowMs`. The key is digested before storage.
 */
export function hit(state: IdentityState, now: number, key: string, limit: number, windowMs: number, message: string) {
  const k = digest("rate", key);
  const hits = (state.rates[k] ?? []).filter((t) => t > now - windowMs);
  if (hits.length >= limit) {
    state.rates[k] = hits;
    throw new HttpError(429, message);
  }
  hits.push(now);
  state.rates[k] = hits;
}

export const userById = (state: IdentityState, id: string) => state.users.find((u) => u.id === id) ?? null;
export const userByEmail = (state: IdentityState, email: string) => state.users.find((u) => u.email === email) ?? null;
export const orgOf = (state: IdentityState, id: string) => state.orgs.find((o) => o.id === id) ?? null;
export const membership = (state: IdentityState, orgId: string, userId: string) =>
  state.members.find((m) => m.org_id === orgId && m.user_id === userId && m.removed_at === null) ?? null;
export const membershipsOf = (state: IdentityState, userId: string) =>
  state.members.filter((m) => m.user_id === userId && m.removed_at === null);
/** The org a fresh session starts in: the platform org when the user belongs to it, otherwise their personal org. */
export function homeOrg(state: IdentityState, user: User) {
  const mine = membershipsOf(state, user.id);
  return (mine.find((m) => m.org_id === DEFAULT_ORG) ?? mine.find((m) => m.org_id === user.personal_org_id) ?? mine[0])?.org_id ?? null;
}
