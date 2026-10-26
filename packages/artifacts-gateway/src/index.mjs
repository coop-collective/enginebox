// The Artifacts gateway Worker. It has no hostname, no workers.dev and no
// preview URLs: the site Worker reaches it only through a service binding
// with `entrypoint: "ArtifactsGateway"` and calls `invoke(envelope)` over RPC.
// Verification (signature, window, nonce) happens here, in src/gateway.mjs.
import { WorkerEntrypoint } from "cloudflare:workers";
import { invoke } from "./gateway.mjs";

export { ReplayGuard } from "./gateway.mjs";

export class ArtifactsGateway extends WorkerEntrypoint {
  /** `{method, path, timestamp, nonce, signature, body}` → `{status, body}`. */
  async invoke(envelope) {
    return invoke(envelope, this.env);
  }
}

// No HTTP surface of its own.
export default {
  fetch() {
    return new Response(null, { status: 404, headers: { "cache-control": "no-store" } });
  },
};
