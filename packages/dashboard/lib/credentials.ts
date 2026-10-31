import { atLeast, bodyJson, config, HttpError, json, requireOrigin, requireSession, type Principal } from "./server";
import { DEFAULT_ORG, identityEnabled } from "./identity/store";

/**
 * Org agent credentials (docs/ORGS.md#agent-credentials), proxied to the
 * controller's `/v1/credentials` acting for the member's org.
 *
 * - Any member connects their own `personal` Codex, Claude or Cursor
 *   subscription. Only its owner signs it in, chooses whether it is shared into
 *   the org's pools, or (with admins) deletes it.
 * - Admins add `team` accounts (signed in the same way) and `api_key` records.
 *   A key is sent once and never returned.
 * - Everyone in the org sees the list (metadata only); another member's
 *   unshared personal credential shows only to its owner and admins.
 * - Each member saves at most one `unity` account (username and password,
 *   checked with Unity by the controller). Only its owner saves or updates
 *   it; it is never shared and the password is never returned. Sessions a
 *   member starts use only their own.
 */
type Credential = {
  id: string;
  provider: string;
  kind: "personal" | "team" | "api_key";
  owner_user_id: string | null;
  share_with_org_pools: boolean;
};

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
        // The controller audits Unity accounts by member and refuses to save one for anyone else.
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

const ID = /^[a-f0-9]{32}$/;
const visible = (who: Principal, c: Credential) =>
  c.kind !== "personal" || c.owner_user_id === who.user_id || c.share_with_org_pools || atLeast(who, "admin");
/** Who may sign in, share or delete a credential. */
function mayManage(who: Principal, c: Credential, action: "login" | "share" | "delete" | "update") {
  if (c.provider === "unity") return c.owner_user_id === who.user_id || (action === "delete" && atLeast(who, "admin"));
  if (action === "update") return false;
  if (c.kind === "personal") return c.owner_user_id === who.user_id || (action === "delete" && atLeast(who, "admin"));
  return action !== "share" && atLeast(who, "admin");
}

export async function credentials(request: Request, segments: string[]) {
  if (!identityEnabled()) throw new HttpError(404, "Organizations are not enabled on this Engine Box.");
  const who = requireSession(request);
  if (request.method !== "GET") requireOrigin(request);
  if (new URL(request.url).search) throw new HttpError(400, "Query parameters are not supported.");
  const [id, action, session, sub] = segments;
  if (segments.length > 4 || (id !== undefined && !ID.test(id)) || (session !== undefined && !ID.test(session)))
    throw new HttpError(404, "Unknown credential operation.");

  if (request.method === "GET" && segments.length === 0) {
    const { status, value } = await controller("v1/credentials", who, "GET");
    const list = Array.isArray(value.credentials) ? (value.credentials as Credential[]) : [];
    return json({ me: who.user_id, role: who.role, credentials: list.filter((c) => visible(who, c)) }, status);
  }
  if (request.method === "POST" && segments.length === 0) {
    const body = await bodyJson(request);
    const kind = body?.kind;
    if (body?.provider === "unity") {
      if (!who.user_id) throw new HttpError(403, "Sign in with your email to save your Unity account.");
      if (kind !== "personal") throw new HttpError(400, "A Unity account is personal: each member saves their own.");
      const { username, password } = body;
      const { status, value } = await controller("v1/credentials", who, "POST", { provider: "unity", kind, owner_user_id: who.user_id, username, password });
      return json(value, status);
    }
    if (kind === "personal") {
      if (!who.user_id) throw new HttpError(403, "Sign in with your email to connect your own subscription.");
      const { provider, label, share_with_org_pools } = body;
      const { status, value } = await controller("v1/credentials", who, "POST", {
        provider,
        kind,
        label,
        owner_user_id: who.user_id,
        ...(share_with_org_pools === undefined ? {} : { share_with_org_pools }),
      });
      return json(value, status);
    }
    if (kind !== "team" && kind !== "api_key") throw new HttpError(400, "Choose personal, team or api_key.");
    if (!atLeast(who, "admin")) throw new HttpError(403, "Only org admins add team accounts and API keys.");
    const { provider, label, key } = body;
    const { status, value } = await controller("v1/credentials", who, "POST", { provider, kind, label, ...(kind === "api_key" ? { key } : {}) });
    return json(value, status);
  }

  // Everything else names one credential: load it in this org to check who may act.
  const listed = await controller("v1/credentials", who, "GET");
  const credential = (Array.isArray(listed.value.credentials) ? (listed.value.credentials as Credential[]) : []).find((c) => c.id === id);
  if (!credential || !visible(who, credential)) throw new HttpError(404, "Unknown credential.");

  const forward = async (path: string, method: string, body?: unknown) => {
    const { status, value } = await controller(path, who, method, body);
    return json(value, status);
  };
  if (request.method === "GET" && action === "sessions" && session && !sub) {
    if (!mayManage(who, credential, "login")) throw new HttpError(404, "Unknown credential.");
    return forward(`v1/credentials/${id}/sessions/${session}`, "GET");
  }
  if (request.method !== "POST") throw new HttpError(404, "Unknown credential operation.");
  if (action === "login" && !session) {
    if (!mayManage(who, credential, "login")) throw new HttpError(403, credential.kind === "personal" ? "Only its owner connects a personal subscription." : "Only org admins connect team accounts.");
    await bodyJson(request).catch(() => ({}));
    return forward(`v1/credentials/${id}/login`, "POST", {});
  }
  if (action === "share" && !session) {
    if (!mayManage(who, credential, "share")) throw new HttpError(403, "Only its owner decides whether a personal subscription is shared.");
    const body = await bodyJson(request);
    if (typeof body?.share !== "boolean") throw new HttpError(400, "Send {share: true|false}.");
    return forward(`v1/credentials/${id}/share`, "POST", { share: body.share });
  }
  if (action === "update" && !session) {
    if (credential.provider !== "unity") throw new HttpError(404, "Unknown credential operation.");
    if (!mayManage(who, credential, "update")) throw new HttpError(403, "Only its owner updates a Unity account.");
    const body = await bodyJson(request);
    return forward(`v1/credentials/${id}/update`, "POST", { username: body?.username, password: body?.password });
  }
  if (action === "delete" && !session) {
    if (!mayManage(who, credential, "delete")) throw new HttpError(403, "You cannot remove this credential.");
    return forward(`v1/credentials/${id}/delete`, "POST", {});
  }
  if (action === "sessions" && session && (sub === "input" || sub === "cancel")) {
    if (!mayManage(who, credential, "login")) throw new HttpError(404, "Unknown credential.");
    const body = await bodyJson(request);
    return forward(`v1/credentials/${id}/sessions/${session}/${sub}`, "POST", sub === "input" ? { text: body?.text } : {});
  }
  throw new HttpError(404, "Unknown credential operation.");
}
