import test from "node:test";
import assert from "node:assert/strict";
import { allowanceRemaining, allowanceWindowLabel, usageObservationCurrent } from "../lib/agent-usage";
import type { AgentUsage } from "../lib/types";

test("allowance stops being current at reset or after a missed refresh; it never rolls to 100%", () => {
  const now = Date.parse("2026-10-19T12:00:00Z");
  const window: AgentUsage["allowance_windows"][number] = {
    id: "codex:primary", label: "codex", used_percentage: 25,
    remaining_percentage: 75, window_minutes: 10080,
    resets_at: "2026-10-19T12:01:00Z", freshness: "current",
  };
  const usage = { freshness: "current", observed_at: "2026-10-19T12:00:00Z" } as AgentUsage;
  assert.equal(allowanceRemaining(usage, window, now), 75);
  assert.equal(usageObservationCurrent(usage, now + 300_000), false);
  assert.equal(allowanceRemaining(usage, window, now + 60_000), null);
  assert.equal(allowanceRemaining({ ...usage, freshness: "stale" }, window, now), null);
  assert.equal(allowanceRemaining(usage, { ...window, freshness: "stale" }, now), null);
  assert.equal(allowanceRemaining({ ...usage, observed_at: new Date(now - 300_000).toISOString() }, window, now), null);
  assert.equal(allowanceRemaining(usage, { ...window, used_percentage: NaN }, now), null);
  assert.equal(allowanceRemaining(usage, { ...window, resets_at: "invalid" }, now), null);
  assert.equal(allowanceWindowLabel(10080), "7-day allowance");
  assert.equal(allowanceWindowLabel(300), "5-hour allowance");
  assert.equal(allowanceWindowLabel(null), "Allowance window");
});
