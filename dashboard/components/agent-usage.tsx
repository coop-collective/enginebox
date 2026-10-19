"use client";

import { useEffect, useState } from "react";
import { api } from "@/lib/client";
import {
  allowanceRemaining,
  allowanceWindowLabel,
  providerUsageLinks,
  usageObservationCurrent,
} from "@/lib/agent-usage";
import type { AgentUsage, ProviderAccount } from "@/lib/types";

function timestamp(value: string) {
  const date = new Date(value);
  return Number.isFinite(date.getTime())
    ? date.toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })
    : "Not reported";
}

export function AgentUsageSummary({ account, refreshToken }: { account: ProviderAccount; refreshToken: number }) {
  const [usage, setUsage] = useState<AgentUsage | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout>;
    setUsage(null);
    setError("");
    setLoading(false);
    if (account.status !== "connected") return;
    async function refresh() {
      setLoading(true);
      try {
        const data = await api<{ usage: AgentUsage }>(`/api/control/v1/agents/${account.id}/usage`, { signal: controller.signal });
        if (controller.signal.aborted) return;
        if (data.usage.provider !== account.id) throw new Error("Usage belongs to a different provider.");
        setUsage(data.usage);
        setError("");
      } catch {
        if (!controller.signal.aborted) {
          // Keep a previous observation as explicitly stale, never current.
          setUsage((previous) => previous ? { ...previous, freshness: "stale" } : null);
          setError("Usage could not be refreshed. Check the provider or try again shortly.");
        }
      } finally {
        if (!controller.signal.aborted) {
          setLoading(false);
          if (account.id === "codex") timer = setTimeout(refresh, 60_000);
        }
      }
    }
    void refresh();
    const clock = setInterval(() => setNow(Date.now()), 1000);
    return () => { controller.abort(); clearTimeout(timer); clearInterval(clock); };
  }, [account.id, account.status, account.auth_method, refreshToken]);

  return (
    <div className="agent-usage" aria-label={`${account.label} usage and billing`}>
      <div className="usage-heading">
        <h4>Usage &amp; billing</h4>
        {usage?.plan && <span>{usage.plan.replaceAll("_", " ")}</span>}
      </div>
      {usage?.allowance_windows.map((window) => {
        const remaining = allowanceRemaining(usage, window, now);
        return <div className="usage-window" key={window.id}>
          <div><span>{allowanceWindowLabel(window.window_minutes)}</span><strong>{remaining === null ? "Last reported" : `${Number(remaining.toFixed(1))}% left`}</strong></div>
          {remaining !== null
            ? <progress max={100} value={remaining} aria-label={`${window.label} ${allowanceWindowLabel(window.window_minutes)} remaining`} />
            : <small>{window.used_percentage}% used at last update. Refresh needed.</small>}
          <small>{window.label !== "codex" ? `${window.label} · ` : ""}Quota reset: {timestamp(window.resets_at)}</small>
        </div>;
      })}
      {!usage?.allowance_windows.length && <p className="usage-empty">{loading && !usage ? "Reading allowance…" : "Allowance not reported"}</p>}
      {usage?.credits.map((credit, index) => <p className="usage-credit" key={`${credit.label}-${index}`}>
        {usageObservationCurrent(usage, now) ? "Provider credits" : "Last reported credits"}: <strong>{credit.unlimited ? "Unlimited" : credit.balance}</strong>
        {credit.label !== "codex" && <small>{credit.label}</small>}
      </p>)}
      {error && <p className="usage-note">{error}</p>}
      {!error && account.status !== "connected" && <p className="usage-note">Sign in to check available account usage.</p>}
      {!error && usage?.status === "unavailable" && <p className="usage-note">{usage.detail}</p>}
      {usage?.observed_at && <p className="usage-observed">{usage.source} · Updated {timestamp(usage.observed_at)}</p>}
      <dl className="usage-billing"><dt>Billing cycle</dt><dd>Not reported</dd></dl>
      <p className="usage-note">Quota resets and billing renewal can have different dates.</p>
      <div className="usage-links">
        <a href={providerUsageLinks[account.id]} target="_blank" rel="noopener noreferrer" referrerPolicy="no-referrer">Open provider usage &amp; billing ↗</a>
      </div>
    </div>
  );
}
