"use client";
import { FormEvent, useCallback, useEffect, useRef, useState } from "react";
import { api, errorMessage } from "@/lib/client";
import { activeAuth, officialAuthorizationUrl, providerIds, providerLabels } from "@/lib/accounts";
import type { AuthSession, ProviderId } from "@/lib/types";

type Credential = {
  id: string;
  provider: ProviderId;
  kind: "personal" | "team" | "api_key";
  label: string;
  owner_user_id: string | null;
  share_with_org_pools: boolean;
  status: "pending" | "connected" | "failed";
  has_key: boolean;
  connected_at: string | null;
};
type Listing = { me: string | null; role: "owner" | "admin" | "member"; credentials: Credential[] };

const kindLabel = { personal: "Personal", team: "Team account", api_key: "API key" } as const;

/**
 * Agent accounts in this org: connect your own subscription (personal), share
 * it into the org's pools if you choose, and (admins) add team accounts and
 * API key records. Runs do not pick from these yet.
 */
export function CredentialsPanel() {
  const [listing, setListing] = useState<Listing | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState("");
  const [provider, setProvider] = useState<ProviderId>("codex");
  const [kind, setKind] = useState<Credential["kind"]>("personal");
  const [label, setLabel] = useState("");
  const [key, setKey] = useState("");
  const [session, setSession] = useState<(AuthSession & { credential_id: string }) | null>(null);
  const [code, setCode] = useState("");
  const sessionRef = useRef(session);
  sessionRef.current = session;

  const refresh = useCallback(async () => {
    try {
      setListing(await api<Listing>("/api/credentials"));
      setError("");
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  // Poll an active sign-in about once a second; stop when it finishes or the panel closes.
  useEffect(() => {
    if (!session || !activeAuth(session)) return;
    const timer = setTimeout(async () => {
      try {
        const next = await api<AuthSession & { credential_id: string }>(`/api/credentials/${session.credential_id}/sessions/${session.id}`);
        setSession(next);
        if (!activeAuth(next)) void refresh();
      } catch (e) {
        setError(errorMessage(e));
      }
    }, 1000);
    return () => clearTimeout(timer);
  }, [session, refresh]);

  async function act(name: string, work: () => Promise<unknown>) {
    setBusy(name);
    setError("");
    try {
      await work();
      await refresh();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy("");
    }
  }
  const post = <T,>(path: string, body: unknown = {}) => api<T>(path, { method: "POST", body: JSON.stringify(body) });
  const connect = (c: Credential) =>
    act(`login:${c.id}`, async () => {
      const r = await post<{ session: AuthSession & { credential_id: string } }>(`/api/credentials/${c.id}/login`);
      setSession(r.session);
    });

  if (!listing) return error ? <div className="alert">{error}</div> : null;
  const admin = listing.role === "owner" || listing.role === "admin";
  const url = session ? officialAuthorizationUrl(session.provider, session.authorization_url) : null;

  return (
    <section className="contributor-panel" aria-label="Org agent accounts">
      <div className="contributor-heading">
        <div>
          <h2>Your and your team's agent accounts</h2>
          <p>Connect your own Codex, Claude or Cursor subscription to this organization. You decide whether the org's pools may use it. Engine Box never shows a stored key again.</p>
        </div>
      </div>
      {error && <div role="alert" className="alert">{error}</div>}
      <div className="host-card">
        <table className="table">
          <thead><tr><th>Account</th><th>Kind</th><th>Status</th><th>Org pools</th><th /></tr></thead>
          <tbody>
            {listing.credentials.length === 0 && <tr><td colSpan={5}>No accounts yet.</td></tr>}
            {listing.credentials.map((c) => {
              const mine = c.kind === "personal" && c.owner_user_id === listing.me;
              const canSignIn = c.kind !== "api_key" && (mine || (c.kind === "team" && admin));
              return (
                <tr key={c.id}>
                  <td>{c.label} · {providerLabels[c.provider]}{mine ? " (yours)" : ""}</td>
                  <td>{kindLabel[c.kind]}</td>
                  <td>{c.status === "connected" ? (c.kind === "api_key" ? "Key stored" : "Connected") : c.status === "failed" ? "Sign-in failed" : "Not connected"}</td>
                  <td>
                    {c.kind === "personal" ? (
                      mine ? (
                        <label>
                          <input type="checkbox" checked={c.share_with_org_pools} disabled={!!busy} onChange={(e) => act(`share:${c.id}`, () => post(`/api/credentials/${c.id}/share`, { share: e.target.checked }))} /> Share
                        </label>
                      ) : c.share_with_org_pools ? "Shared" : "Private"
                    ) : "Org-owned"}
                  </td>
                  <td>
                    {canSignIn && <button className="button small" disabled={!!busy || activeAuth(session)} onClick={() => connect(c)}>{c.status === "connected" ? "Reconnect" : "Connect"}</button>}
                    {(mine || admin) && <button className="button small" disabled={!!busy} onClick={() => window.confirm(`Remove ${c.label}? Its stored sign-in or key is deleted.`) && act(`delete:${c.id}`, () => post(`/api/credentials/${c.id}/delete`))}>Remove</button>}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      {session && (
        <div className="host-card" aria-live="polite">
          <h3>Signing in to {providerLabels[session.provider]}</h3>
          <p>{session.message}</p>
          {url && <p><a href={url} target="_blank" rel="noopener noreferrer">Open the official {providerLabels[session.provider]} sign-in page</a></p>}
          {session.user_code && <p>Enter this code there: <strong>{session.user_code}</strong></p>}
          {session.input_required && (
            <form onSubmit={(e: FormEvent) => { e.preventDefault(); void act("input", async () => { await post(`/api/credentials/${session.credential_id}/sessions/${session.id}/input`, { text: code }); setCode(""); }); }}>
              <input type="password" autoComplete="off" value={code} onChange={(e) => setCode(e.target.value)} placeholder="Authorization code" aria-label="Authorization code" required />
              <button className="button small" type="submit" disabled={busy === "input"}>Submit code</button>
            </form>
          )}
          {activeAuth(session) ? (
            <button className="button small" onClick={() => act("cancel", () => post(`/api/credentials/${session.credential_id}/sessions/${session.id}/cancel`))}>Cancel sign-in</button>
          ) : (
            <button className="button small" onClick={() => setSession(null)}>Close</button>
          )}
        </div>
      )}

      <div className="host-card">
        <h3>Add an account</h3>
        <form onSubmit={(e: FormEvent) => { e.preventDefault(); void act("create", async () => { const r = await post<{ credential: Credential }>("/api/credentials", { provider, kind, label, ...(kind === "api_key" ? { key } : {}) }); setLabel(""); setKey(""); if (kind !== "api_key") await connect(r.credential); }); }}>
          <select value={provider} onChange={(e) => setProvider(e.target.value as ProviderId)} aria-label="Provider">
            {providerIds.map((p) => <option key={p} value={p}>{providerLabels[p]}</option>)}
          </select>
          <select value={kind} onChange={(e) => setKind(e.target.value as Credential["kind"])} aria-label="Kind">
            <option value="personal">My subscription</option>
            {admin && <option value="team">Team account</option>}
            {admin && <option value="api_key">API key</option>}
          </select>
          <input value={label} onChange={(e) => setLabel(e.target.value)} required maxLength={80} placeholder="Label, e.g. My ChatGPT Pro" aria-label="Label" />
          {kind === "api_key" && <input type="password" autoComplete="off" value={key} onChange={(e) => setKey(e.target.value)} required placeholder="API key (stored, never shown again)" aria-label="API key" />}
          <button className="button small" type="submit" disabled={busy === "create"}>{kind === "api_key" ? "Save key" : "Add and connect"}</button>
        </form>
      </div>
    </section>
  );
}
