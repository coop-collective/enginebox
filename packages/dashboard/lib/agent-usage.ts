import type { AgentUsage, ProviderId } from "./types";

export const providerUsageLinks: Record<ProviderId, string> = {
  codex: "https://chatgpt.com/codex/settings/usage",
  claude: "https://claude.ai/settings/usage",
  cursor: "https://cursor.com/dashboard/spending",
};

export function usageObservationCurrent(usage: AgentUsage, now = Date.now()) {
  const observed = Date.parse(usage.observed_at ?? "");
  return usage.freshness === "current" && Number.isFinite(observed) && now - observed < 300_000;
}

export function allowanceRemaining(
  usage: AgentUsage,
  window: AgentUsage["allowance_windows"][number],
  now = Date.now(),
) {
  const reset = Date.parse(window.resets_at);
  if (
    !usageObservationCurrent(usage, now) ||
    window.freshness !== "current" ||
    !Number.isFinite(reset) ||
    reset <= now ||
    !Number.isFinite(window.used_percentage) ||
    window.used_percentage < 0 ||
    window.used_percentage > 100
  ) return null;
  return Math.max(0, 100 - window.used_percentage);
}

export function allowanceWindowLabel(minutes: number | null) {
  if (minutes === null) return "Allowance window";
  if (minutes % 1440 === 0) return `${minutes / 1440}-day allowance`;
  if (minutes % 60 === 0) return `${minutes / 60}-hour allowance`;
  return `${minutes}-minute allowance`;
}
