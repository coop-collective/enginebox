import { createHmac, timingSafeEqual } from "node:crypto";
import { bodyJson, config, HttpError, json, requireOrigin, requireRole } from "./server";

const flowName = "enginebox_github_flow";
function seal(value: string) { return createHmac("sha256", config().secret).update(`github:${value}`).digest("hex"); }
export function flowCookie(state: string, now = Date.now()) {
  const value = `${state}.${now + 600_000}`;
  return `${flowName}=${value}.${seal(value)}; HttpOnly; Path=/api/github; SameSite=Lax; Max-Age=600${config().origin.startsWith("https:") ? "; Secure" : ""}`;
}
export function validFlow(request: Request, state: string, now = Date.now()) {
  const value = request.headers.get("cookie")?.split(";").map(v => v.trim()).find(v => v.startsWith(`${flowName}=`))?.slice(flowName.length + 1);
  if (!value || value.length > 256) return false;
  const [expected, expiry, signature, extra] = value.split(".");
  if (extra || expected !== state || !/^[a-zA-Z0-9_-]{43}$/.test(expected) || !/^[a-f0-9]{64}$/.test(signature ?? "")) return false;
  if (!(Number(expiry) > now && Number(expiry) <= now + 600_000)) return false;
  return timingSafeEqual(Buffer.from(signature), Buffer.from(seal(`${expected}.${expiry}`)));
}
async function controller(path: string, body: unknown) {
  const { apiUrl, apiToken } = config();
  const response = await fetch(`${apiUrl}/v1/github/${path}`, {
    method: "POST", headers: { Authorization: `Bearer ${apiToken}`, "Content-Type": "application/json" },
    body: JSON.stringify(body), redirect: "error", signal: AbortSignal.timeout(30_000), cache: "no-store",
  });
  if (!response.ok) throw new HttpError(502, "GitHub connection failed. Check the App configuration and reconnect.");
  return response.json();
}
export async function connectGitHub(request: Request) {
  requireRole(request, "admin"); requireOrigin(request);
  const { kind } = await bodyJson(request);
  if (!["repos", "runners"].includes(kind)) throw new HttpError(400, "Unknown GitHub connection.");
  const result = await controller(`${kind}/connect`, {});
  const url = new URL(result.url);
  if (url.origin !== "https://github.com" || url.pathname !== "/login/oauth/authorize" || !/^[a-zA-Z0-9_-]{43}$/.test(result.state)) throw new HttpError(502, "Invalid GitHub authorization response.");
  return json({ url: url.href }, 200, { "Set-Cookie": flowCookie(result.state) });
}
export async function githubCallback(request: Request) {
  const url = new URL(request.url), state = url.searchParams.get("state") ?? "", code = url.searchParams.get("code");
  // A temporary signed Lax cookie survives the cross-site OAuth redirect. The
  // ordinary dashboard session stays Strict; callback cannot originate a flow.
  if (!validFlow(request, state)) throw new HttpError(403, "GitHub authorization did not start in this browser or has expired.");
  if (!code || url.searchParams.has("error")) throw new HttpError(400, "GitHub authorization was not completed.");
  await controller("callback", { state, code });
  return new Response(null, { status: 303, headers: {
    Location: `${config().origin}/?panel=github`, "Cache-Control": "no-store", "Referrer-Policy": "no-referrer",
    "Set-Cookie": `${flowName}=; HttpOnly; Path=/api/github; SameSite=Lax; Max-Age=0${config().origin.startsWith("https:") ? "; Secure" : ""}`,
  } });
}
export async function githubWebhook(request: Request, kind: string) {
  if (!["repos", "runners"].includes(kind) || new URL(request.url).search || request.headers.has("origin")) throw new HttpError(404, "Unknown webhook.");
  const reader = request.body?.getReader();
  if (!reader) throw new HttpError(400, "Missing webhook body.");
  const chunks: Uint8Array[] = []; let length = 0;
  while (true) { const { value, done } = await reader.read(); if (done) break; length += value.length; if (length > 1024 * 1024) { await reader.cancel(); throw new HttpError(413, "Webhook too large."); } chunks.push(value); }
  const { apiUrl, apiToken } = config();
  const response = await fetch(`${apiUrl}/v1/github/webhook/${kind}`, { method: "POST", body: Buffer.concat(chunks), headers: {
    // Admit the server-to-server request through the supervisor. The controller
    // still validates GitHub's signature before handling this webhook.
    Authorization: `Bearer ${apiToken}`,
    "Content-Type": "application/json", "X-Hub-Signature-256": request.headers.get("x-hub-signature-256") ?? "", "X-GitHub-Event": request.headers.get("x-github-event") ?? "",
  }, redirect: "error", signal: AbortSignal.timeout(30_000), cache: "no-store" });
  return json(await response.json(), response.status);
}
