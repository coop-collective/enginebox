export class RequestError extends Error {
  constructor(
    message: string,
    public status: number,
  ) {
    super(message);
  }
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
  const data = await response.json();
  if (!response.ok)
    throw new RequestError(
      typeof data.error === "string"
        ? data.error
        : (data.error?.message ??
            data.detail ??
            "The request failed. Try again."),
      response.status,
    );
  return data as T;
}
