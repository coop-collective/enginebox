import { atLeast, bodyJson, config, HttpError, json, readSession, requireOrigin, requireSession, type Principal } from "../server";
import { claimOwnerStart, revokeOrgSessions } from "./auth";
import { send, transport } from "./mail";
import {
  addMember,
  createUser,
  DAY,
  DEFAULT_ORG,
  digest,
  identityEnabled,
  MAX_INVITES,
  membershipsOf,
  mode,
  newId,
  normalizeEmail,
  orgOf,
  randomToken,
  readIdentity,
  ROLES,
  userByEmail,
  userById,
  withIdentity,
  type IdentityState,
  type Member,
  type Role,
} from "./store";

export const INVITE_DAYS = 7;
export const MAX_IMPORT = 200;
const MEMBER_ID = /^mem_[a-f0-9]{24}$/;
const INVITE_ID = /^inv_[a-f0-9]{24}$/;
const ORG_ID = /^(?:default|org_[a-f0-9]{24})$/;
const MAX_TEAM_ORGS_PER_USER = 20;

function me(who: Principal) {
  return {
    kind: who.kind,
    user_id: who.user_id,
    member_id: who.member_id,
    org_id: who.org_id,
    org_kind: who.org_kind,
    role: who.role,
    instance_owner: who.instance_owner,
    email: who.email,
    label: who.label,
  };
}
const iso = (ms: number | null) => (ms === null ? null : new Date(ms).toISOString());
function memberView(state: IdentityState, m: Member) {
  const user = userById(state, m.user_id);
  return {
    id: m.id,
    user_id: m.user_id,
    email: user?.email ?? "",
    name: user?.name ?? "",
    role: m.role,
    instance_owner: user?.instance_owner ?? false,
    joined_by: m.joined_by,
    created_at: iso(m.created_at),
    last_sign_in_at: iso(user?.last_sign_in_at ?? null),
  };
}
const owners = (state: IdentityState, org: string) => state.members.filter((m) => m.org_id === org && m.role === "owner" && m.removed_at === null);
function cleanName(value: unknown) {
  const name = typeof value === "string" ? value.trim() : "";
  if (!name || name.length > 80 || /[\u0000-\u001f\u007f]/.test(name)) throw new HttpError(400, "Use a name of 1-80 printable characters.");
  return name;
}

/** GET /api/org: the principal, their orgs, the active org's members and (admins) pending invites. */
export function orgView(request: Request) {
  const who = requireSession(request);
  if (!identityEnabled()) return json({ enabled: false, mode: mode(), me: me(who) });
  const state = readIdentity();
  const now = Date.now();
  const members = state.members.filter((m) => m.org_id === who.org_id && m.removed_at === null);
  const orgs = who.user_id
    ? membershipsOf(state, who.user_id).map((m) => {
        const org = orgOf(state, m.org_id);
        return { id: m.org_id, name: org?.name ?? m.org_id, kind: org?.kind ?? "team", role: m.role };
      })
    : [{ id: DEFAULT_ORG, name: orgOf(state, DEFAULT_ORG)?.name ?? DEFAULT_ORG, kind: "platform", role: "owner" }];
  return json({
    enabled: true,
    mode: mode(),
    me: me(who),
    org: { id: who.org_id, name: orgOf(state, who.org_id)?.name ?? who.org_id, kind: who.org_kind },
    orgs,
    needs_instance_owner: !state.users.some((u) => u.instance_owner),
    email_transport: transport(),
    members: members.map((m) => memberView(state, m)),
    invites: atLeast(who, "admin")
      ? state.invites
          .filter((i) => i.org_id === who.org_id && i.used_at === null && i.revoked_at === null && i.expires_at > now)
          .map((i) => ({ id: i.id, email: i.email, role: i.role, created_at: iso(i.created_at), expires_at: iso(i.expires_at) }))
      : [],
  });
}

/** Who may give or take `role`: owners manage everyone; admins manage members and admins, never owners. */
function mayAssign(who: Principal, role: Role) {
  return who.role === "owner" || (who.role === "admin" && role !== "owner");
}

/**
 * Who may add people to the active org. A personal org is its owner's alone.
 * In managed mode only the instance owner adds people to the platform org:
 * tenants never join it.
 */
function requireMayAddPeople(who: Principal) {
  if (!atLeast(who, "admin")) throw new HttpError(403, "This needs the admin role in your organization.");
  if (who.org_kind === "personal") throw new HttpError(403, "A personal workspace has no other members. Create a team organization to invite people.");
  if (mode() === "managed" && who.org_id === DEFAULT_ORG && !who.instance_owner)
    throw new HttpError(403, "Only the instance owner adds people to the platform organization.");
}

async function createInvite(request: Request, who: Principal) {
  requireMayAddPeople(who);
  const body = await bodyJson(request);
  const email = normalizeEmail(body?.email);
  const role = body?.role ?? "member";
  if (!email) throw new HttpError(400, "Enter a valid email address.");
  if (!ROLES.includes(role)) throw new HttpError(400, "Choose owner, admin or member.");
  if (!mayAssign(who, role)) throw new HttpError(403, "Only an owner can invite another owner.");
  const token = randomToken(32);
  const created = withIdentity((state, now) => {
    const existing = userByEmail(state, email);
    if (existing && state.members.some((m) => m.org_id === who.org_id && m.user_id === existing.id && m.removed_at === null))
      throw new HttpError(409, "That person is already a member.");
    const live = state.invites.filter((i) => i.org_id === who.org_id && i.used_at === null && i.revoked_at === null && i.expires_at > now);
    if (live.length >= MAX_INVITES) throw new HttpError(409, "Too many pending invites. Revoke some first.");
    // A new invite replaces any pending one for the same address.
    for (const i of live) if (i.email === email) i.revoked_at = now;
    const invite = {
      id: newId("inv"),
      org_id: who.org_id,
      email,
      role: role as Role,
      token_hash: digest("invite", token),
      created_by: who.user_id ?? "operator",
      created_at: now,
      expires_at: now + INVITE_DAYS * DAY,
      used_at: null,
      used_by: null,
      revoked_at: null,
    };
    state.invites.push(invite);
    return { invite, org: orgOf(state, who.org_id)?.name ?? "Engine Box" };
  });
  const sent = await send({
    template: "invite",
    to: email,
    link: `${config().origin}/api/auth/invite?token=${encodeURIComponent(token)}`,
    org_name: created.org,
    role,
    expires_days: INVITE_DAYS,
  });
  const i = created.invite;
  return json({ invite: { id: i.id, email: i.email, role: i.role, created_at: iso(i.created_at), expires_at: iso(i.expires_at) }, email_sent: sent }, 201);
}

function revokeInvite(who: Principal, id: string) {
  if (!atLeast(who, "admin")) throw new HttpError(403, "This needs the admin role in your organization.");
  withIdentity((state, now) => {
    const invite = state.invites.find((i) => i.id === id && i.org_id === who.org_id && i.used_at === null && i.revoked_at === null);
    if (!invite) throw new HttpError(404, "Unknown invite.");
    if (!mayAssign(who, invite.role)) throw new HttpError(403, "Only an owner can revoke an owner invite.");
    invite.revoked_at = now;
  });
  return json({ revoked: true });
}

/**
 * POST /api/org/members/import {members: [{email, role?, name?}]}: add people
 * directly, matched by email (an existing account just gains the membership; a
 * new one is created with its personal org). Used to carry members over from
 * another system such as Ideator. They sign in with an emailed code, which is
 * what proves they control the address. No invite email is sent.
 */
async function importMembers(request: Request, who: Principal) {
  requireMayAddPeople(who);
  const body = await bodyJson(request, 65536);
  const list = Array.isArray(body?.members) ? body.members : null;
  if (!list || !list.length || list.length > MAX_IMPORT) throw new HttpError(400, `Send 1-${MAX_IMPORT} members.`);
  const rows = list.map((row: unknown) => {
    const r = (row ?? {}) as { email?: unknown; role?: unknown; name?: unknown };
    const email = normalizeEmail(r.email);
    const role = (r.role ?? "member") as Role;
    if (!email || !ROLES.includes(role)) throw new HttpError(400, "Each member needs a valid email and role owner, admin or member.");
    if (!mayAssign(who, role)) throw new HttpError(403, "Only an owner can add another owner.");
    const name = typeof r.name === "string" ? r.name.trim().slice(0, 80).replace(/[\u0000-\u001f\u007f]/g, "") : "";
    return { email, role, name };
  });
  const result = withIdentity((state, now) => {
    let created = 0, added = 0, unchanged = 0;
    for (const row of rows) {
      let user = userByEmail(state, row.email);
      if (!user) {
        user = createUser(state, row.email, "import", now);
        created++;
      }
      if (row.name && !user.name) user.name = row.name;
      const had = state.members.some((m) => m.org_id === who.org_id && m.user_id === user.id && m.removed_at === null);
      addMember(state, who.org_id, user.id, row.role, "import", now);
      if (had) unchanged++;
      else added++;
    }
    return { created_accounts: created, added, unchanged };
  });
  return json(result);
}

async function changeRole(request: Request, who: Principal, id: string) {
  if (!atLeast(who, "admin")) throw new HttpError(403, "This needs the admin role in your organization.");
  const body = await bodyJson(request);
  const role = body?.role;
  if (!ROLES.includes(role)) throw new HttpError(400, "Choose owner, admin or member.");
  const view = withIdentity((state) => {
    const m = state.members.find((x) => x.id === id && x.org_id === who.org_id && x.removed_at === null);
    if (!m) throw new HttpError(404, "Unknown member.");
    if (who.org_kind === "personal") throw new HttpError(403, "A personal workspace has only its owner.");
    if (!mayAssign(who, m.role) || !mayAssign(who, role)) throw new HttpError(403, "Only an owner can change an owner's role or make someone an owner.");
    if (m.role === "owner" && role !== "owner" && owners(state, who.org_id).length <= 1)
      throw new HttpError(409, "An organization needs at least one owner. Make someone else an owner first.");
    m.role = role;
    return memberView(state, m);
  });
  return json({ member: view });
}

function removeMember(who: Principal, id: string) {
  withIdentity((state, now) => {
    const m = state.members.find((x) => x.id === id && x.org_id === who.org_id && x.removed_at === null);
    if (!m) throw new HttpError(404, "Unknown member.");
    if (who.org_kind === "personal") throw new HttpError(403, "You cannot leave or remove your personal workspace.");
    if (m.id !== who.member_id && !atLeast(who, "admin")) throw new HttpError(403, "This needs the admin role in your organization.");
    if (m.id !== who.member_id && !mayAssign(who, m.role)) throw new HttpError(403, "Only an owner can remove an owner.");
    if (m.role === "owner" && owners(state, who.org_id).length <= 1) throw new HttpError(409, "An organization needs at least one owner.");
    m.removed_at = now;
    revokeOrgSessions(state, m.user_id, who.org_id, now);
  });
  return json({ removed: true });
}

async function rename(request: Request, who: Principal) {
  if (who.role !== "owner") throw new HttpError(403, "Only an owner can rename the organization.");
  const name = cleanName((await bodyJson(request))?.name);
  withIdentity((state) => {
    const org = orgOf(state, who.org_id);
    if (!org) throw new HttpError(404, "Unknown organization.");
    org.name = name;
  });
  return json({ org: { id: who.org_id, name } });
}

/** POST /api/org/create {name}: a team org owned by the signed-in user. */
async function createOrg(request: Request, who: Principal) {
  if (!who.user_id) throw new HttpError(403, "Sign in with your email to create an organization.");
  const name = cleanName((await bodyJson(request))?.name);
  const org = withIdentity((state, now) => {
    const owned = state.members.filter((m) => m.user_id === who.user_id && m.role === "owner" && m.removed_at === null && orgOf(state, m.org_id)?.kind === "team");
    if (owned.length >= MAX_TEAM_ORGS_PER_USER) throw new HttpError(409, "You own too many organizations.");
    const created = { id: newId("org"), name, kind: "team" as const, created_at: now };
    state.orgs.push(created);
    addMember(state, created.id, who.user_id!, "owner", "invite", now);
    return created;
  });
  return json({ org: { id: org.id, name: org.name, kind: org.kind } }, 201);
}

/** POST /api/org/switch {org_id}: move this session to another org the user belongs to. */
async function switchOrg(request: Request, who: Principal) {
  if (!who.user_id) throw new HttpError(403, "The break-glass session acts only in the platform organization.");
  const target = (await bodyJson(request))?.org_id;
  if (typeof target !== "string" || !ORG_ID.test(target)) throw new HttpError(400, "Choose one of your organizations.");
  const token = readSession(request);
  if (token?.kind !== "member") throw new HttpError(401, "Sign in again.");
  withIdentity((state) => {
    if (!state.members.some((m) => m.org_id === target && m.user_id === who.user_id && m.removed_at === null))
      throw new HttpError(403, "You are not a member of that organization.");
    const record = state.sessions.find((s) => s.id_hash === digest("session", token.sid) && s.revoked_at === null);
    if (!record) throw new HttpError(401, "Sign in again.");
    record.org_id = target;
  });
  return json({ org_id: target });
}

/** POST /api/org/{...}: invites, import, roles, removal, naming, team orgs, switching and the instance-owner claim. */
export async function orgAction(request: Request, segments: string[]) {
  requireOrigin(request);
  if (!identityEnabled()) throw new HttpError(404, "Organizations are not enabled on this Engine Box.");
  const path = segments.join("/");
  if (path === "claim") return claimOwnerStart(request);
  const who = requireSession(request);
  let match: RegExpMatchArray | null;
  if (path === "invites") return createInvite(request, who);
  if ((match = path.match(/^invites\/([^/]+)\/revoke$/)) && INVITE_ID.test(match[1])) return revokeInvite(who, match[1]);
  if (path === "members/import") return importMembers(request, who);
  if ((match = path.match(/^members\/([^/]+)\/role$/)) && MEMBER_ID.test(match[1])) return changeRole(request, who, match[1]);
  if ((match = path.match(/^members\/([^/]+)\/remove$/)) && MEMBER_ID.test(match[1])) return removeMember(who, match[1]);
  if (path === "name") return rename(request, who);
  if (path === "create") return createOrg(request, who);
  if (path === "switch") return switchOrg(request, who);
  throw new HttpError(404, "Unknown organization operation.");
}
