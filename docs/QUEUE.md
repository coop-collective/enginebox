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

A run that was admitted but could not start (workspace stopped or missing, wrong template, an earlier run's unresolved lease, provider not delegable, profile deleted) fails with an `error` and without `execution_uncertain`. This check used to happen during the start request; it now happens at admission, and the run's callback reports the failure.

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

The controller reads these at activation and refuses to start if one is invalid. Only queued runs count toward the hard bounds; running and finished runs do not. Set the per-host limit in the fleet configuration (`max_agent_runs = 12`, from 1 to 256). Older controller releases reject that key, so add it only after upgrading.

## Durability

Each accepted run is written as a private entry under `agent-run-queue/<id>.json` (mode 0600, in the 0700 controller root), next to its usual record and sidecar. On activation the controller restores queued entries in their original order and admits them again. Runs that were running when the controller stopped are marked `failed` with `execution_uncertain`, as before, and owe their callback once.

**Env values are persisted while a run is queued.** A queued run's entry holds its full request, including `env` values, because the run must still be launchable after a restart. The values are removed from the entry when the run is admitted, and the entry is deleted when the run finishes. Records, sidecars, listings and callbacks still never contain env values. Like provider credentials, the entry is protected by owner-only file permissions, not application-level encryption. Callback keys are stored in the entry the same way.

A drain stops admission. Queued runs stay on disk for the next controller to admit, and only running runs hold the drain open.

Finished runs are retained as before: the newest 100 workspace records, plus the newest 100 pool runs that ended before placement. Queued and running runs never count against retention. Idempotency keys live as long as their run is retained.

## Pools

A pool is a named, owner-managed set of workspaces on one or more hosts that share a profile and/or template. A client starts runs on the pool instead of choosing workspaces, and Engine Box spreads them across the pool.

```json
POST /v1/pools
{"name":"workers","description":"Coding agents","profile":"coder","template":"native-agents",
 "workspaces":[{"host_id":"primary","name":"ebox-w1"},{"host_id":"primary","name":"ebox-w2"}],
 "autoscale":{"hosts":["primary","second"],"max_workspaces":8,"size":"recommended"}}
```

- `name` must match `^[a-z][a-z0-9-]{0,31}$`. `workspaces` lists existing members (up to 256), and every listed host must be registered.
- `profile` (optional): every run in the pool uses it. See [profiles](PROFILES.md).
- `template` (optional): the template members must come from. Admission checks it on each placement, and autoscale needs it.
- `autoscale` (optional): when every member is busy and runs are waiting, the scheduler creates new members named `ebox-<pool>-<n>` with the existing creation and readiness path, on the first listed host that has run capacity. It never exceeds `max_workspaces` members in total. A failed creation backs off for 60 seconds and is shown as `last_create_error`. Created members stay in the pool when it is replaced, and they are never deleted automatically.
- Placement picks a free member (no active run and no unresolved lease) on the host with the most free run slots.
- Each run records the workspace it landed on (`host_id`, `name`, null until placed). Once placed, the run is also readable and cancellable on that workspace's routes.
- **Resume stays on its workspace.** Native sessions belong to one workspace. A pool start with `resume_id` is pinned to the workspace that ran that session in this pool, and waits for that workspace even if others are free. An unknown session is refused with HTTP 400. The controller keeps an index of the newest 4096 pool sessions, so a resume works after the original run's record has been pruned.
- A pool with queued or running runs cannot be deleted.

Service tokens are granted pools with `"pools":["workers"]` at mint time. The token's operations (`agent_runs.start`, `get`, `list`, `cancel`) then apply to that pool's routes. Any other pool answers 403, and a token's listing includes runs in its granted pools.

## Not implemented

- A `started` callback event (only `agent_run.finished` is sent).
- Retrying delivery of a callback that was in flight when the controller stopped.
- Per-principal weights or priorities. All principals get equal turns.
- Admission based on GPU, disk I/O or guest load. CPU uses the host CPU count, not the measured load average.
- Automatic removal of idle autoscaled workspaces.
