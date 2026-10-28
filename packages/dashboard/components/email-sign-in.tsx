"use client";
import { FormEvent, useState } from "react";
import { api, errorMessage } from "@/lib/client";

/**
 * Email sign-in: request a 6-digit code (a magic link arrives in the same
 * email), then enter it. The server answers the same way whether or not the
 * address has an account, so this form never says which.
 */
export function EmailSignIn({ onSignedIn }: { onSignedIn: () => void }) {
  const [email, setEmail] = useState("");
  const [code, setCode] = useState("");
  const [challenge, setChallenge] = useState("");
  const [minutes, setMinutes] = useState(10);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  async function request(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      const result = await api<{ challenge: string; expires_minutes: number }>("/api/auth/email", {
        method: "POST",
        body: JSON.stringify({ email }),
      });
      setChallenge(result.challenge);
      setMinutes(result.expires_minutes);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  }
  async function verify(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      await api("/api/auth/email/verify", { method: "POST", body: JSON.stringify({ challenge, code }) });
      setCode("");
      onSignedIn();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="email-sign-in">
      {error && (
        <div role="alert" className="alert">
          {error}
        </div>
      )}
      {!challenge ? (
        <form onSubmit={request}>
          <label>
            Email
            <input type="email" autoComplete="email" value={email} onChange={(e) => setEmail(e.target.value)} required autoFocus placeholder="you@example.com" />
          </label>
          <button className="button primary" disabled={busy} type="submit">
            {busy ? <span className="spinner" /> : "Email me a sign-in code"}
          </button>
        </form>
      ) : (
        <form onSubmit={verify}>
          <p>
            If <strong>{email}</strong> has an Engine Box account, a 6-digit code and a sign-in link are on their way. They work once and expire in {minutes} minutes.
          </p>
          <label>
            Sign-in code
            <input
              inputMode="numeric"
              autoComplete="one-time-code"
              pattern="[0-9]{6}"
              maxLength={6}
              value={code}
              onChange={(e) => setCode(e.target.value.replace(/\D/g, ""))}
              required
              autoFocus
              placeholder="123456"
            />
          </label>
          <button className="button primary" disabled={busy || code.length !== 6} type="submit">
            {busy ? <span className="spinner" /> : "Sign in"}
          </button>
          <button
            className="button small"
            type="button"
            onClick={() => {
              setChallenge("");
              setCode("");
            }}
          >
            Use a different email
          </button>
        </form>
      )}
    </div>
  );
}
