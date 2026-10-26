# Native run queue and pools

Engine Box accepts as many native agent runs as callers send, runs as many as host capacity allows, and keeps the rest in a durable queue. Callers do not need to limit concurrency themselves. A start is refused only when a large hard bound is reached.

## Statuses

A start (`POST /v1/hosts/{host}/instances/{name}/agent-runs` or `POST /v1/pools/{pool}/agent-runs`) answers:

- **HTTP 202, `"status":"queued"`**: accepted and persisted. The scheduler admits it when its workspace and host have room. Every accepted run starts as `queued`, including one that gets admitted a moment later.
- **HTTP 200, `"replayed":true`**: the principal already used this `idempotency_key` (for pool starts, in this pool). The response is that run in its current state: queued, running or finished.
- **HTTP 429 with `Retry-After`**: a hard bound was reached (see below). Nothing was stored.

A run then moves `queued` → `running` → `succeeded`, `failed` or `cancelled`. Run reads and listings add:

- `queue_position`: the run's 1-based place in the scheduler's current order (null unless queued).
- `waiting_reason`: why it is still queued, for example `workspace ebox-a busy`, `waiting for capacity on host primary (3 of 8 runs)`, `waiting for capacity on host primary (memory)`, `waiting for capacity on host primary (resource measurement unavailable)`, `waiting for a free workspace in pool workers`, `creating a workspace for pool workers`, `workspace ebox-w2 busy (resume is pinned to it)`, or `admission is paused while the controller drains`.
- `pool`: the pool the run was started on, or null.

A run that was admitted but could not start (workspace stopped or missing, wrong template, an earlier run's unresolved lease, provider not delegable, profile deleted) fails with an `error` and without `execution_uncertain`. This check used to happen during the start request; it now happens at admission, and the run's callback reports the failure. The run shows why as `failure: {stage, reason}`, and every such failure is logged as a [launch event](#launch-events).

## Ordering and fairness

The order is FIFO per principal (the owner, or one service token) and round-robin across principals: each principal's oldest queued run goes next, the principals take turns, and the next pass begins with the principal after the one served last. One client with a thousand queued runs therefore takes one turn in each round, the same as a client with one.

A queued run that cannot start yet (for example, its workspace is busy) does not hold back that principal's runs for other workspaces. A run never overtakes an earlier run of the same principal that targets the same workspace and provider.

## Admission

A run is admitted when all of these hold:

1. **One writer per workspace and provider.** Other providers may run on the same workspace at the same time, as before. A pool places at most one run on each of its workspaces.
2. **The host's run limit.** `max_agent_runs` on the host in the fleet configuration, or the controller default.
3. **CPU.** Fewer runs on the host than its CPU count minus `reserve_cpu` (at least one).
4. **Memory.** The host's measured available memory must cover its `reserve_memory_gib` plus a per-run headroom for this run and for each run admitted since the last measurement that is still running.

The CPU and memory numbers come from the inventory the controller already reads for VM admission (`cpu_total`, `memory_available_bytes`). Each host is measured at most once per TTL, and only while queued runs need it. If a measurement fails, that host admits nothing until a later measurement succeeds, and its runs show `resource measurement unavailable`.

## Limits and defaults

| Setting | Default | Environment variable |
|---|---|---|
| Queued runs, all principals (hard bound, 429) | 4096 | `ENGINEBOX_RUN_QUEUE_MAX_TOTAL` |
| Queued runs, one principal (hard bound, 429) | 1024 | `ENGINEBOX_RUN_QUEUE_MAX_PER_PRINCIPAL` |
| Concurrent runs per host without `max_agent_runs` | 8 | `ENGINEBOX_HOST_MAX_AGENT_RUNS` |
| Memory headroom per run | 1024 MiB | `ENGINEBOX_AGENT_RUN_MEMORY_MIB` |
| Host measurement TTL | 15 s | `ENGINEBOX_HOST_MEASURE_TTL_SECONDS` |
| `Retry-After` on 429 | 30 s | `ENGINEBOX_RUN_QUEUE_RETRY_AFTER_SECONDS` |

The controller reads these at activation and refuses to start if one is invalid. Under the supervisor, set them in its private `environment_file` (on the allowlist of supervisors built from this release onward) or in the supervisor's service environment. Only queued runs count toward the hard bounds; running and finished runs do not. Set the per-host limit in the fleet configuration (`max_agent_runs = 12`, from 1 to 256). Older controller releases reject that key, so add it only after upgrading.

## Durability

Each accepted run is written as a private entry under `agent-run-queue/<id>.json` (mode 0600, in the 0700 controller root), next to its usual record and sidecar. On activation the controller restores queued entries in their original order and admits them again. Runs that were running when the controller stopped are marked `failed` with `execution_uncertain`, as before, and owe their callback once.

**Env values are persisted while a run is queued.** A queued run's entry holds its full request, including `env` values, because the run must still be launchable after a restart. The values are removed from the entry when the run is admitted, and the entry is deleted when the run finishes. Records, sidecars, listings and callbacks still never contain env values. Like provider credentials, the entry is protected by owner-only file permissions, not application-level encryption. Callback keys are stored in the entry the same way.

A drain stops admission. Queued runs stay on disk for the next controller to admit, and only running runs hold the drain open.

Finished runs are retained as before: the newest 100 workspace records, plus the newest 100 pool runs that ended before placement. Queued and running runs never count against retention. Idempotency keys live as long as their run is retained.

## Pools

A pool is a named, owner-managed set of workspaces on one or more hosts that share a profile and/or template. A client starts runs on the pool instead of choosing workspaces, and Engine Box spreads them across the pool.

```json
POST /v1/pools
{"name":"workers","project":"default","description":"Coding agents","profile":"coder","template":"native-agents",
 "workspaces":[{"host_id":"primary","name":"ebox-w1"},{"host_id":"primary","name":"ebox-w2"}],
 "autoscale":{"hosts":["primary","second"],"max_workspaces":8,"size":"recommended"}}
```

- `name` must match `^[a-z][a-z0-9-]{0,31}$`. `workspaces` lists existing members (up to 256), and every listed host must be registered.
- `project`: the project the pool belongs to, an ID like the fleet Worker's project IDs (`^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$`). Every pool record has one. A new pool that names none is put in `default`; replacing a pool without naming one keeps its project; pool records written before projects existed load as `default` (no migration step: the field is filled in on read and written on the next change). Pool names stay unique across projects. Engine Box's native controller has no other project concept to inherit from: a host's Incus `project` in the fleet configuration is a per-host storage namespace, unrelated to this one.
- `profile` (optional): every run in the pool uses it. See [profiles](PROFILES.md).
- `template` (optional): the template members must come from. Admission checks it on each placement, and autoscale needs it.
- `autoscale` (optional): when every member is busy and runs are waiting, the scheduler creates new members named `ebox-<pool>-<n>` with the existing creation and readiness path, on the first listed host that has run capacity. It never exceeds `max_workspaces` members in total. A failed creation backs off for 60 seconds and is shown as `last_create_error`. Created members stay in the pool when it is replaced, and they are never deleted automatically.
- Placement picks a free member (no active run and no unresolved lease) on the host with the most free run slots.
- Each run records the workspace it landed on (`host_id`, `name`, null until placed). Once placed, the run is also readable and cancellable on that workspace's routes.
- **Resume stays on its workspace.** Native sessions belong to one workspace. A pool start with `resume_id` is pinned to the workspace that ran that session in this pool, and waits for that workspace even if others are free. An unknown session is refused with HTTP 400. The controller keeps an index of the newest 4096 pool sessions, so a resume works after the original run's record has been pruned.
- A pool with queued or running runs cannot be deleted.

Pools are listed with `GET /v1/pools` (every pool, plus `projects`: pool names grouped by project) and `GET /v1/projects/{project}/pools` (one project's pools). Both are owner-only.

Service tokens are granted pools with `"pools":["workers"]` at mint time. Adding `"project":"<id>"` restricts the token to that project's pools: every granted pool must belong to it at mint, and each request checks the pool's current project, so moving a pool to another project removes it from the token (403, and its runs leave the token's listing). The token listing shows each token's pools grouped by project (`pool_grants`). The token's operations (`agent_runs.start`, `get`, `list`, `cancel`) then apply to that pool's routes. Any other pool answers 403, and a token's listing includes runs in its granted pools.

## Launch events

Every admission or placement failure of a run, and every pool member creation (success or failure), produces one structured log line on the controller's stderr (`{"event":"enginebox.launch",…}`) and one event in a bounded history: the newest 500, kept in memory and in the private file `launch-events.json` (mode 0600) under the controller root, so it survives restarts.

Each event has `at`, `kind` (`run_admission_failed`, `pool_member_created`, `pool_member_create_failed`), `outcome` (`failed` or `succeeded`), `run_id`, `pool`, `project`, `host_id`, `workspace`, `template`, `template_revision` (when the registry knows it), `profile`, `profile_revision`, `provider`, `stage`, `reason`, `message`, `duration_ms` (time in the step that produced the event) and `queue_wait_ms` (runs: acceptance to admission). Fields that do not apply are null.

| Stage | Meaning | Reason codes |
|---|---|---|
| `resolve` | The profile (after a restart) or the pool could not be resolved, or the run's [repository](REPOSITORIES.md) token could not be minted | `profile_unavailable`, `pool_unavailable`, `repository_unavailable` |
| `place` | The chosen workspace cannot take the run | `workspace_unavailable` (missing or not running), `template_mismatch`, `lease_unresolved`, `admission_error` (bookkeeping failed; the run stays queued and is retried) |
| `create` | A pool member could not be created | `create_failed`, `pool_has_no_template`, `name_unavailable`; `ok` on success |
| `bootstrap` | The workspace could not be prepared for the run, or a created member could not be recorded | `workspace_locked`, `member_record_failed`, `guest_outdated` (the guest helper does not support repository runs) |
| `delegate` | The provider credential could not be delegated to the workspace | `delegation_failed`, `provider_not_delegable` |
| `start` | The guest helper failed to launch the run (the run is `execution_uncertain`, as before) | `guest_launch_failed` |

Reason codes are stable; new ones are additive. `message` is at most 240 printable characters on one line, with the run's env values and anything token-shaped replaced by `[redacted]`. Events never contain env values, prompts, tokens or callback URLs.

The run itself shows `failure: {stage, reason}` (null otherwise) on reads and listings, and its terminal callback carries the same two codes, never the message.

| Endpoint | |
|---|---|
| `GET /v1/launch-events` | Owner only. `{"events":[…newest first],"retained":n,"max_retained":500}` |
| `GET /v1/pools/{pool}/launch-events` | Owner only. The same, for one pool |
| `GET /v1/projects/{project}/launch-events` | Owner only. The same, for one project's pools |

## Not implemented

- A `started` callback event (only `agent_run.finished` is sent).
- Retrying delivery of a callback that was in flight when the controller stopped.
- Per-principal weights or priorities. All principals get equal turns.
- Admission based on GPU, disk I/O or guest load. CPU uses the host CPU count, not the measured load average.
- Automatic removal of idle autoscaled workspaces.
