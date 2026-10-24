# Persistent development services and exposure

`packages/environment-service` is an optional Linux Docker supervisor for a
project's persistent development service. It follows a configured Git branch or
the Cloudflare project's **accepted** generation, prepares exact code, starts a
bounded container, checks readiness and publishes Traefik routes. The dashboard's
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

Only one process owns writable data: the old process stops before new code starts.
Readiness failure restores prior code and records the failed revision without
repeated retries; a new source revision permits another attempt. Docker supervises
containers while the control daemon restarts. Ownership labels prevent adoption
or removal of unrelated containers. A protected recipe change requires a new
environment ID. Updates can briefly interrupt connections; clients must reconnect.
Framework HMR/WebSockets pass through; Enginebox does not inject reload scripts.
The disposable test app reconnects a WebSocket and reloads on revision changes.
Code rollback does **not** undo database writes/migrations. Schema changes need
application-specific migration and backup procedures.

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
is immutable; a subsequent task uses a new workspace/preview. Live editing/HMR of
an open LayerFS mount and automatic project enrollment are separate capabilities.

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
