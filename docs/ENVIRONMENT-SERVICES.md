# Persistent development services and exposure

`packages/environment-service` is an optional Linux Docker supervisor for a
project's persistent development service. It follows LayerFS main, a closed preview,
the Cloudflare project's **accepted** generation, or a configured Git branch.
It prepares exact code, starts a bounded container, checks readiness and publishes
Traefik routes. The dashboard's
**Environments** section shows running/desired revisions and manages routes. Its
private API token never reaches the browser. CI and agent execution remain separate.

## Source and data ownership

An operator provisions each source, digest-pinned image, preparation/start argument
vectors, ports and resource limits. Agents cannot alter these through submitted
code. Application containers receive no Docker socket, provider home, GitHub write
token or fleet owner credential. Prepared code is read-only; `/data` is a separate
persistent directory, also named by `ENGINEBOX_DATA_DIR`. Writable application
state must use that directory or `/tmp`.

Git polling follows only the configured branch and exact commit. An unmerged agent
branch cannot update the service. Git is optional; normal LayerFS workspaces stay
Git-free. `source.kind: "accepted"` reads the Cloudflare authority's accepted head,
then requires its generation metadata to bind `integration`, `treeHash`,
`layerfsRef` and `layerfsRoot`. The integration export at
`<exportsDir>/<integration>/source.json` and `source/` must match that provenance.
Files are copied and hashed before execution. Changed bytes, escaping links and
Git metadata are refused. Publishing a candidate alone never authorizes activation.

The generic [same-Store integration executor](LOGICAL-INTEGRATION.md) has separate
native qualification. This adapter neither merges branches nor imports a Store.
Its accepted-source activation handoff is tested with an HTTP authority fixture;
that handoff still needs a live fleet-to-service qualification. A missing export binding is
reported explicitly while the existing service remains available. Cross-node
export transport and private Git authentication are not supplied here.

With the default `updateMode: "restart"`, only one process owns writable data:
the old process stops before new code starts.
Readiness failure restores prior code and records the failed revision without
repeated retries; a new source revision permits another attempt. Docker supervises
containers while the control daemon restarts. Ownership labels prevent adoption
or removal of unrelated containers. A protected recipe change requires a new
environment ID. Updates can briefly interrupt connections; clients must reconnect.
Framework HMR/WebSockets pass through; Enginebox does not inject reload scripts.
The disposable test app reconnects a WebSocket and reloads on revision changes.
Code rollback does **not** undo database writes/migrations. Schema changes need
application-specific migration and backup procedures.

## Stable dev with a running file watcher

Use `updateMode: "files"` for a persistent dev command such as `wrangler dev` or
Vite. Accepted main is the project's **stable dev**, including its locally simulated
bindings. A promotion updates files inside the existing source bind mount. The
container, command, port, installed dependencies and `/data` stay in place; no
Enginebox build, process replacement or CD deployment runs for that source update.
The dev server owns rebuilding, request handling and browser HMR/reload. HTTP and
WebSocket traffic use the same generic port route.

The environment runs the project's configured dev command. For a vinext project,
use `"dev": "vinext dev"` in `package.json` and start `pnpm dev` (or the configured
package-manager equivalent). Vinext/Vite owns browser HMR; the Cloudflare Vite
plugin can keep server execution and bindings locally simulated in workerd. Do
not substitute a standalone `wrangler dev` command for the framework's dev script.
The supervisor has no framework selection or custom reload protocol.

The importable `test/vinext` fixture demonstrates this setup with a pinned pnpm
lockfile, local KV under `/data`, an explicit allowed public host and Vite's HMR
client pointed at the public HTTPS port. Its cache mounts are documented in its
README. `test/live-vinext-browser.mjs` checks the public WebSocket, a source update
without page navigation, retained React state and retained simulated KV state.

```json
{
  "updateMode":"files", "writableDirs":[".wrangler","node_modules/.mf"],
  "source":{"kind":"layerfs", "project":"app",
    "command":["/usr/bin/sudo","-n","/opt/enginebox/export-app-main"]},
  "prepare":["npm","ci","--cache","/tmp/npm-cache"],
  "command":["npm","run","dev"],
  "ports":[{"name":"web","protocol":"http","container":8787,"host":18123}],
  "health":{"port":"web","path":"/health"}
}
```

These fields augment the operator-provisioned environment recipe below. For
Wrangler, the project's dev script can use
`wrangler dev --local --ip 0.0.0.0 --port 8787 --live-reload --persist-to /data/wrangler`.
`--local` disables remote bindings; `--live-reload` supplies Wrangler's browser
reload connection. This is a project command, not a framework-specific supervisor.
Configure other dev servers' external host/HMR settings in their own project.

Only verified `layerfs` and `accepted` sources support this mode. Every incoming
file is verified in staging before live writes. Only changed managed files are
replaced, each with an atomic rename; removed managed files are deleted. Unchanged
files, untracked dependencies and generated files survive. An ownership journal
allows an interrupted update to resume. Source errors leave a healthy process and
its route running. On supervisor restart, owned containers receive a fresh health
check before their routes are restored, without waiting for source retrieval.

The app cannot write its source bind mount. `writableDirs` grants bounded, ephemeral
tmpfs directories for framework caches (for example `.wrangler` or
`node_modules/.mf` or `node_modules/.vite`); they cannot overlap managed source. Put durable simulated
databases in `/data`, not these caches. Dependencies are prepared once. Changes to
dependencies, runtime image, command or writable mount configuration may require a
new environment; there is no automatic in-process dependency installation.
Choose an image with the runtime prerequisites (including system CA certificates
for workerd). npm's cache belongs in `/tmp`; the private home has a small quota.
`ENGINEBOX_REVISION` is the startup revision, while the API's revision identifies
the applied source tree, not proof of framework compilation success.

File replacement is atomic per file, not across a whole tree. Dev servers must
watch/debounce changes and handle rebuilds themselves. Invalid application code or
a framework that stops serving during rebuilds can still cause errors; this mode
does not promise universal zero downtime or automatic code rollback.

Wrangler's built-in reload uses `/cdn-cgi/live-reload`. Cloudflare reserves
`/cdn-cgi/` before requests reach a Worker, so the optional public Worker edge
cannot transparently carry that particular connection. A direct port connection
(including an SSH forward) supports it. Public reload needs an ingress that
preserves that path, or a project dev server with a configurable HMR endpoint.
Do not claim public Wrangler reload merely because ordinary WebSockets work.
See Cloudflare's [reserved endpoint documentation](https://developers.cloudflare.com/fundamentals/reference/cdn-cgi-endpoint/).
This restriction is specific to that Wrangler reload endpoint, not vinext/Vite's
HMR transport. Keep the framework's own dev command and configure its public host
and WebSocket client port for the proxy.

## Networking

| Service | Routing identity | Behavior |
| --- | --- | --- |
| HTTP / WebSocket | Approved hostname | Host routing, streaming and Upgrade; optional gateway login |
| TLS TCP | SNI plus provisioned listener | TLS passthrough; application owns certificates/authentication |
| Plain TCP | Provisioned listener/port | One service per listener; application owns authentication |
| UDP | Provisioned listener/port | Datagram/session forwarding; no hostname inspection |

Plain TCP and UDP have no generic domain field. DNS can name the gateway, but a
subdomain cannot distinguish arbitrary UDP services sharing an IP/port. The API
rejects hostnames on raw routes and conflicting claims on transport listeners.
Only approved hostname suffixes, provisioned services and listeners are allowed.
Authenticated HTTP routes require configured password hashes and strip credentials
before forwarding. Public access is explicit. Private transport listeners must
bind loopback/private networks; public listeners require deliberate infrastructure
configuration and application access controls.

The dashboard manages exposure; Traefik carries traffic independently. Generated
routes use serialized atomic writes and omit unhealthy environments. Unknown hosts
return 404. The optional `edge/worker.mjs` streams HTTP/WebSocket traffic through a
fixed Workers VPC Service to the private gateway, rejects unknown public hosts and
strips spoofed forwarding headers. Add each hostname to its allowlist and provision
custom-domain DNS/TLS before sharing. **Saving a dashboard route does not create DNS
or certificates.** External domains require owner-controlled validation. Public
TCP/UDP needs a direct endpoint, private network or separately provisioned Spectrum
service; the HTTP Worker cannot accept arbitrary transport traffic.

## Provisioning

Use Node 24+, Docker and a pinned Traefik release. Qualification used Node 24.22.1,
Traefik 3.7.16 with the official archive checksum, and digest-pinned Node/Ubuntu
images. Traefik receives no Docker socket. Create a `0700` state directory, `0600`
random API-token file and private operator configuration:

```json
{
  "schema":1, "stateDir":"/srv/enginebox-environments/state",
  "listenPort":18790, "tokenFile":"/srv/enginebox-environments/api-token",
  "baseDomains":["dev.example.com"], "httpEntrypoint":"web", "httpTls":false,
  "basicAuthUsers":[], "transportEntrypoints":[], "pollSeconds":5,
  "environments":[{
    "id":"main",
    "source":{"kind":"git", "url":"https://github.com/example/app.git", "branch":"main"},
    "image":"node@sha256:REPLACE_WITH_VERIFIED_IMAGE_DIGEST",
    "prepare":["npm","ci"], "command":["node","server.mjs"],
    "cpu":2, "memoryMiB":2048, "timeoutSeconds":120,
    "health":{"port":"web","path":"/health"},
    "ports":[{"name":"web","protocol":"http","container":3000,"host":18081}]
  }]
}
```

Replace the image placeholder; tags are rejected. Empty `basicAuthUsers` allows
only explicitly public HTTP routes; supply password hashes for authenticated
routes. `httpTls` requires certificates on Traefik's entrypoint. Behind a private
VPC connection HTTP can stay loopback while the edge terminates TLS.

Run `node packages/environment-service/src/main.mjs PRIVATE_CONFIG`. The API binds
only loopback. Traefik's file provider watches `<stateDir>/gateway/`; `routes.yml`
contains JSON-compatible YAML. Provision matching named entrypoints. A transport
entry is `{name,protocol:"tcp"|"udp",port,public}`; private entries must bind
loopback/private addresses. Run both daemons under a service manager. Containers
use bounded logs, CPU/RAM/PIDs, an unprivileged UID, dropped capabilities and a
read-only root filesystem. Retain state and data when updating the package.

Configure dashboard `ENGINEBOX_ENVIRONMENTS_URL` (loopback HTTP or HTTPS) and
`ENGINEBOX_ENVIRONMENTS_TOKEN`. Dashboard login and same-origin mutation checks
protect `GET /api/environments` and `POST /api/environments/routes`. Private API
paths are `/v1/environments` and `/v1/environments/routes`. HTTP route upsert:
`{id,environment,service,hostname,access:"authenticated"|"public"}`. Raw routes
use `entrypoint` instead of hostname and inherit the listener's public/private
policy. Remove with `{remove:id}`. TLS routes supply both hostname and entrypoint.

An accepted-source configuration is:

```json
{"kind":"accepted", "controller":"https://fleet.example.com", "project":"app",
 "tokenFile":"/srv/enginebox-environments/project-read-token",
 "exportsDir":"/srv/integration-exports"}
```

Use a project-scoped read credential and make immutable exports visible to the
service node. The package does not provision that integration binding automatically.

## Verification and limits

`pnpm test` verifies source selection, copies, acceptance/provenance, tampering,
recovery, route conflicts, credential boundaries and the edge. `pnpm test:e2e`
verifies dashboard operations. Set `ENGINEBOX_TEST_FLEET_PORT` and
`ENGINEBOX_TEST_DASHBOARD_PORT` for an isolated run if defaults are occupied.
`test/live-probe.mjs PRIVATE_CONFIG VERSION OUTPUT` explicitly tests the disposable
fixture through actual Traefik: HTTP, WebSocket Upgrade/data, TCP, two UDP sessions,
verified TLS/SNI passthrough, route add/remove and access rejection. It does not
qualify public UDP exposure.

`test/wrangler` is a standalone importable fixture pinned to Wrangler 4.152.0.
Its package lock belongs to that imported project. The opt-in
`test/live-watch-browser.mjs URL BEFORE_VERSION AFTER_VERSION OUTPUT` requires
`ENGINEBOX_LIVE_WATCH=1` and dashboard Playwright dependencies. It keeps a browser
open while an operator promotes a new fixture revision, requires a successful
WebSocket handshake and a real reload without calling `page.reload`, and checks
that simulated KV identity and interactions survive. It writes `OUTPUT.ready`
before the promotion and `OUTPUT` after success. Separately probe HTTP continuously
and compare container ID, start time, restart count and Wrangler PIDs across the
promotion; the browser test alone does not prove process continuity.

Heavy CI and dev services should use separate pools or resource-limited VMs/
containers. This is a single-owner, single-node service, not HA or a multi-tenant
sandbox. It does not allocate physical nodes, install macOS/Windows images, manage
public transport firewalls, automatically provision DNS/TLS, capture online data
snapshots or garbage-collect old releases. Provision filesystem quotas, backups,
retention and network policy on a dedicated service node before general rollout.
Never put persistent data in the agent cache-eviction namespace. A disposable live
deployment does not mean every agent repository was configured automatically.

References: [Traefik TCP](https://doc.traefik.io/traefik/reference/routing-configuration/tcp/routing/router/),
[Traefik UDP](https://doc.traefik.io/traefik/reference/routing-configuration/udp/routing/router/),
[Workers VPC](https://developers.cloudflare.com/workers-vpc/api/),
[Spectrum](https://developers.cloudflare.com/spectrum/).

## Local LayerFS development previews

The single-owner node adapter supports `source.kind: "layerfs"`. Its protected
operator command calls `ebox storage layerfs dev-export PROJECT`, optionally with
`--workspace CLOSED_WORKSPACE`. Main exports follow owned LayerFS main; workspace
exports require a completed, closed checkpoint. A preview can therefore run before
promotion and before any GitHub push. Starting development does not require CI/CD.
Build/test inside the writable LayerFS workspace, checkpoint and close it, then
start its preview. Promotion updates the main service independently of external
GitHub availability.

```json
{"kind":"layerfs", "project":"app", "workspace":"task-one",
 "command":["/usr/bin/sudo","-n","/opt/enginebox/export-app-preview","task-one"]}
```

The command and arguments are administrator-owned configuration, never repository
content or API input. It must return the `dev-export` JSON with `revision`,
`sourceRoot` and `manifest`; a node helper may copy the snapshot into a private
service-readable directory and adjust `sourceRoot`. The service checks project,
workspace, authority, inventory digest and copied file bytes before preparation.
Configure only the intended project and source selector in that helper. Neither
repository credentials nor the node's writable Store belong in the app container.

This adapter identifies its authority as `local-layerfs`. It does not substitute
for the distributed controller's accepted-generation/fencing contract. The
`accepted` adapter remains the path for distributed integration. A closed preview
is immutable; a subsequent task uses a new workspace/preview. File-watcher mode
refreshes accepted main after promotion. Streaming edits from an open LayerFS
workspace and automatic project enrollment remain separate capabilities.

Live qualification found that Docker's archive API does not observe LayerFS mounts
added after container creation. Both dev and GitHub exports now materialize files
inside the sandbox mount namespace before copying them out. Retained older GitHub
exports without the corrected schema are refused. Tests require the actual app
files and selected version, so an empty export cannot pass as a successful demo.
The private test repo exercised import, Git-free editing, tests/build, checkpoint,
branch preview before promotion, an external GitHub commit after promotion, and
main-service refresh with persistent data retained. No Actions workflow or CD job
ran for the app. See `packages/enginebox-storage-layerfs/tests/live-github-dev.py`
and `test/live-project-probe.mjs` for the explicit opt-in qualification helpers.
