import assert from "node:assert/strict";
import { createHash, createHmac } from "node:crypto";
import { test } from "node:test";
import { ReplayGuard, render, send, sign, WINDOW_SECONDS } from "../src/gateway.mjs";

const SECRET = "test-only-email-gateway-secret-0123456789abcdef";
const NOW = 1_800_000_000;
const ORIGIN = "https://enginebox.example";

function replay() {
  const store = new Map();
  let alarm = null;
  const guard = new ReplayGuard({
    storage: {
      get: async (k) => store.get(k),
      put: async (k, v) => void store.set(k, v),
      getAlarm: async () => alarm,
      setAlarm: async (t) => void (alarm = t),
      list: async ({ prefix }) => new Map([...store].filter(([k]) => k.startsWith(prefix))),
      delete: async (keys) => keys.forEach((k) => store.delete(k)),
    },
  });
  return { idFromName: (n) => n, get: () => ({ fetch: (url, init) => guard.fetch(new Request(url, init)) }) };
}
function setup(overrides = {}) {
  const sent = [];
  const env = {
    EMAIL_GATEWAY_SECRET: SECRET,
    FROM_ADDRESS: "login@coopcollective.dev",
    FROM_NAME: "Engine Box",
    DASHBOARD_ORIGIN: ORIGIN,
    REPLAY: replay(),
    EMAIL: { send: async (m) => (sent.push(m), { messageId: "m1" }) },
    ...overrides,
  };
  return { env, sent };
}
let counter = 0;
async function signed(message, { secret = SECRET, timestamp = NOW, nonce, tamper } = {}) {
  const body = JSON.stringify(message);
  nonce ??= (++counter).toString(16).padStart(32, "0");
  return { timestamp: String(timestamp), nonce, signature: `v1=${await sign(secret, String(timestamp), nonce, body)}`, body: tamper ?? body };
}
const signIn = (extra = {}) => ({ template: "sign_in", to: "Dev@Example.com", code: "123456", link: `${ORIGIN}/api/auth/link?c=abc&t=def`, org_name: "Default Org", expires_minutes: 10, ...extra });

test("the dashboard's Node signature matches the gateway's WebCrypto signature", async () => {
  const body = JSON.stringify(signIn());
  const nodeSig = createHmac("sha256", SECRET)
    .update(`v1\nPOST\n/send\n${NOW}\n${"a".repeat(32)}\n${createHash("sha256").update(body).digest("hex")}`)
    .digest("hex");
  assert.equal(await sign(SECRET, String(NOW), "a".repeat(32), body), nodeSig);
});

test("a signed sign-in message is sent from the fixed sender with text and html", async () => {
  const { env, sent } = setup();
  const result = await send(await signed(signIn()), env, NOW);
  assert.equal(result.status, 200);
  assert.equal(sent.length, 1);
  assert.equal(sent[0].to, "dev@example.com");
  assert.deepEqual(sent[0].from, { email: "login@coopcollective.dev", name: "Engine Box" });
  assert.match(sent[0].subject, /123456/);
  assert.match(sent[0].text, /api\/auth\/link/);
  assert.match(sent[0].html, /Sign in/);
});

test("expired, future, replayed, tampered or wrongly keyed envelopes answer 401", async () => {
  const { env, sent } = setup();
  const replayed = await signed(signIn());
  assert.equal((await send(replayed, env, NOW)).status, 200);
  for (const envelope of [
    replayed,
    await signed(signIn(), { timestamp: NOW - WINDOW_SECONDS - 1 }),
    await signed(signIn(), { timestamp: NOW + WINDOW_SECONDS + 1 }),
    await signed(signIn(), { tamper: JSON.stringify(signIn({ code: "999999" })) }),
    await signed(signIn(), { secret: "another-secret-another-secret-another-secret" }),
    { ...(await signed(signIn())), nonce: "not-hex" },
  ])
    assert.deepEqual(await send(envelope, env, NOW), { status: 401, body: { error: "unauthorized" } });
  assert.equal(sent.length, 1);
  const { env: unset } = setup({ EMAIL_GATEWAY_SECRET: "short" });
  assert.equal((await send(await signed(signIn()), unset, NOW)).status, 401);
});

test("only fixed templates with links to the configured dashboard are rendered", () => {
  const env = { DASHBOARD_ORIGIN: ORIGIN };
  assert.ok(render(signIn(), env));
  for (const bad of [
    signIn({ link: "https://attacker.example/api/auth/link?c=1" }),
    signIn({ link: `${ORIGIN}/somewhere-else` }),
    signIn({ link: `https://user:pw@enginebox.example/api/auth/link` }),
    signIn({ code: "12345a" }),
    signIn({ to: "two@example.com, three@example.com" }),
    signIn({ org_name: "<script>" }),
    signIn({ expires_minutes: 999 }),
    { ...signIn(), template: "anything" },
    { template: "invite", to: "a@example.com", link: `${ORIGIN}/api/auth/invite?token=x`, org_name: "Team", role: "root", expires_days: 7 },
    { template: "claim_owner", to: "a@example.com", code: "123456", link: "https://attacker.example", org_name: "Team", expires_minutes: 10 },
  ])
    assert.equal(render(bad, env), null);
  assert.ok(render({ template: "invite", to: "a@example.com", link: `${ORIGIN}/api/auth/invite?token=x`, org_name: "Team", role: "admin", expires_days: 7 }, env));
  assert.ok(render({ template: "claim_owner", to: "a@example.com", code: "123456", link: ORIGIN, org_name: "Team", expires_minutes: 10 }, env));
});

test("binding failures map to a code without leaking the address or message", async () => {
  const logs = [];
  const original = console.error;
  console.error = (line) => logs.push(line);
  try {
    const failing = (code) => setup({ EMAIL: { send: async () => { throw Object.assign(new Error("dev@example.com 123456"), { code }); } } }).env;
    assert.deepEqual(await send(await signed(signIn()), failing("E_SENDER_NOT_VERIFIED"), NOW), { status: 502, body: { error: "send_failed" } });
    assert.deepEqual(await send(await signed(signIn()), failing("E_RATE_LIMIT_EXCEEDED"), NOW), { status: 429, body: { error: "rate_limited" } });
  } finally {
    console.error = original;
  }
  assert.ok(logs.length === 2 && logs.every((l) => !l.includes("example.com") && !l.includes("123456")));
});
