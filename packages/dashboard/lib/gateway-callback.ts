import { config } from "./server";
import { errorPage } from "./oauth/pages";

/**
 * The browser leg of a principal's Xava Inference connection (docs/ENGINE-SESSIONS.md, "Agent forks", gateway connect):
 * xinf sends the browser to `GET /v1/gateway/xinf/callback?state=…&code=…` (or `&error=…`). No session or
 * bearer is needed: `state` is the authenticator, and the controller checks it and the code. The dashboard
 * forwards both with its owner credentials (no actor or org header) and sends the browser on to the
 * `return_to` the controller recorded at connect, which must be an absolute https URL.
 */
const STATE = /^[A-Za-z0-9_-]{1,128}$/;
const CODE = /^[\x20-\x7e]{1,512}$/;
const ERROR = /^[a-z_]{1,64}$/;

export async function xinfCallback(request: Request) {
  const params = new URL(request.url).searchParams;
  const keys = [...params.keys()];
  if (new Set(keys).size !== keys.length) return errorPage("The connection callback repeated a parameter.");
  // `iss` and `error_description` are ignored: the controller knows the issuer, and the description is the provider's text.
  const state = params.get("state");
  const code = params.get("code");
  const error = params.get("error");
  if (state === null || !STATE.test(state)) return errorPage("The connection callback is missing a valid state.");
  if ((code === null) === (error === null)) return errorPage("The connection callback must carry either a code or an error.");
  if (code !== null && !CODE.test(code)) return errorPage("The connection callback's code is not valid.");
  if (error !== null && !ERROR.test(error)) return errorPage("The connection callback's error is not valid.");

  const { apiUrl, apiToken } = config();
  let response: Response;
  try {
    response = await fetch(`${apiUrl}/v1/gateway/xinf/callback`, {
      method: "POST",
      headers: { Authorization: `Bearer ${apiToken}`, "Content-Type": "application/json" },
      body: JSON.stringify(code !== null ? { state, code } : { state, error }),
      cache: "no-store",
      redirect: "error",
      signal: AbortSignal.timeout(15_000),
    });
  } catch {
    return errorPage("The fleet service is unavailable. Try connecting again in a moment.", 502);
  }
  const value = (await response.json().catch(() => null)) as { return_to?: unknown; error?: unknown } | null;
  if (response.status !== 200) {
    const message = typeof value?.error === "string" && value.error.trim() ? value.error.slice(0, 300) : "The connection could not be completed.";
    return errorPage(message, response.status >= 400 && response.status < 500 ? response.status : 502);
  }
  const target = returnTo(value?.return_to);
  if (!target) return errorPage("The connection finished, but the app's return address is not valid.", 502);
  return new Response(null, { status: 303, headers: { Location: target, "Cache-Control": "no-store", "Referrer-Policy": "no-referrer" } });
}

/** An absolute https URL without credentials, normalized; null otherwise. */
function returnTo(value: unknown): string | null {
  if (typeof value !== "string" || value.length > 2048 || /[\s\u0000-\u001f\u007f]/.test(value)) return null;
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    return null;
  }
  return url.protocol === "https:" && url.hostname && !url.username && !url.password ? url.href : null;
}
