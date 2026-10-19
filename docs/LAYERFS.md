# LayerFS evaluation

Decision recorded 2026-10-19: preserve a first-class place for a workspace filesystem provider, with LayerFS as the first candidate. Keep Incus as the current compute and VM checkpoint backend. LayerFS is not installed, linked into the release, or selected by default in this migration.

The [disposable Linux validation](LAYERFS-VALIDATION.md) now establishes real independent mounts, cheap file branches and a corrected offline pnpm-cache recipe at a pinned preview revision. It also records mtime, Git metadata, recovery and topology limits. This is experimental qualification, not a production storage integration.

The upstream [README](https://github.com/Ephemeral-AI-Lab/layerfs) describes independent writable workspaces over a shared store, copy-on-write branches, content-addressed deduplication, chunking and versioned checkpoints. This matches Engine Box's goal of reusing large checkouts and installed dependencies without giving agents a shared writable project directory. Upstream performance examples are their measurements, not Engine Box benchmarks.

As reviewed, the repository announces its first supported 1.0 release for October 21, 2026. Its current SDK and store format are explicitly unstable. The [SDK surface document](https://github.com/Ephemeral-AI-Lab/layerfs/blob/main/sdk.md) also records changes to the Rust API and storage access model. Pin and qualify a released revision before adding a dependency; do not build against a moving main branch.

## Pinned source review, October 19, 2026

The disposable evaluation pins upstream commit `968a5d0be0ea3e9456b06f9148b65523138e9490`. This is a preview evaluation, not a release dependency. The README's storage ratios and millisecond-fork claim are upstream results; they are not our checkout or VM startup measurements.

The source establishes several important boundaries:

| Finding | Consequence for Engine Box |
| --- | --- |
| The public direct Store path explicitly selects `Disposable`; SQLite uses WAL with `synchronous=OFF`. | A successful Commit is not a power-loss durability guarantee. Keep private agent homes and irreplaceable edits on the existing persistent storage path. |
| FUSE `flush`, `fsync` and `fsyncdir` return `ENOSYS`; the kernel can subsequently report success without stable-media synchronization. | An application's successful `fsync` is not evidence that LayerFS has made that write durable. Ordinary close/reopen tests cannot qualify crash recovery. |
| Store openers must share one Linux kernel and a local ext4, XFS or Btrfs filesystem. The guard refuses other platforms/filesystems. | Sharing the database across our independent Incus VMs, fleet nodes, NFS or host-to-Docker file sharing is not a supported storage topology. |
| The supported sandbox path mounts the Store into Docker containers; its root filesystem daemon has `CAP_SYS_ADMIN`, while commands run under a separate unprivileged identity. | A Docker mount experiment does not implement an Incus workspace adapter or establish hostile tenant isolation. Keep the raw Store and Docker socket out of agent access. |
| The current API excludes daemon/container restart recovery. Committed history garbage collection is not implemented. | Recovery, retention and bounded disk growth remain integration gates even if forks are fast and deduplicated. |
| Mounts receive fresh `/workspaces/N` paths. Upstream records stale absolute paths in pnpm shims and an open finding where mtime-only build repairs disappear after remount. | Reusing stored dependencies does not necessarily avoid reinstall checks or repeated builds. Require stable project paths and a real incremental-build comparison before claiming cache savings. |

Pinned references: [Store opening](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/crates/layerfs-store-service/src/store_service.rs), [SQLite profile](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/crates/layerfs-persistence/src/backend/sqlite/profile.rs), [FUSE synchronization](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/crates/layerfs-fuse/src/operations/flush.rs), [kernel guard](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/crates/layerfs-store-service/src/kernel_guard.rs), [Docker topology](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/crates/layerfs-sandbox/src/backend/docker/container.rs), [restart contract](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/sdk/js/README.md), [history retention](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/docs/architecture/84-global-sqlite-store.md).

Engine Box already uses the configured Incus pool's snapshot/copy facilities; LayerFS must be compared with that path, not only with a full byte copy. Its potential additional value is content deduplication across independently produced files and fine-grained project history. A branch reference is not a booted VM, a mounted workspace or a completed dependency build.

The dependency/build findings are upstream reports at the pinned revision, not our measurements: [F-034 and F-056](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/docs/issues/352/BUGS.md), [pnpm mount-path behavior](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/docs/issues/352/NODE_RULE.md).

The integration direction is therefore optional, node-local, credential-free checkout/dependency storage first. Keep existing Incus VM and private-home persistence. Sharing one store across multiple current workspace VMs requires a new mount/export design that leaves every Store opener in one kernel, plus explicit file/RAM checkpoint ordering. Containers inside one Incus VM could satisfy the same-kernel condition, but would change the compute isolation and per-agent memory-checkpoint model; that is a separate backend decision.

## Placement

Engine Box would own tenant authorization, scheduling, resource limits, native provider credentials and compute lifecycle. A filesystem provider would own file branches, workspace mounts and durable file checkpoints. A LayerFS adapter belongs in `packages/enginebox-storage-layerfs/` once implemented; shared storage contracts belong in a separate Rust package when a second working provider justifies that split.

The intended flow is: select an authorized base revision, create a private writable branch, attach it to an isolated workspace, launch the original CLI with that workspace's private home, then checkpoint the file state. This does not replace the native harness or require routing its inference through Engine Box. Tool-call-by-tool-call history is a separate integration decision; the current native CLI launch interface does not expose every internal tool call.

The current documented sandbox path uses Docker. An Incus VM mount adapter is not established by that support: the first experiment must prove how a selected branch reaches the guest, which process owns the mount, and what survives either side restarting. An agent should receive its authorized workspace view, not unrestricted access to the underlying shared Store. Keep this adapter optional until that boundary works with the existing VM lifecycle.

File checkpoints and memory checkpoints must remain separate capabilities. Incus pause/hibernate captures VM process state. A file branch alone cannot restore RAM, active sockets, browser sessions or a running compiler. A combined checkpoint needs a consistency barrier so the filesystem revision and VM memory describe the same state.

## Security and data scope

- Use separate stores and authorization boundaries for unrelated tenants. Deduplication is an optimization, not access control. Do not rely on branch names or a shared database to isolate secrets.
- Shared template bases contain no provider credentials, refresh tokens, browser cookies, tenant home, private scratch notes or personal repository credentials.
- Keep `.claude`, `.codex`, Cursor configuration, session transcripts and browser profiles private to their owner. Share approved dependency contents and repository bases only within the intended trust boundary.
- Treat `.git`, ignored files, caches and build output as real data that may contain secrets. Upstream import includes more than Git-tracked files; an import policy and preview are required before a reusable base can be published.
- Do not mount a writable store from arbitrary fleet nodes over NFS or SMB. Storage locking, corruption recovery and network-filesystem support require explicit upstream support and our qualification.

## Qualification before integration

1. Verify the released Linux kernel/filesystem/container requirements and pin the Rust SDK or supported process interface. Current README requirements include a Linux store on ext4, XFS or Btrfs and Docker for its sandbox integration. A laptop Docker VM is not proof of native macOS or Windows filesystem support.
2. Test a real native Claude, Codex and Cursor session against the mounted tree, including exact-session resume, Git worktrees, symlinks, file watchers, atomic rename, executable bits, locks and large generated files.
3. Benchmark cold import, warm fork, mount readiness, write-heavy builds, package installation, checkpoint latency, disk usage and recovery. Include pnpm hardlinks and Cargo caches; report p50/p95 with concurrent agents and resource budgets.
4. Test cross-branch write isolation, tenant denial, deleted-file behavior, mount failure, full disk, killed workers, killed storage process, restart and interrupted checkpoint. Confirm which committed states survive a power loss under the selected durability policy.
5. Specify file revision/VM checkpoint ordering and rollback compatibility. Preserve a stopped workspace for inspection on uncertain recovery; never silently discard uncommitted changes.
6. Prove install, version pinning, upgrade, downgrade refusal for incompatible store formats and removal using disposable test data. A controller rollback must not silently downgrade a store.

The first useful experiment is one Linux node with two disposable agents sharing a credential-free project base, independently installing/building and checkpointing. No production migration or universal instant-start claim follows from that experiment alone.
