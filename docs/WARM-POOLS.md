# Warm workspace pools

A warm pool keeps prepared, credential-free VMs booted and paused before a person
requests a workspace. Allocation reserves one eligible VM, resumes it and checks
readiness. The native Codex, Claude and Cursor applications remain unchanged;
owners sign in after allocation. Pooling does not combine subscription balances
or execute agent prompts.

## Opt in per host

Warm pools are disabled unless configured. Add a pool after that host's template
definition in the [fleet configuration](FLEET.md):

```toml
[[hosts.warm_pools]]
template = "native-agents"
size = "small"
ready = 2
```

The template must identify an existing clean, disk-only prepared snapshot. Sizes
can select the template's `minimum` or `recommended` resources, or the legacy
`small`, `standard` and `large` presets when they meet its minimum. `ready` is the
desired number of unassigned paused VMs for that template and size, subject to the
host's admission limits. Duplicate template/size policies and counts outside
0–32 are rejected. A target of zero stops replacement creation; it does not remove
existing VMs. The API starts the refill worker for configured pools and revisits
their state periodically and after completed jobs.

A paused VM still occupies RAM, CPU allocation, its full disk commitment and an
instance slot. Reserving warm capacity trades those resources for lower allocation
latency. Refills use normal admission checks and never reduce host reserves to
reach the requested pool count. Assigning an existing paused VM consumes its
already admitted allocation; it does not require capacity for another VM.

## Request a workspace

Send an authenticated JSON request to
`POST /v1/hosts/{host_id}/allocations` using the same bearer token as the fleet API:

```json
{"name":"ebox-my-project","template":"native-agents","size":"small"}
```

Omit `size` to use the template's recommended resources, or supply a complete
custom request instead:

```json
{"name":"ebox-my-project","template":"native-agents","resources":{"cpu":6,"memory_gib":8,"disk_gib":60}}
```

Custom resources must meet the template minimum and match an existing member
exactly to use it. The endpoint returns HTTP 202 with a job; poll
`GET /v1/jobs/{job_id}` before treating the workspace as usable. A successful warm
result includes, for example:

```json
{"name":"ebox-warm-0123456789abcdef","workspace_name":"ebox-my-project","startup":"warm","status":"Running"}
```

Both physical and friendly names currently follow the existing `ebox-` naming
rules and must not conflict with another workspace in the project. The separate
`/instances` creation endpoint always makes an ordinary cold clone. Lifecycle
requests against unassigned or quarantined pool members are rejected; inspect
those artifacts through the operator tools instead.

`GET /v1/fleet` includes each host's pool target, ready/preparing counts, selected
resources and any refill error. A count is a point-in-time observation, not a
reservation for a later request.

## Identity and assignment

Each member has its own machine identity and private writable disk before it
enters the pool. Eligibility requires the expected template generation, size,
storage pool and actual resource limits, a completed preparation marker and the
Incus `Frozen` state. A stale `Running` or `Stopped` VM does not become available
merely because its metadata once called it ready.

The physical Incus name remains unchanged at assignment. A separate friendly
workspace name identifies the allocation. Callers must use the physical `name`
returned by the allocation job for lifecycle operations. Renaming a paused VM is
not part of assignment.

When no matching member is available, allocation falls back to ordinary cold
creation with normal capacity checks. The job result reports `startup: "warm"`
or `startup: "cold"`; a configured pool does not guarantee a warm hit. Two requests
cannot both receive the same reserve: the first claim removes it from eligibility,
and the other request needs another matching member or capacity for a cold clone.

The controller writes a persistent claim into the instance's description before
resuming it. The claim records the friendly workspace name, job ID and template
generation. Cooperating controllers serialize selection and reservation with the
existing host lock. Incus metadata is the reservation record; an old API job
record alone cannot make a VM available again.

Once claimed, a VM never returns to the available pool, including after a failed
resume, failed readiness check or controller restart. Inspect a failed allocation
before taking further action. The original VM may contain private account state
after an owner starts using it, so cleanup or recycling is an explicit operator
decision.

Preparing replacements releases the host mutation lock while waiting for guest
readiness. This lets other requests claim already prepared members while a new
guest boots. A replacement becomes eligible only after successful readiness and
pause. An interrupted preparation remains excluded and still counts toward actual
capacity. Current-generation quarantined members stop automatic refill for that
pool until an operator inspects them, preventing repeated failed replacements
from consuming the host's remaining capacity.

## Template updates and limits

Pool members belong to a particular template generation. Changing the configured
template or replacing its snapshot must not silently reuse an earlier generation.
Old or mismatched members remain visible for operator inspection and are excluded
from new assignments. The controller does not delete existing workspaces to make
room.

Only trusted Engine Box operators should have administrative access to pool VMs.
Do not sign into an unassigned member or store account state in its base snapshot.
The clean-base and pool markers are managed-workflow safeguards, not a universal
credential scanner or a tenant authorization boundary.

Pausing preserves a running machine's RAM; a host restart does not preserve that
paused RAM. Network connections and provider sessions can expire while paused.
Allocation performs the same guest identity, tools and networking readiness
checks as ordinary creation. It does not validate provider credentials or spend
model quota.

## Verification

Run the offline allocation, admission and recovery regressions with:

```sh
PYTHONPATH=runtime runtime/.venv/bin/python -m unittest discover -s tests -p 'test_warm_pool.py' -v
PYTHONPATH=runtime runtime/.venv/bin/python -m unittest discover -s tests -p 'test_fleet_templates.py' -v
```

These checks include the real authenticated API and job queue with an in-memory
Incus boundary. They do not boot VMs, sign into providers or execute inference.

On October 19, 2026, a new disposable VM on Incus 6.0.5 passed ordinary fleet
admission and readiness, then entered `Frozen`. Updating its description property
with an `enginebox-pool:v1:` reservation persisted in **0.688 seconds** while it
remained frozen. The test then resumed, stopped and deleted only that disposable
VM. This establishes the required metadata behavior; it is not an end-to-end warm
allocation latency measurement.

This metadata path matters on the tested Incus version: updating `user.*`
configuration can wait for the paused guest agent, and a frozen VM cannot be
renamed. The claim therefore uses the description property and preserves the
physical name.
