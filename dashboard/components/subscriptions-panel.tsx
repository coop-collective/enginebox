"use client";

import { FormEvent, useCallback, useEffect, useRef, useState } from "react";
import { api } from "@/lib/client";
import {
  activeAuth,
  assignedWorkspace,
  officialAuthorizationUrl,
  providerIds,
  providerLabels,
} from "@/lib/accounts";
import type { AuthSession, Host, Instance, ProviderAccount } from "@/lib/types";

function ProviderCard({
  account,
  host,
  name,
  changed,
}: {
  account: ProviderAccount;
  host: string;
  name: string;
  changed: () => Promise<void>;
}) {
  const bound = (session?: AuthSession | null) =>
    !!session &&
    session.host_id === host &&
    session.name === name &&
    session.provider === account.id;
  const [session, setSession] = useState<AuthSession | null>(
    bound(account.session) ? account.session! : null,
  );
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const pendingRequests = useRef(new Set<AbortController>());
  const pollRequest = useRef<AbortController | null>(null);
  const base = `/api/control/v1/hosts/${encodeURIComponent(host)}/instances/${encodeURIComponent(name)}/accounts/${account.id}`;
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
            "The sign-in response belongs to a different workspace.",
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
          "The sign-in response belongs to a different workspace.",
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
      {account.auth_method === "api_key" && (
        <p className="account-detail">
          This workspace currently authenticates with an API key.
        </p>
      )}
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

function WorkspaceAccounts({
  host,
  instance,
}: {
  host: Host;
  instance: Instance;
}) {
  const [providers, setProviders] = useState<ProviderAccount[] | null>(null);
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
        `/api/control/v1/hosts/${encodeURIComponent(host.id)}/instances/${encodeURIComponent(instance.name)}/accounts`,
        { signal: controller.signal },
      );
      if (!controller.signal.aborted) setProviders(data.providers);
    } catch (err) {
      if (!controller.signal.aborted) setError((err as Error).message);
    } finally {
      if (!controller.signal.aborted) setLoading(false);
    }
  }, [host.id, instance.name]);
  useEffect(() => {
    void load();
    return () => current.current?.abort();
  }, [load]);
  return (
    <>
      <div className="account-toolbar">
        <span>
          VM: <code>{instance.name}</code>
        </span>
        <button className="button small" disabled={loading} onClick={load}>
          {loading ? "Refreshing…" : "Refresh accounts"}
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
            ? "Reading native sign-in status…"
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
              host={host.id}
              name={instance.name}
              changed={load}
            />
          ))}
        </div>
      )}
    </>
  );
}

export function SubscriptionsPanel({
  hosts,
  initialSelection,
  pending,
  resume,
  close,
}: {
  hosts: Host[];
  initialSelection?: { host: string; name: string } | null;
  pending: string[];
  resume: (host: string, name: string) => void;
  close: () => void;
}) {
  const firstHost =
    hosts.find(
      (item) => item.online && item.instances.some(assignedWorkspace),
    ) ?? hosts[0];
  const [hostId, setHostId] = useState(
    initialSelection?.host ?? firstHost?.id ?? "",
  );
  const selectedHost = hosts.find((item) => item.id === hostId);
  const workspaces = selectedHost?.instances.filter(assignedWorkspace) ?? [];
  const [name, setName] = useState(
    initialSelection?.name ??
      workspaces.find((item) => item.status === "Running")?.name ??
      workspaces[0]?.name ??
      "",
  );
  const instance = workspaces.find((item) => item.name === name);
  const isPending = pending.includes(`${hostId}/${name}`);
  function chooseHost(id: string) {
    setHostId(id);
    const available =
      hosts
        .find((item) => item.id === id)
        ?.instances.filter(assignedWorkspace) ?? [];
    setName(
      available.find((item) => item.status === "Running")?.name ??
        available[0]?.name ??
        "",
    );
  }
  return (
    <section
      className="subscriptions-panel"
      aria-labelledby="subscriptions-title"
    >
      <div className="section-title">
        <div>
          <h2 id="subscriptions-title">Subscriptions</h2>
          <p>
            Connect native clients inside a private workspace. Credentials stay
            in its VM.
          </p>
        </div>
        <button className="button small" onClick={close}>
          Close subscriptions
        </button>
      </div>
      <div className="account-selectors">
        <label>
          Account host
          <select
            value={hostId}
            onChange={(event) => chooseHost(event.target.value)}
          >
            <option value="" disabled>
              Select a host
            </option>
            {hosts.map((item) => (
              <option key={item.id} value={item.id}>
                {item.label}
                {!item.online ? " (offline)" : ""}
              </option>
            ))}
          </select>
        </label>
        <label>
          Private workspace
          <select
            value={instance ? name : ""}
            onChange={(event) => setName(event.target.value)}
          >
            <option value="" disabled>
              Select an assigned workspace
            </option>
            {workspaces.map((item) => (
              <option key={item.name} value={item.name}>
                {item.workspace_name || item.name}
                {item.workspace_name && item.workspace_name !== item.name
                  ? ` (${item.name})`
                  : ""}
              </option>
            ))}
          </select>
        </label>
      </div>
      {!instance ? (
        <p className="account-detail">
          Choose an assigned workspace. Prepared reserves and unmanaged bases
          cannot receive account credentials.
        </p>
      ) : !selectedHost?.online ? (
        <p className="account-detail">
          This host is offline. Reconnect it before signing in.
        </p>
      ) : instance.status !== "Running" ? (
        <div className="account-resume">
          <p>
            Resume this workspace to connect accounts. Sign-in commands need a
            running workspace.
          </p>
          {["Stopped", "Frozen"].includes(instance.status) && (
            <button
              className="button"
              disabled={isPending}
              onClick={() => resume(hostId, name)}
            >
              {isPending ? "Resuming…" : "Resume workspace"}
            </button>
          )}
        </div>
      ) : (
        <WorkspaceAccounts
          key={`${hostId}/${name}`}
          host={selectedHost}
          instance={instance}
        />
      )}
      <p className="workspace-hint">
        Sign-in status does not confirm a paid plan or remaining usage. Accounts
        are not pooled or copied between workspaces.
      </p>
    </section>
  );
}
