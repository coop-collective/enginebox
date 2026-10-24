import { env } from "cloudflare:workers";
import { afterEach, expect, it, vi } from "vitest";
import { machineIce } from "../src/machine-turn.ts";

afterEach(() => vi.restoreAllMocks());
const cloud = { keyId: "test-key", apiToken: "private-provider-token", ttlSeconds: 300 };
const relay = { urls: ["turn:turn.cloudflare.com:3478?transport=udp", "turn:turn.cloudflare.com:53?transport=udp", "turns:turn.cloudflare.com:443?transport=tcp"], username: "ephemeral-user", credential: "ephemeral-secret" };
const settings = (config: unknown) => ({ ...env, MACHINE_TURN_CONFIG: JSON.stringify(config) }) as Env;

it("mints bounded ephemeral relay credentials without exposing its provider key", async () => {
  const upstream = vi.spyOn(globalThis, "fetch").mockResolvedValue(Response.json({ iceServers: [{ urls: "stun:stun.cloudflare.com:3478" }, relay] }));
  const before = Date.now();
  const ice = await machineIce(settings(cloud), "watch");
  expect(upstream).toHaveBeenCalledOnce();
  const [url, init] = upstream.mock.calls[0];
  expect(url).toBe("https://rtc.live.cloudflare.com/v1/turn/keys/test-key/credentials/generate-ice-servers");
  expect(init?.redirect).toBe("error");
  expect(JSON.parse(String(init?.body))).toEqual({ ttl: 300 });
  expect(new Headers(init?.headers).get("authorization")).toBe("Bearer private-provider-token");
  expect(ice.iceTransportPolicy).toBe("relay");
  expect(ice.iceExpires).toBeGreaterThanOrEqual(before + 300_000);
  expect(ice.iceExpires).toBeLessThanOrEqual(Date.now() + 300_000);
  expect(JSON.stringify(ice)).not.toContain(cloud.apiToken);
  expect(JSON.stringify(ice)).not.toContain(":53?");
  expect(ice.iceServers[1].credential).toBe("ephemeral-secret");
});

it("does not contact a provider for missing or invalid configuration", async () => {
  const upstream = vi.spyOn(globalThis, "fetch");
  for (const config of [null, {}, { ...cloud, keyId: "../other" }, { ...cloud, ttlSeconds: 86400 }, { ...cloud, iceTransportPolicy: "invalid" }]) {
    await expect(machineIce(settings(config), "watch")).rejects.toMatchObject({ status: 503 });
  }
  await expect(machineIce({ ...env, MACHINE_TURN_CONFIG: undefined } as Env, "watch")).rejects.toMatchObject({ status: 503 });
  expect(upstream).not.toHaveBeenCalled();
});

it("permits explicit local fixtures only in the local controller mode", async () => {
  const upstream = vi.spyOn(globalThis, "fetch");
  const local = { local: true, iceServers: [{ urls: "turn:coturn:3478?transport=udp", username: "fixture", credential: "fixture" }], iceTransportPolicy: "relay" };
  expect((await machineIce(settings(local), "watch")).iceServers).toHaveLength(1);
  await expect(machineIce({ ...settings(local), ALLOW_HTTP_NODES: "false" }, "watch")).rejects.toMatchObject({ status: 503 });
  expect(upstream).not.toHaveBeenCalled();
});

it("redacts provider failures and rejects oversized, foreign, or incomplete responses", async () => {
  const upstream = vi.spyOn(globalThis, "fetch");
  for (const response of [
    new Response(cloud.apiToken, { status: 403 }),
    new Response("x".repeat(17 * 1024)),
    Response.json({ iceServers: [{ ...relay, urls: "turn:foreign.example:3478" }] }),
    Response.json({ iceServers: [{ urls: "stun:stun.cloudflare.com:3478" }] }),
    Response.json({ iceServers: [{ urls: relay.urls, username: "missing credential" }] }),
  ]) {
    upstream.mockResolvedValueOnce(response);
    await expect(machineIce(settings(cloud), "watch")).rejects.toThrow("Machine relay unavailable");
  }
  upstream.mockRejectedValueOnce(new Error(cloud.apiToken));
  await expect(machineIce(settings(cloud), "watch")).rejects.toThrow("Machine relay unavailable");
});

it("derives production transport from the authorized mode while retaining TURN fallback", async () => {
  const upstream = vi
    .spyOn(globalThis, "fetch")
    .mockImplementation(async () => Response.json({ iceServers: [relay] }));
  for (const override of [undefined, "all", "relay"] as const) {
    const config = {
      ...cloud,
      ...(override ? { iceTransportPolicy: override } : {}),
    };
    const control = await machineIce(settings(config), "control");
    const watch = await machineIce(settings(config), "watch");
    expect(control.iceTransportPolicy).toBe(
      override === "relay" ? "relay" : "all",
    );
    expect(watch.iceTransportPolicy).toBe("relay");
    expect(control.iceServers).toEqual(watch.iceServers);
    expect(control.iceServers[0].credential).toBe("ephemeral-secret");
  }
  expect(upstream).toHaveBeenCalledTimes(6);
});

it("does not mint credentials without an authorized mode", async () => {
  const upstream = vi.spyOn(globalThis, "fetch");
  await expect(
    machineIce(settings(cloud), undefined as never),
  ).rejects.toMatchObject({ status: 403 });
  expect(upstream).not.toHaveBeenCalled();
});
