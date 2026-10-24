import { ProtocolError, readBounded, requireThat } from "@enginebox/fleet-protocol";

declare global {
  interface Env { MACHINE_TURN_CONFIG?: string }
}

export type MachineIce = {
  iceServers: Array<{ urls: string | string[]; username?: string; credential?: string }>;
  iceTransportPolicy: "all" | "relay";
  iceExpires: number;
};
export type MachineIceConfiguration = MachineIce;

const unavailable = () => new ProtocolError(503, "Machine relay unavailable; configure Cloudflare TURN credentials");
const object = (value: unknown): value is Record<string, unknown> =>
  !!value && typeof value === "object" && !Array.isArray(value);
const boundedString = (value: unknown, max: number): value is string =>
  typeof value === "string" && value.length > 0 && value.length <= max && !/[\x00-\x1f\x7f]/.test(value);

function servers(value: unknown, local: boolean): MachineIce["iceServers"] {
  requireThat(Array.isArray(value) && value.length > 0 && value.length <= 8, "Invalid TURN response", 503);
  const result: MachineIce["iceServers"] = [];
  let relay = false;
  for (const item of value) {
    requireThat(object(item), "Invalid TURN response", 503);
    const urls = typeof item.urls === "string" ? [item.urls] : item.urls;
    requireThat(Array.isArray(urls) && urls.length > 0 && urls.length <= 8, "Invalid TURN response", 503);
    const clean: string[] = [];
    let credentialsRequired = false;
    for (const url of urls) {
      requireThat(boundedString(url, 512) && /^(?:stun|turn|turns):[^\s/@?#]+(?::\d+)?(?:\?transport=(?:udp|tcp))?$/.test(url), "Invalid TURN response", 503);
      if (!local) {
        requireThat(/^(?:stun|turn|turns):(?:stun|turn)\.cloudflare\.com:(?:3478|443|5349|53|80)(?:\?transport=(?:udp|tcp))?$/.test(url), "Invalid TURN response", 503);
        // Browsers block port53. Keep UDP3478/443 and TCP/TLS fallbacks.
        if (/:53(?:\?|$)/.test(url)) continue;
      }
      if (/^turns?:/.test(url)) { credentialsRequired = true; relay = true; }
      clean.push(url);
    }
    if (!clean.length) continue;
    if (credentialsRequired) {
      requireThat(boundedString(item.username, 1024) && boundedString(item.credential, 2048), "Invalid TURN response", 503);
      result.push({ urls: clean, username: item.username, credential: item.credential });
    } else result.push({ urls: clean });
  }
  requireThat(relay, "TURN response contains no relay", 503);
  return result;
}

/** Call only after authorization. Persist the result privately with its admission;
 * never mint on each poll, log it, or return the provider key to clients. */
export async function machineIce(env: Env): Promise<MachineIce> {
  if (!env.MACHINE_TURN_CONFIG) throw unavailable();
  let config: unknown;
  try { config = JSON.parse(env.MACHINE_TURN_CONFIG); } catch { throw unavailable(); }
  if (!object(config)) throw unavailable();
  const ttl = config.ttlSeconds ?? 3600;
  requireThat(Number.isInteger(ttl) && Number(ttl) >= 120 && Number(ttl) <= 3600, "Invalid machine relay lifetime", 503);
  const iceExpires = Date.now() + Number(ttl) * 1000;
  if (config.local === true) {
    requireThat(String(env.ALLOW_HTTP_NODES) === "true", "Local relay configuration is disabled", 503);
    requireThat(Object.keys(config).every(key => ["local", "iceServers", "iceTransportPolicy", "ttlSeconds"].includes(key)), "Invalid local relay configuration", 503);
    requireThat(config.iceTransportPolicy === "all" || config.iceTransportPolicy === "relay", "Invalid local relay policy", 503);
    return { iceServers: servers(config.iceServers, true), iceTransportPolicy: config.iceTransportPolicy, iceExpires };
  }
  requireThat(Object.keys(config).every(key => ["keyId", "apiToken", "ttlSeconds", "iceTransportPolicy"].includes(key)) &&
    typeof config.keyId === "string" && /^[a-zA-Z0-9_-]{1,128}$/.test(config.keyId) && boundedString(config.apiToken, 4096) &&
    (config.iceTransportPolicy === undefined || config.iceTransportPolicy === "relay"), "Invalid machine relay configuration", 503);
  try {
    const response = await fetch(`https://rtc.live.cloudflare.com/v1/turn/keys/${config.keyId}/credentials/generate-ice-servers`, {
      method: "POST", redirect: "error", signal: AbortSignal.timeout(5000),
      headers: { authorization: `Bearer ${config.apiToken}`, "content-type": "application/json" },
      body: JSON.stringify({ ttl }),
    });
    if (!response.ok) { await response.body?.cancel(); throw unavailable(); }
    const value: unknown = JSON.parse(new TextDecoder().decode(await readBounded(response.body, 16 * 1024)));
    if (!object(value)) throw unavailable();
    return { iceServers: servers(value.iceServers, false), iceTransportPolicy: "relay", iceExpires };
  } catch {
    // Provider errors may contain credentials or internal request metadata.
    throw unavailable();
  }
}
