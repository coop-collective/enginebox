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
not appear in tracked files. Schema version 2 adds secret *requirements* by name
(see [Agent profiles and secret requirements](#agent-profiles-and-secret-requirements-schema-version-2));
the environment service validates and reports them but does not yet inject
values into dev containers. Source remains read-only at runtime; `writableDirs` grants bounded
ephemeral caches. Persistent simulated bindings and application data belong in
`ENGINEBOX_DATA_DIR` (`/data`). Cache paths cannot overlap tracked source files.

Dev templates, native-agent VM templates and CI toolchain templates have different
validated schemas and execution lifetimes. A CI automation continues selecting
its own `job.template`; a native or CI template cannot be used as a dev runtime.
Existing CI/CD automation formats are unchanged. Main promotion runs no CD job.

## Agent profiles and secret requirements (schema version 2)

`"schema_version": 2` keeps everything above and adds two things: named **agent
profiles** for runs on this repository, and **secret requirements** for both the
agent profiles and the `dev` section. Version 1 files keep working unchanged in
the environment service and the controller. In version 2 the `dev` section is
optional, so a repository may declare only agent profiles.

### Security model

Repository content is untrusted: any pull request or agent can edit it. So:

- A repository only **declares** what it wants: secret names with a qualifier and
  run kinds, catalog connections by name, its own MCP server URLs. It never
  contains a value and can never grant itself a connection or a secret.
- The org's [connections catalog](CONNECTIONS.md) **satisfies** a requirement only
  through an explicit **approved mapping**: repository + repository profile +
  requirement → a connection (a `secret_source` for a secret, an MCP connection
  for an MCP name). Only an org owner or admin approves one, and it is audited.
  Agents and service tokens with access to the repository may *propose* one.
- An approval records the qualifier it was granted for. If the repository changes
  a requirement's qualifier (`staging` → `production`, `read-only` → `write`) or a
  repository MCP server's URL, the mapping shows **needs re-approval** and no
  longer applies.
- A requirement without an approved mapping is **unmet**: the run starts without
  it and the run's details list it under `repo_profile.unmet`. If the requirement
  is marked `"required": true`, the start is refused (HTTP 409) instead.
- Repository-declared MCP servers carry a URL only, never headers, so they cannot
  pull a run's secrets into a request. They need approval like everything else.

### Format

```json
{
  "schema_version": 2,
  "dev": {"template": "./templates/web-dev.json",
          "secrets": [{"name": "DATABASE_URL", "qualifier": "staging"}]},
  "agents": {"profiles": {
    "<name>": {
      "default": true,
      "description": "…",
      "template": "native-dev", "size": "recommended",
      "setup": ["pnpm", "install", "--frozen-lockfile"],
      "dev": "dev",
      "test": ["pnpm", "test"],
      "env": {"NODE_ENV": "development"},
      "mcp": ["kitbash", {"name": "docs", "url": "https://docs.example.com/mcp"}],
      "secrets": [{"name": "DATABASE_URL", "qualifier": "staging", "run_kinds": ["builder"], "required": false}],
      "ci": [{"name": "unit", "command": ["pnpm", "test"]}]
    }
  }}
}
```

- **Profiles**: names match `^[a-z][a-z0-9-]{0,31}$`; at most 16; exactly one has
  `"default": true`.
- **Commands**: `setup`, `dev` and `test` are argv lists (1–32 arguments).
  `"dev": "dev"` reuses the `dev` section's command, so the dev command is defined
  once.
- **`env`**: non-secret variables under the [per-run environment](FLEET.md#per-run-environment)
  rules and denylist. They may not share a name with a secret requirement.
- **`mcp`**: catalog connection names, or `{name, url}` for a server the repository
  declares (HTTPS, no credentials).
- **`secrets`**: up to 64 requirements: `name` (a variable a run may set),
  `qualifier` (`^[a-z][a-z0-9_.:-]{0,63}$`, for example `staging`, `read-only`,
  `deploy`), optional `run_kinds` (which kinds of run need it; empty means every
  run), `required` and `description`.
- **`ci`**: optional named jobs (`name`, `command`, optional `template`). These are
  declared for tools that read them; Engine Box CI does not run them yet.
- `dev.secrets` uses the same requirement shape. Its mappings use the repository
  profile name `:dev`.

### Choosing a profile and what a run gets

A run on a hosted repository (`"repository": "<project>/<name>"`) uses the
repository's default agent profile unless the start request names another with
`"repo_profile": "debug"`. Ideator passes one per kind of work, together with
`run_kind`. For that profile and run kind the controller:

1. adds the profile's non-secret `env` and each met secret requirement's value
   (from the mapped secret source) to the run's env, which reaches the native CLI
   on stdin and is scrubbed from output like any run env value. Variables the
   start request sets itself win. Like any run env, these values sit in the run's
   private queue entry (0600) until admission;
2. with an Engine Box profile on the run, adds met MCP connections and approved
   repository servers to that provider's generated MCP config. Without one, MCP
   requirements are recorded as unmet (`needs_profile`);
3. skips requirements whose `run_kinds` exclude the run's kind (a run without a
   kind gets only requirements with no `run_kinds`);
4. records `repo_profile: {name, commit, met, unmet}` in the run's details, with
   names and reasons only.

`template`, `size`, `setup`, `dev`, `test` and `ci` are declarations for the
caller and for the dashboard. The controller records the profile but does not yet
choose a workspace template, run setup commands or start CI from them.

### Where the controller reads the config

Before each run on a repository the controller reads `.enginebox/config.json`
at the head of the repository's **default branch** and registers it (as
`registered_by: "repository"`). It uses a five-minute read token sent as a git
header from the environment (never argv), `git ls-remote` to see whether the
head moved, and a blobless shallow fetch only when it did; a head read in the
last 30 seconds is not read again. If the read fails, the last registered
config stays in force and `enginebox.repo_config.source_read_failed` is logged.
If the file is removed, a config that came from the repository is unregistered.

A config can also be **registered** through
`POST /v1/projects/{project}/repositories/{name}/agent-config {commit, config}`
or the `repo_config_register` MCP tool, for a repository without the file (or
before it lands). The repository's own file replaces it on the next run. Who
registers does not affect security, because a config only declares. A run on
another branch still uses the default branch's config: changes to what a
repository asks for take effect when they are merged.

### API and tools

| Route | Who | Purpose |
|---|---|---|
| `GET /v1/projects/{p}/repositories/{n}/requirements` | owner; service tokens with a repository grant | each profile's requirements with `status` (`met`, `unmet`, `proposed`, `needs_reapproval`) and mapping |
| `POST …/agent-config` | owner; service tokens with a repository grant | register the config (and commit) |
| `POST …/requirements/propose` | owner; service tokens with a repository grant | `{repo_profile, kind?, requirement, connection?, source_name?}` |
| `POST …/requirements/approve` | owner token only (dashboard: org owners and admins) | same body; replaces any earlier mapping for the requirement |
| `POST …/requirements/revoke` | owner token only | `{repo_profile, kind?, requirement}` |

`kind` is `secret` (default), `mcp` or `mcp_server`. `source_name` names the value
in the secret source when it differs from the requirement name. Every write is
audited (`repo.config_register`, `repo.requirement_propose`,
`repo.requirement_approve`, `repo.requirement_revoke`). The fleet MCP server
exposes `repo_requirements_get`, `repo_config_register` and
`repo_requirement_propose_mapping`, with no approve tool. The dashboard's
**Connections** page shows each repository's requirements with an **Approve**
action for owners and admins.

### Example: a Cloudflare Workers repository

```json
{
  "schema_version": 2,
  "dev": {
    "template": "./templates/workers-dev.json",
    "secrets": [{"name": "CLOUDFLARE_API_TOKEN", "qualifier": "read-only"}]
  },
  "agents": {"profiles": {
    "default": {
      "default": true,
      "description": "Build features against staging.",
      "template": "native-dev", "size": "recommended",
      "setup": ["pnpm", "install", "--frozen-lockfile"],
      "dev": "dev",
      "test": ["pnpm", "test"],
      "env": {"NODE_ENV": "development"},
      "mcp": ["kitbash"],
      "secrets": [{"name": "DATABASE_URL", "qualifier": "staging", "run_kinds": ["builder"]}]
    },
    "debug": {
      "description": "Read-only Cloudflare and database access for investigating production.",
      "template": "native-dev",
      "mcp": ["cf-readonly", {"name": "docs", "url": "https://developers.cloudflare.com/mcp"}],
      "secrets": [
        {"name": "CLOUDFLARE_API_TOKEN", "qualifier": "read-only"},
        {"name": "DATABASE_URL", "qualifier": "production-read-only"}
      ]
    },
    "deploy": {
      "description": "Run the tests, then deploy the Worker.",
      "template": "native-dev",
      "test": ["pnpm", "test"],
      "secrets": [{"name": "CLOUDFLARE_API_TOKEN", "qualifier": "deploy", "run_kinds": ["deploy"], "required": true}],
      "ci": [{"name": "deploy", "command": ["pnpm", "wrangler", "deploy"]}]
    }
  }}
}
```

An admin then approves, for example, `default` / `DATABASE_URL` → the
`canvalytics-staging` Kitbash source, `debug` / `CLOUDFLARE_API_TOKEN` → a
read-only token source, and `deploy` / `CLOUDFLARE_API_TOKEN` → the deploy token
source. A pull request that changes `debug`'s qualifier to `write`, or adds a new
requirement, gets nothing until someone approves it.

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
