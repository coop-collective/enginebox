# LayerFS evaluation

Decision recorded 2026-10-19: make [LayerFS checkout storage](LAYERFS-BACKEND.md) a first-class Engine Box capability, available through controller registration, durable jobs and MCP. Keep Incus responsible for compute and VM lifecycle. Storage nodes must satisfy the pinned runtime and capability checks before allocation; the normal feature does not require an experimental opt-in switch. Provider homes remain private.

Owner decision, October 23: LayerFS is the game workspace/history foundation.
[Game workspaces](GAME-WORKSPACES.md) maps the
upstream core ideas to the existing container backend and its remaining gates.
[Store distribution](STORE-DISTRIBUTION.md) specifies a storage origin, local NVMe
worker Stores, immutable generation transfer and safe disk-pressure eviction.
Those distributed services are planned; the local Docker integration already exists.

The [disposable Linux validation](reports/LAYERFS-VALIDATION.md) establishes real independent mounts, cheap file branches and a corrected offline pnpm-cache recipe at a pinned preview revision. Its mtime, Git metadata, recovery and topology limits still apply. Making the backend first-class does not turn that evidence into a production durability guarantee.

The upstream [README](https://github.com/Ephemeral-AI-Lab/layerfs) describes independent writable workspaces over a shared store, copy-on-write branches, content-addressed deduplication, chunking and versioned checkpoints. This matches Engine Box's goal of reusing large checkouts and installed dependencies without giving agents a shared writable project directory. Upstream performance examples are their measurements, not Engine Box benchmarks.

The separate October 23 probe qualifies selected v1.0.0 storage operations, while
the product still pins the earlier preview below. Their SDK/API and Store-format
boundaries differ. Pin and qualify the complete SDK/daemon/format combination
before upgrading; do not build against a moving main branch or silently migrate
existing Stores. [Current platform and format contract](https://layerfs.ai/docs/platform/).

## Pinned source review, October 19, 2026

The disposable evaluation pins upstream commit `968a5d0be0ea3e9456b06f9148b65523138e9490`. This is a preview evaluation, not a release dependency. The README's storage ratios and millisecond-fork claim are upstream results; they are not our checkout or VM startup measurements.

The source establishes several important boundaries:

| Finding | Consequence for Engine Box |
| --- | --- |
| The public direct Store path explicitly selects `Disposable`; SQLite uses WAL with `synchronous=OFF`. | A successful Commit is not a power-loss durability guarantee. Keep private agent homes and irreplaceable edits on the existing persistent storage path. |
| FUSE `flush`, `fsync` and `fsyncdir` return `ENOSYS`; the kernel can subsequently report success without stable-media synchronization. | An application's successful `fsync` is not evidence that LayerFS has made that write durable. Ordinary close/reopen tests cannot qualify crash recovery. |
| Store openers must share one Linux kernel and a local ext4, XFS or Btrfs filesystem. The guard refuses other platforms/filesystems. | Sharing the database across our independent Incus VMs, fleet nodes, NFS or host-to-Docker file sharing is not a supported storage topology. |
| The supported sandbox path mounts the Store into Docker containers; its root filesystem daemon has `CAP_SYS_ADMIN`, while commands run under a separate unprivileged identity. | A Docker mount experiment does not implement an Incus workspace adapter or establish hostile tenant isolation. Keep the raw Store and Docker socket out of agent access. |
| The pinned upstream API excludes daemon/container restart recovery. Committed history garbage collection is not implemented. | Engine Box's bounded operation recovery and explicit checkpoint replacement need their own receipts and qualification. Retention, host-reboot repair and power-loss durability remain gates. |
| Mounts receive fresh `/workspaces/N` paths. Upstream records stale absolute paths in pnpm shims and an open finding where mtime-only build repairs disappear after remount. | Reusing stored dependencies does not necessarily avoid reinstall checks or repeated builds. Require stable project paths and a real incremental-build comparison before claiming cache savings. |

Pinned references: [Store opening](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/crates/layerfs-store-service/src/store_service.rs), [SQLite profile](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/crates/layerfs-persistence/src/backend/sqlite/profile.rs), [FUSE synchronization](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/crates/layerfs-fuse/src/operations/flush.rs), [kernel guard](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/crates/layerfs-store-service/src/kernel_guard.rs), [Docker topology](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/crates/layerfs-sandbox/src/backend/docker/container.rs), [restart contract](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/sdk/js/README.md), [history retention](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/docs/architecture/84-global-sqlite-store.md).

Engine Box already uses the configured Incus pool's snapshot/copy facilities; LayerFS must be compared with that path, not only with a full byte copy. Its potential additional value is content deduplication across independently produced files and fine-grained project history. A branch reference is not a booted VM, a mounted workspace or a completed dependency build.

The dependency/build findings are upstream reports at the pinned revision, not our measurements: [F-034 and F-056](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/docs/issues/352/BUGS.md), [pnpm mount-path behavior](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/docs/issues/352/NODE_RULE.md).

The implementation uses node-local, credential-free checkout/dependency storage with explicit capability checks. Existing Incus VM and private-home persistence remains. The [Incus adapter](LAYERFS-INCUS.md) leaves every Store opener in the host kernel and exports one checkout through a directory disk. File/RAM checkpoint ordering remains a separate gate; marked attached VMs cannot be hibernated, snapshotted or forked.

## Placement

Engine Box owns fleet authorization, admission, resource limits, native provider credentials and compute lifecycle. The backend lives in `packages/enginebox-storage-layerfs/` and is exposed through `ebox storage layerfs`, registered controller operations, fleet MCP and the Storage dashboard; its Docker workspaces are not fleet nodes. It implements file branching, mounts, limits, operation receipts, file checkpoints and bounded recovery, with no power-loss durability guarantee. Incus attachment remains a separately qualified node-administrator operation. Shared storage contracts belong in a separate Rust package only when another working provider justifies that split.

The intended flow is: select an authorized base revision, create a private writable branch, attach it to an isolated workspace, launch the original CLI with that workspace's private home, then checkpoint the file state. This does not replace the native harness or require routing its inference through Engine Box. Tool-call-by-tool-call history is a separate integration decision; the current native CLI launch interface does not expose every internal tool call.

The upstream sandbox path uses Docker. Engine Box's separately qualified Incus adapter must establish how a selected branch reaches the guest, which process owns the mount, and what survives either side restarting. See the [attachment contract and current boundaries](LAYERFS-INCUS.md). An agent receives its authorized workspace view, never unrestricted access to the underlying shared Store. Attachment is an explicit operator action; fleet allocation does not attach it automatically.

File checkpoints and memory checkpoints must remain separate capabilities. Incus pause/hibernate captures VM process state. A file branch alone cannot restore RAM, active sockets, browser sessions or a running compiler. A combined checkpoint needs a consistency barrier so the filesystem revision and VM memory describe the same state.

## Wrangler environment state forks (proposed)

The proposed [development-environment service](DEV-ENVIRONMENTS.md) keeps a persistent core environment following the project's main branch. Its data survives code updates; an optional project dev command starts its application, and explicit sleep/wake controls its compute lifecycle. The online core publishes consistent, versioned data baselines for independent disposable agent environments. LayerFS can reuse prepared code, dependencies and captured seed data across those forks. The managed service and its Wrangler publication adapter are not implemented; filesystem branching alone supplies no database-consistency guarantee. DuckDB is not required; local D1 uses its existing SQLite-based development runtime.

Wrangler and the Cloudflare Vite plugin normally persist local D1, KV, R2 and Durable Object data under `.wrangler/state`. Wrangler's `--persist-to` can select an explicit directory; data-management commands must use the same directory and local mode. Internal layouts and resource identifiers must be pinned to the qualified Wrangler/Miniflare versions. Remote bindings are external services and are not captured by a local filesystem fork. [Cloudflare local data](https://developers.cloudflare.com/workers/local-development/local-data/)

| State | Proposed ownership and sharing |
| --- | --- |
| Core environment and its live application data | Persistent project service; code follows main without resetting data |
| Prepared source, dependencies and published development data | Immutable, authorized revision reused by many forks |
| Each environment's live database and local service files | Private writable state owned by that environment's runtime |
| Project notes, task results, coordination and intentionally shared services | Project-scoped controller/service APIs with transactions or version checks |
| Provider credentials, session databases and personal agent memory | Private owner-scoped storage; excluded from reusable checkpoints |

Publishing from the online core requires a database-aware capture adapter and, where necessary, a brief application write barrier across the included services. Build and validate an immutable candidate outside the core's live writable state, then atomically publish its manifest with code, binding, schema and toolchain versions. A failed publication leaves the previous revision available. Fork that manifest into independent environments with separate endpoints and one owning runtime per environment. Do not copy only the main SQLite file during live WAL writes: the WAL can contain committed transactions. A database-aware backup can capture one database, but does not by itself make multiple bindings consistent. The adapter must report its actual write interruption and restart semantics. [SQLite WAL lifecycle](https://www.sqlite.org/wal.html), [SQLite backup snapshots](https://www.sqlite.org/backup.html)

The earlier [local Wrangler experiment](DEV-ENVIRONMENTS.md#existing-local-proof) passed 15 checks for stopped-state copying, inheritance, isolated changes and restart. It used ordinary copies of a small fixture on macOS, not LayerFS or an online publication adapter. That proof is a useful starting point, not a completed managed staging template.

Agents can hand a checkpoint to another agent, or call an intentionally shared application service. Filesystem forks do not provide live cross-branch synchronization, row-level database merges, RAM restoration or cross-host replication. Publish code changes through reviewable patches and data changes through application migrations/imports; do not merge SQLite files as text or point independent Wrangler processes at one writable persistence directory.

The current Incus qualification proves script/ELF execution and a shared-mmap smoke test. It does **not** qualify workerd, SQLite WAL/locking, Durable Object alarms, database recovery or Wrangler startup performance. The pinned LayerFS preview's fsync and power-loss limitations above still apply. Initially treat any LayerFS-backed local service data as disposable test data; authoritative shared project state belongs on separately qualified persistent storage behind its service API.

The product feature is coordinated environment branching: code, prepared dependencies, local data, service startup and explicit sharing policy in one operation. LayerFS supplies reusable filesystem history; it is not exclusive to Engine Box. Ordinary `docker commit` excludes mounted volume data, so a container image alone does not capture this environment state. [Docker commit scope](https://docs.docker.com/reference/cli/docker/container/commit/)

## Security and data scope

- Use separate stores and authorization boundaries for unrelated tenants. Deduplication is an optimization, not access control. Do not rely on branch names or a shared database to isolate secrets.
- Shared template bases contain no provider credentials, refresh tokens, browser cookies, tenant home, private scratch notes or personal repository credentials.
- Keep `.claude`, `.codex`, Cursor configuration, session transcripts and browser profiles private to their owner. Share approved dependency contents and repository bases only within the intended trust boundary.
- Treat `.git`, ignored files, caches and build output as real data that may contain secrets. Upstream import includes more than Git-tracked files; an import policy and preview are required before a reusable base can be published.
- Do not mount a writable store from arbitrary fleet nodes over NFS or SMB. Storage locking, corruption recovery and network-filesystem support require explicit upstream support and our qualification.

## Remaining qualification gates

1. Qualify a supported upstream release beyond the pinned preview, including its Linux kernel/filesystem/container requirements and storage-format compatibility. The current topology uses a Linux store on ext4, XFS or Btrfs and local Docker. A laptop Docker VM is not proof of native macOS or Windows filesystem support.
2. Test a real native Claude, Codex and Cursor session against the mounted tree, including exact-session resume, Git worktrees, symlinks, file watchers, atomic rename, executable bits, locks and large generated files.
3. Benchmark cold import, warm fork, mount readiness, write-heavy builds, package installation, checkpoint latency, disk usage and recovery. Include pnpm hardlinks and Cargo caches; report p50/p95 with concurrent agents and resource budgets.
4. Extend the recorded branch-isolation, CLI interruption and storage-daemon recovery tests to full disk, interrupted checkpoint, host reboot, hostile tenants and representative workloads. Confirm which committed states survive a power loss under the selected durability policy; the existing orderly recovery results do not establish that guarantee.
5. Specify file revision/VM checkpoint ordering and rollback compatibility. Preserve a stopped workspace for inspection on uncertain recovery; never silently discard uncommitted changes.
6. Prove install, version pinning, upgrade, downgrade refusal for incompatible store formats and removal using disposable test data. A controller rollback must not silently downgrade a store.

The existing [backend](reports/LAYERFS-BACKEND-VALIDATION.md), [recovery](reports/LAYERFS-RECOVERY-VALIDATION.md) and [Incus attachment](LAYERFS-INCUS.md) qualifications establish the initial disposable-workspace path. The next work extends that path to native agent sessions, representative projects and the proposed database publication adapters. No production migration or universal instant-start claim follows from the small fixtures alone.
