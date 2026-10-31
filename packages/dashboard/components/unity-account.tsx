"use client";
import { FormEvent, useState } from "react";
import { api, errorMessage } from "@/lib/client";

export type UnityAccount = {
  id: string;
  username: string;
  status: "connected" | "unverified";
  last_error: string | null;
  validated_at: string | null;
};

const reasons: Record<string, string> = {
  unreachable: "Unity could not be reached",
  service_error: "Unity's sign-in service had an error",
  unexpected_response: "Unity gave an answer Engine Box did not recognize",
};
const when = (at: string | null) => (at ? new Date(at).toLocaleString() : "");

/**
 * The signed-in member's own Unity account: one email and password per
 * member, checked with Unity when saved, updatable at any time. The password
 * is sent once and never shown again.
 */
export function UnityAccountCard({ account, signedIn, refresh }: { account: UnityAccount | null; signedIn: boolean; refresh: () => Promise<void> }) {
  const [editing, setEditing] = useState(false);
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState<"" | "save" | "remove">("");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");

  const open = !account || editing;
  async function save(e: FormEvent) {
    e.preventDefault();
    setBusy("save");
    setError("");
    setNotice("");
    try {
      const body = JSON.stringify({ provider: "unity", kind: "personal", username, password });
      const r = account
        ? await api<{ credential: UnityAccount }>(`/api/credentials/${account.id}/update`, { method: "POST", body: JSON.stringify({ username, password }) })
        : await api<{ credential: UnityAccount }>("/api/credentials", { method: "POST", body });
      setNotice(r.credential.status === "connected" ? "Connected ✓" : `Saved, but not verified: ${reasons[r.credential.last_error ?? ""] ?? "Unity could not be asked"}. Engine Box checks it again when a session uses it.`);
      setPassword("");
      setEditing(false);
      await refresh();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy("");
    }
  }
  async function remove() {
    if (!account || !window.confirm("Remove your Unity account? Its saved password is deleted.")) return;
    setBusy("remove");
    setError("");
    setNotice("");
    try {
      await api(`/api/credentials/${account.id}/delete`, { method: "POST", body: "{}" });
      await refresh();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy("");
    }
  }

  return (
    <div className="host-card" aria-label="Unity account">
      <h3>Unity account</h3>
      <p>Sessions you start use your own Unity license, never anyone else's.</p>
      {!signedIn ? (
        <p>Sign in with your email to save your Unity account.</p>
      ) : (
        <>
          {account && (
            <p aria-live="polite">
              <strong>{account.username}</strong> ·{" "}
              {account.status === "connected"
                ? `Connected ✓ (checked ${when(account.validated_at)})`
                : `Saved but not verified: ${reasons[account.last_error ?? ""] ?? "Unity could not be asked"}`}
            </p>
          )}
          {error && <div role="alert" className="alert">{error}</div>}
          {notice && !error && <p role="status">{notice}</p>}
          {open ? (
            <form onSubmit={save}>
              <input type="email" autoComplete="username" value={username} onChange={(e) => setUsername(e.target.value)} required maxLength={254} placeholder="Unity ID email" aria-label="Unity ID email" />
              <input type="password" autoComplete="current-password" value={password} onChange={(e) => setPassword(e.target.value)} required maxLength={1024} placeholder="Unity password" aria-label="Unity password" />
              <button className="button small" type="submit" disabled={!!busy}>{busy === "save" ? "Validating…" : "Save"}</button>
              {account && <button className="button small" type="button" disabled={!!busy} onClick={() => { setEditing(false); setPassword(""); setError(""); }}>Cancel</button>}
            </form>
          ) : (
            <div>
              <button className="button small" disabled={!!busy} onClick={() => { setUsername(account?.username ?? ""); setNotice(""); setEditing(true); }}>Update</button>
              <button className="button small" disabled={!!busy} onClick={remove}>{busy === "remove" ? "Removing…" : "Remove"}</button>
            </div>
          )}
          <p>Turn off two-factor sign-in for this Unity account: Engine Box signs in with the password and cannot answer a code.</p>
        </>
      )}
    </div>
  );
}
