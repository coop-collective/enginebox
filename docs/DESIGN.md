# Portable node runtime and Cloudflare fleet design

Status: implementation design updated October 24, 2026. The Rust runtime, bootstrap, native-client/browser preparation, fleet REST/MCP and Vinext dashboard are implemented. Compatible Rust service upgrades and rollback are tested; initial legacy cutover, portable cache services, CI and backup/recovery qualification remain separate work. See the operational guides for supported commands.

October 24 fleet direction: **Cloudflare Workers and Durable Objects own the target fleet authority**. Every production bare-metal machine is a managed compute and/or storage node. The same Worker runs under local Wrangler for development; nodes never elect a replacement controller. The [local fleet implementation](LOCAL-FLEET.md) adds scoped enrollment, leases, durable generation acceptance, R2-backed Store transfer, optional storage-node read-through caches and local eviction. One node may keep its project local; two or more nodes sharing generations need a shared object service, which R2 alone can provide. Compute and storage can share a server.

LayerFS and containers remain the project-workspace foundation. Immutable SQLite generations are distribution/recovery units; independently written Stores require logical integration, not database-file merging. The existing Rust/Incus architecture below describes the deployed runtime, whose provider state and execution adapters still need an explicit migration before Cloudflare replaces its authority.

## Outcome

An owner connects a Linux server, prepares a workspace once, signs into the official Codex, Cursor Agent or Claude Code client, and works through the ordinary terminal. Files, compatible dependencies and caches remain available. Optional CI uses the same preparation contracts to return reliable build/test results sooner.

Start with one trusted owner and one server. Support different server providers and existing compatible Incus installations through configuration, rather than hardcoding one machine. Launching an official CLI and resuming its native conversation belong in the environment API. Enginebox owns reusable task dispatch, agent workspace coordination, merging, batched validation and promotion; apps consume its APIs. Native harness planning and inference remain in the official clients. See [the integration/CI contract](INTEGRATION-CI.md).

## Components

The following diagram describes the implemented native controller. The required
Cloudflare target and migration boundary follow it.

```mermaid
flowchart TD
  W[Owner: Vinext dashboard] --> F[Authenticated fleet API]
  M[Operator fleet MCP] --> F
  F --> T[SSH or local transport]
  O[Owner: CLI] --> T
  T --> C[Environment control and configuration]
  C --> I[Incus backend]
  I --> A[Private agent workspace VM]
  A --> N[Native Codex / Cursor / Claude]
  A --> D[Optional guest desktop MCP]
  I --> B[Optional separate CI workspace]
  G[GitHub Actions] --> B
  A --> S[Eligible cache and reference storage]
  B --> S
  R[Versioned preparation recipes] --> A
  R --> B
```

The core owns environment lifecycle, preparation manifests, storage/mount interfaces and resource limits. The native-client recipe installs official binaries and configures supported local tools; it does not sit between the harness and provider. The CI module owns runner registration, job preparation, cache mappings and CI measurements. A workspace remains usable without GitHub or a runner. LayerFS owns project source and branch history; [GitHub connections](GITHUB-SYNC.md) import/export files without placing Git in normal workspaces. Dedicated reconciliation and Actions compatibility environments may use Git; specialized-agent conflict resolution and conditional external commits still follow LayerFS main acceptance.

[Required LayerFS preparation for Actions](GITHUB-ACTIONS-ACCELERATION.md) is implemented as an opt-in profile: native runner work/cache directories use isolated prepared storage, and a post-checkout pnpm action validates actual inputs before installed-tree reuse or locked-install fallback. Single-repository workflow-restricted admission and host-side actual-job verification bind private generations to authorized jobs. The fleet catalog and verified Store activation are implemented; direct SDK reopen and branch isolation are tested. Live Incus/Actions qualification, automatic warm compute placement and comparative speed/cost evidence remain release gates. See [setup](PREPARED-ACTIONS-SETUP.md).

The first backend is Incus. Keep backend-specific operations behind a small internal interface, but implement only this backend until another real deployment requires more. Local and SSH transports should support the same operations.

## Required Cloudflare fleet control plane

Cloudflare owns node registration, authorization, desired state, workload placement,
resource reservations, task/engine leases, accepted project revisions and recovery
decisions. This applies even when one physical server combines compute and storage.
Bare-metal machines run the native Engine Box node service and existing execution
backends. Their local journals and inventories record execution evidence; they do
not supersede the Cloudflare authority or become a fallback fleet controller.

```mermaid
flowchart TD
    UI[Dashboard and operator MCP] --> API[Cloudflare Workers: authenticated fleet API]
    API --> Registry[Durable Objects: tenant membership and policy]
    API --> Projects[Durable Objects: project tasks, leases and accepted revisions]
    API --> Nodes[Durable Objects: node sessions and capacity reservations]
    Projects --> Nodes
    Nodes <-->|Node-initiated authenticated connection| Runtime[Bare-metal Engine Box node service]
    Runtime --> Agent[Private agent containers and engine processes]
    Agent --> Local[Node-local LayerFS Store and NVMe caches]
    Runtime <-->|Authorized object transfer| R2[R2: optional single-node storage; shared generations and recovery]
    Runtime <-->|Optional nearby origin| Storage[Managed storage node]
    Storage <--> R2
```

Use Workers for authentication, API/MCP ingress and routing. Use SQLite-backed
Durable Objects for authoritative coordination, partitioned by tenant membership,
node and project rather than one object serializing the entire fleet. A node's
object owns its reservations; a project's object owns its accepted-head transitions
and task decisions. Keep global listings or search in a derived index such as D1
only if needed; that index cannot issue leases or override authoritative records.
Each Durable Object has transactional, strongly consistent storage; this is not a
transaction spanning multiple objects, nodes and R2. Persist before acknowledging
state changes. [Durable Object storage](https://developers.cloudflare.com/durable-objects/best-practices/access-durable-objects-storage/).

Placement uses a durable attempt ID: reserve capacity through the node authority,
bind the reservation to the project attempt, and dispatch only after both records
agree. Expire/reconcile partial reservations. Retries must return the recorded
outcome or continue the same attempt, never allocate another engine implicitly.
The node still checks actual local capacity under its existing host lock and may
reject a reservation that no longer fits. Build and merge execution stays on nodes;
only the Cloudflare project authority may accept the resulting revision.

Nodes enroll through an owner-authorized, expiring registration and get revocable
identities scoped to their tenant/node. Prefer node-initiated authenticated HTTPS
or WebSocket control connections, with bounded messages, reconnect cursors and
boot/session identity. Agent containers never receive these credentials. Durable
Object WebSocket hibernation is available for idle connections; the session and
assignment state must survive object restarts. [WebSockets](https://developers.cloudflare.com/durable-objects/best-practices/websockets/).

Every dispatched operation binds its project, node, attempt, desired revision,
lease expiry and fencing epoch. Nodes journal operations before execution and
report receipts idempotently. Reconnection reconciles receipts against Cloudflare
state instead of replaying every command. Queues can provide retryable notification
delivery when needed, with a durable outbox and deduplication; queue messages are
not assignment authority. Delivery can occur more than once. Long native tasks do
not occupy a Worker request while they run. [Queues delivery](https://developers.cloudflare.com/queues/reference/delivery-guarantees/).

Cloudflare unavailability blocks new placement, lease renewal and promotion. A
node can retain isolated local edits and finish work within an existing valid
lease; exclusive shared effects must stop at lease expiry or revocation. Missing
heartbeats do not prove an old process stopped. Cloudflare must fence/reconcile
uncertain attempts before assigning conflicting work, and reject stale results.
There is no bare-metal leader election during an outage.

Control state and large project bytes remain separate. R2 or an authorized storage
origin serves immutable generations directly to nodes; Workers/Durable Objects
authorize access and record receipts rather than buffer multi-GB Stores. Optional
storage nodes register like compute nodes and receive replication/repair work from
Cloudflare. They cannot publish accepted revisions. See [storage modes and recovery](STORE-DISTRIBUTION.md).

Migration must preserve the existing native execution, private provider homes and
host admission contracts. Establish Cloudflare identities and durable state, import
versioned desired state, drain the old controller, reconcile uncertain jobs, then
fence its fleet mutation authority before cutover. Never run both controllers as
independent writers. Native authentication helpers remain native; moving refresh
ownership and secret access requires its own qualified migration. The current
Rust REST service, direct SSH operator path and controller-owned credential store
remain implementation facts, not evidence that this Cloudflare design is deployed.

### Local development and test fleets

Run the same Worker/Durable Object controller code under `wrangler dev` for local
development and disposable integration tests. This satisfies the Cloudflare-based
controller design without requiring a deployed account for fully simulated
resources. Wrangler uses Miniflare/workerd locally, and Durable Objects and R2
have local simulations. Production still uses the deployed Cloudflare authority;
the local dev server is not a production outage fallback. [Local development](https://developers.cloudflare.com/workers/local-development/),
[binding support](https://developers.cloudflare.com/workers/local-development/bindings-per-env/).

| Mode | Controller | Project storage | Dedicated storage machine |
|---|---|---|---|
| One local agent node | Local Wrangler, persistent local Durable Objects | Local LayerFS Store; optional local R2 simulation | None |
| Two-node local integration test | One reachable local Wrangler controller | Shared development object API backed by one local R2 simulation, or an isolated real R2 test bucket | None |
| Distributed staging/production | Deployed Worker and Durable Objects | R2 directly; optional nearby managed storage replicas | Optional |

Wrangler can run on the developer host or inside the Linux development environment.
The node connects through its configured controller URL; container/VM `localhost`
does not generally identify a host-side Wrangler process. Qualify the actual
private network address, authentication and reconnect behavior. For one test fleet,
both nodes connect to the same controller instance/namespace. Separate Wrangler
controllers with separate persistence directories simulate separate fleets.
Do not make independent runtimes share a live `.wrangler/state` directory.

Persist controller development state in a private ignored directory, outside task
branches and LayerFS checkout mounts; Wrangler's `--persist-to` selects that path.
Use the same path for local data commands. Application previews inside agent
workspaces may run their own Wrangler processes with independent data, but those
processes are not additional fleet controllers. Agent test code must not gain the
fleet's state directory or credentials. [Local persistence](https://developers.cloudflare.com/workers/local-development/local-data/).

LayerFS requires Linux and local ext4, XFS or Btrfs for the Store, with every opener
sharing that Linux kernel. A Linux developer machine needs no extra VM for this
requirement; the existing Engine Box backend still uses Docker workspaces. On
macOS/Windows, put the node's Store/helper/daemon in a qualified Linux VM, commonly
provided by a Linux-container platform, with a volume actually backed by a supported
Linux filesystem. A macOS/Windows directory bind-mounted into a container is not
equivalent. Validate filesystem type, FUSE/mount permissions, architecture and pinned
artifacts; a container alone does not establish compatibility. Laptop node enrollment
and this complete local stack remain unqualified. [LayerFS contract](https://layerfs.ai/docs/core-ideas/),
[platform scope](PORTABLE-HOSTS.md).

Pin Wrangler/Miniflare with the future controller package. Engine Box currently has
no controller Worker or Wrangler fleet launcher; these are acceptance requirements,
not working commands supplied by this PR. Require local restart/reconnect tests,
two nodes with separate Store volumes, then two separate Linux kernels against an
isolated real bucket. Two containers sharing one VM only qualify the same-kernel
case. Local simulations do not establish remote latency, host-loss recovery or
Cloudflare production availability.

## Initial support contract

| Area | Initial target and evidence |
|---|---|
| Operator | Native Rust CLI/controller on Linux, macOS and Windows; OpenSSH for remote Linux nodes. Native builds/tests passed on all three platforms. Personal resource contribution is a separate backend qualification. |
| Host | Linux x86_64 with `/dev/kvm`, sufficient disk/RAM and administrator access. Reference prototype: Ubuntu 26.04, Incus 6.0.5, QEMU 10.2. |
| Guest | Ubuntu 24.04 x86_64, configurable non-root user and workspace paths. |
| Server provider | Dedicated hardware, or a VM exposing usable KVM; provider-independent configuration. |
| Next qualification tracks | Personal macOS/Windows resource contributors, ARM Linux containers, native Windows/macOS workers and explicit GPU allocation. These are planned backends, not capabilities of the current Incus implementation. |
| Separate acceptance gates | Windows/macOS guest images, GPU editor support, cross-host migration, hostile tenancy and automatic fleet scheduling. |

Publish a tested host/kernel/Incus/QEMU matrix. Current upstream [Incus requirements](https://linuxcontainers.org/incus/docs/main/requirements/) must not be confused with the requirements of the specific release tested in the prototype. Do not install a moving package stream and claim support for every Ubuntu host.

## Configuration and bootstrap

Use a versioned, secret-free TOML file validated by the Rust runtime. [The bootstrap example](../examples/enginebox.example.toml) is supported by `init`, `doctor`, `plan` and `apply`. [Fleet configuration](../examples/fleet.example.toml) lists hosts, reserves and approved snapshots. The lower-level runtime uses explicit environment variables or internal configuration objects.

Configuration describes transport/host, Incus project/profile, existing or managed network, storage pool, guest resources/identity, native-client selection and optional modules. Mount/cache policies remain a separate unimplemented extension. Credentials belong in native clients or short-lived setup input, never this file.

Command status:

| Command | Contract |
|---|---|
| `ebox init` | Write an example config; no host access or mutation. |
| `ebox doctor` | Read-only capability and conflict checks; readable and JSON results. |
| `ebox plan` | Show desired/observed differences, ownership, operations, resource impact and conflicts. |
| `ebox apply` | Apply a validated plan with a host lock, recheck relevant state, record progress and verify postconditions. |
| Clean-base preparation | Implemented within `ebox apply`; no separate `ebox image prepare` command yet. Writes installed versions and captures a pre-login snapshot. |
| `ebox module ci ...` (planned) | Explicit optional runner setup and maintenance; core bootstrap never registers runners implicitly. |

Existing runtime commands (`list`, `shell`, `exec`, `pause`, `resume`, `hibernate`, `stop`, `snapshot`, `fork`, `restore`) use the environment values returned by bootstrap. Fleet operations construct per-host runtime objects, keeping hosts isolated from process-wide environment changes.

Bootstrap stages:

1. Inspect OS/architecture, KVM, Incus/QEMU compatibility, available resources, storage, networks, routes and existing services. Check stateful-VM capability and device compatibility when memory restoration is requested; reserve sufficient memory-state storage (`size.state`) for configured RAM plus overhead and include it in disk-reserve calculations.
2. Resolve a plan. Reject conflicting names/subnets/ports and insufficient resources before package or network changes.
3. Install supported packages only when requested; record versions. Reuse a compatible existing installation.
4. Create owned storage/network/profile resources, or explicitly adopt selected existing ones. Names alone do not establish ownership.
5. Launch and prepare a guest, install selected official clients as its user, and verify versions and tools without signing in.
6. Create a clean base, write a preparation manifest and show the native login/access instructions.

A second apply should be a no-op when desired state is unchanged. An interrupted apply should resume from verified state; it must not assume an earlier step succeeded. Do not automatically retry destructive operations. Removal targets only owned resources and requires an explicit data-deletion action for user disks.

## Storage, cache and network model

Prefer an existing compatible storage pool. Offer bounded loop-backed storage as an explicit development option; never automatically repartition a disk. [Incus documents storage backend tradeoffs](https://linuxcontainers.org/incus/docs/main/explanation/storage/).

| Data | Handling |
|---|---|
| Writable project and installed dependency tree | Private VM disk, or an explicitly selected private LayerFS container branch at `/project`; prepared checkpoints retain compatible dependencies. |
| Large repository and asset references | Versioned read-only mounts near compute; each workspace gets independent writable state. |
| Download/build caches | Tool-supported reuse, separated by trust domain, OS/architecture and relevant toolchain/input keys. |
| Provider account state | Private controller refresh credentials; access-only credentials delivered to the same owner’s assigned workspaces. Never stored in caches or clean bases. |
| Git and browser account state | Private workspace identity, with separate explicit authorization. |
| External data | Its own backup and versioning policy; not implicitly captured by a VM snapshot. |

The October 23 game-workspace decision uses LayerFS with containers.
The backend already creates and limits private Docker
workspaces over node-local project Stores. The qualified Incus adapter exports a
checkout view while keeping every Store opener in the host kernel; an attached
VM never receives the backing SQLite file. See [core-idea mapping and current
boundaries](GAME-WORKSPACES.md).

The proposed [distributed storage model](STORE-DISTRIBUTION.md) puts recoverable,
immutable project generations in R2 and/or an optional storage origin, with active
Stores on worker NVMe. Single-node project storage can remain local; cross-node
execution requires shared storage. Cloudflare placement favors compatible cached
projects. Engine Box owns residency leases, capacity reservations and eviction
after remote recoverability is proved. The storage and worker roles may initially
run on the same managed node.
Cross-worker edits need logical revision integration; live database files are
not shared across kernels and independently changed SQLite copies are not merged.

The prototype used guest NFS because its Incus version rejected direct shared devices with stateful migration enabled. Generalize this as an optional mount backend. Initially use an existing suitable NFS endpoint or an isolated, explicitly provisioned service. Do not disable unrelated rpcbind/statd services, replace global NFS bindings or claim arbitrary shared mounts support memory restoration.

Detect UFW/nftables/Docker policies and subnet overlaps. Plan narrow, owned rules; preserve unrelated workloads. Desktop access must be authenticated and inaccessible to unrelated guests, even on a private bridge. A private IP is not an authentication mechanism.

## Lifecycle and identity

- Pause retains RAM and consumes host memory. Resume continues that process tree.
- Hibernate writes memory to storage and frees running memory; time depends on memory contents and storage. Connections can expire while suspended.
- A disk-only fork boots fresh. It does not continue the source process tree.
- A running private checkpoint may contain disk, RAM, provider tokens and conversation history. Keep it within the owner's access boundary.
- A reusable base is built before login. Template preparation must handle machine identity, hostname and host keys in addition to unique network identity.
- Snapshot retention and off-host backup are separate functions. Test export/restore; a same-pool snapshot does not survive losing the pool. [Incus backup semantics](https://linuxcontainers.org/incus/docs/main/howto/instances_backup/)

Lifecycle changes need serialized operations and state rechecks. Reject conflicting operations. Host reboot recovery, saved-state compatibility after upgrades, disk reserve and failed-restore recovery belong in release acceptance. Do not promise instant hibernation or GPU memory restoration.

## Native clients and MCP

### Persistent private agent state

Native conversation history and filesystem memory are different from process RAM. Exact conversation resume starts the provider's native CLI against its saved conversation. Pause or hibernation can preserve the running process when the backend supports it. Neither operation promises provider-side prompt-cache reuse.

The current controller authenticates one trusted owner. Each assigned workspace has its own persistent guest home and project disk; native sessions in that workspace reuse those directories. Private state includes provider history, project memory, skills, settings, scratch files and explicitly authorized browser profiles. Reusable bases, download caches and unassigned warm reserves must never contain that state. Two unrelated tenants must never share a writable provider home or account connection.

Future multi-tenant service design must add an authenticated tenant principal, tenant-scoped account storage, authorization on every workspace/session operation, and a private persistent state volume. Tenant identity cannot be a trusted request-body field. Separate session IDs do not isolate users who share a Unix account or writable home. A guest must not receive another tenant's storage, controller refresh credentials or host control sockets.

Moving native memory between a tenant's workspaces requires stable project paths and an exclusive writer lease for mutable provider state, with crash recovery and tested native compatibility. Conversation databases and credential/configuration files cannot be treated like concurrent package caches. Preserve provider formats; do not merge opaque history databases. Read-only, versioned skills and explicitly shared project notes can be distributed separately. Portable tenant-volume mounting and public tenant authorization remain release gates; persistent state in today's private VM is not a substitute for either.

### Native execution and access

Codex, Cursor Agent and Claude Code run unmodified in the guest. Native installation recipes record selected and resolved versions, installation source and update policy. Where providers cannot pin a release, say so and record what was installed rather than promising byte-for-byte reproducibility.

The owner completes each native sign-in. Subscription availability, refresh and quota behavior must be validated per provider and account mode. Installation and `--version` checks do not establish successful inference. See [native-client design](NATIVE-CLIENTS.md).

The dashboard's **Agents** panel starts native browser/device authorization on the controller. The controller keeps refresh credentials in private provider homes and delivers only the access material needed by the owner's assigned workspaces. Adoption of an existing signed-in workspace is explicit and retires its refresh ownership. Reconciliation preserves manual credential changes and checks host binding and guest identity; clean bases and unassigned warm reserves receive no accounts. OS file permissions protect the storage; this is not an encrypted secret vault. A connected account does not prove a paid entitlement or available quota. Native processes keep their provider's own limits, and long-running processes may need restarting after an access token expires.

Operator MCP uses the operator's existing SSH/Incus authority. Guest MCP exposes computer tools only and receives no host keys or Incus socket. Its restricted tool list is not itself an authorization boundary; actual isolation depends on credentials and host/guest permissions. Installing an MCP server does not disable the native harness's approval policy. Desktop is optional and should be provisioned only when requested; guest computer MCP requires it, while operator MCP can be enabled independently.

[Incus administrator access](https://linuxcontainers.org/incus/docs/main/explanation/security/) is host-level authority. The initial product is for trusted owners. Running native agents in private VMs does not make the operator control plane or existing privileged CI runner suitable for hostile tenants.

## Fleet and fast readiness

The fleet controller clones only configured disk snapshots marked as clean bases. It copies into the configured storage pool and applies per-VM CPU/RAM/disk limits. Prepared tools and dependencies live on the copy-on-write disk; no provider installer runs during a fork. Headless browser preparation avoids a desktop by default. Completed cloud-init is disabled in the reusable image, while clone identity and ordinary networking remain enabled.

Admission counts active instances across Incus projects, includes frozen RAM, preserves configured host/CI reserves and budgets full root/state disks even for stopped instances. The API bounds its work queue and shares a host lock with bootstrap. The current implementation provides manual multi-host fleet management; automatic workload placement belongs to the planned Cloudflare control plane. Task scheduling and integration belong to Engine Box; the native harness retains its planning loop.

Registered templates are controller-owned desired state. Importing or revising one schedules installation on every registered node, subject to capacity and compatibility checks. Adding a host to the inventory and restarting the controller enrolls it in synchronization; live host registration is not yet implemented. Per-node readiness/errors remain visible, and existing workspaces keep their original image.

The Vinext server authenticates the owner and proxies a narrow REST surface. API tokens stay server-side. Operator MCP calls the same REST service. A guest receives browser/computer tools, not fleet credentials. See [fleet operations](FLEET.md) and [dashboard deployment](DASHBOARD.md).

## Optional CI module

Keep GitHub Actions workflows, scheduling, checks and logs. The module supplies prepared execution environments, runner lifecycle integration, persistent cache policies, resource limits and measurements. Model account credentials are not required for ordinary build/test CI or for this repository's tests.

The owner currently has one retained runner on their server; preserve that capacity decision for the first integration. A portable setup should put new CI execution in a separate VM/service boundary, with no default host Docker socket or writable agent home. Drain jobs before runner maintenance. Moving an existing privileged host runner is an explicit migration, not a bootstrap side effect.

Agent and CI jobs can reuse compatible recipes and eligible caches. They must use separate working directories and validate a precise commit. Untrusted pull-request jobs cannot write caches later used by privileged jobs or authenticated agents. Shared compiler outputs need tool-supported concurrency or serialized writers.

Existing private workflow patterns include a persistent pnpm store, mounted Cargo/build caches and serialization of heavy builds. Their performance has not been benchmarked here. First measure cold/warm setup and build/test times, queue delay, cache behavior, correctness, resource contention and cost per successful validation.

## Source-available and commercial boundary

The foundation, usable generic workspace recipe and reusable optional modules belong in this source-available repository under the Business Source License 1.1. Personal, non-commercial and non-production use is free; production business use needs a per-node commercial license; hosted or managed service offerings require a separate agreement. Managed operations and original specialized workspace recipes may be maintained separately. Grants under earlier licenses remain valid for their versions. User code remains the user's. Do not import proprietary recipes, provider binaries or engine source into this repository. See [licensing](LICENSING.md).

Unreal/Unity workspaces and the Cloudflare data-fork experiment remain separate workload candidates. They use the same Enginebox task, integration, validation and resource APIs; engine-specific recipes require separate qualification.

## GitHub account and Actions adapter

The optional native controller adapter is specified in [GitHub Actions](GITHUB-ACTIONS.md). It adds GitHub App account connections and repository-scoped credential handoff to the existing LayerFS import/acceptance/export path. The same App can grant organization runner administration using one installation and shared OAuth session; separate registrations remain optional, and signed webhooks request bounded disposable native JIT runners. Core runtime and existing CI automations remain forge-independent. This adapter does not replace the required Cloudflare fleet authority or claim that runner-pool distribution and live workflow acceptance are qualified.
