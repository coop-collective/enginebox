// Writes the private deploy configuration .state/email-gateway/wrangler.json.
//   ENGINEBOX_EMAIL_ACCOUNT_ID     Cloudflare account ID (32 hex)                      required
//   ENGINEBOX_EMAIL_FROM           sender on a domain onboarded to Email Sending        required
//                                  (for example login@coopcollective.dev)
//   ENGINEBOX_DASHBOARD_ORIGIN     the dashboard's exact public origin                  required
//                                  (links in emails must start with it)
//   ENGINEBOX_EMAIL_FROM_NAME      display name (default "Engine Box")
//   ENGINEBOX_EMAIL_WORKER_NAME    Worker name (default "enginebox-email-gateway")
// The gateway has no hostname, workers.dev or preview URL: only the site
// Worker reaches it, through a service binding (entrypoint EmailGateway).
// Set its secret afterwards with `pnpm secret` (EMAIL_GATEWAY_SECRET, the same
// value as the dashboard's ENGINEBOX_EMAIL_SECRET). Nothing secret is written here.
import { mkdir, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const env = process.env;
const account = env.ENGINEBOX_EMAIL_ACCOUNT_ID ?? "";
const from = (env.ENGINEBOX_EMAIL_FROM ?? "").trim().toLowerCase();
const fromName = (env.ENGINEBOX_EMAIL_FROM_NAME ?? "Engine Box").trim();
const origin = env.ENGINEBOX_DASHBOARD_ORIGIN ?? "";
const name = env.ENGINEBOX_EMAIL_WORKER_NAME ?? "enginebox-email-gateway";
let originOk = false;
try {
  const url = new URL(origin);
  originOk = url.origin === origin && (url.protocol === "https:" || ["127.0.0.1", "localhost"].includes(url.hostname));
} catch {}
if (!/^[a-f0-9]{32}$/.test(account) || !/^[^\s@]{1,64}@[a-z0-9.-]{3,253}$/.test(from) || !originOk ||
    !/^[a-z0-9][a-z0-9-]{0,62}$/.test(name) || !fromName || fromName.length > 80) {
  throw new Error("Set ENGINEBOX_EMAIL_ACCOUNT_ID, ENGINEBOX_EMAIL_FROM and an exact HTTPS ENGINEBOX_DASHBOARD_ORIGIN.");
}
const directory = new URL("../../.state/email-gateway/", import.meta.url);
await mkdir(directory, { recursive: true, mode: 0o700 });
await writeFile(new URL("wrangler.json", directory), JSON.stringify({
  name,
  account_id: account,
  main: fileURLToPath(new URL("src/index.mjs", import.meta.url)),
  compatibility_date: "2026-10-23",
  workers_dev: false,
  preview_urls: false,
  observability: { enabled: true },
  send_email: [{ name: "EMAIL", allowed_sender_addresses: [from] }],
  vars: { FROM_ADDRESS: from, FROM_NAME: fromName, DASHBOARD_ORIGIN: origin },
  durable_objects: { bindings: [{ name: "REPLAY", class_name: "ReplayGuard" }] },
  migrations: [{ tag: "v1", new_sqlite_classes: ["ReplayGuard"] }],
}, null, 2) + "\n", { mode: 0o600 });
console.log(`Wrote private .state/email-gateway/wrangler.json (sender ${from}).`);
