import { randomInt, timingSafeEqual } from "node:crypto";
import { page } from "../oauth/pages";
import {
  bodyJson,
  config,
  cookie,
  HttpError,
  json,
  memberSessionCookie,
  readSession,
  requireOrigin,
  requireSession,
  sessionSeconds,
} from "../server";
import { send, type Message } from "./mail";
import {
  addMember,
  createUser,
  DEFAULT_ORG,
  digest,
  hit,
  homeOrg,
  makeInstanceOwner,
  MINUTE,
  normalizeEmail,
  openSignup,
  orgOf,
  randomToken,
  readIdentity,
  requireIdentity,
  userByEmail,
  withIdentity,
  type Challenge,
  type IdentityState,
  type User,
} from "./store";

/** Sign-in codes and links live ten minutes; a challenge allows five code attempts. */
export const CHALLENGE_MINUTES = 10;
export const MAX_CODE_ATTEMPTS = 5;
/** Per-email, per-client-address and global limits on starting a sign-in. */
export const LIMITS = {
  startPerEmail: { limit: 5, window: 15 * MINUTE },
  startPerClient: { limit: 20, window: 15 * MINUTE },
  startGlobal: { limit: 200, window: 15 * MINUTE },
  verifyPerClient: { limit: 30, window: 15 * MINUTE },
  verifyGlobal: { limit: 500, window: 15 * MINUTE },
  invitePerClient: { limit: 20, window: 15 * MINUTE },
};
const TOO_MANY = "Too many sign-in attempts. Wait a few minutes and try again.";
const BAD_CODE = "That code is not valid or has expired. Request a new one.";
const BAD_LINK = "This sign-in link is not valid, has expired or was already used. Request a new one.";

/**
 * The client address used for per-client limits. Only the header named by
 * ENGINEBOX_CLIENT_IP_HEADER (for example `cf-connecting-ip` behind the
 * Cloudflare tunnel, where the edge sets it and the origin is unreachable
 * otherwise) is trusted; without it every client shares one bucket, which the
 * per-email and global limits still bound.
 */
export function clientKey(request: Request) {
  const name = (process.env.ENGINEBOX_CLIENT_IP_HEADER ?? "").trim().toLowerCase();
  if (!name || !/^[a-z0-9-]{1,64}$/.test(name)) return "client:shared";
  const value = (request.headers.get(name) ?? "").split(",")[0].trim();
  return /^[0-9A-Fa-f.:]{2,45}$/.test(value) ? `client:${value.toLowerCase()}` : "client:shared";
}

const sixDigits = () => String(randomInt(0, 1_000_000)).padStart(6, "0");
function same(a: string, b: string) {
  const x = Buffer.from(a), y = Buffer.from(b);
  return x.length === y.length && timingSafeEqual(x, y);
}

/** Create a server-side session for `user` acting in `orgId` (default: their home org); returns the Set-Cookie value. */
export function openSession(state: IdentityState, user: User, now: number, orgId?: string | null) {
  const org = orgId ?? homeOrg(state, user);
  if (!org) throw new HttpError(403, "This account has no organization.");
  const sid = randomToken(32);
  const expires = now + sessionSeconds * 1000;
  state.sessions.push({ id_hash: digest("session", sid), user_id: user.id, org_id: org, created_at: now, expires_at: expires, revoked_at: null });
  user.last_sign_in_at = now;
  return memberSessionCookie(sid, expires, now);
}

function newChallenge(state: IdentityState, email: string, purpose: Challenge["purpose"], now: number) {
  // Requesting a new code ends every earlier one for this address and purpose.
  for (const c of state.challenges) if (c.email === email && c.purpose === purpose && c.used_at === null) c.used_at = now;
  const id = randomToken(24);
  const code = sixDigits();
  const link = randomToken(32);
  state.challenges.push({
    id_hash: digest("challenge", id),
    email,
    purpose,
    code_hash: digest("code", `${id}\0${code}`),
    link_hash: digest("link", `${id}\0${link}`),
    attempts: 0,
    created_at: now,
    expires_at: now + CHALLENGE_MINUTES * MINUTE,
    used_at: null,
  });
  return { id, code, link };
}

/** Fire-and-forget: the response never waits on (or reveals) delivery. */
function deliver(message: Message) {
  void send(message).catch(() => {});
}

/**
 * POST /api/auth/email {email}. Always answers `{sent: true, challenge}` for a
 * well-formed address, whether or not it has an account: the challenge handle
 * for an unknown address is random and never verifies (unless open sign-up is
 * on, when it creates the account and its personal org), the same limits
 * apply, and the email is sent after the response is decided.
 */
export async function startEmailSignIn(request: Request) {
  requireOrigin(request);
  requireIdentity();
  const body = await bodyJson(request);
  const email = normalizeEmail(body?.email);
  if (!email) throw new HttpError(400, "Enter a valid email address.");
  const client = clientKey(request);
  const { origin } = config();
  const result = withIdentity((state, now) => {
    hit(state, now, "start:global", LIMITS.startGlobal.limit, LIMITS.startGlobal.window, TOO_MANY);
    hit(state, now, `start:${client}`, LIMITS.startPerClient.limit, LIMITS.startPerClient.window, TOO_MANY);
    hit(state, now, `start:email:${email}`, LIMITS.startPerEmail.limit, LIMITS.startPerEmail.window, TOO_MANY);
    const user = userByEmail(state, email);
    if (!user && !openSignup()) return { challenge: randomToken(24), message: null };
    const { id, code, link } = newChallenge(state, email, user ? "sign_in" : "sign_up", now);
    const orgId = user ? homeOrg(state, user) : null;
    const message: Message = {
      template: "sign_in",
      to: email,
      code,
      link: `${origin}/api/auth/link?c=${encodeURIComponent(id)}&t=${encodeURIComponent(link)}`,
      org_name: (orgId && orgOf(state, orgId)?.name) || "Engine Box",
      expires_minutes: CHALLENGE_MINUTES,
    };
    return { challenge: id, message };
  });
  if (result.message) deliver(result.message);
  return json({ sent: true, challenge: result.challenge, expires_minutes: CHALLENGE_MINUTES });
}

type Consumed = { ok: true; setCookie: string } | { ok: false; error: string };

/** Consume a challenge by code or link inside one locked update. */
function consume(
  request: Request,
  challengeId: string,
  check: (c: Challenge) => boolean,
  failure: string,
  operatorRequired = false,
): Consumed {
  const client = clientKey(request);
  return withIdentity((state, now) => {
    hit(state, now, "verify:global", LIMITS.verifyGlobal.limit, LIMITS.verifyGlobal.window, TOO_MANY);
    hit(state, now, `verify:${client}`, LIMITS.verifyPerClient.limit, LIMITS.verifyPerClient.window, TOO_MANY);
    const c = state.challenges.find((x) => x.id_hash === digest("challenge", challengeId));
    if (!c || c.used_at !== null || c.expires_at <= now || c.attempts >= MAX_CODE_ATTEMPTS) return { ok: false, error: failure };
    if (!check(c)) {
      c.attempts += 1;
      if (c.attempts >= MAX_CODE_ATTEMPTS) c.used_at = now;
      return { ok: false, error: failure };
    }
    if (c.purpose === "claim_owner") {
      // Only the break-glass operator who started the claim can finish it, and only while there is no instance owner.
      if (!operatorRequired) return { ok: false, error: failure };
      c.used_at = now;
      if (state.users.some((u) => u.instance_owner)) return { ok: false, error: "This Engine Box already has an instance owner." };
      const user = userByEmail(state, c.email) ?? createUser(state, c.email, "operator", now);
      makeInstanceOwner(state, user, "operator", now);
      return { ok: true, setCookie: openSession(state, user, now, DEFAULT_ORG) };
    }
    c.used_at = now;
    let user = userByEmail(state, c.email);
    // Sign-up: every new account gets its personal org.
    if (!user && c.purpose === "sign_up" && openSignup()) user = createUser(state, c.email, "signup", now);
    if (!user) return { ok: false, error: failure };
    return { ok: true, setCookie: openSession(state, user, now) };
  });
}

/** POST /api/auth/email/verify {challenge, code}. */
export async function verifyEmailCode(request: Request) {
  requireOrigin(request);
  requireIdentity();
  const body = await bodyJson(request);
  const challenge = typeof body?.challenge === "string" && /^[A-Za-z0-9_-]{32}$/.test(body.challenge) ? body.challenge : null;
  const code = typeof body?.code === "string" ? body.code.replace(/\s+/g, "") : "";
  if (!challenge || !/^\d{6}$/.test(code)) throw new HttpError(400, BAD_CODE);
  const operator = readSession(request)?.kind === "operator";
  const result = consume(request, challenge, (c) => same(c.code_hash, digest("code", `${challenge}\0${code}`)), BAD_CODE, operator);
  if (!result.ok) throw new HttpError(401, result.error);
  return json({ authenticated: true }, 200, { "Set-Cookie": result.setCookie });
}

const ID = /^[A-Za-z0-9_-]{32}$/;
const TOKEN = /^[A-Za-z0-9_-]{43}$/;
const escape = (value: string) => value.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
function htmlStatus(response: Response, status: number) {
  return new Response(response.body, { status, headers: response.headers });
}

/**
 * GET /api/auth/link?c=&t=. Renders a confirmation button instead of signing in:
 * mail scanners prefetch links, and a GET must never consume a single-use token.
 */
export function magicLinkPage(request: Request) {
  requireIdentity();
  const url = new URL(request.url);
  const c = url.searchParams.get("c") ?? "", t = url.searchParams.get("t") ?? "";
  if (!ID.test(c) || !TOKEN.test(t)) return htmlStatus(page("Sign in to Engine Box", `<h1>Sign-in link not valid</h1><p>${escape(BAD_LINK)}</p><p><a href="${escape(config().origin)}/">Back to Engine Box</a></p>`), 400);
  return page(
    "Sign in to Engine Box",
    `<h1>Sign in to Engine Box</h1><p class="muted">Continue to sign in on this browser. The link works once and expires ${CHALLENGE_MINUTES} minutes after it was sent.</p>` +
      `<form method="post" action="/api/auth/link"><input type="hidden" name="c" value="${escape(c)}"><input type="hidden" name="t" value="${escape(t)}">` +
      `<div class="actions"><button class="primary" type="submit">Sign in</button></div></form>`,
  );
}

async function readSmallForm(request: Request) {
  if (!request.headers.get("content-type")?.startsWith("application/x-www-form-urlencoded")) throw new HttpError(415, "Send a form.");
  const text = await request.text();
  if (text.length > 4096) throw new HttpError(413, "Request is too large.");
  return new URLSearchParams(text);
}
function sameOriginForm(request: Request) {
  const origin = request.headers.get("origin");
  if (origin && origin !== "null") return requireOrigin(request);
  if (request.headers.get("sec-fetch-site") !== "same-origin") throw new HttpError(403, "Request origin is not allowed.");
}
const redirectHome = (setCookie: string) =>
  new Response(null, { status: 303, headers: { Location: `${config().origin}/`, "Set-Cookie": setCookie, "Cache-Control": "no-store", "Referrer-Policy": "no-referrer" } });

/** POST /api/auth/link (form c, t): consumes the link and signs in. */
export async function magicLinkConsume(request: Request) {
  requireIdentity();
  try {
    sameOriginForm(request);
    const form = await readSmallForm(request);
    const c = form.get("c") ?? "", t = form.get("t") ?? "";
    if (!ID.test(c) || !TOKEN.test(t)) throw new HttpError(400, BAD_LINK);
    const result = consume(request, c, (x) => same(x.link_hash, digest("link", `${c}\0${t}`)), BAD_LINK);
    if (!result.ok) throw new HttpError(401, result.error);
    return redirectHome(result.setCookie);
  } catch (error) {
    const status = error instanceof HttpError ? error.status : 500;
    const message = error instanceof HttpError && status < 500 ? error.message : "Sign-in failed.";
    return htmlStatus(page("Sign in to Engine Box", `<h1>Sign-in failed</h1><p>${escape(message)}</p><p><a href="${escape(config().origin)}/">Back to Engine Box</a></p>`), status);
  }
}

/**
 * POST /api/org/claim {email}: the break-glass operator names the instance
 * owner (owner of the platform org) when ENGINEBOX_OWNER_EMAIL was not set. A
 * code goes to that address; verifying it (POST /api/auth/email/verify, still
 * in the operator session) creates the account, with its personal org, and
 * switches the browser to that member session.
 */
export async function claimOwnerStart(request: Request) {
  requireOrigin(request);
  requireIdentity();
  const who = requireSession(request);
  if (who.kind !== "operator") throw new HttpError(403, "Only the host operator's break-glass session can claim ownership.");
  const body = await bodyJson(request);
  const email = normalizeEmail(body?.email);
  if (!email) throw new HttpError(400, "Enter a valid email address.");
  const client = clientKey(request);
  const result = withIdentity((state, now) => {
    if (state.users.some((u) => u.instance_owner)) throw new HttpError(409, "This Engine Box already has an instance owner.");
    hit(state, now, `start:${client}`, LIMITS.startPerClient.limit, LIMITS.startPerClient.window, TOO_MANY);
    hit(state, now, `start:email:${email}`, LIMITS.startPerEmail.limit, LIMITS.startPerEmail.window, TOO_MANY);
    const { id, code } = newChallenge(state, email, "claim_owner", now);
    return { id, code, org: orgOf(state, DEFAULT_ORG)?.name ?? "Engine Box" };
  });
  const sent = await send({ template: "claim_owner", to: email, code: result.code, link: config().origin, org_name: result.org, expires_minutes: CHALLENGE_MINUTES });
  if (!sent) throw new HttpError(503, "The verification email could not be sent. Check the email gateway configuration.");
  return json({ sent: true, challenge: result.id, expires_minutes: CHALLENGE_MINUTES });
}

// ---------------------------------------------------------------- invites

/** GET /api/auth/invite?token=: shows the invite and an accept button (GET never consumes). */
export function invitePage(request: Request) {
  requireIdentity();
  const token = new URL(request.url).searchParams.get("token") ?? "";
  const state = readIdentity();
  const now = Date.now();
  const invite = TOKEN.test(token) ? state.invites.find((i) => i.token_hash === digest("invite", token)) : undefined;
  if (!invite || invite.used_at !== null || invite.revoked_at !== null || invite.expires_at <= now)
    return htmlStatus(page("Engine Box invite", `<h1>This invite cannot be used</h1><p>It has expired, was revoked or was already accepted. Ask an admin for a new one.</p><p><a href="${escape(config().origin)}/">Back to Engine Box</a></p>`), 400);
  const org = orgOf(state, invite.org_id)?.name ?? "Engine Box";
  return page(
    "Engine Box invite",
    `<h1>Join ${escape(org)} on Engine Box</h1><p>You were invited as <strong>${escape(invite.role)}</strong> with <strong>${escape(invite.email)}</strong>.</p>` +
      `<p class="muted">Accepting signs this browser in. Later, sign in with a code sent to that address.</p>` +
      `<form method="post" action="/api/auth/invite"><input type="hidden" name="token" value="${escape(token)}"><div class="actions"><button class="primary" type="submit">Accept invite</button></div></form>`,
  );
}

/**
 * POST /api/auth/invite (form token): single use, unexpired and unrevoked. The token
 * reached the invited address, so accepting proves control of it.
 */
export async function acceptInvite(request: Request) {
  requireIdentity();
  try {
    sameOriginForm(request);
    const form = await readSmallForm(request);
    const token = form.get("token") ?? "";
    if (!TOKEN.test(token)) throw new HttpError(400, "This invite cannot be used.");
    const client = clientKey(request);
    const setCookie = withIdentity((state, now) => {
      hit(state, now, `invite:${client}`, LIMITS.invitePerClient.limit, LIMITS.invitePerClient.window, TOO_MANY);
      const invite = state.invites.find((i) => i.token_hash === digest("invite", token));
      if (!invite || invite.used_at !== null || invite.revoked_at !== null || invite.expires_at <= now)
        throw new HttpError(400, "This invite has expired, was revoked or was already accepted.");
      if (!orgOf(state, invite.org_id)) throw new HttpError(400, "This invite's organization no longer exists.");
      // New people get an account (with its personal org) here; existing users just gain the membership.
      const user = userByEmail(state, invite.email) ?? createUser(state, invite.email, "invite", now);
      if (state.members.some((m) => m.org_id === invite.org_id && m.user_id === user.id && m.removed_at === null))
        throw new HttpError(409, "You are already a member of this organization. Sign in instead.");
      addMember(state, invite.org_id, user.id, invite.role, "invite", now);
      invite.used_at = now;
      invite.used_by = user.id;
      return openSession(state, user, now, invite.org_id);
    });
    return redirectHome(setCookie);
  } catch (error) {
    const status = error instanceof HttpError ? error.status : 500;
    const message = error instanceof HttpError && status < 500 ? error.message : "The invite could not be accepted.";
    return htmlStatus(page("Engine Box invite", `<h1>Invite not accepted</h1><p>${escape(message)}</p><p><a href="${escape(config().origin)}/">Back to Engine Box</a></p>`), status);
  }
}

/** DELETE /api/session: clears the cookie and, for a member, revokes the server-side session. */
export function signOut(request: Request) {
  requireOrigin(request);
  const session = readSession(request);
  if (session?.kind === "member" && readIdentityEnabled())
    withIdentity((state, now) => {
      const record = state.sessions.find((s) => s.id_hash === digest("session", session.sid));
      if (record && record.revoked_at === null) record.revoked_at = now;
    });
  return json({ authenticated: false }, 200, { "Set-Cookie": cookie("", 0) });
}
function readIdentityEnabled() {
  try {
    requireIdentity();
    return true;
  } catch {
    return false;
  }
}

/** Revoke a user's live sessions in one org (membership removed). */
export function revokeOrgSessions(state: IdentityState, userId: string, orgId: string, now: number) {
  for (const s of state.sessions) if (s.user_id === userId && s.org_id === orgId && s.revoked_at === null) s.revoked_at = now;
}
