import {
  createHash,
  createHmac,
  randomBytes,
  timingSafeEqual,
} from "node:crypto";

const cookieName = "enginebox_session";
const sessionSeconds = 8 * 60 * 60;

export class HttpError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

export function config() {
  const password = process.env.ENGINEBOX_DASHBOARD_PASSWORD ?? "";
  const secret = process.env.ENGINEBOX_SESSION_SECRET ?? "";
  const apiToken = process.env.ENGINEBOX_API_TOKEN ?? "";
  if (password.length < 16 || secret.length < 32 || apiToken.length < 32) {
    throw new HttpError(
      503,
      "Dashboard credentials are not configured. Check the server environment.",
    );
  }
  if (password === apiToken)
    throw new HttpError(
      503,
      "Use separate dashboard and fleet API credentials.",
    );
  const origin =
    process.env.ENGINEBOX_DASHBOARD_ORIGIN ??
    `http://127.0.0.1:${process.env.PORT ?? "3000"}`;
  const apiUrl = process.env.ENGINEBOX_API_URL ?? "http://127.0.0.1:8765";
  for (const value of [origin, apiUrl]) {
    let url: URL;
    try {
      url = new URL(value);
    } catch {
      throw new HttpError(503, "Invalid dashboard server URL configuration.");
    }
    if (
      url.username ||
      url.password ||
      url.search ||
      url.hash ||
      !["http:", "https:"].includes(url.protocol)
    ) {
      throw new HttpError(503, "Invalid dashboard server URL configuration.");
    }
    if (
      url.protocol === "http:" &&
      !["127.0.0.1", "localhost", "[::1]"].includes(url.hostname)
    ) {
      throw new HttpError(
        503,
        "Non-loopback dashboard and API URLs must use HTTPS.",
      );
    }
  }
  if (new URL(origin).origin !== origin)
    throw new HttpError(
      503,
      "ENGINEBOX_DASHBOARD_ORIGIN must be an exact origin without a path or trailing slash.",
    );
  return {
    password,
    secret,
    apiToken,
    origin,
    apiUrl: apiUrl.replace(/\/$/, ""),
  };
}

export function json(
  value: unknown,
  status = 200,
  extra: Record<string, string> = {},
) {
  return Response.json(value, {
    status,
    headers: {
      "Cache-Control": "no-store",
      "X-Content-Type-Options": "nosniff",
      Vary: "Cookie",
      ...extra,
    },
  });
}

export function errorResponse(error: unknown) {
  return error instanceof HttpError
    ? json({ error: error.message }, error.status)
    : json({ error: "The dashboard could not complete this request." }, 500);
}

export function requireOrigin(request: Request) {
  if (request.headers.get("origin") !== config().origin)
    throw new HttpError(403, "Request origin is not allowed.");
}

function equal(a: string, b: string) {
  return timingSafeEqual(
    createHash("sha256").update(a).digest(),
    createHash("sha256").update(b).digest(),
  );
}

function sign(payload: string) {
  const { secret, password } = config();
  return createHmac("sha256", secret)
    .update(password)
    .update("\0")
    .update(payload)
    .digest("base64url");
}

export function sessionCookie(now = Date.now()) {
  const payload = Buffer.from(
    JSON.stringify({
      expires: now + sessionSeconds * 1000,
      nonce: randomBytes(24).toString("hex"),
    }),
  ).toString("base64url");
  return cookie(`${payload}.${sign(payload)}`, sessionSeconds);
}

export function cookie(value: string, maxAge: number) {
  return `${cookieName}=${value}; HttpOnly; Path=/; SameSite=Strict; Max-Age=${maxAge}${config().origin.startsWith("https:") ? "; Secure" : ""}`;
}

export function authenticated(request: Request, now = Date.now()) {
  config();
  const token = request.headers
    .get("cookie")
    ?.split(";")
    .map((part) => part.trim())
    .find((part) => part.startsWith(`${cookieName}=`))
    ?.slice(cookieName.length + 1);
  if (!token || token.length > 1024) return false;
  const [payload, signature, extra] = token.split(".");
  if (!payload || !signature || extra || !equal(sign(payload), signature))
    return false;
  try {
    const session = JSON.parse(
      Buffer.from(payload, "base64url").toString("utf8"),
    );
    return (
      typeof session.expires === "number" &&
      session.expires > now &&
      session.expires <= now + sessionSeconds * 1000
    );
  } catch {
    return false;
  }
}

export function requireSession(request: Request) {
  if (!authenticated(request))
    throw new HttpError(401, "Sign in to access your fleet.");
}

export async function bodyJson(request: Request, maxBytes = 8192) {
  if (!request.headers.get("content-type")?.startsWith("application/json"))
    throw new HttpError(415, "Send an application/json request.");
  if (Number(request.headers.get("content-length") ?? 0) > maxBytes)
    throw new HttpError(413, "Request is too large.");
  const reader = request.body?.getReader();
  if (!reader) throw new HttpError(400, "Request body is required.");
  let bytes = 0;
  const chunks: Uint8Array[] = [];
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    bytes += value.byteLength;
    if (bytes > maxBytes) {
      await reader.cancel();
      throw new HttpError(413, "Request is too large.");
    }
    chunks.push(value);
  }
  try {
    return JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    throw new HttpError(400, "Request body must be valid JSON.");
  }
}

// A small global limit suits this single-operator service without trusting
// user-controlled proxy address headers. Restarting the process resets it.
let attempts: number[] = [];
/** Counts one sign-in attempt against the shared limit, then checks the password. */
export function verifyOperatorPassword(password: unknown) {
  const now = Date.now();
  attempts = attempts.filter((time) => time > now - 5 * 60 * 1000);
  if (attempts.length >= 20)
    throw new HttpError(
      429,
      "Too many sign-in attempts. Try again in five minutes.",
    );
  attempts.push(now);
  if (typeof password !== "string" || !equal(password, config().password)) {
    throw new HttpError(401, "That operator password was not accepted.");
  }
}
export async function login(request: Request) {
  requireOrigin(request);
  const body = await bodyJson(request);
  verifyOperatorPassword(body?.password);
  return json({ authenticated: true }, 200, { "Set-Cookie": sessionCookie() });
}

export async function proxy(request: Request, segments: string[]) {
  requireSession(request);
  if (request.method !== "GET") requireOrigin(request);
  const path = segments.join("/");
  const accountRead =
    /^v1\/agents(?:\/(?:codex|claude|cursor)\/(?:usage|sessions\/[a-f0-9]{32}))?$/.test(
      path,
    );
  const accountWrite =
    /^v1\/agents\/(?:(?:codex|claude|cursor)\/(?:login|sessions\/[a-f0-9]{32}\/cancel)|claude\/sessions\/[a-f0-9]{32}\/input)$/.test(
      path,
    );
  const allowed =
    request.method === "GET"
      ? accountRead || /^v1\/github(?:\/(?:repos|runners)\/installations|\/installations\/[1-9][0-9]*\/repositories)?$/.test(path) ||
        path === "v1/ci/templates" ||
        /^v1\/(fleet|templates|storage|jobs\/[a-zA-Z0-9_-]{1,100})$/.test(path)
      : request.method === "POST" &&
        (accountWrite || /^v1\/github\/(?:(?:repos|runners)\/disconnect|select|remove|drain)$/.test(path) ||
          /^v1\/ci\/templates(?:\/[a-zA-Z0-9_-]{1,48}\/(?:enable|disable|retry))?$/.test(path) ||
          /^v1\/storage\/hosts\/[a-z0-9][a-z0-9-]{0,48}\/(inspect|list|preview|import|fork|deps|checkpoint|remount|close|recover|github-preview|github-import|github-fork|github-settings|github-promote|github-delivery|dev-start|dev-status)$/.test(path) ||
          /^(?:v1\/templates(?:\/[a-z][a-z0-9-]{0,31}\/sync)?|v1\/hosts\/[a-zA-Z0-9_-]{1,63}\/(?:templates\/[a-z][a-z0-9-]{0,31}\/install|allocations|instances(?:\/ebox-[a-z0-9-]{1,49}\/actions)?))$/.test(
            path,
          ));
  if (
    !allowed ||
    segments.some((part) => part.includes("/") || part.includes(".."))
  )
    throw new HttpError(404, "Unknown fleet operation.");
  const { apiUrl, apiToken } = config();
  const body =
    request.method === "POST"
      ? JSON.stringify(
          await bodyJson(request, path === "v1/templates" ? 1024 * 1024 : path === "v1/ci/templates" ? 65536 : 8192),
        )
      : undefined;
  let response: Response;
  try {
    response = await fetch(`${apiUrl}/${path}`, {
      method: request.method,
      headers: {
        Authorization: `Bearer ${apiToken}`,
        "Content-Type": "application/json",
      },
      body,
      cache: "no-store",
      redirect: "error",
      signal: AbortSignal.timeout(30_000),
    });
  } catch {
    throw new HttpError(
      502,
      "The fleet service is unavailable. Check its connection and try again.",
    );
  }
  if ([401, 403].includes(response.status))
    throw new HttpError(
      502,
      "The fleet service rejected the dashboard's credentials. Check the server configuration.",
    );
  let result: unknown;
  try {
    result = await response.json();
  } catch {
    throw new HttpError(502, "The fleet service returned an invalid response.");
  }
  return json(result, response.status);
}
