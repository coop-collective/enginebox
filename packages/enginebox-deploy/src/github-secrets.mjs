// Trusted operator adapter for Kitbash's allowlisted Infisical `run` workflow.
// Run outside the coding-agent boundary. Values never go to stdout or argv.
import { mkdir, writeFile, rm } from "node:fs/promises";
import { isAbsolute, resolve } from "node:path";
import { pathToFileURL } from "node:url";

export const secretFiles = Object.freeze({
  ENGINEBOX_GITHUB_REPOS_CLIENT_SECRET: "repos-client-secret",
  ENGINEBOX_GITHUB_REPOS_PRIVATE_KEY: "repos-private-key",
  ENGINEBOX_GITHUB_REPOS_WEBHOOK_SECRET: "repos-webhook-secret",
  ENGINEBOX_GITHUB_RUNNERS_CLIENT_SECRET: "runners-client-secret",
  ENGINEBOX_GITHUB_RUNNERS_PRIVATE_KEY: "runners-private-key",
  ENGINEBOX_GITHUB_RUNNERS_WEBHOOK_SECRET: "runners-webhook-secret",
});

export async function materialize(directory, env = process.env, mode = false) {
  if (!directory || !isAbsolute(directory)) throw new Error("Choose a new absolute private credential directory.");
  if (![false, true, "single-app"].includes(mode)) throw new Error("Unknown GitHub credential mode.");
  const entries = Object.entries(secretFiles).filter(([name]) => mode === "single-app" ? name.includes("_RUNNERS_") : !mode || name.includes("_REPOS_"));
  for (const [name] of entries) {
    const value = env[name];
    if (typeof value !== "string" || !value.trim() || value.length > 65536 || value.includes("\0")) throw new Error(`Missing or invalid ${name}.`);
    if (name.endsWith("WEBHOOK_SECRET") && value.trim().length < 32) throw new Error(`Invalid ${name}; use a broker-generated application key.`);
    if (name.endsWith("PRIVATE_KEY") && !/^-----BEGIN (RSA )?PRIVATE KEY-----\r?\n/.test(value)) throw new Error(`Invalid ${name}; use the provider-issued PEM.`);
  }
  // mkdir is exclusive: existing credentials are never overwritten or rotated.
  await mkdir(directory, { mode: 0o700 });
  try {
    for (const [name, file] of entries) await writeFile(resolve(directory, file), env[name], { mode: 0o600, flag: "wx" });
  } catch {
    await rm(directory, { recursive: true, force: true });
    throw new Error("Credential staging failed; newly created files removed.");
  }
  return { created: true, names: entries.map(([name]) => name), files: entries.length };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    const args = process.argv.slice(2);
    if (args[0] !== "--directory" || ![2, 3].includes(args.length) || (args.length === 3 && !["--repos-only", "--single-app"].includes(args[2]))) throw new Error("Usage: github-secrets.mjs --directory /private/new-directory [--single-app | --repos-only]");
    console.log(JSON.stringify(await materialize(args[1], process.env, args[2] === "--single-app" ? "single-app" : args[2] === "--repos-only")));
  } catch {
    console.error("GitHub secret staging failed. Check registered secret names, file formats, and a new writable absolute destination; existing directories are preserved.");
    process.exitCode = 1;
  }
}
