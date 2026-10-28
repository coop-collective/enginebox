import { createHash, createHmac, randomBytes } from "node:crypto";
import { appendFileSync, chmodSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { requireIdentity } from "./store";

/**
 * Transactional email for sign-in codes, magic links and invites.
 *
 * The dashboard holds no Cloudflare credential. It posts a signed, fixed-shape
 * message to the email gateway Worker (packages/email-gateway) through the
 * site Worker's `/_email/send` route; the gateway verifies the HMAC, renders
 * one of its fixed templates and sends with its Email Sending binding, which is
 * restricted to one sender address. A leaked dashboard secret can therefore
 * only send Engine Box sign-in/invite mail whose links point at the configured
 * dashboard origin, never arbitrary content.
 *
 *   ENGINEBOX_EMAIL_ENDPOINT   https://<site>/_email/send (loopback http allowed for development)
 *   ENGINEBOX_EMAIL_SECRET     shared HMAC secret (≥32 chars), the gateway's EMAIL_GATEWAY_SECRET
 *   ENGINEBOX_EMAIL_TRANSPORT  `file` writes messages to <identity dir>/outbox/outbox.jsonl
 *                              (0600) instead: local development and tests only.
 *
 * Request headers match the Artifacts gateway's scheme:
 *   X-EngineBox-Timestamp, X-EngineBox-Nonce (32 hex), X-EngineBox-Signature: v1=<hex>
 * over "v1\nPOST\n/send\n<timestamp>\n<nonce>\n<hex sha256(body)>".
 */
export type Message =
  | { template: "sign_in" | "claim_owner"; to: string; code: string; link: string; org_name: string; expires_minutes: number }
  | { template: "invite"; to: string; link: string; org_name: string; role: string; expires_days: number };

export type Transport = "gateway" | "file" | "none";
export function transport(): Transport {
  if (process.env.ENGINEBOX_EMAIL_TRANSPORT === "file") return "file";
  return process.env.ENGINEBOX_EMAIL_ENDPOINT && (process.env.ENGINEBOX_EMAIL_SECRET ?? "").length >= 32 ? "gateway" : "none";
}

export function signRequest(secret: string, body: string, timestamp: string, nonce: string) {
  const bodyHash = createHash("sha256").update(body).digest("hex");
  return createHmac("sha256", secret).update(`v1\nPOST\n/send\n${timestamp}\n${nonce}\n${bodyHash}`).digest("hex");
}

function endpoint(): URL | null {
  try {
    const url = new URL(process.env.ENGINEBOX_EMAIL_ENDPOINT ?? "");
    const loopback = ["127.0.0.1", "localhost", "[::1]"].includes(url.hostname);
    if ((url.protocol === "https:" || (url.protocol === "http:" && loopback)) && !url.username && !url.password && !url.hash) return url;
  } catch {}
  return null;
}

/** Hook for tests: replaces the network send. */
let fetcher: typeof fetch = (...args) => fetch(...args);
export function setMailFetch(next: typeof fetch) {
  fetcher = next;
}

/**
 * Send one message. Resolves true when the gateway (or the file outbox)
 * accepted it. Never throws and never logs the code, link or address.
 */
export async function send(message: Message): Promise<boolean> {
  const mode = transport();
  if (mode === "file") {
    try {
      const dir = join(requireIdentity(), "outbox");
      mkdirSync(dir, { recursive: true, mode: 0o700 });
      chmodSync(dir, 0o700);
      appendFileSync(join(dir, "outbox.jsonl"), `${JSON.stringify({ ...message, at: Date.now() })}\n`, { mode: 0o600 });
      return true;
    } catch {
      console.error(JSON.stringify({ event: "identity_email_outbox_failed", template: message.template }));
      return false;
    }
  }
  const url = endpoint();
  const secret = process.env.ENGINEBOX_EMAIL_SECRET ?? "";
  if (mode === "none" || !url) {
    console.error(JSON.stringify({ event: "identity_email_not_configured", template: message.template }));
    return false;
  }
  const body = JSON.stringify(message);
  const timestamp = String(Math.floor(Date.now() / 1000));
  const nonce = randomBytes(16).toString("hex");
  try {
    const response = await fetcher(url, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-EngineBox-Timestamp": timestamp,
        "X-EngineBox-Nonce": nonce,
        "X-EngineBox-Signature": `v1=${signRequest(secret, body, timestamp, nonce)}`,
      },
      body,
      redirect: "error",
      cache: "no-store",
      signal: AbortSignal.timeout(15_000),
    });
    if (!response.ok) console.error(JSON.stringify({ event: "identity_email_rejected", template: message.template, status: response.status }));
    return response.ok;
  } catch {
    console.error(JSON.stringify({ event: "identity_email_unavailable", template: message.template }));
    return false;
  }
}
