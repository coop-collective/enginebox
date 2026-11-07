import { createHash, createHmac, timingSafeEqual } from "node:crypto";
import { HttpError } from "./http-error";
import {
  DEFAULT_ORG,
  digest,
  identityDir,
  membership,
  orgOf,
  readIdentity,
  roleRank,
  userById,
  type OrgKind,
  type Role,
} from "./identity/store";
import { granted, hostOrg, tenancy, type TenancyView } from "./tenancy";

export { HttpError };

const cookieName = "enginebox_session";
export const sessionSeconds = 8 * 60 * 60;

export function config() {
  const secret = process.env.ENGINEBOX_SESSION_SECRET ?? "";
  const apiToken = process.env.ENGINEBOX_API_TOKEN ?? "";
  if (secret.length < 32 || apiToken.length < 32) {
    throw new HttpError(
      503,
      "Dashboard credentials are not configured. Check the server environment.",
    );
  }
  if (secret === apiToken)
    throw new HttpError(
      503,
      "Use separate dashboard and fleet API credentials.",
    );
  // Email sign-in is the only way in, so the identity store is required.
  if (!identityDir())
    throw new HttpError(
      503,
      "Email sign-in is not configured. Set ENGINEBOX_IDENTITY_STATE_DIR.",
    );
  const origin =
    process.env.ENGINEBOX_DASHBOARD_ORIGIN ??
    `http://127.0.0.1:${process.env.PORT ?? "3000"}`;
  const apiUrl = process.env.ENGINEBOX_API_URL ?? "http://127.0.0.1:8765";
  for (const value of [origin, apiUrl]) {
    let url: URL;
    try {
      url = new URL(value);
    } catch {
      throw new HttpError(503, "Invalid dashboard server URL configuration.");
    }
    if (
      url.username ||
      url.password ||
      url.search ||
      url.hash ||
      !["http:", "https:"].includes(url.protocol)
    ) {
      throw new HttpError(503, "Invalid dashboard server URL configuration.");
    }
    if (
      url.protocol === "http:" &&
      !["127.0.0.1", "localhost", "[::1]"].includes(url.hostname)
    ) {
      throw new HttpError(
        503,
        "Non-loopback dashboard and API URLs must use HTTPS.",
      );
    }
  }
  if (new URL(origin).origin !== origin)
    throw new HttpError(
      503,
      "ENGINEBOX_DASHBOARD_ORIGIN must be an exact origin without a path or trailing slash.",
    );
  return {
    secret,
    apiToken,
    origin,
    apiUrl: apiUrl.replace(/\/$/, ""),
  };
}

export function json(
  value: unknown,
  status = 200,
  extra: Record<string, string> = {},
) {
  return Response.json(value, {
    status,
    headers: {
      "Cache-Control": "no-store",
      "X-Content-Type-Options": "nosniff",
      Vary: "Cookie",
      ...extra,
    },
  });
}

export function errorResponse(error: unknown) {
  return error instanceof HttpError
    ? json({ error: error.message }, error.status)
    : json({ error: "The dashboard could not complete this request." }, 500);
}

export function requireOrigin(request: Request) {
  if (request.headers.get("origin") !== config().origin)
    throw new HttpError(403, "Request origin is not allowed.");
}

function equal(a: string, b: string) {
  return timingSafeEqual(
    createHash("sha256").update(a).digest(),
    createHash("sha256").update(b).digest(),
  );
}

/** Member sessions: signed with the secret only (domain-separated); each is also a revocable server-side record. */
function signMember(payload: string) {
  const { secret } = config();
  return createHmac("sha256", secret)
    .update("enginebox-member-session\0")
    .update(payload)
    .digest("base64url");
}

/** A member session cookie carrying the session ID whose digest the identity store keeps. */
export function memberSessionCookie(sessionId: string, expires: number, now = Date.now()) {
  const payload = Buffer.from(
    JSON.stringify({ kind: "member", sid: sessionId, expires }),
  ).toString("base64url");
  return cookie(
    `${payload}.${signMember(payload)}`,
    Math.max(0, Math.floor((expires - now) / 1000)),
  );
}

export function cookie(value: string, maxAge: number) {
  return `${cookieName}=${value}; HttpOnly; Path=/; SameSite=Strict; Max-Age=${maxAge}${config().origin.startsWith("https:") ? "; Secure" : ""}`;
}

export type SessionToken = { kind: "member"; sid: string; expires: number };

/** The verified session cookie, or null. Does not consult the identity store. */
export function readSession(request: Request, now = Date.now()): SessionToken | null {
  config();
  const token = request.headers
    .get("cookie")
    ?.split(";")
    .map((part) => part.trim())
    .find((part) => part.startsWith(`${cookieName}=`))
    ?.slice(cookieName.length + 1);
  if (!token || token.length > 1024) return null;
  const [payload, signature, extra] = token.split(".");
  if (!payload || !signature || extra) return null;
  let session: { kind?: unknown; expires?: unknown; sid?: unknown };
  try {
    session = JSON.parse(Buffer.from(payload, "base64url").toString("utf8"));
  } catch {
    return null;
  }
  if (
    typeof session?.expires !== "number" ||
    session.expires <= now ||
    session.expires > now + sessionSeconds * 1000
  )
    return null;
  // Only member sessions exist; operator cookies from the retired password sign-in are rejected.
  if (session.kind !== "member") return null;
  if (!equal(signMember(payload), signature)) return null;
  if (typeof session.sid !== "string" || !/^[A-Za-z0-9_-]{43}$/.test(session.sid)) return null;
  return { kind: "member", sid: session.sid, expires: session.expires };
}

/** Who is acting: a signed-in user in one of their orgs. */
export type Principal = {
  kind: "member";
  /** The org this session acts in. */
  org_id: string;
  org_kind: OrgKind;
  user_id: string;
  member_id: string;
  role: Role;
  /** Operator of the install (the user named by ENGINEBOX_OWNER_EMAIL). */
  instance_owner: boolean;
  email: string;
  /** Owner of OAuth grants this principal approves: the user ID. */
  grant_owner: string;
  label: string;
};

/**
 * The request's principal, or null. A session is valid only while its
 * server-side record is live and the user still belongs to the session's org.
 */
export function principal(request: Request, now = Date.now()): Principal | null {
  const session = readSession(request, now);
  if (!session) return null;
  const state = readIdentity();
  const record = state.sessions.find(
    (s) => s.id_hash === digest("session", session.sid),
  );
  if (!record || record.revoked_at !== null || record.expires_at <= now) return null;
  const user = userById(state, record.user_id);
  const member = user && membership(state, record.org_id, user.id);
  const org = member && orgOf(state, member.org_id);
  if (!user || !member || !org) return null;
  return {
    kind: "member",
    org_id: org.id,
    org_kind: org.kind,
    user_id: user.id,
    member_id: member.id,
    role: member.role,
    instance_owner: user.instance_owner,
    email: user.email,
    grant_owner: user.id,
    label: user.name || user.email,
  };
}

export function authenticated(request: Request, now = Date.now()) {
  return principal(request, now) !== null;
}

export function requireSession(request: Request): Principal {
  const who = principal(request);
  if (!who) throw new HttpError(401, "Sign in to access your fleet.");
  return who;
}

export const atLeast = (who: Principal, role: Role) => roleRank[who.role] >= roleRank[role];
/** Require a session with at least `role` in its org. */
export function requireRole(request: Request, role: Role): Principal {
  const who = requireSession(request);
  if (!atLeast(who, role))
    throw new HttpError(403, `This needs the ${role} role in your organization.`);
  return who;
}

export async function bodyJson(request: Request, maxBytes = 8192) {
  if (!request.headers.get("content-type")?.startsWith("application/json"))
    throw new HttpError(415, "Send an application/json request.");
  if (Number(request.headers.get("content-length") ?? 0) > maxBytes)
    throw new HttpError(413, "Request is too large.");
  const reader = request.body?.getReader();
  if (!reader) throw new HttpError(400, "Request body is required.");
  let bytes = 0;
  const chunks: Uint8Array[] = [];
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    bytes += value.byteLength;
    if (bytes > maxBytes) {
      await reader.cancel();
      throw new HttpError(413, "Request is too large.");
    }
    chunks.push(value);
  }
  try {
    return JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    throw new HttpError(400, "Request body must be valid JSON.");
  }
}

/**
 * POST operations an org `member` may make: running agents on pools, using and
 * allocating workspaces, creating hosted repositories and day-to-day storage
 * reads. Every other mutation (templates, GitHub connections, CI templates,
 * storage maintenance, the org's controller-scope agent accounts) needs `admin`.
 * Reads need only membership.
 */
export function memberMayPost(path: string) {
  return (
    /^v1\/pools\/[a-z][a-z0-9-]{0,31}\/agent-runs$/.test(path) ||
    /^v1\/projects\/[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?\/repositories$/.test(path) ||
    /^v1\/hosts\/[a-zA-Z0-9_-]{1,63}\/(?:allocations|instances(?:\/ebox-[a-z0-9-]{1,49}\/actions)?)$/.test(path) ||
    /^v1\/storage\/hosts\/[a-z0-9][a-z0-9-]{0,48}\/(?:inspect|list|preview|dev-start|dev-status)$/.test(path)
  );
}

/**
 * What an org sees of a controller listing. Pools, repositories and profiles
 * carry `org_id` (absent: the platform org); hosts and nodes belong to the org
 * the tenancy view assigns (unassigned: the platform org). An org also sees
 * platform pools shared with it by a capacity grant. The platform org never
 * sees another org's records.
 */
export function scopeToOrg(result: unknown, org: string, view: TenancyView): unknown {
  if (!result || typeof result !== "object" || Array.isArray(result)) return result;
  const out: Record<string, unknown> = { ...(result as Record<string, unknown>) };
  const ownerOf = (item: unknown) => {
    const owner = item && typeof item === "object" ? (item as { org_id?: unknown }).org_id : undefined;
    return typeof owner === "string" ? owner : DEFAULT_ORG;
  };
  for (const key of ["pools", "repositories", "profiles"]) {
    const list = out[key];
    if (Array.isArray(list))
      out[key] = list.filter((item) => {
        const owner = ownerOf(item);
        if (owner === org) return true;
        const name = (item as { name?: unknown })?.name;
        return key === "pools" && owner === DEFAULT_ORG && typeof name === "string" && granted(view, org, name);
      });
  }
  // A single record of another org reads as absent.
  const single = out.repository;
  if (single && typeof single === "object" && ownerOf(single) !== org) throw new HttpError(404, "Not found.");
  const hosts = out.hosts;
  if (Array.isArray(hosts))
    out.hosts = hosts.filter((h) => typeof (h as { id?: unknown })?.id === "string" && hostOrg(view, (h as { id: string }).id) === org);
  return out;
}

/** The host a POST path operates on, if any. */
function hostOfPath(path: string) {
  return /^v1\/hosts\/([a-zA-Z0-9_-]{1,63})\//.exec(path)?.[1] ?? /^v1\/storage\/hosts\/([a-z0-9][a-z0-9-]{0,48})\//.exec(path)?.[1] ?? null;
}

export async function proxy(request: Request, segments: string[]) {
  const who = requireSession(request);
  if (request.method !== "GET") requireOrigin(request);
  const path = segments.join("/");
  const accountRead =
    /^v1\/agents(?:\/(?:codex|claude|cursor|glm)\/(?:usage|sessions\/[a-f0-9]{32}))?$/.test(
      path,
    );
  const accountWrite =
    /^v1\/agents\/(?:(?:codex|claude|cursor|glm)\/(?:login|sessions\/[a-f0-9]{32}\/cancel)|claude\/sessions\/[a-f0-9]{32}\/input)$/.test(
      path,
    );
  const allowed =
    request.method === "GET"
      ? accountRead || /^v1\/github(?:\/(?:repos|runners)\/installations|\/installations\/[1-9][0-9]*\/repositories)?$/.test(path) ||
        path === "v1/ci/templates" ||
        // Hosted repositories: the project list (from pools), repositories and the redacted backend state.
        /^v1\/(?:workspaces|pools|repositories|repository-backends)$/.test(path) ||
        /^v1\/projects\/[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?\/repositories(?:\/[a-z0-9][a-z0-9._-]{0,62})?$/.test(path) ||
        /^v1\/(fleet|compute|templates|storage|jobs\/[a-zA-Z0-9_-]{1,100})$/.test(path)
      : request.method === "POST" &&
        (accountWrite || /^v1\/github\/(?:(?:repos|runners)\/disconnect|select|remove|drain)$/.test(path) ||
          /^v1\/projects\/[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?\/repositories(?:\/github)?$/.test(path) ||
          /^v1\/pools\/[a-z][a-z0-9-]{0,31}\/agent-runs$/.test(path) ||
          /^v1\/ci\/templates(?:\/[a-zA-Z0-9_-]{1,48}\/(?:enable|disable|retry))?$/.test(path) ||
          /^v1\/storage\/hosts\/[a-z0-9][a-z0-9-]{0,48}\/(inspect|list|preview|import|fork|deps|checkpoint|remount|close|recover|github-preview|github-import|github-fork|github-settings|github-promote|github-delivery|dev-start|dev-status)$/.test(path) ||
          /^(?:v1\/templates(?:\/[a-z][a-z0-9-]{0,31}\/sync)?|v1\/hosts\/[a-zA-Z0-9_-]{1,63}\/(?:templates\/[a-z][a-z0-9-]{0,31}\/install|allocations|instances(?:\/ebox-[a-z0-9-]{1,49}\/actions)?))$/.test(
            path,
          ));
  if (
    !allowed ||
    segments.some((part) => part.includes("/") || part.includes(".."))
  )
    throw new HttpError(404, "Unknown fleet operation.");
  if (request.method === "POST" && !memberMayPost(path) && !atLeast(who, "admin"))
    throw new HttpError(403, "This needs the admin role in your organization.");
  const platform = who.org_id === DEFAULT_ORG;
  // Templates, GitHub connections, CI templates and the controller-scope agent accounts belong to the
  // whole install: platform org only. (Per-member agent credentials are the follow-up.)
  if (request.method === "POST" && !platform && /^v1\/(?:templates|github|ci|agents)\b/.test(path))
    throw new HttpError(403, "Only the platform organization manages templates, GitHub, CI and controller agent accounts for this install.");
  const view = await tenancy();
  const host = request.method === "POST" ? hostOfPath(path) : null;
  if (host && hostOrg(view, host) !== who.org_id)
    throw new HttpError(403, "That machine belongs to another organization.");
  const { apiUrl, apiToken } = config();
  const body =
    request.method === "POST"
      ? JSON.stringify(
          await bodyJson(request, path === "v1/templates" ? 1024 * 1024 : path === "v1/ci/templates" ? 65536 : 8192),
        )
      : undefined;
  let response: Response;
  try {
    response = await fetch(`${apiUrl}/${path}`, {
      method: request.method,
      headers: {
        Authorization: `Bearer ${apiToken}`,
        "Content-Type": "application/json",
        // The controller stamps new pools, profiles and repositories with this org and enforces run tenancy.
        // Omitted for the platform org, so those requests are exactly as before orgs.
        ...(platform ? {} : { "X-EngineBox-Org": who.org_id }),
      },
      body,
      cache: "no-store",
      redirect: "error",
      signal: AbortSignal.timeout(30_000),
    });
  } catch {
    throw new HttpError(
      502,
      "The fleet service is unavailable. Check its connection and try again.",
    );
  }
  if (response.status === 401 || (response.status === 403 && platform))
    throw new HttpError(
      502,
      "The fleet service rejected the dashboard's credentials. Check the server configuration.",
    );
  let result: unknown;
  try {
    result = await response.json();
  } catch {
    throw new HttpError(502, "The fleet service returned an invalid response.");
  }
  return json(request.method === "GET" ? scopeToOrg(result, who.org_id, view) : result, response.status);
}
