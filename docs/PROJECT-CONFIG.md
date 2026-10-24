# Repository-defined development environments

A project can select its dev template in `.enginebox/config.json`:

```json
{"schema_version":1,"dev":{"template":"./templates/web-dev.json"}}
```

Local paths resolve inside `.enginebox/templates/`. A service-catalog reference is
`"web-dev@1"`: both ID and revision are required. The config has no separate
dependency list. The template supplies the runtime, setup and startup contract;
application packages remain in their package manager's manifests and lockfiles.
The [vinext fixture](../packages/environment-service/test/vinext/.enginebox/config.json)
is a complete importable example.

## Dev template v1

```json
{
  "schema_version": 1,
  "kind": "dev",
  "id": "web-dev",
  "revision": 1,
  "image": "registry.example.com/web@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "prepare": ["pnpm", "install", "--frozen-lockfile"],
  "command": ["pnpm", "dev"],
  "cwd": ".",
  "installInputs": ["package.json", "pnpm-lock.yaml"],
  "ports": [{"name":"web","protocol":"http","port":3000,"expose":true}],
  "health": {"port":"web","path":"/health"},
  "env": {},
  "cpu": 1,
  "memoryMiB": 1024,
  "timeoutSeconds": 300,
  "writableDirs": ["node_modules/.vite"]
}
```

Replace the example image with a real digest-pinned image containing the required
toolchain. A missing image is pulled and cached by Docker; template installation
receipts report success or failure under the service's private state directory.
Setup executes inside a bounded, unprivileged container with only a disposable
source copy. It cannot mount the project's persistent data, a host shell, the
Docker socket or controller credentials. This format does not build Dockerfiles
or install packages on the host. Custom runtime images must already be published.

`prepare` is optional. `command`, `image`, `installInputs`, `ports` and `health`
are required. `cwd` defaults to the source root. CPU, RAM and startup timeout
default to 1, 1024 MiB and 300 seconds, subject to operator limits. Declare every
file affecting dependency installation in `installInputs`, including workspace
package manifests and lockfiles; paths are literal, relative and must exist.
There are no globs or implicit package-manager guesses. Setup and startup argv
are executed directly; use an explicit shell inside the container when needed.

Ports are named container ports. Engine Box reserves host ports from its own
configured range. `expose: true` requests an HTTP route under the configured
domain and access policy; HTTP WebSockets use that same route. TCP/UDP/TLS ports
can be declared, but their external listeners still require operator routing.
The dev command must listen on `0.0.0.0`. Engine Box supplies
`ENGINEBOX_PUBLIC_HOST` and `ENGINEBOX_PUBLIC_URL` for the exposed readiness port
(or first exposed HTTP port); frameworks use these for allowed-host/HMR settings.
No framework-specific reload code is injected.

`env` contains ordinary nonsecret values. Secrets and provider credentials must
not appear in tracked files. Secret references are not implemented by this
version. Source remains read-only at runtime; `writableDirs` grants bounded
ephemeral caches. Persistent simulated bindings and application data belong in
`ENGINEBOX_DATA_DIR` (`/data`). Cache paths cannot overlap tracked source files.

Dev templates, native-agent VM templates and CI toolchain templates have different
validated schemas and execution lifetimes. A CI automation continues selecting
its own `job.template`; a native or CI template cannot be used as a dev runtime.
Existing CI/CD automation formats are unchanged. Main promotion runs no CD job.

## Import and startup

After a successful reviewed import, the controller queues dev bootstrap using
the same path for dashboard and MCP clients. Import completion and dev readiness
are separate results. A repo without the config reports `not-configured` and is
still imported. Invalid configuration, unavailable templates and failed setup
are visible failures, not successful startup. Repeated registration is idempotent.

Use MCP `storage_dev_start` with `host_id` and `project` to bootstrap an existing
project or retry a failure. Poll its `fleet_job`, then `storage_dev_status` for
readiness, errors and URLs. The Storage UI has the same start/retry and status
controls; Environments displays project readiness and links. If the dev service
is unavailable during import, source import remains committed and reports that
dev setup was not queued. Retry explicitly; the controller does not replay imports.

The service reads only immutable exports of accepted local LayerFS main. Config,
local templates and declared install inputs are bound to that export's hashes.
The private project receipt records the resolved template digest, input hashes,
selected ports and active environment. No generated lockfile is written back to
LayerFS or Git. Template revision/digest plus application lockfiles provide the
reproducible inputs; image caches and installation receipts remain private.

Code-only promotions update files under the existing dev process and retain its
dependencies and data. Config/template/install-input changes prepare a replacement
away from the live workspace. A preparation failure keeps the old app serving.
The old process stops before the replacement can mount the same writable data.
After readiness succeeds, routes switch; failed readiness restores the old
process. These runtime changes can briefly interrupt service. An interrupted
handoff is recovered before another writer starts. Rollback never undoes app
database writes or migrations. Identical failed inputs are not retried on every
poll; a new source revision or explicit retry permits another attempt.

## Operator setup

Enable `bootstrap` in the private environment service config:

```json
{
  "hosts": [{"id":"node-one","command":["/opt/enginebox/export-project"]}],
  "templates": [],
  "portRange": [20000, 20999],
  "domain": "dev.example.com",
  "urlScheme": "https",
  "access": "authenticated",
  "maxCpu": 2,
  "maxMemoryMiB": 2048,
  "maxProjects": 8
}
```

The protected command receives the project ID as its final argv item and returns
the existing `ebox storage layerfs dev-export PROJECT` response. It must select
the configured owning storage node, executable and private LayerFS config;
these cannot come from a repo or HTTP request. The environment service requires
local access to the immutable export files. `templates` is an operator-provided
catalog of dev manifests available to this service, not the native fleet template
registry. The service supports at most 24 managed projects and 64 total active,
staged and static environment records. Reserve host capacity and a port range
that other services do not use. A busy external port fails readiness rather than
taking over its process.

The domain must be allowed by `baseDomains`; configure wildcard DNS, ingress and
TLS for it. The route hostname is a stable project key plus port name. The public
URL scheme can differ from the private gateway transport. Authenticated exposure
requires the service's existing gateway authentication configuration.

Create `<controller-state>/storage/dev-services.json` to bind the registered storage
host to its dev service:

```json
{"schema_version":1,"hosts":{"node-one":{"url":"http://127.0.0.1:18790/","token_file":"/private/environment-api-token"}}}
```

The token file is on the controller, not in the repo or source node's command.
The endpoint must be HTTPS or loopback HTTP. Requests cannot override it.
`POST /v1/environments/projects` accepts `{ "host": "node-one", "project": "app" }`
and optional `retry: true`, returning a durable queued registration. `GET` reads
project status. Both require the existing private service credential. Exports,
source authority and source-host bindings remain operator-controlled.

This initial adapter covers the existing single-owner Linux Docker service and
local LayerFS export path. It does not claim distributed dev placement, automatic
native template installation on every fleet node, data forks or secret delegation.
