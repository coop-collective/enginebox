// Guided setup for organizations and email sign-in on a self-hosted Engine Box
// behind Cloudflare (docs/SELF-HOSTING-ORGS.md).
//
//   node packages/enginebox-deploy/src/onboard-orgs.mjs \
//     --domain example.com --origin https://enginebox.example.com \
//     --account <32-hex Cloudflare account id> --owner-email you@example.com \
//     [--from login@example.com] [--identity-dir /opt/enginebox-control/state/identity] \
//     [--secret-file ~/.enginebox/email-gateway-secret] [--env-out ./orgs-environment.json] \
//     [--deploy-site] [--apply]
//
// Without --apply it only plans: it checks the wrangler login, reads which
// domains have Email Sending and the DNS records Email Sending needs, and
// prints every command it would run. With --apply it enables Email Sending on
// the domain (Cloudflare adds the SPF and DKIM records), writes the gateway's
// private wrangler config, deploys the gateway, sets EMAIL_GATEWAY_SECRET from
// a private file over stdin, optionally redeploys the site Worker, and writes
// the dashboard's environment fragment (mode 0600) for the supervisor's
// environment.json.
//
// Secrets never reach argv, stdout or the repository: the shared secret is
// generated into a 0600 file (never overwritten) and piped to wrangler.
// Domain registration itself is a Cloudflare dashboard step (Registrar); the
// script checks for it and says what to do.
import { spawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, isAbsolute, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");
const DOMAIN = /^(?=.{3,253}$)(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$/;
const EMAIL = /^[^\s@<>()",;:\\[\]]{1,64}@[a-z0-9.-]{3,253}$/;

export function parseArgs(argv, env = process.env) {
  const flags = {};
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (!arg.startsWith("--")) throw new Error(`Unexpected argument ${arg}`);
    const name = arg.slice(2);
    if (["apply", "deploy-site"].includes(name)) flags[name] = true;
    else {
      const value = argv[++i];
      if (value === undefined || value.startsWith("--")) throw new Error(`--${name} needs a value`);
      flags[name] = value;
    }
  }
  const known = ["domain", "origin", "account", "owner-email", "from", "identity-dir", "secret-file", "env-out", "deploy-site", "apply", "from-name"];
  for (const name of Object.keys(flags)) if (!known.includes(name)) throw new Error(`Unknown option --${name}`);
  const domain = (flags.domain ?? env.ENGINEBOX_EMAIL_DOMAIN ?? "").toLowerCase();
  const options = {
    domain,
    origin: flags.origin ?? env.ENGINEBOX_DASHBOARD_ORIGIN ?? "",
    account: flags.account ?? env.CLOUDFLARE_ACCOUNT_ID ?? "",
    ownerEmail: (flags["owner-email"] ?? env.ENGINEBOX_OWNER_EMAIL ?? "").toLowerCase(),
    from: (flags.from ?? (domain ? `login@${domain}` : "")).toLowerCase(),
    fromName: flags["from-name"] ?? "Engine Box",
    identityDir: flags["identity-dir"] ?? "/opt/enginebox-control/state/identity",
    secretFile: flags["secret-file"] ?? resolve(homedir(), ".enginebox/email-gateway-secret"),
    envOut: flags["env-out"] ?? null,
    deploySite: !!flags["deploy-site"],
    apply: !!flags.apply,
  };
  return validate(options);
}

export function validate(o) {
  const problems = [];
  if (!DOMAIN.test(o.domain)) problems.push("--domain must be a domain name such as example.com");
  let origin = null;
  try {
    origin = new URL(o.origin);
  } catch {}
  if (!origin || origin.origin !== o.origin || origin.protocol !== "https:") problems.push("--origin must be the dashboard's exact HTTPS origin, without a path");
  if (!/^[a-f0-9]{32}$/.test(o.account)) problems.push("--account must be the 32-hex Cloudflare account ID");
  if (!EMAIL.test(o.ownerEmail)) problems.push("--owner-email must be the instance owner's email");
  if (!EMAIL.test(o.from) || !o.from.endsWith(`@${o.domain}`)) problems.push("--from must be an address on --domain");
  if (!isAbsolute(o.identityDir)) problems.push("--identity-dir must be absolute");
  if (!isAbsolute(o.secretFile)) problems.push("--secret-file must be absolute");
  if (!o.fromName || o.fromName.length > 80 || /[\u0000-\u001f<>]/.test(o.fromName)) problems.push("--from-name must be 1-80 printable characters");
  if (problems.length) throw new Error(problems.join("\n"));
  return o;
}

/** The dashboard environment the supervisor needs. The secret is a placeholder unless `secret` is given. */
export function dashboardEnvironment(o, secret = "<contents of the secret file>") {
  return {
    ENGINEBOX_DASHBOARD_ORIGIN: o.origin,
    ENGINEBOX_IDENTITY_STATE_DIR: o.identityDir,
    ENGINEBOX_OWNER_EMAIL: o.ownerEmail,
    ENGINEBOX_EMAIL_ENDPOINT: `${o.origin}/_email/send`,
    ENGINEBOX_EMAIL_SECRET: secret,
    ENGINEBOX_CLIENT_IP_HEADER: "cf-connecting-ip",
  };
}

const gatewayConfig = () => resolve(repo, ".state/email-gateway/wrangler.json");

/**
 * Every step, in order. `run` steps execute only with --apply; `check` steps
 * are read-only and always run. Commands never contain a secret.
 */
export function plan(o) {
  const wrangler = (...args) => ["pnpm", "--dir", resolve(repo, "packages/email-gateway"), "exec", "wrangler", ...args];
  const steps = [
    { id: "login", kind: "check", say: "Wrangler is logged in to Cloudflare", command: wrangler("whoami") },
    { id: "sending-list", kind: "check", say: `Domains with Email Sending (is ${o.domain} listed? If the zone is missing, register or add it in the Cloudflare dashboard first)`, command: wrangler("email", "sending", "list") },
    { id: "sending-enable", kind: "run", say: `Enable Email Sending on ${o.domain} (Cloudflare adds the SPF and DKIM DNS records)`, command: wrangler("email", "sending", "enable", o.domain) },
    { id: "sending-dns", kind: "check", say: `DNS records Email Sending needs on ${o.domain} (add a DMARC TXT record at _dmarc.${o.domain} if there is none)`, command: wrangler("email", "sending", "dns", "get", o.domain) },
    { id: "secret", kind: "run", say: `Create the shared email secret in ${o.secretFile} (0600, kept if it exists)` },
    {
      id: "configure",
      kind: "run",
      say: "Write the gateway's private wrangler config (.state/email-gateway/wrangler.json)",
      command: ["node", resolve(repo, "packages/email-gateway/configure.mjs")],
      env: { ENGINEBOX_EMAIL_ACCOUNT_ID: o.account, ENGINEBOX_EMAIL_FROM: o.from, ENGINEBOX_EMAIL_FROM_NAME: o.fromName, ENGINEBOX_DASHBOARD_ORIGIN: o.origin },
    },
    { id: "deploy-gateway", kind: "run", say: "Deploy the email gateway Worker", command: wrangler("deploy", "--config", gatewayConfig()) },
    { id: "gateway-secret", kind: "run", say: "Set EMAIL_GATEWAY_SECRET on the gateway (piped from the secret file)", command: wrangler("secret", "put", "EMAIL_GATEWAY_SECRET", "--config", gatewayConfig()), stdin: "secret" },
  ];
  if (o.deploySite)
    steps.push({ id: "deploy-site", kind: "run", say: "Redeploy the site Worker with its EMAIL_GATEWAY binding", command: ["pnpm", "--dir", resolve(repo, "packages/site"), "run", "deploy"] });
  steps.push({
    id: "dashboard-env",
    kind: "run",
    say: o.envOut ? `Write the dashboard environment fragment to ${o.envOut} (0600) for the supervisor's environment.json` : "Print the dashboard environment to add to the supervisor's environment.json (secret shown as a placeholder)",
  });
  return steps;
}

function ensureSecret(file) {
  if (existsSync(file)) {
    const value = readFileSync(file, "utf8").trim();
    if (value.length < 32) throw new Error(`${file} exists but holds fewer than 32 characters; replace it.`);
    return value;
  }
  mkdirSync(dirname(file), { recursive: true, mode: 0o700 });
  const value = randomBytes(32).toString("hex");
  writeFileSync(file, `${value}\n`, { mode: 0o600, flag: "wx" });
  chmodSync(file, 0o600);
  return value;
}

/** True when `wrangler email sending list` already shows the domain enabled (re-runs and pre-enabled zones). */
export function sendingEnabled(o, spawn = spawnSync) {
  const result = spawn("pnpm", ["--dir", resolve(repo, "packages/email-gateway"), "exec", "wrangler", "email", "sending", "list"], {
    cwd: repo,
    env: { ...process.env, ...(o.account ? { CLOUDFLARE_ACCOUNT_ID: o.account } : {}) },
    encoding: "utf8",
  });
  if (result.status !== 0) return false;
  const domain = o.domain.toLowerCase();
  return String(result.stdout ?? "")
    .split("\n")
    .some((line) => {
      const cells = line.split(/[│|]/).map((c) => c.trim().toLowerCase());
      return cells.includes(domain) && cells.includes("yes");
    });
}

export function writeEnvironment(file, o, secret) {
  writeFileSync(file, `${JSON.stringify(dashboardEnvironment(o, secret), null, 2)}\n`, { mode: 0o600, flag: "wx" });
  chmodSync(file, 0o600);
}

function execute(step, o, log, spawn = spawnSync) {
  const input = step.stdin === "secret" ? `${ensureSecret(o.secretFile)}\n` : undefined;
  const result = spawn(step.command[0], step.command.slice(1), {
    cwd: repo,
    env: { ...process.env, ...(o.account ? { CLOUDFLARE_ACCOUNT_ID: o.account } : {}), ...(step.env ?? {}) },
    input,
    stdio: [input === undefined ? "inherit" : "pipe", "inherit", "inherit"],
    encoding: "utf8",
  });
  if (result.status !== 0) throw new Error(`Step "${step.id}" failed (exit ${result.status ?? "signal"}).`);
  log(`  done: ${step.id}`);
}

export function run(o, { log = console.log, spawn = spawnSync } = {}) {
  log(o.apply ? "Applying the orgs setup:" : "Plan only (re-run with --apply to make changes):");
  for (const step of plan(o)) {
    log(`- ${step.say}`);
    if (step.command) log(`    $ ${step.command.join(" ")}`);
    if (step.kind === "check") {
      execute(step, o, log, spawn);
      continue;
    }
    if (!o.apply) continue;
    if (step.id === "sending-enable" && sendingEnabled(o, spawn)) {
      log(`  done: ${step.id} (Email Sending is already enabled on ${o.domain})`);
    } else if (step.id === "secret") {
      ensureSecret(o.secretFile);
      log("  done: secret");
    } else if (step.id === "dashboard-env") {
      if (o.envOut) {
        writeEnvironment(o.envOut, o, ensureSecret(o.secretFile));
        log(`  done: wrote ${o.envOut}`);
      } else {
        log(JSON.stringify(dashboardEnvironment(o), null, 2));
      }
    } else {
      execute(step, o, log, spawn);
    }
  }
  if (!o.apply) log(JSON.stringify(dashboardEnvironment(o), null, 2));
  log("Then restart the supervisor and sign in with your email. Checklist: docs/SELF-HOSTING-ORGS.md");
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    run(parseArgs(process.argv.slice(2)));
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}
