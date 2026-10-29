// Engine Box email gateway core: verifies a signed envelope from the dashboard
// and sends one of three fixed transactional emails (sign-in code + magic
// link, owner verification code, org invite) with the Cloudflare Email
// Sending binding. The Worker (src/index.mjs) exposes it only over RPC to the
// site Worker, which forwards the dashboard's request unchanged and holds no
// secret. No Cloudflare API token exists anywhere.
//
// Authentication matches the Artifacts gateway (packages/artifacts-gateway):
//   X-EngineBox-Timestamp: <unix seconds>
//   X-EngineBox-Nonce:     <32 lowercase hex>
//   X-EngineBox-Signature: v1=<hex HMAC-SHA256(EMAIL_GATEWAY_SECRET, canonical)>
// where canonical = "v1\nPOST\n/send\n" + timestamp + "\n" + nonce + "\n" +
// hex(SHA-256(body)). ±300 s window; each nonce is accepted once.
//
// Abuse resistance: the gateway renders the message itself from validated
// fields. Codes must be 6 digits; links must start with the configured
// DASHBOARD_ORIGIN's sign-in or invite paths; the sender is fixed (and the
// binding is restricted to it). A leaked secret can only send Engine Box
// sign-in mail pointing at this dashboard. Addresses, codes and links are
// never logged.

export const WINDOW_SECONDS = 300;
export const MAX_BODY = 4096;
const NONCE = /^[a-f0-9]{32}$/;
const EMAIL = /^[^\s@<>()",;:\\[\]]{1,64}@[A-Za-z0-9](?:[A-Za-z0-9-]{0,62}[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,62}[A-Za-z0-9])?)+$/;
const encoder = new TextEncoder();

export const reply = (body, status = 200) => ({ status, body });
const unauthorized = () => reply({ error: "unauthorized" }, 401);
const invalid = () => reply({ error: "invalid_input" }, 400);

function hex(buffer) {
  return [...new Uint8Array(buffer)].map((b) => b.toString(16).padStart(2, "0")).join("");
}
export async function sign(secret, timestamp, nonce, body) {
  const key = await crypto.subtle.importKey("raw", encoder.encode(secret), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  const bodyHash = hex(await crypto.subtle.digest("SHA-256", encoder.encode(body)));
  return hex(await crypto.subtle.sign("HMAC", key, encoder.encode(`v1\nPOST\n/send\n${timestamp}\n${nonce}\n${bodyHash}`)));
}
export function equalHex(a, b) {
  if (typeof a !== "string" || typeof b !== "string" || a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

async function verify(envelope, env, now) {
  const secret = env.EMAIL_GATEWAY_SECRET;
  if (typeof secret !== "string" || secret.length < 32) return false;
  const { timestamp, nonce, signature, body } = envelope;
  if (typeof timestamp !== "string" || typeof nonce !== "string" || typeof signature !== "string") return false;
  if (!/^\d{1,12}$/.test(timestamp) || !NONCE.test(nonce) || !signature.startsWith("v1=")) return false;
  if (Math.abs(now - Number(timestamp)) > WINDOW_SECONDS) return false;
  if (!equalHex(await sign(secret, timestamp, nonce, body), signature.slice(3))) return false;
  const stub = env.REPLAY.get(env.REPLAY.idFromName("nonces"));
  const response = await stub.fetch("https://replay.invalid/claim", {
    method: "POST",
    body: JSON.stringify({ nonce, expires: Number(timestamp) + WINDOW_SECONDS * 2 }),
  });
  return response.status === 200;
}

const escape = (value) => String(value).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
/** Org names and roles come from the dashboard; keep them short single-line text. */
const label = (value, max = 80) => typeof value === "string" && value.length >= 1 && value.length <= max && !/[\u0000-\u001f\u007f<>]/.test(value);

function linkAllowed(link, origin, path) {
  if (typeof link !== "string" || link.length > 1024 || typeof origin !== "string") return false;
  try {
    const url = new URL(link);
    return url.origin === origin && url.pathname === path && !url.username && !url.password && !url.hash;
  } catch {
    return false;
  }
}

function layout(title, lines, button) {
  const html =
    `<!doctype html><html><body style="font:15px/1.5 system-ui,-apple-system,'Segoe UI',sans-serif;color:#14181c;background:#f7f9fa;margin:0;padding:24px">` +
    `<div style="max-width:520px;margin:0 auto;background:#fff;border:1px solid #d8dee3;border-radius:12px;padding:24px">` +
    `<h1 style="font-size:20px;margin:0 0 12px">${escape(title)}</h1>` +
    lines.map((l) => `<p style="margin:8px 0">${l}</p>`).join("") +
    (button ? `<p style="margin:20px 0"><a href="${escape(button.href)}" style="background:#0b6bcb;color:#fff;padding:10px 18px;border-radius:6px;text-decoration:none">${escape(button.text)}</a></p>` : "") +
    `<p style="margin:16px 0 0;color:#5b6670;font-size:13px">If you did not expect this email, you can ignore it.</p></div></body></html>`;
  return html;
}

/** Validate a message and render it, or return null. */
export function render(message, env) {
  if (!message || typeof message !== "object" || Array.isArray(message)) return null;
  const to = typeof message.to === "string" ? message.to.trim().toLowerCase() : "";
  if (to.length > 254 || !EMAIL.test(to) || !label(message.org_name)) return null;
  const origin = env.DASHBOARD_ORIGIN;
  const org = message.org_name;
  if (message.template === "sign_in" || message.template === "claim_owner") {
    const minutes = message.expires_minutes;
    if (typeof message.code !== "string" || !/^\d{6}$/.test(message.code) || !Number.isInteger(minutes) || minutes < 1 || minutes > 60) return null;
    if (message.template === "sign_in") {
      if (!linkAllowed(message.link, origin, "/api/auth/link")) return null;
      return {
        to,
        subject: `Your Engine Box sign-in code: ${message.code}`,
        text: `Your Engine Box sign-in code for ${org} is ${message.code}.\n\nOr open this link to sign in: ${message.link}\n\nThe code and link work once and expire in ${minutes} minutes. If you did not try to sign in, ignore this email.`,
        html: layout("Sign in to Engine Box", [`Your sign-in code for <strong>${escape(org)}</strong>:`, `<span style="font-size:28px;letter-spacing:4px;font-weight:600">${message.code}</span>`, `The code and link work once and expire in ${minutes} minutes.`], { href: message.link, text: "Sign in" }),
      };
    }
    if (message.link !== origin) return null;
    return {
      to,
      subject: `Confirm the Engine Box owner: ${message.code}`,
      text: `The host operator of ${org} is making this address the Engine Box instance owner. Your verification code is ${message.code}. It expires in ${minutes} minutes. If you did not expect this, ignore this email.`,
      html: layout("Confirm the Engine Box owner", [`The host operator of <strong>${escape(org)}</strong> is making this address the instance owner.`, `<span style="font-size:28px;letter-spacing:4px;font-weight:600">${message.code}</span>`, `The code expires in ${minutes} minutes.`]),
    };
  }
  if (message.template === "invite") {
    const days = message.expires_days;
    if (!linkAllowed(message.link, origin, "/api/auth/invite") || !["owner", "admin", "member"].includes(message.role) || !Number.isInteger(days) || days < 1 || days > 30) return null;
    return {
      to,
      subject: `You're invited to ${org} on Engine Box`,
      text: `You have been invited to join ${org} on Engine Box as ${message.role}.\n\nAccept the invite: ${message.link}\n\nThe invite works once and expires in ${days} days.`,
      html: layout(`Join ${org} on Engine Box`, [`You have been invited to join <strong>${escape(org)}</strong> as <strong>${escape(message.role)}</strong>.`, `The invite works once and expires in ${days} days.`], { href: message.link, text: "Accept invite" }),
    };
  }
  return null;
}

/**
 * Verify and send one envelope `{timestamp, nonce, signature, body}` (body:
 * the exact request text). Returns `{status, body}`. `now` is injectable.
 */
export async function send(envelope, env, now = Math.floor(Date.now() / 1000)) {
  if (!envelope || typeof envelope !== "object" || typeof envelope.body !== "string") return invalid();
  if (encoder.encode(envelope.body).byteLength > MAX_BODY) return reply({ error: "too_large" }, 413);
  if (!(await verify(envelope, env, now))) return unauthorized();
  let message;
  try {
    message = JSON.parse(envelope.body);
  } catch {
    return invalid();
  }
  const rendered = render(message, env);
  if (!rendered) return invalid();
  if (typeof env.FROM_ADDRESS !== "string" || !EMAIL.test(env.FROM_ADDRESS) || !env.EMAIL) return reply({ error: "not_configured" }, 503);
  try {
    await env.EMAIL.send({
      to: rendered.to,
      from: { email: env.FROM_ADDRESS, name: typeof env.FROM_NAME === "string" && label(env.FROM_NAME) ? env.FROM_NAME : "Engine Box" },
      subject: rendered.subject,
      text: rendered.text,
      html: rendered.html,
    });
  } catch (error) {
    const code = typeof error?.code === "string" ? error.code : "unknown";
    // Template and error code only: never the address, code or link.
    console.error(JSON.stringify({ event: "email_gateway_failed", template: message.template, code }));
    return reply({ error: code === "E_RATE_LIMIT_EXCEEDED" || code === "E_DAILY_LIMIT_EXCEEDED" ? "rate_limited" : "send_failed" }, code.includes("RATE") || code.includes("DAILY") ? 429 : 502);
  }
  console.log(JSON.stringify({ event: "email_gateway_sent", template: message.template }));
  return reply({ sent: true });
}

/** Remembers each accepted nonce until its window has passed (same as the Artifacts gateway). */
export class ReplayGuard {
  constructor(ctx) {
    this.ctx = ctx;
  }
  async fetch(request) {
    const { nonce, expires } = await request.json();
    if (typeof nonce !== "string" || !NONCE.test(nonce) || !Number.isInteger(expires)) return new Response(null, { status: 400 });
    const key = `n:${nonce}`;
    if ((await this.ctx.storage.get(key)) !== undefined) return new Response(null, { status: 409 });
    await this.ctx.storage.put(key, expires);
    if ((await this.ctx.storage.getAlarm()) === null) await this.ctx.storage.setAlarm(Date.now() + WINDOW_SECONDS * 2000);
    return new Response(null, { status: 200 });
  }
  async alarm() {
    const now = Math.floor(Date.now() / 1000);
    const entries = await this.ctx.storage.list({ prefix: "n:" });
    const stale = [...entries].filter(([, expires]) => expires <= now).map(([key]) => key);
    for (let i = 0; i < stale.length; i += 128) await this.ctx.storage.delete(stale.slice(i, i + 128));
    if (entries.size > stale.length) await this.ctx.storage.setAlarm(Date.now() + WINDOW_SECONDS * 2000);
  }
}
