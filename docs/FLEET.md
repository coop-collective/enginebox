# Fleet API and MCP

The fleet service creates prepared Incus VMs and manages their lifecycle through the same capacity checks for the dashboard and MCP clients. It operates as a trusted owner using configured SSH identities and administrative Incus access. It does not schedule agent prompts, pool accounts across unrelated owners or implement public multi-tenant hosting.

## Configure the hosts and templates

Start from [the fleet example](../examples/fleet.example.toml). Each entry identifies one machine, one Incus project and the storage pool against which new VMs are admitted. Configure a physical machine once; different SSH aliases for the same machine are not automatically recognized as duplicates.

```toml
schema_version = 1

[[hosts]]
id = "primary"
label = "My server"
target = "my-server"
project = "enginebox"
pool = "workspace-pool"
reserve_cpu = 2
reserve_memory_gib = 4
reserve_disk_gib = 10
disk_gib = 30
max_instances = 16

[hosts.guest]
uid = 1001
gid = 1001
user = "agent"
home = "/home/agent"
workspace = "/workspace"
display = ":99"

[[hosts.templates]]
id = "native-agents"
label = "Native agents with browser"
source = "ebox-base"
snapshot = "prepared-example"
```

Replace the snapshot name with the prepared snapshot returned by [bootstrap](BOOTSTRAP.md). The template must be a disk-only VM snapshot marked `user.enginebox.clean-base=true`, captured before provider login. It must contain the installed-version manifest and completed clone-identity setup. A configured template is checked when a creation job runs; appearing in inventory is not proof that its snapshot still exists or is valid.

Guest settings must match the prepared image. Unknown keys are rejected; in particular the guest path key is `workspace`, not `path`. Defaults are independent of environment variables used by another operator terminal. Legacy size presets use the host `disk_gib`; template Minimum/Recommended/Custom selections carry their own CPU, RAM and disk values. The fleet refuses automatic disk shrinking. Copies explicitly use the configured admission pool, even if the source template resides in another pool.

## Start the API

Install the runtime from the checkout, then create a private token file without printing the token:

```sh
umask 077
mkdir -p "$HOME/.local/state/enginebox"
(set -C; openssl rand -hex 32 > "$HOME/.local/state/enginebox/api.token")
export ENGINEBOX_API_TOKEN_FILE="$HOME/.local/state/enginebox/api.token"
enginebox-controller --config fleet.toml
```

The token-generation command refuses to replace an existing token. The service accepts `ENGINEBOX_API_TOKEN_FILE` or `ENGINEBOX_API_TOKEN`; the file takes precedence. Tokens must contain at least 32 printable non-space ASCII characters. Generate a random token rather than choosing a memorable string.

The API binds `127.0.0.1:8765`; `--port` changes the port. Remote access should use an SSH tunnel or a separately authenticated TLS proxy. The API itself does not expose a public listener or configure TLS. Direct requests with a browser `Origin` header are rejected; the [dashboard](DASHBOARD.md) uses its authenticated server-side proxy.

The controller root defaults to `~/.local/state/enginebox`; override it with `--state-dir`. Jobs live in its `jobs/` child. Unlike v0.2, `--state-dir` selects the parent controller root, not the jobs directory. It must belong to the service user and have mode `0700`. Job records are private, atomically written files with mode `0600`. Do not put this directory or token files in Git.

## Endpoints

Every `/v1/` request requires `Authorization: Bearer <token>`. The owner token reaches every endpoint below. A scoped service token (`ebst_…`, minted by the owner) reaches only the agent-run endpoints it was granted, on the workspaces it names; every other route answers 403, and an unknown, revoked or expired service token answers 401. POST bodies must be JSON objects and cannot exceed 16 KiB, except template registration which accepts up to 1 MiB. Additional payload fields are rejected.

| Request | Result |
|---|---|
| `GET /healthz` | Unauthenticated process-health response containing only `{"ok":true}`; does not check hosts. |
| `GET /v1/fleet` | Configured hosts, capacity estimates, instance metadata, template choices and sizes. |
| `GET /v1/templates` | Portable manifests and per-host installed revisions; see [templates](TEMPLATES.md). |
| `POST /v1/templates` | Register `{"manifest":{...}}` or import `{"url":"https://…/template.json"}` without installing it. |
| `POST /v1/hosts/{host_id}/templates/{template}/install` | Queue installation using `{}`. |
| `POST /v1/hosts/{host_id}/allocations` | Assign a never-used warm VM, or create a cold clone. Body: `{"name":"ebox-work","template":"native-agents","size":"recommended"}`. |
| `POST /v1/hosts/{host_id}/instances` | Queue a clone: `{"name":"ebox-work","template":"native-agents","size":"standard"}`. |
| `POST /v1/hosts/{host_id}/instances/{name}/actions` | Queue `{"action":"pause"}`, `resume`, `hibernate` or `stop`. |
| `GET /v1/jobs/{job_id}` | Poll the job's `queued`, `running`, `succeeded` or `failed` state. |
| `GET /v1/agents` | Controller Codex, Claude and Cursor connection/delegation status. |
| `GET /v1/agents/{provider}/usage` | Sanitized native allowance observation or explicit unavailable result; billing renewal is not inferred. |
| `POST /v1/hosts/{host_id}/instances/{name}/agent-runs` | Launch the official native CLI with `provider`, `prompt` and optional exact `resume_id`, relative `workdir`, `model`, bounded `timeout_seconds` (1–86400) and `env`. Service token operation: `agent_runs.start`. |
| `GET /v1/hosts/{host_id}/instances/{name}/agent-runs/{run_id}` | Private bounded output, run state, native session ID, `started_by` and `env_keys`; scoped to the named workspace. Service token operation: `agent_runs.get`. |
| `POST /v1/hosts/{host_id}/instances/{name}/agent-runs/{run_id}/cancel` | Request cancellation using `{}`. Service token operation: `agent_runs.cancel`. |
| `GET /v1/agent-runs` | Run summaries, newest active first. A service token sees only runs on its own workspaces. Service token operation: `agent_runs.list`. |
| `POST /v1/service-tokens` | Owner only. Mint `{"label":"ci","scopes":[{"host_id":"primary","name":"ebox-work"}],"operations":["agent_runs.start","agent_runs.get","agent_runs.list","agent_runs.cancel"],"expires_at":"2026-12-31T00:00:00Z"}` (`expires_at` optional). HTTP 201 returns the token's public record and its `secret`, which is shown only this once. |
| `GET /v1/service-tokens` | Owner only. Token IDs, labels, scopes, operations, expiry, revocation and `active`; never secrets or digests. |
| `POST /v1/service-tokens/{id}/revoke` | Owner only. Revoke using `{}`; the token is refused (401) on its next request. |
| `POST /v1/agents/{provider}/login` | Start native controller sign-in using `{}`; HTTP 202 with a session. |
| `GET /v1/agents/{provider}/sessions/{id}` | Poll the in-memory sign-in session. |
| `POST /v1/agents/{provider}/sessions/{id}/input` | Submit the one-time authorization code as `{"text":"…"}`. |
| `POST /v1/agents/{provider}/sessions/{id}/cancel` | Cancel pending sign-in using `{}`. |
| `POST /v1/templates/{id}/sync` | Retry synchronization on all registered nodes using `{}`. |
| `GET /v1/hosts/{host_id}/instances/{name}/accounts` | Legacy native workspace status for diagnostics; workspace login creation returns 409. |

Creation/lifecycle requests return HTTP 202 with a job. Poll that job before treating the operation as complete. Instance names must use the `ebox-` prefix. Lifecycle actions are limited to fleet-created VMs in the configured project and pool; original workspaces are not implicitly adopted. The API has no raw shell-command, snapshot-restore or deletion endpoint. Native agent runs can execute tools under the configured native CLI policy and consume provider allowance; they are operator-authorized operations, not guest-accessible infrastructure tools.

### Per-run environment

`env` is an optional object of string names to string values passed to that run's native CLI process only. Names must match `^[A-Z][A-Z0-9_]{0,63}$`; at most 32 variables totalling 8 KiB of names plus values are accepted, and the whole request must still fit the 16 KiB launch bound. Names Engine Box controls are refused: everything the launcher sets (`PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `LANG`, `TERM`, `DISPLAY`, `BROWSER`, …), provider credentials and selectors (`ANTHROPIC_*`, `CLAUDE_*`, `OPENAI_*`, `AZURE_OPENAI_*`, `CODEX_*`, `CURSOR_*`), loader variables (`LD_*`, `DYLD_*`), proxy and TLS trust (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`, `SSL_CERT_*`, `NODE_EXTRA_CA_CERTS`, …), shell and runtime start-up hooks (`BASH_ENV`, `ENV`, `NODE_OPTIONS`, `RUST_*`, `XDG_*`, …) and `ENGINEBOX_*`. The full list is `DENIED_ENV` in `packages/enginebox-agents/src/native.rs`.

**Treat values as secrets.** They travel to the guest helper on stdin (never argv), are absent from the version preflight, and are never written to run records, sidecars, logs or job output; records keep only `env_keys`. Values of 8 or more bytes are scrubbed (raw and JSON-escaped) from the returned output as `[secret]`, but this is best effort: a native tool that transforms a value before printing it defeats it, and anything the agent writes into the workspace stays there. A guest helper older than this field rejects requests that carry `env`; requests without it are unchanged.

Use `size: "minimum"` or `size: "recommended"` for template-defined resources, or replace `size` with `resources: {"cpu":4,"memory_gib":8,"disk_gib":40}`. All three custom values must be integers at or above the registered minimum. Registered templates become allocatable only after the current manifest revision is installed on that exact host/project/pool/guest binding.

Legacy `small`, `standard` and `large` select 2/4/8 vCPUs and 2/4/8 GiB RAM respectively. `disk_gib` is configured per host. Saved-memory storage is the selected RAM size plus 2 GiB.

For optional prebooted reserves, see [warm pools](WARM-POOLS.md). Allocation returns the actual Incus VM `name` plus the requested `workspace_name`; subsequent lifecycle calls use the actual name. Assigned VMs never return to the fresh pool.

## Provider accounts

Use the dashboard's **Agents** panel to connect the owner once. The control-plane installer prepares original CLIs in persistent private storage. Native browser/device authorization runs on the controller; validated links, one-time codes and status are exposed, never credential contents. Refresh credentials stay in controller provider homes. The same owner's assigned private VMs receive compatible native access material after create/allocation/resume and through periodic reconciliation. Frozen/stopped workspaces catch up after resume. Bases and unassigned warm reserves remain credential-free.

Existing signed-in workspaces are adopted explicitly with the Rust `AgentService::adopt(host_id, name, provider)` administrative interface while that provider's native sessions are idle. Adoption retains the native account centrally and converts the source to delegated access without asking for sign-in again. Automatic discovery refuses to overwrite unmarked/manual credentials. Stored attachment receipts bind to host configuration and guest UUID. Host inventory changes take effect on service restart; there is no live node-registration API yet.

Private files use owner-only permissions, not application-level encryption. Native credentials never enter template manifests, job journals, API responses or browser storage. Private workspace checkpoints can contain access credentials and must remain private. Expiry/refresh behavior differs by provider; running CLIs may cache access tokens until restarted. See [native-client authentication](NATIVE-CLIENTS.md) and [usage reporting research](AGENT-USAGE.md).

Connection status does not prove remaining quota. The fleet does not schedule prompts or pool unrelated accounts. Provider refresh and experimental version-qualified formats are not a general credential-rotation service.

## Capacity and coordination

Admission is recalculated immediately before each creation or stopped-VM resume. It accounts for:

- CPU and configured memory allocations across **all Incus projects** on the machine. Frozen and transitional/unknown states retain their allocations; only `Stopped` releases CPU/RAM commitments.
- Actual available host memory, with configured host reserves retained.
- Actual pool/backing-filesystem free space and full root-disk plus saved-memory commitments for every VM in the admission pool, including stopped VMs.
- The configured limit on fleet-created instances in the project.

The model is deliberately conservative about copy-on-write storage: inexpensive initial clones still reserve their full configured disk capacity. Ordinary host processes and CI jobs are not Incus allocations; reserve enough headroom for them. These checks do not guarantee latency or prevent unrelated administrator workloads from consuming resources afterward.

`max_additional` is a point-in-time estimate for repeatedly creating one size, not three budgets that can all be consumed independently. Queued jobs have not reserved capacity yet. A job can be rejected when it reaches execution even if inventory previously showed room.

One in-process operation runs per host. A root-owned `/run/lock/enginebox-host.lock` also coordinates fleet mutations with bootstrap and other cooperating Engine Box controllers. The controller holds that lock across its SSH calls. Independent Incus administrators do not participate in this lock, and a broken SSH connection is not a distributed transaction guarantee.

There are four background workers and at most 32 admitted queued/running jobs per service. This is a bounded operation queue, not a fair cluster scheduler. Templates and workloads remain on the selected host; automatic placement and cross-host migration are not implemented.

Inventory uses Incus metadata and bridge leases rather than asking frozen guests for state. Its IP addresses are lease observations, not a claim that a suspended guest is reachable.

## What “ready” means

A create or resume job reports success only after a probe confirms:

1. The configured guest UID/GID, username, home and working directory match.
2. The clone-identity service is active; the VM identity marker, filesystem machine ID and PID 1's D-Bus machine ID agree; completed cloud-init setup is disabled.
3. The preparation manifest matches the user, recorded native client executables are present, and the browser executable exists when the manifest declares browser support.
4. The guest has an IPv4 address, a default route and working DNS resolution for `example.com`.

The probe retries for up to approximately 180 seconds. Its network check does not authenticate to a model provider or prove inference availability. Client presence does not establish subscription eligibility, quota or account validity. Successful creation includes copy timing, total readiness timing and the readiness evidence in its job result.

The clone is marked as fleet-owned and loses the clean-base label atomically during copy. Its writable snapshots must therefore be treated as private, potentially authenticated state. A clone may remain stopped or running after a later preparation/readiness failure; failure does not trigger destructive cleanup.

## Jobs, recovery and restart

The service retains up to 200 jobs in its in-memory history and removes completed records as new jobs exceed that bound. On startup, persisted `queued` or `running` jobs become `failed` with an instruction to inspect the VM. They are **not replayed**, because an Incus operation may have completed before the controller stopped receiving its result.

Graceful shutdown waits for admitted work to finish. After a forced termination, lost connection or failed job, inspect the named VM and actual capacity before retrying. A repeated create with an existing name is rejected. Failed artifacts remain counted where they exist; an explicit operator must decide whether to repair or remove them. There is no cancel/retry/delete endpoint or durable exactly-once job execution.

## MCP connection

Run the operator MCP server with the same API token and origin:

```sh
export ENGINEBOX_API_URL=http://127.0.0.1:8765
export ENGINEBOX_API_TOKEN_FILE="$HOME/.local/state/enginebox/api.token"
enginebox-fleet-mcp
```

Register that command through your client's ordinary MCP configuration. The current stdio implementation supports handshake revisions through `2025-11-25`; it does not advertise the separate `2026-07-28` discovery/per-request versioning protocol. It exposes `fleet_inventory`, `fleet_allocate`, `fleet_create`, `fleet_action`, `fleet_job`, `template_catalog`, `template_register`, `template_install`, `template_sync`, `agent_connections`, `agent_launch`, `agent_runs`, `agent_run` and `agent_cancel`. Creation and lifecycle tools return jobs to poll; they use the API's queue, locks and admission checks rather than bypassing them. Native launches have a separate bounded run queue and private result endpoint; see [native runs and state](TENANT-AGENT-STATE.md).

The MCP client only permits HTTP for loopback origins, requires HTTPS for remote origins, rejects URL credentials and disables redirects and ambient forward proxies. A redirect must not forward the bearer token to another service. Use a direct API connection or SSH tunnel. Do not install fleet API tokens or operator SSH authority inside agent guests; guest computer/browser MCP is a separate interface.

## Verification and current limits

```sh
pnpm exec cargo test -p enginebox-core -p enginebox-controller -p enginebox-cli
```

The tests cover reserve accounting, cross-project/frozen allocations, template trust and pool selection, disk-shrink rejection, exact guest mapping, readiness-contract handling, serialized jobs, restart records, queue-slot recovery and bearer authentication/redirect protection. They do not create live VMs. Real clone, networking, identity and RAM continuity checks are reported separately in [validation](reports/VALIDATION.md).

The service assumes trusted operators and prepared Linux VM templates. It does not provide tenant-level authorization, automatic credential rotation, agent orchestration, CI scheduling, arbitrary environment upgrades, fleet-wide storage replication or GPU state restoration.

## ACP and terminal tasks

`ebox tasks --watch` displays native runs across all hosts and providers. `ebox acp` exposes the stable Agent Client Protocol v1 session lifecycle over stdio. Both use the existing authenticated controller and native workspace state. See [ACP and fleet tasks](ACP.md) for client configuration, cancellation, usage semantics and current limitations.
