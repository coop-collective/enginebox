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
# Optional: running vCPUs admitted per physical core left after reserve_cpu
# (1.0-64.0, default 4.0). Older controller releases reject this key.
cpu_overcommit = 4.0
# Optional: disk accounting, "copy_on_write" or "allocated". Unset detects
# copy-on-write from the Incus pool driver (btrfs, zfs); any other or unknown
# driver is "allocated". Older controller releases reject this key.
# disk_accounting = "copy_on_write"
# Optional: sum of allocated disks admitted per byte of pool left after
# reserve_disk_gib (1.0-64.0; default 4.0 for copy-on-write, 1.0 otherwise).
# Older controller releases reject this key.
# disk_overcommit = 4.0
# Optional: concurrent native agent runs on this host (1–256, default 8).
# Older controller releases reject this key; set it only after upgrading.
max_agent_runs = 8

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

Every `/v1/` request requires `Authorization: Bearer <token>`. The owner token reaches every endpoint below. A scoped service token (`ebst_…`, minted by the owner) reaches only the agent-run endpoints it was granted, on the workspaces it names, and the [repository](REPOSITORIES.md) endpoints of projects it was granted; every other route answers 403, and an unknown, revoked or expired service token answers 401. The owner-only repository backend configuration is the one `PUT`/`DELETE` resource; everything else uses GET and POST. POST bodies must be JSON objects and cannot exceed 16 KiB, except template registration (1 MiB), profile files (1.5 MiB, one base64-encoded file of up to 1 MiB) and profiles (256 KiB). Additional payload fields are rejected.

Third-party apps get service tokens through OAuth instead of from the owner by hand: the dashboard is an OAuth 2.1 authorization server that mints an hour-long service token per consented connection and proxies the agent-run endpoints on its public origin. The owner's service-token store prunes records revoked or expired for over a day when it reaches its limit. See [OAuth for connected apps](OAUTH.md).

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
| `POST /v1/hosts/{host_id}/instances/{name}/agent-runs` | Launch the official native CLI with `provider`, `prompt` and optional exact `resume_id`, relative `workdir`, `model`, bounded `timeout_seconds` (1–86400) and `env`, plus the optional v2 fields `profile`, `idempotency_key`, `callback` and `repository` (see [agent runs v2](#agent-runs-v2)). HTTP 202 with the new run (status `queued` until the scheduler admits it), or HTTP 200 with `"replayed":true` for a repeated idempotency key. HTTP 429 with `Retry-After` only when a hard queue bound is reached (see [the run queue](QUEUE.md)). Service token operation: `agent_runs.start`. |
| `GET /v1/hosts/{host_id}/instances/{name}/agent-runs/{run_id}` | Private bounded output, run state, native session ID, `started_by`, `env_keys`, `profile` (`{name,revision}` or null), `callback` (`{origin,state,attempts,last_status,updated_at}` or null), `repository` (`"<project>/<name>"` or null) and `failure` (`{stage,reason}` or null; see [launch events](QUEUE.md#launch-events)); scoped to the named workspace. Service token operation: `agent_runs.get`. |
| `POST /v1/hosts/{host_id}/instances/{name}/agent-runs/{run_id}/cancel` | Request cancellation using `{}`. Service token operation: `agent_runs.cancel`. |
| `GET /v1/agent-runs` | Run summaries, newest active first, each with `pool`, `queue_position` and `waiting_reason`. A service token sees only runs on its own workspaces or in its granted pools. Service token operation: `agent_runs.list`. |
| `POST /v1/pools/{pool}/agent-runs` | Start a run on any free workspace of a [pool](QUEUE.md#pools); same body, statuses and v2 fields as the workspace route. The run's `host_id` and `name` are null until it is placed. A `resume_id` lands on the workspace holding that session. Service token: `agent_runs.start` plus the pool in `pools`. |
| `GET /v1/pools/{pool}/agent-runs/{run_id}` | The run, placed or not. Once placed it is also readable on its workspace route. Service token: `agent_runs.get` plus the pool. |
| `POST /v1/pools/{pool}/agent-runs/{run_id}/cancel` | Cancel using `{}`: an unplaced run ends at once; a placed one is cancelled on its workspace. Service token: `agent_runs.cancel` plus the pool. |
| `POST /v1/pools` | Owner only. Create or replace a pool `{"name","description","profile","template","workspaces":[{"host_id","name"}],"autoscale":{"hosts":[…],"max_workspaces":N,"size":"recommended"}}`. |
| `GET /v1/pools`, `GET /v1/pools/{pool}` | Owner only. Pool definitions; the single read adds member busy state, queued/running counts, creations in progress and the last creation error. |
| `POST /v1/pools/{pool}/delete` | Owner only. Delete using `{}` when no run is queued or running in it; its workspaces are kept. |
| `POST /v1/service-tokens` | Owner only. Mint `{"label":"ci","scopes":[{"host_id":"primary","name":"ebox-work"}],"operations":["agent_runs.start","agent_runs.get","agent_runs.list","agent_runs.cancel"],"expires_at":"2026-12-31T00:00:00Z","profiles":["coder"],"pools":["workers"],"project":"default","callback_secret":"…"}` (`expires_at`, `profiles`, `pools`, `project`, `repositories` and `callback_secret` optional; `repositories: [{"project":"alpha","access":"read"|"write"}]` grants one project's [repositories](REPOSITORIES.md#grants), and a token with only repository grants needs no `operations`; `project` restricts the pool and repository grants to that project, checked at mint and again on every request, so a pool moved to another project stops being reachable with the token; `scopes` may be empty when `pools` is not). `pools` names existing pools whose runs the token may start, read and cancel, by its operations; a pool grant includes the pool's profile. `profiles` names existing profiles the token may run; without it the token may only start runs without a profile. `callback_secret` (32–256 printable ASCII characters) signs callbacks for the token's runs and is never returned. HTTP 201 returns the token's public record and its `secret`, which is shown only this once. |
| `GET /v1/service-tokens` | Owner only. Token IDs, labels, scopes, operations, `profiles`, `pools`, `project`, `repositories`, `pool_grants` (the pools grouped by their current project), `has_callback_secret`, expiry, revocation and `active`; never secrets, callback keys or digests. |
| `PUT`, `GET`, `DELETE /v1/repository-backends/artifacts-worker` | Owner only. Configure the Artifacts gateway, reached through the site Worker (`{"url":"https://<site>/_artifacts","secret","namespace_prefix"}`); the shared signing secret is write-only and never returned. `GET /v1/repository-backends` shows the selected backend. See [repositories](REPOSITORIES.md#setup). |
| `GET /v1/repositories` | Owner only. Every hosted repository record. |
| `POST /v1/projects/{project}/repositories` | Create `{"name","description"}` (idempotent on name: 201 created, 200 existing). Returns `{"repository":{project,name,backend,description,remote,default_branch,created_at,last_push_at}}`, never a token. Service token: a `write` repository grant on the project. |
| `GET /v1/projects/{project}/repositories`, `GET /v1/projects/{project}/repositories/{name}` | List or read. Service token: any repository grant on the project. |
| `POST /v1/projects/{project}/repositories/{name}/tokens` | Mint `{"access":"read"|"write","ttl_seconds":60–86400}` for the caller's own short use; HTTP 201 `{"token","expires_at","remote","default_branch","access"}`. A `read` grant mints only read tokens. |
| `POST /v1/profile-files` | Owner only. Store one file `{"content_base64":"…"}` (1 byte–1 MiB) under its SHA-256; HTTP 201 returns `{"sha256","size"}`. Re-uploading identical bytes is a no-op. See [profiles](PROFILES.md). |
| `POST /v1/profiles` | Owner only. Create or replace a profile `{"name","description","template","providers","callback_secret"}`; every referenced file must already be uploaded. Returns the public record with its new `revision`. |
| `GET /v1/profiles`, `GET /v1/profiles/{name}` | Owner only. Public profile records: env names (`env_keys`) instead of values, `has_callback_secret` instead of the key. |
| `POST /v1/profiles/{name}/delete` | Owner only. Delete using `{}`. Uploaded files are kept. |
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

### Agent runs v2

All five fields are optional; a request without them is handled exactly as before (same native argv, same guest helper command and payload).

- **`profile`** — the name of an owner-managed [profile](PROFILES.md). The profile must configure the requested `provider`, and when it names a `template` the workspace must have been created from it. A service token may name only profiles it was granted (`profiles` at mint); any other profile answers 403, an unknown profile 404. The profile's default `env` is merged under the run's `env` (the run wins) and the merged set must stay within the [per-run environment](#per-run-environment) bounds. Profile runs need a guest helper that supports `run-profile`; runs without a profile work with older helpers.
- **`repo_profile`** — with `repository`, the repository agent profile to use (default: the one the repository's registered config marks `default: true`). Its met requirements add values to the run's env and MCP servers to the profile's config; unmet ones are recorded, and a required unmet one refuses the start with 409. See [PROJECT-CONFIG.md](PROJECT-CONFIG.md#choosing-a-profile-and-what-a-run-gets).
- **`run_kind`** — the kind of work this run does (`planner`, `builder`, `debug`, `deploy`…), matching `^[a-z][a-z0-9_-]{0,31}$`. With a profile it selects which of the profile's [connections](CONNECTIONS.md#policy-per-kind-of-run) apply (its `connection_policy`); without one, or for a kind the policy does not list, every connection of the profile applies. The run's details record it under `profile.run_kind`, with the connections used (`profile.connections`: name, revision, kind; never a value). It is part of the idempotency binding when set. A connection that cannot be resolved (unknown, a missing secret, a variable clash) refuses the start with HTTP 409 and a message naming the connection and the rule, never a value.
- **`repository`** — `"<project>/<name>"` of a [hosted repository](REPOSITORIES.md#runs-with-a-repository). A service token needs a `write` repository grant on that project (otherwise 403); a pool run needs the repository's project to equal the pool's (otherwise 403, for the owner too); an unknown repository answers 404 and a missing backend 409. At admission the controller mints a write token for `timeout_seconds` + 600 s, the guest writes it into the run's private home as a git `http.<remote>.extraHeader` scoped to that remote, and the CLI process gets `ENGINEBOX_REPO_REMOTE` and `ENGINEBOX_REPO_DEFAULT_BRANCH`. The run works in a checkout emptied at its start, at a stable path per repository, `<workspace>/<name>`, named by `ENGINEBOX_RUN_DIR` (stable so harness state keyed by cwd persists; see [checkouts](REPOSITORIES.md#checkouts)). The run records only the name; the token never reaches records, logs, launch events or callbacks, and is revoked when the run ends. Repository runs need a guest helper from the controller's release; placement [refreshes](REPOSITORIES.md#guest-helper-refresh) an older one, and one that cannot be refreshed fails the run with `{"stage":"bootstrap","reason":"guest_outdated"}` before it starts.
- **`idempotency_key`** — 1–128 characters of `A-Z a-z 0-9 . _ : -`. The same key from the same principal (owner, or one service token) — and, for pool starts, the same pool — returns the existing run, whether it is still queued, running or finished, with HTTP 200 and `"replayed":true` instead of starting another. The key is bound to its first request (provider, prompt, resume ID, workdir, timeout, model, env names, profile, callback URL and repository; env values are never hashed) and workspace; reusing it with a different request is refused. Keys are remembered for as long as the run is retained (the newest 100 finished runs).
- **`callback`** — `{"url":"https://…"}`. When the run reaches a terminal state the controller POSTs a JSON summary: `schema`, `event` (`agent_run.finished`), `run_id`, `host_id`, `name`, `provider`, `profile`, `status`, `exit_code`, `timed_out`, `execution_uncertain`, `created_at`, `started_at`, `finished_at`. It never contains output, prompts, env names or values, or errors. Headers: `X-EngineBox-Event`, `X-EngineBox-Delivery` (one ID across retries, for deduplication) and `X-EngineBox-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256 of "<t>.<raw body>">`. The key is never taken from the request: a service token's runs are signed with that token's `callback_secret`, owner runs with the named profile's `callback_secret`; without one the launch is refused. Delivery succeeds on 2xx; 408, 429, 5xx and network errors are retried after 2, 10, 60 and 300 seconds; any other status stops. The run records only the URL's origin and the delivery state. The URL must be HTTPS without credentials or fragment; `localhost` names and IP literals in private, loopback, link-local, CGNAT or other non-public ranges are refused at launch, and the host is resolved before every attempt with every answer required to be public, then pinned for that connection (no proxy, no redirects). The callback target and key are kept with the run's private queue entry (mode 0600) from acceptance until the run finishes, so a restart keeps delivery: a queued run's callback fires when it later ends, and a run a restart interrupted fires once with `"execution_uncertain":true`. The payload also carries `pool` (null for workspace runs), `repository` (the name, or null) and `failure`: `{"stage","reason"}` codes when the launch stopped before or while the guest started the run (see [launch events](QUEUE.md#launch-events)), otherwise null. It carries codes only, never the failure message. Delivery that was already in progress when the controller stopped is not retried.

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

- CPU and configured memory allocations across **all Incus projects** on the machine, by instance state:

  | State | CPU | Memory | Disk |
  | --- | --- | --- | --- |
  | `Running`, transitional (`Starting`, `Stopping`, `Migrating`) and unknown/`Error` | counted | counted | counted |
  | `Frozen` (paused, e.g. a warm spare) | not counted | counted (it holds its RAM) | counted |
  | `Stopped` (templates, parked workspaces) | not counted | not counted | counted |

- A CPU budget of `(cpu_total - reserve_cpu) * cpu_overcommit` running vCPUs. An instance's `limits.cpu` is a ceiling, not a reservation, and agent workloads mostly wait on inference, so `cpu_overcommit` defaults to `4.0` (range 1.0–64.0; set `1.0` for strict one-vCPU-per-core accounting). A frozen VM's vCPUs count again once it runs.
- Memory with no overcommit: the configured memory of every instance that holds RAM plus `reserve_memory_gib` must fit in total memory, and new instances must also fit in the actual available host memory after the reserve.
- An optional `contribution_cpu` / `contribution_memory_gib` ceiling on a contributed machine is an owner's promise and is never overcommitted.
- Disk, with two guards that must both admit the new VM's full root disk plus saved-memory size (RAM + 2 GiB):
  - **Actual usage:** the smaller of the pool's free space (`pool_total - pool_used`) and the backing filesystem's free space, minus `reserve_disk_gib` (`disk_usage_available_bytes`).
  - **Allocation:** the configured root plus saved-memory disks of every VM in the admission pool, stopped VMs included, may reach `(pool_total - reserve_disk_gib) * disk_overcommit` (`disk_allocation_budget_bytes`; what is left is `disk_allocation_available_bytes`).
  - `disk_budget_available_bytes` is the smaller of the two.
- The configured limit on fleet-created instances in the project.

Disk accounting follows the admission pool's Incus driver, which the host inventory reports as `pool_driver`:

| `disk_accounting` | Selected when | Default `disk_overcommit` | Effect |
| --- | --- | --- | --- |
| `copy_on_write` | driver `btrfs` or `zfs` | `4.0` | Clones share blocks with their template, so admission follows real usage; allocations may total four times the pool after the reserve. |
| `allocated` | any other driver, or an inventory from an older host agent without `pool_driver` | `1.0` | Strict: every allocated byte is reserved, as before this setting existed. |

Set `disk_accounting` on the host to override detection (for example while the host still runs an older `enginebox-host` that does not report the driver), and `disk_overcommit` (1.0–64.0) to tighten or loosen the allocation guard; `disk_overcommit = 1.0` restores strict accounting on a copy-on-write pool. Fleet files without either key behave as the table says. Thin LVM is treated as `allocated`: running a thin pool out of space fails hard, so opt in explicitly. Overcommitted guests can still fill the pool if they all write their full disks; watch `pool_free_bytes` and keep `reserve_disk_gib` large enough for growth between admissions.

Ordinary host processes and CI jobs are not Incus allocations; reserve enough headroom for them. These checks do not guarantee latency or prevent unrelated administrator workloads from consuming resources afterward.

A refused creation names the first limiting resource, for example `Insufficient CPU, memory, disk or instance slots; preserve reserves (memory: 4 GiB requested, 2 GiB free after the 40 GiB reserve)`. A disk refusal shows both disk guards: `disk: 36 GiB requested; 410 GiB actually free in the pool after the 32 GiB reserve, 20 of 1790 GiB allocation budget left (copy_on_write accounting, disk_overcommit 4.0)`. `GET /v1/fleet` reports `cpu_overcommit`, `cpu_budget`, `allocated_cpu` (running vCPUs) and `allocated_memory_bytes` (running and frozen) per host, and for disk `pool_driver`, `disk_accounting`, `disk_overcommit`, `reserve_disk_bytes`, `disk_committed_bytes` (allocated), `disk_usage_available_bytes`, `disk_allocation_budget_bytes`, `disk_allocation_available_bytes` and `disk_budget_available_bytes`.

Disk example: a 515 GB btrfs pool with 485 GB free, `reserve_disk_gib = 32` and 13 VMs whose disks total 481 GB allocated. Under strict accounting it admits nothing (515 GB − 34.4 GB − 481 GB < 0). Under copy-on-write it has 450.6 GB (419 GiB) of actual room, so 11 standard VMs (30 GiB root + 6 GiB saved memory) fit at once, and an allocation budget of (515 − 34.4) × 4 = 1922.6 GB with 1441.6 GB left, so about 37 more standard VMs can be created as long as clones keep writing little.

Each host in `GET /v1/fleet` also lists `stale_instances`: stopped `ebox-template-*` instances in the host project that no template on that host names as its `source`, and a stopped `ebox-base` that no template uses. Entries show their allocated `disk_bytes` and `state_disk_bytes`. The list is read-only; Engine Box never deletes them. Review and remove them with `incus delete` once nothing depends on them. It is `null` when the template registry cannot be read.

Example: a 48-core, 125 GiB host with `reserve_cpu = 12`, `reserve_memory_gib = 40` and 13 VMs of 4 vCPU / 4 GiB, of which 5 are stopped, 4 frozen and 4 running, has a CPU budget of 144 vCPU with 16 used, and 125 − 40 − 32 = 53 GiB of memory budget, so it admits 13 more standard VMs.

`max_additional` is a point-in-time estimate for repeatedly creating one size, not three budgets that can all be consumed independently. Queued jobs have not reserved capacity yet. A job can be rejected when it reaches execution even if inventory previously showed room.

One in-process operation runs per host. A root-owned `/run/lock/enginebox-host.lock` also coordinates fleet mutations with bootstrap and other cooperating Engine Box controllers. The controller holds that lock across its SSH calls. Independent Incus administrators do not participate in this lock, and a broken SSH connection is not a distributed transaction guarantee.

There are four background workers and at most 32 admitted queued/running jobs per service. This is a bounded operation queue, not a fair cluster scheduler. Templates and workloads remain on the selected host; cross-host migration is not implemented. Native agent runs are scheduled separately, with placement only within a [pool](QUEUE.md#pools).

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

Register that command through your client's ordinary MCP configuration. The current stdio implementation supports handshake revisions through `2025-11-25`; it does not advertise the separate `2026-07-28` discovery/per-request versioning protocol. It exposes `fleet_inventory`, `fleet_allocate`, `fleet_create`, `fleet_action`, `fleet_job`, `template_catalog`, `template_register`, `template_install`, `template_sync`, `agent_connections`, `agent_launch`, `agent_runs`, `agent_run` and `agent_cancel`. Creation and lifecycle tools return jobs to poll; they use the API's queue, locks and admission checks rather than bypassing them. Native launches have a separate durable run queue and private result endpoint; see [the run queue](QUEUE.md) and [native runs and state](TENANT-AGENT-STATE.md).

The dashboard's **MCP** section explains this local connection and retains OAuth
app-grant management. The dashboard origin is not a remote MCP URL: the supported
operator transport is the installed `enginebox-fleet-mcp` command over stdio,
connecting to the controller origin above. OAuth app integrations use the
separate scoped REST API described in [OAuth](OAUTH.md).

For project work, start with `workspace_catalog` and `repository_list` (a
`project` argument). `repository_create` takes `project`, `name`, and an optional
`description`; the configured hosted backend must already support that project.
`repository_link_github` takes `project`, `name`, and `full_name` (`owner/repo`),
with an optional description. GitHub linking remains owner-only and requires the
source to have been selected through the existing GitHub connection. A source
record does not automatically prepare a LayerFS environment.

Owner/admin catalog tools (`connection_list`, `connection_create`,
`connection_update`, `connection_delete`, `connection_test`, `profile_get`,
`profile_attach_connection`, `profile_detach_connection`, `catalog_apply`) set up
shared MCP servers and secret sources for profiles; see
[CONNECTIONS.md](CONNECTIONS.md#mcp-tools). Set `ENGINEBOX_ORG` to act for an
org other than the platform org.

Use the existing `storage_*` tools for reviewed source preparation and storage
operations, polling `fleet_job` for asynchronous results. `pool_agent_launch`
takes `pool`, `provider`, `prompt`, optional `repository` (`project/name`),
`timeout_seconds`, and `model`. Use `pool_agent_run` or `pool_agent_cancel` with
`pool` and `run_id`; `agent_runs` helps reconcile an unconfirmed submission.
Never automatically replay an uncertain launch. Pool grants, source grants,
resource admission and native credential delegation are enforced by the same
controller as the dashboard. Timing receipts do not approve results or replace
the existing review and acceptance flow.

The MCP client only permits HTTP for loopback origins, requires HTTPS for remote origins, rejects URL credentials and disables redirects and ambient forward proxies. A redirect must not forward the bearer token to another service. Use a direct API connection or SSH tunnel. Do not install fleet API tokens or operator SSH authority inside agent guests; guest computer/browser MCP is a separate interface.

## Verification and current limits

```sh
pnpm exec cargo test -p enginebox-core -p enginebox-controller -p enginebox-cli
```

The tests cover reserve accounting, cross-project/frozen allocations, template trust and pool selection, disk-shrink rejection, exact guest mapping, readiness-contract handling, serialized jobs, restart records, queue-slot recovery and bearer authentication/redirect protection. They do not create live VMs. Real clone, networking, identity and RAM continuity checks are reported separately in [validation](reports/VALIDATION.md).

The service assumes trusted operators and prepared Linux VM templates. It does not provide tenant-level authorization, automatic credential rotation, agent orchestration, CI scheduling, arbitrary environment upgrades, fleet-wide storage replication or GPU state restoration.

## ACP and terminal tasks

`ebox tasks --watch` displays native runs across all hosts and providers. `ebox acp` exposes the stable Agent Client Protocol v1 session lifecycle over stdio. Both use the existing authenticated controller and native workspace state. See [ACP and fleet tasks](ACP.md) for client configuration, cancellation, usage semantics and current limitations.
