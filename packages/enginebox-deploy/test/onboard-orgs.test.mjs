import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { dashboardEnvironment, parseArgs, plan, run } from "../src/onboard-orgs.mjs";

const base = ["--domain", "coopcollective.dev", "--origin", "https://enginebox.coopcollective.dev", "--account", "a".repeat(32), "--owner-email", "owner@example.com"];

test("options are validated and the sender defaults to login@<domain>", () => {
  const o = parseArgs(base, {});
  assert.equal(o.from, "login@coopcollective.dev");
  assert.equal(o.apply, false);
  for (const bad of [
    ["--domain", "not a domain"],
    ["--origin", "http://enginebox.coopcollective.dev"],
    ["--origin", "https://enginebox.coopcollective.dev/dashboard"],
    ["--account", "123"],
    ["--from", "login@elsewhere.example"],
    ["--identity-dir", "relative/dir"],
  ]) {
    const args = [...base];
    const i = args.indexOf(bad[0]);
    if (i >= 0) args[i + 1] = bad[1];
    else args.push(...bad);
    assert.throws(() => parseArgs(args, {}), undefined, bad.join(" "));
  }
  assert.throws(() => parseArgs([...base, "--surprise", "x"], {}));
});

test("the plan enables Email Sending, deploys the gateway and pipes the secret; no command carries a secret", () => {
  const o = parseArgs([...base, "--deploy-site"], {});
  const steps = plan(o);
  assert.deepEqual(steps.map((s) => s.id), ["login", "sending-list", "sending-enable", "sending-dns", "secret", "configure", "deploy-gateway", "gateway-secret", "deploy-site", "dashboard-env"]);
  assert.deepEqual(steps.find((s) => s.id === "sending-enable").command.slice(-4), ["email", "sending", "enable", "coopcollective.dev"]);
  assert.equal(steps.find((s) => s.id === "gateway-secret").stdin, "secret");
  assert.ok(steps.every((s) => s.kind === "check" || s.kind === "run"));
  assert.deepEqual(steps.filter((s) => s.kind === "check").map((s) => s.id), ["login", "sending-list", "sending-dns"]);
});

test("plan mode runs only read-only checks; apply writes the secret and env fragment privately and pipes the secret", () => {
  const dir = mkdtempSync(join(tmpdir(), "enginebox-onboard-"));
  try {
    const calls = [];
    const spawn = (cmd, args, opts) => (calls.push({ cmd, args, input: opts.input }), { status: 0 });
    const lines = [];
    const options = parseArgs([...base, "--secret-file", join(dir, "secret"), "--env-out", join(dir, "env.json")], {});
    run(options, { log: (l) => lines.push(l), spawn });
    assert.deepEqual(calls.map((c) => c.args.slice(-3).join(" ")), ["exec wrangler whoami", "email sending list", "dns get coopcollective.dev"]);

    calls.length = 0;
    lines.length = 0;
    run({ ...options, apply: true }, { log: (l) => lines.push(l), spawn });
    const secret = readFileSync(join(dir, "secret"), "utf8").trim();
    assert.equal(secret.length, 64);
    if (process.platform !== "win32") {
      assert.equal(statSync(join(dir, "secret")).mode & 0o777, 0o600);
      assert.equal(statSync(join(dir, "env.json")).mode & 0o777, 0o600);
    }
    assert.equal(JSON.parse(readFileSync(join(dir, "env.json"), "utf8")).ENGINEBOX_EMAIL_SECRET, secret);
    const piped = calls.find((c) => c.args.includes("EMAIL_GATEWAY_SECRET"));
    assert.equal(piped.input, `${secret}\n`);
    // The secret never appears in a command line or the log.
    assert.ok(calls.every((c) => !c.args.join(" ").includes(secret)));
    assert.ok(lines.every((l) => !l.includes(secret)));
    // A second apply keeps the existing secret and refuses to overwrite the env file.
    assert.throws(() => run({ ...options, apply: true }, { log: () => {}, spawn }));
    assert.equal(readFileSync(join(dir, "secret"), "utf8").trim(), secret);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the printed dashboard environment uses a placeholder for the secret", () => {
  const env = dashboardEnvironment(parseArgs(base, {}));
  assert.equal(env.ENGINEBOX_EMAIL_ENDPOINT, "https://enginebox.coopcollective.dev/_email/send");
  assert.match(env.ENGINEBOX_EMAIL_SECRET, /^</);
});

test("Email Sending already enabled on the domain is detected, so a re-run skips enabling it", async () => {
  const { sendingEnabled } = await import("../src/onboard-orgs.mjs");
  const table = "│ zone │ name │ enabled │\n│ coopcollective.dev │ coopcollective.dev │ yes │\n│ other.dev │ other.dev │ no │\n";
  const fake = (out, status = 0) => () => ({ status, stdout: out });
  assert.equal(sendingEnabled({ domain: "coopcollective.dev", account: "a" }, fake(table)), true);
  assert.equal(sendingEnabled({ domain: "other.dev", account: "a" }, fake(table)), false);
  assert.equal(sendingEnabled({ domain: "missing.dev", account: "a" }, fake(table)), false);
  assert.equal(sendingEnabled({ domain: "coopcollective.dev", account: "a" }, fake(table, 1)), false);
});
