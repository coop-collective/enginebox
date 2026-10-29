import { config, HttpError } from "../server";
import type { Fleet } from "../types";
import type { PoolGrant, RepositoryGrant, WorkspaceGrant } from "./store";

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

/** The controller refused the grant itself (a workspace, pool or profile no longer exists). */
export class GrantRefused extends Error {}

/** Everything one access token carries: the consented workspaces, pools and repository grants, and the grant's callback key. */
export type TokenGrant = {
  workspaces: WorkspaceGrant[];
  pools: PoolGrant[];
  repositories?: RepositoryGrant[];
  callback_secret?: string | null;
  project?: string | null;
  /** The grant's org; sent only when it is not the platform org, so older controllers keep accepting default-org grants. */
  org_id?: string | null;
};

/**
 * Mint a short-lived scoped service token: the OAuth access token. Profiles
 * named on workspaces become the token's profile grant (the controller grants
 * profiles per token, not per workspace); a pool grant carries its own profile.
 */
export async function mintAccessToken(grantId: string, grant: TokenGrant, ttlSeconds: number): Promise<Minted> {
  const profiles = [...new Set(grant.workspaces.flatMap((w) => w.profiles))].sort();
  const repositories = grant.repositories ?? [];
  const response = await call("v1/service-tokens", {
    method: "POST",
    body: {
      label: `oauth grant ${grantId}`,
      scopes: grant.workspaces.map((w) => ({ host_id: w.host, name: w.instance })),
      operations: AGENT_RUN_OPERATIONS,
      expires_at: new Date(Date.now() + ttlSeconds * 1000).toISOString(),
      // Optional fields only when used, so older controllers keep accepting workspace-only grants.
      ...(profiles.length ? { profiles } : {}),
      ...(grant.pools.length ? { pools: grant.pools.map((p) => p.pool) } : {}),
      ...(repositories.length ? { repositories: repositories.map((r) => ({ project: r.project, access: r.access })) } : {}),
      ...(grant.callback_secret ? { callback_secret: grant.callback_secret } : {}),
      // Restricts the pool and repository grants to one project; the controller re-checks it on every request.
      ...(grant.project && (grant.pools.length || repositories.length) ? { project: grant.project } : {}),
      ...(grant.org_id && grant.org_id !== "default" ? { org_id: grant.org_id } : {}),
    },
  });
  const body = (await response.json().catch(() => null)) as {
    token?: { id?: unknown; expires_at?: unknown };
    secret?: unknown;
  } | null;
  // 400: the controller validated the request and refused it, so this grant cannot be honoured any more.
  if (response.status === 400) throw new GrantRefused("The consented workspaces, pools, repositories or profiles are no longer available.");
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

export type Pool = { name: string; project: string; description: string; profile: string | null; members: number; org_id: string };

/** Pools listed by a controller without projects belong to this one. */
export const DEFAULT_PROJECT = "default";
/** The controller's project IDs (the fleet Worker's identifier shape). */
export const PROJECT = /^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$/;

/** Owner-managed pools the owner can grant, sorted by project then name. */
export async function grantablePools(): Promise<Pool[]> {
  const response = await call("v1/pools", { method: "GET" });
  if (!response.ok) throw new HttpError(503, "The fleet service could not list pools.");
  const body = (await response.json()) as { pools?: { name?: unknown; project?: unknown; description?: unknown; profile?: unknown; members?: unknown; org_id?: unknown }[] };
  return (body.pools ?? [])
    .filter((p) => typeof p.name === "string")
    .map((p) => ({
      name: p.name as string,
      project: typeof p.project === "string" && PROJECT.test(p.project) ? p.project : DEFAULT_PROJECT,
      description: typeof p.description === "string" ? p.description : "",
      profile: typeof p.profile === "string" ? p.profile : null,
      members: Array.isArray(p.members) ? p.members.length : 0,
      org_id: typeof p.org_id === "string" ? p.org_id : "default",
    }))
    .sort((a, b) => a.project.localeCompare(b.project) || a.name.localeCompare(b.name));
}

/** Names of the owner's profiles, so consent never stores one the controller would refuse at mint. */
export async function profileNames(): Promise<string[]> {
  const response = await call("v1/profiles", { method: "GET" });
  if (!response.ok) throw new HttpError(503, "The fleet service could not list profiles.");
  const body = (await response.json()) as { profiles?: { name?: unknown }[] };
  return (body.profiles ?? []).map((p) => p.name).filter((n): n is string => typeof n === "string");
}

/**
 * Projects whose repositories the owner can grant: every project that has a pool or a repository.
 * A controller without repositories (older release) contributes only its pool projects.
 */
export async function grantableRepositoryProjects(pools: Pool[]): Promise<string[]> {
  const projects = new Set(pools.map((p) => p.project));
  try {
    const response = await call("v1/repositories", { method: "GET" });
    if (response.ok) {
      const body = (await response.json()) as { repositories?: { project?: unknown }[] };
      for (const r of body.repositories ?? []) if (typeof r.project === "string" && PROJECT.test(r.project)) projects.add(r.project);
    }
  } catch {}
  return [...projects].sort();
}
