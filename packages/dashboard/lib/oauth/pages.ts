import type { Pool, Workspace } from "./controller";
import type { UserAuthenticator } from "./identity";

const escape = (value: string) =>
  value.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);

const style = `
:root{color-scheme:light dark;--bg:#f7f9fa;--card:#fff;--text:#14181c;--muted:#5b6670;--line:#d8dee3;--accent:#0b6bcb;--danger:#b42318}
@media (prefers-color-scheme:dark){:root{--bg:#0f1316;--card:#171c20;--text:#e8edf1;--muted:#9aa6b0;--line:#2a3238;--accent:#5aa9ff;--danger:#ff8a80}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--text);font:15px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif}
main{max-width:560px;margin:48px auto;padding:0 16px}.card{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:24px}
h1{font-size:20px;margin:0 0 4px}p{margin:8px 0}.muted{color:var(--muted)}.warn{color:var(--danger)}code{font-size:13px}
fieldset{border:1px solid var(--line);border-radius:8px;margin:16px 0;padding:8px 12px}legend{padding:0 4px;color:var(--muted)}
.ws{display:block;padding:8px 0;border-bottom:1px solid var(--line)}.ws:last-child{border-bottom:0}
label.field{display:block;margin:12px 0}input[type=text],input[type=password]{width:100%;padding:8px;border:1px solid var(--line);border-radius:6px;background:transparent;color:inherit;font:inherit}
.profiles{margin:6px 0 0 24px}.profiles input{font-size:13px}
.actions{display:flex;gap:8px;justify-content:flex-end;margin-top:16px}
button{font:inherit;padding:8px 16px;border-radius:6px;border:1px solid var(--line);background:transparent;color:inherit;cursor:pointer}
button.primary{background:var(--accent);border-color:var(--accent);color:#fff}`;

export function page(title: string, body: string, opts: { formTarget?: string; refresh?: string } = {}) {
  const formAction = ["'self'", opts.formTarget].filter(Boolean).join(" ");
  return new Response(
    `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">` +
      `<meta name="referrer" content="same-origin">${opts.refresh ? `<meta http-equiv="refresh" content="0;url=${escape(opts.refresh)}">` : ""}` +
      `<title>${escape(title)}</title><style>${style}</style></head><body><main><div class="card">${body}</div></main></body></html>`,
    {
      status: 200,
      headers: {
        "Content-Type": "text/html; charset=utf-8",
        "Cache-Control": "no-store",
        "X-Frame-Options": "DENY",
        "X-Content-Type-Options": "nosniff",
        // same-origin (not no-referrer) so browsers send Origin on the consent form post.
        "Referrer-Policy": "same-origin",
        "Content-Security-Policy": `default-src 'none'; style-src 'unsafe-inline'; form-action ${formAction}; frame-ancestors 'none'; base-uri 'none'`,
      },
    },
  );
}

export function errorPage(message: string, status = 400) {
  const response = page("Authorization failed", `<h1>This authorization request cannot continue</h1><p>${escape(message)}</p><p class="muted">Return to the app that sent you here and try connecting again.</p>`);
  return new Response(response.body, { status, headers: response.headers });
}

export function signInPage(users: UserAuthenticator, request: string, clientName: string, error?: string, continueUrl?: string) {
  const fields = users.signInFields
    .map((f) => `<label class="field">${escape(f.label)}<input type="${f.type}" name="${escape(f.name)}" required autocomplete="${f.type === "password" ? "current-password" : "username"}"></label>`)
    .join("");
  return page(
    "Sign in to Engine Box",
    `<h1>Sign in to Engine Box</h1><p class="muted">“${escape(clientName)}” is asking to connect. Sign in to review what it can access.</p>` +
      (error ? `<p class="warn" role="alert">${escape(error)}</p>` : "") +
      `<form method="post" action="/oauth/authorize"><input type="hidden" name="request" value="${escape(request)}"><input type="hidden" name="action" value="sign_in">${fields}` +
      `<div class="actions"><button class="primary" type="submit">Sign in</button></div></form>` +
      (continueUrl ? `<p class="muted">Already signed in on this browser? <a href="${escape(continueUrl)}">Continue</a>.</p>` : ""),
  );
}

export function consentPage(opts: {
  request: string;
  clientName: string;
  clientId: string;
  redirectUri: string;
  workspaces: Workspace[];
  pools?: Pool[];
  /** The app asked for this project's pools only. */
  project?: string | null;
  error?: string;
}) {
  const redirect = new URL(opts.redirectUri);
  const pools = opts.pools ?? [];
  const list = opts.workspaces.length
    ? opts.workspaces
        .map((w) => {
          const value = `${w.host}/${w.instance}`;
          return `<label class="ws"><input type="checkbox" name="workspace" value="${escape(value)}"> <strong>${escape(w.label)}</strong> <span class="muted">on ${escape(w.host_label)} · <code>${escape(value)}</code></span>` +
            `<div class="profiles"><input type="text" name="profiles:${escape(value)}" maxlength="400" placeholder="Profiles (optional, comma-separated; empty means none named)" aria-label="Profiles for ${escape(value)}"></div></label>`;
        })
        .join("")
    : `<p class="muted">There are no assigned workspaces to share yet. Create one in the dashboard first.</p>`;
  const poolRow = (p: Pool) =>
    `<label class="ws"><input type="checkbox" name="pool" value="${escape(p.name)}"> <strong>${escape(p.name)}</strong> <span class="muted">${p.members} workspace${p.members === 1 ? "" : "s"}` +
    `${p.profile ? ` · runs use profile <code>${escape(p.profile)}</code>` : ""}${p.description ? ` · ${escape(p.description)}` : ""}</span></label>`;
  // Pools are grouped by the project they belong to.
  const projects = [...new Set(pools.map((p) => p.project))];
  const poolList = projects
    .map(
      (project) =>
        `<div class="project"><p class="muted">Project <code>${escape(project)}</code></p>${pools
          .filter((p) => p.project === project)
          .map(poolRow)
          .join("")}</div>`,
    )
    .join("");
  const poolLegend = opts.project
    ? `Pools in project <code>${escape(opts.project)}</code> it may run agents on (the app asked for this project only; Engine Box picks a free workspace)`
    : "Pools it may run agents on, by project (Engine Box picks a free workspace)";
  const any = opts.workspaces.length > 0 || pools.length > 0;
  return page(
    "Connect an app to Engine Box",
    `<h1>Connect “${escape(opts.clientName)}”?</h1>` +
      `<p>It will be sent back to <strong>${escape(redirect.host)}</strong>${redirect.protocol === "http:" ? " (on this computer)" : ""}.</p>` +
      `<p class="muted">The app chose its own name; check that you started this connection. Client ID <code>${escape(opts.clientId)}</code>.</p>` +
      (opts.error ? `<p class="warn" role="alert">${escape(opts.error)}</p>` : "") +
      `<form method="post" action="/oauth/authorize"><input type="hidden" name="request" value="${escape(opts.request)}">` +
      (pools.length ? `<fieldset><legend>${poolLegend}</legend>${poolList}</fieldset>` : "") +
      `<fieldset><legend>Workspaces it may run agents on</legend>${list}</fieldset>` +
      `<p class="muted">It can start, read, list and cancel agent runs in the workspaces and pools you select, and nothing else: no fleet, template, storage, provider sign-in or account settings. You can disconnect it at any time from the dashboard.</p>` +
      `<div class="actions"><button type="submit" name="action" value="deny">Deny</button><button class="primary" type="submit" name="action" value="approve"${any ? "" : " disabled"}>Allow access</button></div></form>`,
    { formTarget: redirect.origin },
  );
}
