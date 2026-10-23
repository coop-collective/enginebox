import {
  canonical,
  ProtocolError,
  readBounded,
  MAX_JSON_BYTES,
} from "@enginebox/fleet-protocol";

export class Client {
  url: string;
  token: string;
  epoch = 0;
  session = "";
  constructor(url: string, token: string) {
    this.url = new URL(url).origin;
    this.token = token;
  }
  async request(
    path: string,
    init: RequestInit = {},
    timeout = 60_000,
  ): Promise<Response> {
    const headers = new Headers(init.headers);
    headers.set("authorization", `Bearer ${this.token}`);
    if (this.session) {
      headers.set("x-enginebox-session", this.session);
      headers.set("x-enginebox-epoch", String(this.epoch));
    }
    const response = await fetch(`${this.url}${path}`, {
      ...init,
      headers,
      redirect: "error",
      signal: init.signal ?? AbortSignal.timeout(timeout),
    });
    if (!response.ok && !(init.method === "HEAD" && response.status === 404)) {
      const text = new TextDecoder().decode(
        await readBounded(response.body, MAX_JSON_BYTES),
      );
      throw new ProtocolError(response.status, `Controller ${path}: ${text}`);
    }
    return response;
  }
  async json<T = unknown>(
    path: string,
    body?: unknown,
    method = body === undefined ? "GET" : "POST",
  ): Promise<T> {
    const response = await this.request(path, {
      method,
      ...(body === undefined
        ? {}
        : {
            body: canonical(body),
            headers: { "content-type": "application/json" },
          }),
    });
    return response.json() as Promise<T>;
  }
}
