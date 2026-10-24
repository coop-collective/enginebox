export class RequestError extends Error {
  constructor(
    message: string,
    public status: number,
  ) {
    super(message);
  }
}

const GENERIC_FAILURE = "The request failed. Try again.";

export function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === "string" && err) return err;
  return GENERIC_FAILURE;
}

/** Parses a response body without throwing; non-JSON bodies (e.g. a proxy's 502 HTML page) yield undefined. */
export async function readJson(response: Response): Promise<unknown> {
  const text = await response.text();
  if (!text) return undefined;
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

type ErrorBody = {
  error?: unknown;
  detail?: unknown;
};

export function failureMessage(data: unknown): string {
  if (!data || typeof data !== "object") return GENERIC_FAILURE;
  const { error, detail } = data as ErrorBody;
  if (typeof error === "string") return error;
  if (error && typeof error === "object") {
    const message = (error as { message?: unknown }).message;
    if (typeof message === "string") return message;
  }
  return typeof detail === "string" ? detail : GENERIC_FAILURE;
}

export async function api<T>(
  path: string,
  options: RequestInit = {},
): Promise<T> {
  const response = await fetch(path, {
    ...options,
    headers: { "Content-Type": "application/json", ...options.headers },
    credentials: "same-origin",
    cache: "no-store",
  });
  const data = await readJson(response);
  if (!response.ok)
    throw new RequestError(failureMessage(data), response.status);
  if (data === undefined)
    throw new RequestError(
      "The server returned an unreadable response. Try again.",
      response.status,
    );
  return data as T;
}
