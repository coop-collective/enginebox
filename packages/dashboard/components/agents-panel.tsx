"use client";

import { FormEvent, useCallback, useEffect, useRef, useState } from "react";
import { api } from "@/lib/client";
import {
  activeAuth,
  officialAuthorizationUrl,
  providerIds,
  providerLabels,
  delegationLabel,
} from "@/lib/accounts";
import type { AuthSession, ProviderAccount } from "@/lib/types";
import { AgentUsageSummary } from "@/components/agent-usage";

function ProviderCard({
  account,
  changed,
  refreshToken,
}: {
  account: ProviderAccount;
  changed: () => Promise<void>;
  refreshToken: number;
}) {
  const bound = (session?: AuthSession | null) =>
    !!session && session.provider === account.id;
  const [session, setSession] = useState<AuthSession | null>(
    bound(account.session) ? account.session! : null,
  );
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const pendingRequests = useRef(new Set<AbortController>());
  const pollRequest = useRef<AbortController | null>(null);
  const base = `/api/control/v1/agents/${account.id}`;
  useEffect(
    () => () => {
      for (const request of pendingRequests.current) request.abort();
    },
    [],
  );
  useEffect(() => {
    if (bound(account.session)) setSession(account.session!);
  }, [account.session]);
  useEffect(() => {
    if (!session || !activeAuth(session) || busy) return;
    const controller = new AbortController();
    pollRequest.current = controller;
    let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      try {
        const data = await api<{ session: AuthSession }>(
          `${base}/sessions/${session!.id}`,
          { signal: controller.signal },
        );
        if (controller.signal.aborted) return;
        if (!bound(data.session))
          throw new Error(
            "The sign-in response belongs to a different provider.",
          );
        setSession(data.session);
        setError("");
        if (data.session.status === "succeeded") void changed();
        if (activeAuth(data.session)) timer = setTimeout(poll, 1000);
      } catch (err) {
        if (controller.signal.aborted) return;
        setError(`Could not refresh sign-in: ${(err as Error).message}`);
        timer = setTimeout(poll, 2000);
      }
    }
    timer = setTimeout(poll, 1000);
    return () => {
      clearTimeout(timer);
      controller.abort();
    };
  }, [session?.id, session?.status, base, busy, changed]);
  async function mutate(path: string, body: object = {}) {
    pollRequest.current?.abort();
    const controller = new AbortController();
    pendingRequests.current.add(controller);
    setBusy(true);
    setError("");
    try {
      const data = await api<{ session: AuthSession }>(`${base}/${path}`, {
        method: "POST",
        body: JSON.stringify(body),
        signal: controller.signal,
      });
      if (controller.signal.aborted) return;
      if (!bound(data.session))
        throw new Error(
          "The sign-in response belongs to a different provider.",
        );
      setSession(data.session);
      if (data.session.status === "succeeded") void changed();
    } catch (err) {
      if (!controller.signal.aborted) setError((err as Error).message);
    } finally {
      pendingRequests.current.delete(controller);
      if (!controller.signal.aborted) setBusy(false);
    }
  }
  function sendCode(event: FormEvent) {
    event.preventDefault();
    if (
      !session ||
      account.id !== "claude" ||
      !session.input_required ||
      !activeAuth(session) ||
      !code.trim()
    )
      return;
    const text = code.trim();
    setCode("");
    void mutate(`sessions/${session.id}/input`, { text });
  }
  const pending = activeAuth(session);
  const url = officialAuthorizationUrl(account.id, session?.authorization_url);
  const status =
    !account.installed || account.status === "unavailable"
      ? "Unavailable"
      : account.status === "connected"
        ? account.auth_method === "api_key"
          ? "API key configured"
          : "Signed in"
        : account.status === "disconnected"
          ? "Not signed in"
          : "Status unknown";
  const expires = session ? Date.parse(session.expires_at) : NaN;
  return (
    <article className="account-card" aria-label={providerLabels[account.id]}>
      <div className="account-heading">
        <h3>{providerLabels[account.id]}</h3>
        <span className={`account-status account-${account.status}`}>
          {status}
        </span>
      </div>
      {account.detail && <p className="account-detail">{account.detail}</p>}
      {!account.installed && !account.detail && (
        <p className="account-detail">
          The native client is unavailable on the controller.
        </p>
      )}
      <p
        className={`account-delegation delegation-${account.delegation_status ?? "unknown"}`}
      >
        {delegationLabel(account)}
      </p>
      {account.auth_method === "api_key" && (
        <p className="account-detail">
          This agent currently authenticates with an API key.
        </p>
      )}
      <AgentUsageSummary account={account} refreshToken={refreshToken} />
      {error && (
        <p className="account-error" role="alert">
          {error}
        </p>
      )}
      {session && (
        <div
          className={`auth-session auth-${session.status}`}
          aria-live="polite"
        >
          <strong>
            {session.status === "succeeded"
              ? "Sign-in completed"
              : session.status.charAt(0).toUpperCase() +
                session.status.slice(1)}
          </strong>
          <p>{session.message}</p>
          {pending && (
            <>
              {url && (
                <a
                  className="button small"
                  href={url}
                  target="_blank"
                  rel="noopener noreferrer"
                  referrerPolicy="no-referrer"
                >
                  Open provider sign-in
                </a>
              )}
              {session.authorization_url && !url && (
                <p className="account-error">
                  The provider sign-in URL could not be verified. Cancel and try
                  again.
                </p>
              )}
              {session.user_code && (
                <div className="device-code">
                  <span>Device code</span>
                  <code>{session.user_code}</code>
                </div>
              )}
              {Number.isFinite(expires) && (
                <small>
                  Expires{" "}
                  {new Date(expires).toLocaleTimeString([], {
                    hour: "2-digit",
                    minute: "2-digit",
                  })}
                </small>
              )}
              {account.id === "claude" && session.input_required && (
                <form className="auth-code-form" onSubmit={sendCode}>
                  <label>
                    Claude authorization code
                    <input
                      type="password"
                      autoComplete="off"
                      value={code}
                      onChange={(event) => setCode(event.target.value)}
                      minLength={8}
                      maxLength={2048}
                      required
                    />
                  </label>
                  <button
                    className="button small"
                    disabled={busy || !code.trim()}
                    type="submit"
                  >
                    Submit code
                  </button>
                </form>
              )}
            </>
          )}
        </div>
      )}
      <div className="account-actions">
        {pending ? (
          <button
            className="button small"
            type="button"
            disabled={busy}
            onClick={() => void mutate(`sessions/${session!.id}/cancel`)}
          >
            Cancel sign-in
          </button>
        ) : (
          <button
            className="button small"
            type="button"
            disabled={
              busy || !account.installed || account.status === "unavailable"
            }
            onClick={() => {
              setCode("");
              void mutate("login");
            }}
          >
            {busy
              ? "Starting…"
              : account.status === "connected"
                ? "Reconnect"
                : "Connect"}
          </button>
        )}
      </div>
    </article>
  );
}

function ControllerAgents() {
  const [providers, setProviders] = useState<ProviderAccount[] | null>(null);
  const [refreshToken, setRefreshToken] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const current = useRef<AbortController | null>(null);
  const load = useCallback(async () => {
    current.current?.abort();
    const controller = new AbortController();
    current.current = controller;
    setLoading(true);
    setError("");
    try {
      const data = await api<{ providers: ProviderAccount[] }>(
        "/api/control/v1/agents",
        { signal: controller.signal },
      );
      if (!controller.signal.aborted) {
        setProviders(data.providers);
        setRefreshToken((value) => value + 1);
      }
    } catch (err) {
      if (!controller.signal.aborted) {
        setError((err as Error).message);
        setProviders(null);
      }
    } finally {
      if (!controller.signal.aborted) setLoading(false);
    }
  }, []);
  useEffect(() => {
    void load();
    return () => current.current?.abort();
  }, [load]);
  return (
    <>
      <div className="account-toolbar">
        <span>Controller connections</span>
        <button className="button small" disabled={loading} onClick={load}>
          {loading ? "Refreshing…" : "Refresh agents"}
        </button>
      </div>
      {error && (
        <div className="alert" role="alert">
          {error}
        </div>
      )}
      {providers === null ? (
        <p className="account-detail">
          {loading
            ? "Reading agent connection status…"
            : "Account status is unavailable. Refresh to try again."}
        </p>
      ) : (
        <div className="accounts-grid">
          {providerIds.map((id) => (
            <ProviderCard
              key={id}
              account={
                providers.find((provider) => provider.id === id) ?? {
                  id,
                  label: providerLabels[id],
                  installed: false,
                  status: "unknown",
                  auth_method: "unknown",
                  detail: "No status was returned for this provider.",
                }
              }
              changed={load}
              refreshToken={refreshToken}
            />
          ))}
        </div>
      )}
    </>
  );
}

export function AgentsPanel({ close }: { close: () => void }) {
  return (
    <section className="agents-panel" aria-labelledby="agents-title">
      <div className="section-title">
        <div>
          <h2 id="agents-title">Agents</h2>
          <p>
            Connect once. Available to your private workspaces across the fleet.
          </p>
        </div>
        <button className="button small" onClick={close}>
          Close agents
        </button>
      </div>
      <ControllerAgents />
      <p className="workspace-hint">
        Each provider's workspace access is shown on its card. Sign-in status
        does not confirm a paid plan or remaining usage.
      </p>
    </section>
  );
}
