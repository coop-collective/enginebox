"use client";
import { FormEvent, useCallback, useEffect, useState } from "react";
import { api, errorMessage } from "@/lib/client";

type Role = "owner" | "admin" | "member";
type Member = { id: string; email: string; name: string; role: Role; instance_owner: boolean; last_sign_in_at: string | null };
type Invite = { id: string; email: string; role: Role; expires_at: string };
type OrgSummary = { id: string; name: string; kind: "platform" | "personal" | "team"; role: Role };
type View = {
  enabled: boolean;
  mode: "self_hosted" | "managed";
  me: { kind: "operator" | "member"; role: Role; email: string | null; label: string; org_id: string; org_kind?: string; instance_owner: boolean };
  org?: { id: string; name: string; kind: OrgSummary["kind"] };
  orgs?: OrgSummary[];
  needs_instance_owner?: boolean;
  email_transport?: "gateway" | "file" | "none";
  members?: Member[];
  invites?: Invite[];
};

const kindLabel = (kind: string) => (kind === "platform" ? "platform" : kind === "personal" ? "personal workspace" : "team");
const post = (path: string, body: unknown) => api(path, { method: "POST", body: JSON.stringify(body) });

/** Organization: switch orgs, manage members and invites, and (operator) name the instance owner. */
export function OrgPanel({ onSwitched }: { onSwitched: () => void }) {
  const [view, setView] = useState<View | null>(null);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState("");
  const [inviteEmail, setInviteEmail] = useState("");
  const [inviteRole, setInviteRole] = useState<Role>("member");
  const [importText, setImportText] = useState("");
  const [teamName, setTeamName] = useState("");
  const [claimEmail, setClaimEmail] = useState("");
  const [claim, setClaim] = useState<{ challenge: string; code: string } | null>(null);

  const refresh = useCallback(async () => {
    try {
      setView(await api<View>("/api/org"));
      setError("");
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);
  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function act(key: string, work: () => Promise<unknown>, done?: string) {
    setBusy(key);
    setError("");
    setNotice("");
    try {
      await work();
      if (done) setNotice(done);
      await refresh();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy("");
    }
  }

  if (!view) return <section className="contributor-panel">{error ? <div className="alert">{error}</div> : <span className="spinner" />}</section>;
  if (!view.enabled)
    return (
      <section className="contributor-panel" aria-label="Organization">
        <h2>Organization</h2>
        <p>Organizations and email sign-in are not enabled on this Engine Box. The host operator signs in with the dashboard password. See docs/SELF-HOSTING-ORGS.md to turn on orgs.</p>
      </section>
    );
  const admin = view.me.role === "owner" || view.me.role === "admin";
  const personal = view.org?.kind === "personal";

  return (
    <section className="contributor-panel" aria-label="Organization">
      <div className="contributor-heading">
        <div>
          <h2>{view.org?.name}</h2>
          <p>
            {kindLabel(view.org?.kind ?? "team")} · you are <strong>{view.me.role}</strong>
            {view.me.kind === "operator" ? " (host operator, break-glass session)" : view.me.instance_owner ? " · instance owner" : ""}
            {view.mode === "managed" ? " · managed install" : ""}
          </p>
        </div>
        {view.orgs && view.orgs.length > 1 && (
          <label>
            Switch organization{" "}
            <select
              value={view.org?.id}
              disabled={!!busy}
              onChange={(e) => act("switch", () => post("/api/org/switch", { org_id: e.target.value }).then(onSwitched))}
            >
              {view.orgs.map((o) => (
                <option key={o.id} value={o.id}>
                  {o.name} ({kindLabel(o.kind)})
                </option>
              ))}
            </select>
          </label>
        )}
      </div>
      {error && <div role="alert" className="alert">{error}</div>}
      {notice && <div role="status" className="notice">{notice}</div>}
      {view.email_transport === "none" && <div className="alert">Email sending is not configured: sign-in codes and invites cannot be delivered. See docs/SELF-HOSTING-ORGS.md.</div>}

      {view.me.kind === "operator" && view.needs_instance_owner && (
        <div className="host-card">
          <h3>Name the instance owner</h3>
          <p>You are signed in with the break-glass operator password. Verify the owner's email to create their account; they own the platform org and sign in by email from then on.</p>
          {!claim ? (
            <form onSubmit={(e: FormEvent) => { e.preventDefault(); void act("claim", async () => { const r = await api<{ challenge: string }>("/api/org/claim", { method: "POST", body: JSON.stringify({ email: claimEmail }) }); setClaim({ challenge: r.challenge, code: "" }); }); }}>
              <input type="email" value={claimEmail} onChange={(e) => setClaimEmail(e.target.value)} required placeholder="owner@example.com" aria-label="Owner email" />
              <button className="button small" disabled={busy === "claim"} type="submit">Send code</button>
            </form>
          ) : (
            <form onSubmit={(e: FormEvent) => { e.preventDefault(); void act("claim", () => post("/api/auth/email/verify", claim).then(onSwitched), "You are now signed in as the instance owner."); }}>
              <input inputMode="numeric" maxLength={6} value={claim.code} onChange={(e) => setClaim({ ...claim, code: e.target.value.replace(/\D/g, "") })} required placeholder="123456" aria-label="Verification code" />
              <button className="button small" disabled={claim.code.length !== 6 || busy === "claim"} type="submit">Verify</button>
            </form>
          )}
        </div>
      )}

      <div className="host-card">
        <h3>Members</h3>
        <table className="table">
          <thead><tr><th>Email</th><th>Role</th><th>Last sign-in</th>{admin && !personal && <th />}</tr></thead>
          <tbody>
            {(view.members ?? []).map((m) => (
              <tr key={m.id}>
                <td>{m.email}{m.instance_owner ? " · instance owner" : ""}</td>
                <td>
                  {admin && !personal ? (
                    <select value={m.role} disabled={!!busy} aria-label={`Role for ${m.email}`} onChange={(e) => act(`role:${m.id}`, () => post(`/api/org/members/${m.id}/role`, { role: e.target.value }))}>
                      <option value="member">member</option>
                      <option value="admin">admin</option>
                      <option value="owner">owner</option>
                    </select>
                  ) : m.role}
                </td>
                <td>{m.last_sign_in_at ? new Date(m.last_sign_in_at).toLocaleString() : "never"}</td>
                {admin && !personal && (
                  <td>
                    <button className="button small" disabled={!!busy} onClick={() => window.confirm(`Remove ${m.email} from ${view.org?.name}?`) && act(`remove:${m.id}`, () => post(`/api/org/members/${m.id}/remove`, {}))}>Remove</button>
                  </td>
                )}
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {admin && !personal && (
        <>
          <div className="host-card">
            <h3>Invite someone</h3>
            <form onSubmit={(e: FormEvent) => { e.preventDefault(); void act("invite", async () => { const r = await api<{ email_sent: boolean }>("/api/org/invites", { method: "POST", body: JSON.stringify({ email: inviteEmail, role: inviteRole }) }); setInviteEmail(""); if (!r.email_sent) throw new Error("The invite was created but its email could not be sent. Check email sending, then invite again."); }, "Invite sent. It works once and expires in 7 days."); }}>
              <input type="email" value={inviteEmail} onChange={(e) => setInviteEmail(e.target.value)} required placeholder="teammate@example.com" aria-label="Invite email" />
              <select value={inviteRole} onChange={(e) => setInviteRole(e.target.value as Role)} aria-label="Invite role">
                <option value="member">member</option>
                <option value="admin">admin</option>
                {view.me.role === "owner" && <option value="owner">owner</option>}
              </select>
              <button className="button small" disabled={busy === "invite"} type="submit">Send invite</button>
            </form>
            {(view.invites ?? []).length > 0 && (
              <ul>
                {view.invites!.map((i) => (
                  <li key={i.id}>
                    {i.email} · {i.role} · expires {new Date(i.expires_at).toLocaleDateString()}{" "}
                    <button className="button small" disabled={!!busy} onClick={() => act(`revoke:${i.id}`, () => post(`/api/org/invites/${i.id}/revoke`, {}))}>Revoke</button>
                  </li>
                ))}
              </ul>
            )}
          </div>
          <div className="host-card">
            <h3>Carry members over</h3>
            <p>Add people directly by email (one per line, optionally <code>email,role</code>), for example your Ideator team. They sign in with an emailed code; no invite is sent.</p>
            <form onSubmit={(e: FormEvent) => { e.preventDefault(); const members = importText.split(/\n+/).map((l) => l.trim()).filter(Boolean).map((l) => { const [email, role] = l.split(",").map((x) => x.trim()); return { email, ...(role ? { role } : {}) }; }); void act("import", async () => { const r = await api<{ added: number; created_accounts: number }>("/api/org/members/import", { method: "POST", body: JSON.stringify({ members }) }); setImportText(""); setNotice(`Added ${r.added} (${r.created_accounts} new accounts).`); }); }}>
              <textarea rows={4} value={importText} onChange={(e) => setImportText(e.target.value)} aria-label="Members to add" placeholder={"dev@example.com\nlead@example.com,admin"} />
              <button className="button small" disabled={busy === "import" || !importText.trim()} type="submit">Add members</button>
            </form>
          </div>
        </>
      )}

      {view.me.kind === "member" && (
        <div className="host-card">
          <h3>New team organization</h3>
          <p>Your personal workspace is yours alone. Create a team organization to work with others; it has its own pools, machines and repositories.</p>
          <form onSubmit={(e: FormEvent) => { e.preventDefault(); void act("create", async () => { await post("/api/org/create", { name: teamName }); setTeamName(""); }, "Organization created. Switch to it above."); }}>
            <input value={teamName} onChange={(e) => setTeamName(e.target.value)} required maxLength={80} placeholder="Studio name" aria-label="Organization name" />
            <button className="button small" disabled={busy === "create"} type="submit">Create</button>
          </form>
        </div>
      )}
    </section>
  );
}
