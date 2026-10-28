import { config } from "./server";
import { DEFAULT_ORG } from "./identity/store";

/**
 * The controller's tenancy view (`GET /v1/tenancy`): which org owns each host
 * or enrolled node (unlisted: the platform org) and the capacity grants that
 * share platform pools with other orgs. Cached briefly; an older controller
 * without the endpoint reads as "everything belongs to the platform org".
 */
export type CapacityGrant = {
  id: string;
  tenant_org: string;
  pool?: string;
  capacity_class?: string;
  limits?: Record<string, number>;
  revoked_at?: string | null;
};
export type TenancyView = { mode: string; host_orgs: Record<string, string>; grants: CapacityGrant[] };

let cached: { at: number; view: TenancyView } | null = null;
const empty = (): TenancyView => ({ mode: "self_hosted", host_orgs: {}, grants: [] });

export async function tenancy(now = Date.now()): Promise<TenancyView> {
  if (cached && now - cached.at < 5_000) return cached.view;
  const { apiUrl, apiToken } = config();
  let view = empty();
  try {
    const response = await fetch(`${apiUrl}/v1/tenancy`, {
      headers: { Authorization: `Bearer ${apiToken}` },
      cache: "no-store",
      redirect: "error",
      signal: AbortSignal.timeout(10_000),
    });
    if (response.ok) {
      const body = (await response.json()) as Partial<TenancyView>;
      view = {
        mode: typeof body.mode === "string" ? body.mode : "self_hosted",
        host_orgs: body.host_orgs && typeof body.host_orgs === "object" ? body.host_orgs : {},
        grants: Array.isArray(body.grants) ? body.grants : [],
      };
    }
  } catch {}
  cached = { at: now, view };
  return view;
}
/** Drop the cache (after an assignment, and in tests). */
export function forgetTenancy() {
  cached = null;
}

export const hostOrg = (view: TenancyView, host: string) => view.host_orgs[host] ?? DEFAULT_ORG;
/** A platform pool shared with `org` by a live capacity grant. */
export const granted = (view: TenancyView, org: string, pool: string) =>
  view.grants.some((g) => !g.revoked_at && g.tenant_org === org && g.pool === pool);
