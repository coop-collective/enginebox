// The email gateway Worker. It has no hostname, no workers.dev and no preview
// URLs: the site Worker reaches it only through a service binding with
// `entrypoint: "EmailGateway"` and calls `send(envelope)` over RPC.
// Verification (signature, window, nonce) and rendering happen in src/gateway.mjs.
import { WorkerEntrypoint } from "cloudflare:workers";
import { send } from "./gateway.mjs";

export { ReplayGuard } from "./gateway.mjs";

export class EmailGateway extends WorkerEntrypoint {
  /** `{timestamp, nonce, signature, body}` → `{status, body}`. */
  async send(envelope) {
    return send(envelope, this.env);
  }
}

// No HTTP surface of its own.
export default {
  fetch() {
    return new Response(null, { status: 404, headers: { "cache-control": "no-store" } });
  },
};
