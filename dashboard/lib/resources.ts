import type { Host, Resources } from "./types";

export function validResources(resources: Resources, minimum: Resources) {
  return (["cpu", "memory_gib", "disk_gib"] as const).every(
    (key) =>
      Number.isSafeInteger(resources[key]) &&
      resources[key] >= minimum[key] &&
      resources[key] > 0,
  );
}

export function coldSlots(host: Host | undefined, resources: Resources) {
  const c = host?.capacity;
  if (
    !host?.online ||
    !c ||
    !validResources(resources, { cpu: 1, memory_gib: 1, disk_gib: 1 })
  )
    return 0;
  // These budgets include host reserves and disk commitments. The extra disk
  // covers a memory checkpoint plus the fleet service's fixed overhead.
  return Math.max(
    0,
    Math.floor(
      Math.min(
        (c.cpu_available ?? 0) / resources.cpu,
        (c.memory_budget_available_bytes ?? 0) /
          (resources.memory_gib * 1024 ** 3),
        (c.disk_budget_available_bytes ?? 0) /
          ((resources.disk_gib + resources.memory_gib + 2) * 1024 ** 3),
        c.instance_slots_available ?? 0,
      ),
    ),
  );
}

export function matchingPools(
  host: Host | undefined,
  template: string,
  size: string,
  resources?: Resources,
) {
  return (
    host?.warm_pools?.filter(
      (pool) =>
        pool.template === template &&
        (resources
          ? !!pool.resources &&
            (["cpu", "memory_gib", "disk_gib"] as const).every(
              (key) => pool.resources![key] === resources[key],
            )
          : pool.size === size),
    ) ?? []
  );
}
