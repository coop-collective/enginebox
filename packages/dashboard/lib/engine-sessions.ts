import { bodyJson, config, HttpError, json, requireOrigin, requireSession, type Principal } from "./server";
import { DEFAULT_ORG, identityEnabled } from "./identity/store";

/**
 * Engine sessions (docs/ENGINE-SESSIONS.md), proxied to the controller's
 * `/v1/projects/{project}/engine-sessions` acting for the signed-in member.
 *
 * - Any member starts, reads, renews and ends their own sessions. The
 *   controller records the member (`X-EngineBox-Actor`) as the session's
 *   starter, places it only on that member's own engine machine and uses only
 *   their own Unity account.
 * - Only the starter's own reads carry the stream block (relay, namespace,
 *   subscribe token, control key); the controller enforces that.
 * - The owner-wide list and end (`/v1/engine-sessions`) are not proxied.
 *
 * TODO(engine-sessions OAuth): Gamejam will reach the same routes as an OAuth
 * client with an `engine_sessions` grant (RESOURCE_ROUTES in oauth/server.ts).
 */
const PROJECT = /^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$/;
const ID = /^[a-f0-9]{32}$/;

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
        "X-EngineBox-Actor": who.user_id as string,
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

export async function engineSessions(request: Request, segments: string[]) {
  if (!identityEnabled()) throw new HttpError(404, "Organizations are not enabled on this Engine Box.");
  const who = requireSession(request);
  if (!who.user_id) throw new HttpError(403, "Engine sessions belong to a member. Sign in with your own account.");
  if (request.method !== "GET") requireOrigin(request);
  if (new URL(request.url).search) throw new HttpError(400, "Query parameters are not supported.");
  const [project, id, action] = segments;
  const allowed =
    project !== undefined && PROJECT.test(project) && segments.length <= 3 &&
    (id === undefined || ID.test(id)) &&
    (request.method === "GET"
      ? action === undefined
      : request.method === "POST" && (segments.length === 1 || (segments.length === 3 && (action === "renew" || action === "end"))));
  if (!allowed) throw new HttpError(404, "Unknown engine session operation.");
  const body = request.method === "POST" ? await bodyJson(request, 16000) : undefined;
  if (body !== undefined && (typeof body !== "object" || body === null || Array.isArray(body))) throw new HttpError(400, "Send a JSON object.");
  const path = `v1/projects/${project}/engine-sessions${id ? `/${id}` : ""}${action ? `/${action}` : ""}`;
  const { status, value } = await controller(path, who, request.method, body);
  return json(value, status);
}
