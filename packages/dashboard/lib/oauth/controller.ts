import { config, HttpError } from "../server";
import type { Fleet } from "../types";
import type { WorkspaceGrant } from "./store";

/** Every agent-run operation; the `agent-runs` scope grants exactly these. */
export const AGENT_RUN_OPERATIONS = ["agent_runs.start", "agent_runs.get", "agent_runs.list", "agent_runs.cancel"];

async function call(path: string, init: { method: string; body?: unknown }) {
  const { apiUrl, apiToken } = config();
  try {
    return await fetch(`${apiUrl}/${path}`, {
      method: init.method,
      headers: { Authorization: `Bearer ${apiToken}`, "Content-Type": "application/json" },
      body: init.body === undefined ? undefined : JSON.stringify(init.body),
      cache: "no-store",
      redirect: "error",
      signal: AbortSignal.timeout(15_000),
    });
  } catch {
    throw new HttpError(503, "The fleet service is unavailable.");
  }
}

export type Minted = { id: string; secret: string; expires_at: number };

/** Mint a short-lived scoped service token: the OAuth access token. */
export async function mintAccessToken(grantId: string, workspaces: WorkspaceGrant[], ttlSeconds: number): Promise<Minted> {
  const response = await call("v1/service-tokens", {
    method: "POST",
    body: {
      label: `oauth grant ${grantId}`,
      scopes: workspaces.map((w) => ({ host_id: w.host, name: w.instance })),
      operations: AGENT_RUN_OPERATIONS,
      expires_at: new Date(Date.now() + ttlSeconds * 1000).toISOString(),
    },
  });
  const body = (await response.json().catch(() => null)) as {
    token?: { id?: unknown; expires_at?: unknown };
    secret?: unknown;
  } | null;
  const id = body?.token?.id, secret = body?.secret, expires = Date.parse(String(body?.token?.expires_at));
  if (response.status !== 201 || typeof id !== "string" || !/^[a-f0-9]{32}$/.test(id) || typeof secret !== "string" || !secret.startsWith(`ebst_${id}.`) || !Number.isFinite(expires))
    throw new HttpError(503, "The fleet service could not issue an access token.");
  return { id, secret, expires_at: expires };
}

/** Revoke a service token. True when the controller confirms it is (or is already) unusable. */
export async function revokeAccessToken(id: string): Promise<boolean> {
  try {
    const response = await call(`v1/service-tokens/${id}/revoke`, { method: "POST", body: {} });
    // 4xx other than auth means the controller does not know it (nothing to revoke).
    return response.ok || (response.status >= 400 && response.status < 500 && ![401, 403, 429].includes(response.status));
  } catch {
    return false;
  }
}

export type Workspace = { host: string; host_label: string; instance: string; label: string };

/** Assigned, managed workspaces the owner can grant (no reserves or unmanaged bases). */
export async function grantableWorkspaces(): Promise<Workspace[]> {
  const response = await call("v1/fleet", { method: "GET" });
  if (!response.ok) throw new HttpError(503, "The fleet service could not list workspaces.");
  const fleet = (await response.json()) as Fleet;
  return (fleet.hosts ?? []).flatMap((host) =>
    (host.instances ?? [])
      .filter((i) => i.managed && (!i.warm_pool_state || i.warm_pool_state === "claimed"))
      .map((i) => ({ host: host.id, host_label: host.label || host.id, instance: i.name, label: i.workspace_name || i.name })),
  );
}
