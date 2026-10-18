# Implementation plan

October 18, 2026. Scope: portable environments for native Codex, Cursor Agent and Claude Code; optional CI optimization; reusable open-source foundation. This plan distinguishes the extracted runtime from the complete bootstrap still to build.

## Milestones

| ID | Deliverable | Depends on | Acceptance |
|---|---|---|---|
| P0 | Clean OSS repository and reusable runtime | None | No owner host defaults or private artifacts; explicit license; offline config/command/lifecycle tests; honest capability documentation. |
| P1 | Versioned config, inventory and `doctor` | P0 | Validate host/guest parameters and capabilities without mutation; actionable results for missing KVM, incompatible versions, low disk and conflicting networks/resources. |
| P2 | Deterministic `plan` and resumable `apply` | P1 | Fresh supported host gets one workspace; second apply is unchanged; failure injection resumes; unrelated Incus/Docker/NFS resources remain intact. |
| P3 | Native-client preparation and clean base | P2 | Official Codex/Cursor/Claude clients launch directly; version/update policy recorded; no auth in reusable base; supported MCP config merges without overwriting user entries. |
| P4 | Storage, desktop and lifecycle acceptance | P2, P3 | Disposable tests prove RAM continuity, disk isolation, identity uniqueness, eligible cache reuse, authenticated desktop, host reboot behavior and backup restore. |
| P5 | Optional GitHub Actions module | P2, P4 | Separate job state, explicit runner registration, compatible persistent caches, graceful drain and measured build/test feedback; core works with module disabled. |
| P6 | Self-hosting release | P1–P4 | Another person bootstraps a second supported machine from documentation alone; upgrade/rollback tested; supported versions and remaining limits published. CI can remain optional/experimental. |

P0 is the initial repository milestone. No later milestone is complete merely because similar behavior worked on the original private host. Login/inference acceptance is recorded separately from installation and environment tests.

## First implementation slice: configuration and inspection

Proposed code boundaries (names may change during implementation):

- `runtime/config.py`: schema, defaults, explicit config loading and environment overrides.
- `runtime/transport.py`: local/SSH argv execution, structured results, timeout and cancellation contract.
- `runtime/backends/incus.py`: inventory and existing lifecycle operations.
- `runtime/doctor.py`: read-only host/guest capability checks and diagnostics.
- `runtime/planner.py`: desired-state diff, operation ordering, ownership/conflict checks.
- `examples/`: minimal existing-Incus and fresh-dedicated-server configurations.
- `tests/`: offline regression tests; disposable integration harness added separately.

Do not implement a generic plugin framework before a second backend exists. Keep config parsing and planning independent of live mutation so they can be tested without administrator access.

P1 is done when an explicit example config can inspect a fresh supported host and the owner's existing server, produce useful diagnostics, and leave both unchanged. Test custom project names, UID/GID, paths and subnets, plus malformed config and absent/unreachable hosts. For stateful VMs, inspect memory-state capacity against RAM plus overhead, available disk reserve and device compatibility; reject a hibernation configuration the host cannot support.

## Safe bootstrap slice

Implement a host lock and operation journal outside the repository. Mark created resources with ownership and schema/recipe metadata. Plans include package changes, storage allocation, network rules and service changes. Recheck the relevant inventory before applying; reject stale/conflicting plans.

Use an existing storage pool by default. A development loop pool requires an explicit bound. Never infer permission to repartition a disk or replace an existing profile because its name matches. Existing NFS/firewall changes require explicit integration support; unknown configurations fail with an actionable diagnostic.

Required scenarios: first apply, unchanged second apply, interrupted installation, name/subnet collision, disk reserve exhaustion, host reboot and selective removal. Validation must preserve an unrelated fixture workload. Destructive rollback is not an automatic recovery strategy.

## Native-client slice

Create provider-specific installation recipes and a resolved manifest, not a harness abstraction. Select only requested clients. Install as the guest user; verify binaries, paths and version reporting. Pin where supported; record and expose update behavior where exact pinning is unavailable.

Add desktop MCP only when selected. Merge an Engine Box entry through each client's supported configuration interface; repeated setup must neither duplicate it nor erase unrelated servers. Create the reusable base before any provider or Git login.

For each client, the operator then validates native sign-in, one bounded prompt, reconnect/session resume and account state after a VM restart. Document quota/expiry behavior from supported diagnostics or real observations; do not consume an allowance merely to force exhaustion. Account tokens and raw auth logs stay out of Git and public CI.

## Disposable environment validation

The integration harness creates its own uniquely named VM/project artifacts and records what it owns. It refuses an arbitrary existing workspace as its destructive test target. Each failed run retains only its own diagnostic artifacts until explicit cleanup.

Cover these behaviors:

1. Native binaries and optional guest MCP tools are present; guest MCP has no host lifecycle authority.
2. A RAM-only random token, process identity and boot identity survive pause and supported memory restoration.
3. A disk fork writes independently, starts with the intended fresh identity and has distinct network identity.
4. Read-only references reject writes; package reuse works across compatible instances; incompatible/trust-separated namespaces stay separate.
5. Reusable base contains no test account material. Private checkpoints are explicitly classified as credential-bearing.
6. Desktop access works through authenticated transport and is rejected from an unrelated guest/network.
7. Storage snapshots exclude external mounts as documented; an exported backup restores onto the tested target.
8. Host reboot, interrupted operations and version incompatibility yield a recoverable state or an explicit diagnostic.

Report individual operation timings and resource conditions. Separate pause/resume, hibernate/wake, disk fork and cold readiness. A mostly idle single measurement is not a general startup guarantee.

## Optional CI slice

First support one native GitHub Actions runner and one representative build/test workflow. Runner registration is explicit and token input is transient. Use compatible versioned tools and persistent caches with separate working directories; keep runner maintenance aware of active jobs.

Benchmark the same commit/resources with a cold cache and repeated warm runs. Include dependency/toolchain invalidation, a known failing test, cache removal/recovery, job cancellation and an agent workspace running alongside CI. Use test cache namespaces and workflows without deployment/signing side effects. Measure queue/preparation/build/test time, failures, disk use and resource contention; do not infer savings solely from cache size.

The existing owner runner remains in place until an explicit migration can preserve routing and drain active work. The portable module must not copy that privileged host setup as its default isolation model.

## Owner-server rollout

1. Inventory the existing Incus project, prepared VM, mounts and single GitHub runner read-only.
2. Test P2–P4 on a disposable VM and isolated names/paths. Existing caches and signed-in workspaces are not test fixtures.
3. Confirm compatibility and record a backup/recovery path before adopting selected existing resources.
4. Keep native login an owner step; do not transfer accounts from the operator laptop implicitly.
5. Add the optional CI module only after its own validation and a runner migration plan if required.

A clean external-host installation is a separate release gate. Success on the owner's customized server alone does not establish easy self-hosting.

## Deferred work

Managed tenancy/billing, fleet placement, swarm orchestration, GPU memory restoration, Windows/ARM support, provider credential brokering, a new CI scheduler and premium engine recipes are outside the initial bootstrap. Unreal/Unity and Cloudflare preparations can use the resulting interfaces later.
