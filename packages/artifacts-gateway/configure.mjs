// Writes the private deploy configuration .state/artifacts-gateway/wrangler.json.
//   ENGINEBOX_ARTIFACTS_ACCOUNT_ID   Cloudflare account ID (32 hex)        required
//   ENGINEBOX_ARTIFACTS_HOSTNAME     custom domain, e.g. artifacts.example.com required
//   ENGINEBOX_ARTIFACTS_PROJECTS     comma-separated Engine Box project IDs (default "default")
//   ENGINEBOX_ARTIFACTS_NAMESPACE_PREFIX  namespace prefix (default "enginebox")
//   ENGINEBOX_ARTIFACTS_WORKER_NAME  Worker name (default "enginebox-artifacts-gateway")
// Each project gets one Artifacts binding for namespace `<prefix>-<project>`;
// add a project here and redeploy before creating repositories in it.
import { mkdir, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const env = process.env;
const account = env.ENGINEBOX_ARTIFACTS_ACCOUNT_ID ?? "";
const hostname = env.ENGINEBOX_ARTIFACTS_HOSTNAME ?? "";
const prefix = env.ENGINEBOX_ARTIFACTS_NAMESPACE_PREFIX ?? "enginebox";
const name = env.ENGINEBOX_ARTIFACTS_WORKER_NAME ?? "enginebox-artifacts-gateway";
const projects = [...new Set((env.ENGINEBOX_ARTIFACTS_PROJECTS ?? "default").split(",").map((p) => p.trim()).filter(Boolean))];
const PROJECT = /^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$/;
if (!/^[a-f0-9]{32}$/.test(account) || !/^[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?\.[a-z]{2,}$/.test(hostname) || hostname.endsWith(".workers.dev") ||
    !/^[a-z0-9][a-z0-9-]{0,13}$/.test(prefix) || !/^[a-z0-9][a-z0-9-]{0,62}$/.test(name) ||
    !projects.length || projects.length > 64 || !projects.every((p) => PROJECT.test(p))) {
  throw new Error("Set valid ENGINEBOX_ARTIFACTS_ACCOUNT_ID, ENGINEBOX_ARTIFACTS_HOSTNAME (not workers.dev) and ENGINEBOX_ARTIFACTS_PROJECTS.");
}
const artifacts = projects.map((project, i) => ({ binding: `ARTIFACTS_${i}`, namespace: `${prefix}-${project}` }));
const directory = new URL("../../.state/artifacts-gateway/", import.meta.url);
await mkdir(directory, { recursive: true, mode: 0o700 });
await writeFile(new URL("wrangler.json", directory), JSON.stringify({
  name,
  account_id: account,
  main: fileURLToPath(new URL("src/index.mjs", import.meta.url)),
  compatibility_date: "2026-10-23",
  workers_dev: false,
  preview_urls: false,
  observability: { enabled: true },
  routes: [{ pattern: hostname, custom_domain: true }],
  artifacts,
  vars: {
    NAMESPACE_PREFIX: prefix,
    NAMESPACES: JSON.stringify(Object.fromEntries(artifacts.map((a) => [a.namespace, a.binding]))),
  },
  durable_objects: { bindings: [{ name: "REPLAY", class_name: "ReplayGuard" }] },
  migrations: [{ tag: "v1", new_sqlite_classes: ["ReplayGuard"] }],
}, null, 2) + "\n", { mode: 0o600 });
console.log(`Wrote private .state/artifacts-gateway/wrangler.json (${artifacts.length} namespace${artifacts.length === 1 ? "" : "s"}).`);
