import assert from "node:assert/strict";
import { test } from "node:test";
import { coldSlots, matchingPools, validResources } from "../lib/resources";
import type { Host } from "../lib/types";

const GiB = 1024 ** 3;
const minimum = { cpu: 2, memory_gib: 2, disk_gib: 30 };
const recommended = { cpu: 4, memory_gib: 4, disk_gib: 40 };
const host: Host = {
  id: "test",
  label: "Test host",
  online: true,
  instances: [],
  capacity: {
    cpu_total: 32,
    memory_total_bytes: 128 * GiB,
    memory_available_bytes: 100 * GiB,
    disk_free_bytes: 1000 * GiB,
    reserve_cpu: 4,
    reserve_memory_bytes: 16 * GiB,
    allocated_cpu: 12,
    allocated_memory_bytes: 12 * GiB,
    cpu_available: 16,
    memory_budget_available_bytes: 16 * GiB,
    disk_budget_available_bytes: 100 * GiB,
    instance_slots_available: 8,
    max_additional: {},
  },
  warm_pools: [
    {
      template: "native",
      size: "standard",
      resources: recommended,
      target: 2,
      ready: 1,
      preparing: 1,
    },
  ],
};

test("custom sizing requires positive integers that meet every template minimum", () => {
  assert.equal(validResources(recommended, minimum), true);
  for (const resource of [
    { ...recommended, cpu: 1 },
    { ...recommended, memory_gib: 1.5 },
    { ...recommended, disk_gib: 29 },
    { ...recommended, cpu: NaN },
  ])
    assert.equal(validResources(resource, minimum), false);
});

test("cold capacity includes checkpoint disk overhead and instance slots", () => {
  assert.equal(coldSlots(host, recommended), 2);
  assert.equal(
    coldSlots(
      { ...host, capacity: { ...host.capacity!, instance_slots_available: 0 } },
      recommended,
    ),
    0,
  );
  assert.equal(coldSlots({ ...host, online: false }, recommended), 0);
  assert.equal(coldSlots({ ...host, capacity: undefined }, recommended), 0);
});

test("warm reserves match resolved resources across preset names, including custom sizing", () => {
  assert.equal(
    matchingPools(host, "native", "custom", recommended)[0].ready,
    1,
  );
  assert.equal(
    matchingPools(host, "native", "recommended", recommended).length,
    1,
  );
  assert.equal(matchingPools(host, "native", "minimum", minimum).length, 0);
  assert.equal(matchingPools(host, "other", "standard", recommended).length, 0);
  assert.equal(
    matchingPools(host, "native", "custom", { ...recommended, disk_gib: 41 })
      .length,
    0,
  );
});
