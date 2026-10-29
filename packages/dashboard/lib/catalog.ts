import { atLeast, bodyJson, config, HttpError, json, requireOrigin, requireSession, type Principal } from "./server";
import { DEFAULT_ORG } from "./identity/store";

/**
 * The connections catalog and profile builder (docs/CONNECTIONS.md), proxied
 * to the controller for the session's org.
 *
 * - Every member reads connections (metadata and secret *names* only), the
 *   org's profiles and the runs that used them.
 * - Only org owners and admins create, change, test, delete and attach
 *   connections or change a profile's run-kind policy. The member's user ID
 *   goes to the controller as `X-EngineBox-Actor` for `created_by` and the
 *   audit log.
 * - Secret values are write-only: the dashboard forwards them once and the
 *   controller never returns them.
 */
const NAME = /^[a-z0-9][a-z0-9-]{0,62}$/;
const PROJECT = /^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$/;
const REPO = /^[a-z0-9][a-z0-9._-]{0,62}$/;

async function controller(path: string, who: Principal, method: string, body?: unknown) {
  const { apiUrl, apiToken } = config();
  let response: Response;
  try {
    response = await fetch(`${apiUrl}/${path}`, {
      method,
      headers: {
        Authorization: `Bearer ${apiToken}`,
        "Content-Type": "application/json",
        ...(who.org_id === DEFAULT_ORG ? {} : { "X-EngineBox-Org": who.org_id }),
        ...(who.user_id ? { "X-EngineBox-Actor": who.user_id } : {}),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
      cache: "no-store",
      redirect: "error",
      signal: AbortSignal.timeout(30_000),
    });
  } catch {
    throw new HttpError(502, "The fleet service is unavailable. Check its connection and try again.");
  }
  if (response.status === 401) throw new HttpError(502, "The fleet service rejected the dashboard's credentials. Check the server configuration.");
  const value = await response.json().catch(() => null);
  if (value === null) throw new HttpError(502, "The fleet service returned an invalid response.");
  return { status: response.status, value };
}

type Run = { profile?: { name?: unknown } | null; created_at?: unknown };

/** Routes: GET connections[/name], profiles[/name], profiles/name/runs, audit; POST the controller's catalog writes. */
export async function catalog(request: Request, segments: string[]) {
  const who = requireSession(request);
  if (request.method !== "GET") requireOrigin(request);
  if (new URL(request.url).search) throw new HttpError(400, "Query parameters are not supported.");
  if (segments.some((s) => s.includes("/") || s.includes("..")) || segments.length > 4) throw new HttpError(404, "Unknown catalog operation.");
  const [kind, name, sub, action] = segments;
  if (name !== undefined && !NAME.test(name)) throw new HttpError(404, "Unknown catalog operation.");
  if (kind === "repositories" && sub !== undefined && !REPO.test(sub)) throw new HttpError(404, "Unknown catalog operation.");
  const admin = atLeast(who, "admin");

  if (request.method === "GET") {
    const forward = async (path: string) => {
      const { status, value } = await controller(path, who, "GET");
      return { status, value };
    };
    if (kind === "connections" && segments.length <= 2) {
      const { status, value } = await forward(name ? `v1/connections/${name}` : "v1/connections");
      return json({ ...value, role: who.role, can_manage: admin }, status);
    }
    if (kind === "profiles" && segments.length === 1) {
      const { status, value } = await forward("v1/profiles");
      return json({ ...value, role: who.role, can_manage: admin }, status);
    }
    if (kind === "profiles" && name && segments.length === 2) {
      const { status, value } = await forward(`v1/profiles/${name}`);
      return json({ ...value, role: who.role, can_manage: admin }, status);
    }
    if (kind === "profiles" && name && sub === "runs" && segments.length === 3) {
      // The profile must be visible to this org before its runs are listed.
      const profile = await forward(`v1/profiles/${name}`);
      if (profile.status !== 200) throw new HttpError(404, "Unknown profile.");
      const { value } = await forward("v1/agent-runs");
      const runs = (Array.isArray(value.runs) ? (value.runs as Run[]) : []).filter((r) => r.profile?.name === name).slice(0, 25);
      return json({ runs });
    }
    if (kind === "repositories" && segments.length === 1) {
      const { status, value } = await forward("v1/repositories");
      const own = (Array.isArray(value.repositories) ? value.repositories : []).filter((r: { org_id?: string }) => (r.org_id ?? DEFAULT_ORG) === who.org_id);
      return json({ repositories: own, can_manage: admin }, status);
    }
    if (kind === "repositories" && name && sub && segments.length === 3 && PROJECT.test(name) && REPO.test(sub)) {
      const { status, value } = await forward(`v1/projects/${name}/repositories/${sub}/requirements`);
      return json({ ...value, can_manage: admin }, status);
    }
    if (kind === "audit" && segments.length === 1) {
      if (!admin) throw new HttpError(403, "The audit log needs the admin role in your organization.");
      const { status, value } = await forward("v1/audit");
      return json(value, status);
    }
    throw new HttpError(404, "Unknown catalog operation.");
  }

  if (request.method !== "POST") throw new HttpError(404, "Unknown catalog operation.");
  if (!admin) throw new HttpError(403, "Only org owners and admins manage connections and profiles.");
  const body = await bodyJson(request, 65536);
  let path: string | null = null;
  if (kind === "connections" && !name) path = "v1/connections";
  else if (kind === "connections" && name && !sub) path = `v1/connections/${name}`;
  else if (kind === "connections" && name && (sub === "delete" || sub === "test") && !action) path = `v1/connections/${name}/${sub}`;
  else if (kind === "profiles" && name && sub === "connections" && ["attach", "detach", "policy"].includes(action ?? "")) path = `v1/profiles/${name}/connections/${action}`;
  else if (kind === "repositories" && name && sub && PROJECT.test(name) && REPO.test(sub) && ["approve", "revoke"].includes(action ?? ""))
    path = `v1/projects/${name}/repositories/${sub}/requirements/${action}`;
  if (!path) throw new HttpError(404, "Unknown catalog operation.");
  const { status, value } = await controller(path, who, "POST", body);
  return json(value, status);
}
