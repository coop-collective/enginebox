# Implementation plan

Updated October 20, 2026. Scope: portable environments for native Codex, Cursor Agent and Claude Code; persistent private agent state; optional personal-computer resources with owner-set CPU/RAM limits; optional CI optimization; a source-available foundation with a managed offering. This plan distinguishes implemented capabilities from platform and release acceptance still required.

## Milestones

| ID | Deliverable | Depends on | Acceptance |
|---|---|---|---|
| P0 | Clean source-available repository and reusable runtime | None | No owner host defaults or private artifacts; Sustainable Use License and historical Apache notices; offline config/command/lifecycle tests; honest capability documentation. |
| P1 | Versioned config, inventory and `doctor` | P0 | Validate host/guest parameters and capabilities without mutation; actionable results for missing KVM, incompatible versions, low disk and conflicting networks/resources. |
| P2 | Deterministic `plan` and resumable `apply` | P1 | Fresh supported host gets one workspace; second apply is unchanged; failure injection resumes; unrelated Incus/Docker/NFS resources remain intact. |
| P3 | Native-client preparation and clean base | P2 | Official Codex/Cursor/Claude clients launch directly; version/update policy recorded; no auth in reusable base; supported MCP config merges without overwriting user entries. |
| P4 | Storage, desktop and lifecycle acceptance | P2, P3 | Disposable tests prove RAM continuity, disk isolation, identity uniqueness, eligible cache reuse, authenticated desktop, host reboot behavior and backup restore. |
| P5 | Optional GitHub Actions module | P2, P4 | Separate job state, explicit runner registration, compatible persistent caches, graceful drain and measured build/test feedback; core works with module disabled. |
| P6 | Self-hosting release | P1–P4 | Another person bootstraps a second supported machine from documentation alone; upgrade/rollback tested; supported versions and remaining limits published. CI can remain optional/experimental. |
| P7 | Native session API and private state | P3 | MCP launches all three native harnesses; exact conversation resume; bounded execution/output; state persists in the private workspace; overlapping writers and conflicting lifecycle actions rejected. Protocol fixtures and live provider inference are reported separately. |
| P8 | Optional personal-resource node | P1, P7 | Explicit opt-in enrollment; enforce chosen aggregate CPU/RAM budget locally and at admission; one disposable Docker workspace validated on Mac and Windows separately; disconnect/sleep/reconnect cannot duplicate a run; no daemon socket in agent containers. |
| P9 | Movable tenant state and native engine hosts | P7, P8 | Authenticated tenant authorization, account separation and private volume attach/lease tests before public multi-tenancy; independent Windows/macOS worker and GPU/image qualification. |

Current implementation includes P0, P1 and the existing-Incus path of P2/P3, plus capacity-aware fleet REST/MCP and a Vinext dashboard. Live validation uses newly created VMs in a separate project on the reference host. Fresh-host package/network installation, host reboot, off-host backup restore, independent installation and wider deployment qualification remain release gates. P4 is partial; P5/P6 are not complete. Login/inference acceptance is separate from installation and environment tests.

The fleet slice adds explicit host selection, clean-template cloning, bounded asynchronous jobs, capacity admission, lifecycle controls, owner authentication, persistent portable templates and opt-in warm workspace pools. A native launch/resume API executes official CLIs without supplying an agent loop. It does not add agent orchestration or automatic placement.

## Current agent-readiness work

The reference Linux fleet now runs Rust 0.3.1-live.0 with its original account state preserved. Two private workspaces received native helper/browser migration, and native-dev revision 4 plus two fresh reserves are synchronized. The migration now supports an external private state root. Real supervisor update/rollback checks preserve that root, browser sessions and stdio MCP access; a separate [metadata qualification](RUST-STATE-QUALIFICATION.md) loaded the deployed Python controller's completed jobs, runs and templates in an isolated copy.

The [native qualification](RUST-NATIVE-QUALIFICATION.md) passed actual file changes, exact conversation resume after service recreation, browser MCP interaction and independent screenshot verification for Codex and Claude. Cursor reached its provider but remains usage-limit blocked; its inference and resume are not qualified. Fresh Rust measurements retain a 0.505-second median frozen resume and 6.288-second median cold readiness across three samples each. These are host-local readiness measurements, not a controlled Python comparison or a model-response guarantee.

Fresh templates need current guest preparation: syncing binaries cannot repair an older image's missing sandbox profile. Existing Engine Box browser entries also need an explicit migration from the Python launcher while preserving custom configuration. The Linux release has passed archive installation, transient service execution and the initial reference-host cutover. A fresh revision-4 warm allocation also passed Codex/Claude code assertions through the installed operator MCP and HTTP controller, then exact conversation resume after MCP pause/resume with unchanged boot identity. Cursor was not retried after its quota rejection; LayerFS-mounted native inference remains unqualified.

The initial cutover completed on October 20, 2026. Qualification on disposable VMs and production activation remain distinct evidence; neither establishes inference through every interface or storage backend. Preserve running private workspaces and account paths. Qualify the new clean template generation and capacity for its fresh reserves before routing allocations to it; stale reserves consume disk commitments even when their physical data is small. Provider quota failures remain separate from infrastructure failures.

LayerFS now has compatibility-checked, durable allocation preparation and conservative reconciliation of already-completed attachment effects. Automatic attachment/finalization, release rollback fencing against active attachments, recovery after a real host restart and native agent execution on an attached checkout remain open. See [allocation](LAYERFS-ALLOCATION.md) and [attachment recovery](LAYERFS-INCUS.md). The persistent core dev environment and disposable database forks remain the next environment feature; they are not supplied by checkout caching alone.

## Implemented code boundaries

- `packages/enginebox-core`: validated configuration, SSH/Incus transport, conservative capacity, bootstrap, guest preparation, templates, fresh warm pools and host capability inventory.
- `packages/enginebox-agents`: original CLI installation, central native credentials, access-only delegation, bounded native runs and exact resume, usage observations, private browser/computer tools.
- `packages/enginebox-controller`: authenticated REST, private durable jobs, reconciliation and exclusive controller ownership.
- `packages/enginebox-cli`: operator terminal and fleet MCP access.
- `packages/enginebox-install`: immutable native release artifacts, checksum-verified installation, platform service plans, supervised upgrade and compatible rollback.
- `packages/enginebox-storage-layerfs`: first-class checkout storage with pinned node-local execution, stable project paths, immutable pnpm reuse and file checkpoints. Controller registration and bounded API/MCP jobs expose the backend. Private provider homes and RAM snapshots retain separate contracts; see [backend scope and limits](LAYERFS-BACKEND.md).
- `packages/dashboard`: Vinext owner dashboard, session authentication and server-side proxy.
- `examples/`: host/fleet configuration and portable template manifests. Tests live with their owning package.

The Rust port uses root pnpm with Cargo and JavaScript lockfiles. Native Linux, macOS and Windows checks passed for the merged port. Real controller upgrade/rollback through the stable supervisor has been tested with concurrent requests and the real dashboard; Windows service installation and fresh-host bootstrap remain distinct gates; the reference deployment has completed its initial Python-to-Rust cutover. See [migration](RUST-MIGRATION.md) and [updates](UPDATES.md).

Do not implement a generic plugin framework before a second backend exists. Keep config parsing and planning independent of live mutation so they can be tested without administrator access.

P1 is done when an explicit example config can inspect a fresh supported host and the owner's existing server, produce useful diagnostics, and leave both unchanged. Test custom project names, UID/GID, paths and subnets, plus malformed config and absent/unreachable hosts. For stateful VMs, inspect memory-state capacity against RAM plus overhead, available disk reserve and device compatibility; reject a hibernation configuration the host cannot support.

## Template and warm workspace slice

Implemented: Minimum/Recommended/Custom resource selection, API-enforced floors, serialized JSON recipes independent of host bootstrap, controller-owned desired state, automatic synchronization to all registered nodes and per-node revision status, and warm allocation with cold fallback. Template imports do not run arbitrary scripts; the initial recipe supports official native clients and optional browser/desktop on Ubuntu 24.04.

A registered revision is offered only on hosts whose installation receipt matches that revision and their current host/project/pool/guest binding. Installation consumes normal host admission under the shared lock. Existing workspaces retain their original image and files.

Warm reserves are booted and paused fresh clones with independent identities. Assignment commits a permanent claim before resume; signed-in workspaces are never recycled. Refill releases the mutation lock during guest readiness, respects reserves and quarantines interrupted/stale artifacts. Frozen RAM occupies real capacity; deletion and stale-generation cleanup remain explicit operator operations. Automatic placement, arbitrary template plugins and cross-host image replication remain separate work.

## Safe bootstrap slice

Implement a host lock and operation journal outside the repository. Mark created resources with ownership and schema/recipe metadata. Plans include package changes, storage allocation, network rules and service changes. Recheck the relevant inventory before applying; reject stale/conflicting plans.

Use an existing storage pool by default. A development loop pool requires an explicit bound. Never infer permission to repartition a disk or replace an existing profile because its name matches. Existing NFS/firewall changes require explicit integration support; unknown configurations fail with an actionable diagnostic.

Required scenarios: first apply, unchanged second apply, interrupted installation, name/subnet collision, disk reserve exhaustion, host reboot and selective removal. Validation must preserve an unrelated fixture workload. Destructive rollback is not an automatic recovery strategy.

## Native-client slice

Create provider-specific installation recipes and a resolved manifest, not a harness abstraction. Select only requested clients. Install as the guest user; verify binaries, paths and version reporting. Pin where supported; record and expose update behavior where exact pinning is unavailable.

Add desktop MCP only when selected. Merge an Engine Box entry through each client's supported configuration interface; repeated setup must neither duplicate it nor erase unrelated servers. Create the reusable base before any provider or Git login.

For each client, the operator then validates native sign-in, one bounded prompt, reconnect/session resume and account state after a VM restart. Document quota/expiry behavior from supported diagnostics or real observations; do not consume an allowance merely to force exhaustion. Account tokens and raw auth logs stay out of Git and public CI.

The dashboard **Agents** panel connects the owner once at the controller. Assigned private workspaces receive native access credentials after create, allocation and resume, with periodic reconciliation for existing workspaces and refreshed tokens. Clean bases and unassigned warm reserves remain credential-free. Provider-specific compatibility and expiry limitations are documented in [native clients](NATIVE-CLIENTS.md); this does not add a prompt router or account pooling across unrelated owners.

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

Personal computers are an explicit next delivery track, not an implicit consequence of a portable native installer. Keep Incus as the tested Linux VM backend. Add a small Docker execution adapter for Linux workloads on laptops after its disposable proof, then a local resource service with authenticated enrollment and enforced owner-selected CPU/RAM limits. Node contribution is opt-in; it must not take over all of a personal machine. Existing physical reserves and other workloads still reduce admissible capacity.

CPU/RAM limits are the first resource contract. GPU contribution is a separate opt-in capability with real driver/workload tests and exclusive device leases by default. Windows/macOS operating-system images and native Unity/Unreal execution require distinct backend, platform and license qualification. Native OS workers do not inherit Incus RAM-restore guarantees. See [portable hosts](PORTABLE-HOSTS.md).

Public multi-tenancy requires real tenant authorization and private account/state storage. The current single-owner controller and per-workspace native homes do not establish that guarantee. Shared writable `.claude`, `.codex` or Cursor state across unrelated users is prohibited. Moving a tenant's state between workspaces needs stable paths, exclusive writer leases and recovery tests; see [native state](TENANT-AGENT-STATE.md).

Automatic placement, swarm orchestration, GPU memory restoration, a new CI scheduler and premium engine recipes remain separate work. Usage widgets never equate model context, local token costs or quota reset dates with a subscription balance or billing renewal. A preview release may publish tested operator/runtime artifacts while P6/P8/P9 remain incomplete, provided its release notes state those limits.

## Checkout storage and large game assets

[LayerFS](LAYERFS.md) is a first-class checkout provider exposed through controller registration, durable API jobs, MCP and the Storage dashboard. The pinned preview remains explicitly provisioned on qualified Linux nodes; it is not downloaded by a general installer. Keep file checkpoints separate from Incus RAM checkpoints, and retain uncertain state for operator inspection. See [the backend contract](LAYERFS-BACKEND.md) and [Incus attachment](LAYERFS-INCUS.md) for the tested boundaries and remaining release gates.

The October 23 owner decision selects [LayerFS game workspaces](GAME-WORKSPACES.md)
and removes Lore as a planned dependency. The existing backend already combines
private container branches with stable `/project` mounts and prepared checkpoint
forks. Preserve native provider homes and the separately qualified Incus export.
The design maps all ten upstream core ideas to implemented behavior or explicit
remaining work; a LayerFS version upgrade is not implicit in this decision.

[Store distribution](STORE-DISTRIBUTION.md) separates the storage server from
workers' local NVMe Stores. The same existing server may hold both roles during
qualification. Work proceeds through these dependent boundaries:

1. Qualify the pinned container backend for game ingestion, exact task checkpoints
   and native engine cache reuse; complete allocation/attachment readiness.
2. Publish consistent, verified Store generations; qualify desync chunk delivery
   against SQLite's remote-copy baseline; add manifest authorization, single-flight
   hydration, residency leases, space reservations and recoverability-gated eviction.
3. Add logical cross-worker result import into the canonical LayerFS history and
   Gamejam's batched candidate checks. Database copying must never stand in for
   merging independently written Stores or accepting a tested artifact.

Automatic eviction, the distributed provider and cross-worker integration remain
unimplemented. Large-project archive and checksum measurements do not qualify
those paths or first playable frame. The retired Lore comparison remains
[historical research](../research/storage-benchmarks/lore-assessment.md).

## Persistent project environments and disposable agent forks

The proposed [development-environment service](DEV-ENVIRONMENTS.md) keeps one persistent core per project, tracking its main branch while retaining application data across code updates. A project can configure an optional dev start command, readiness check and service ports. Sleep/wake controls the environment's compute lifecycle without treating its data as a fresh template on each start. These configuration fields and controller operations are not implemented yet.

The online core publishes a consistent versioned baseline; agents receive disposable environments with private writable code/data state from that exact revision. Publication needs database-aware capture and any brief write barrier required to coordinate the included services. Validate the complete candidate before changing the published pointer. Failure must retain the prior revision and leave the core's outcome visible. A continuously running core is the product model; the earlier stop-and-copy probe is evidence for local state inheritance, not the final publication design.

The first [Wrangler recipe](LAYERFS.md#wrangler-environment-state-forks-proposed) covers local D1/KV/R2/SQLite Durable Object data and connected Workers. The historical 15-check local prototype is documented with its exact paths and versions in [the evidence record](DEV-ENVIRONMENTS.md#existing-local-proof). It does not qualify online capture, LayerFS database locking, managed staging, main-branch reconciliation or hosted lifecycle. DuckDB is not a product dependency, and remote Cloudflare bindings are not included in filesystem forks.

Delivery order: project registry and owned startup service; code updates that preserve data and apply explicit migrations; sleep/wake and application readiness; consistent version publication under concurrent writes; two isolated agent forks with restart/readback and failed-publication tests; then dashboard/API/MCP integration and representative performance measurements. Each fork has its own persistence directory. Intentionally shared services are accessed through scoped APIs. Return code through Git review and data through explicit migrations/imports, never an automatic merge of database files.
