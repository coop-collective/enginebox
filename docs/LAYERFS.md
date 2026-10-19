# LayerFS evaluation

Decision recorded 2026-10-19: preserve a first-class place for a workspace filesystem provider, with LayerFS as the first candidate. Keep Incus as the current compute and VM checkpoint backend. LayerFS is not installed, linked into the release, or selected by default in this migration.

The upstream [README](https://github.com/Ephemeral-AI-Lab/layerfs) describes independent writable workspaces over a shared store, copy-on-write branches, content-addressed deduplication, chunking and versioned checkpoints. This matches Engine Box's goal of reusing large checkouts and installed dependencies without giving agents a shared writable project directory. Upstream performance examples are their measurements, not Engine Box benchmarks.

As reviewed, the repository announces its first supported 1.0 release for October 21, 2026. Its current SDK and store format are explicitly unstable. The [SDK surface document](https://github.com/Ephemeral-AI-Lab/layerfs/blob/main/sdk.md) also records changes to the Rust API and storage access model. Pin and qualify a released revision before adding a dependency; do not build against a moving main branch.

## Placement

Engine Box would own tenant authorization, scheduling, resource limits, native provider credentials and compute lifecycle. A filesystem provider would own file branches, workspace mounts and durable file checkpoints. A LayerFS adapter belongs in `packages/enginebox-storage-layerfs/` once implemented; shared storage contracts belong in a separate Rust package when a second working provider justifies that split.

The intended flow is: select an authorized base revision, create a private writable branch, attach it to an isolated workspace, launch the original CLI with that workspace's private home, then checkpoint the file state. This does not replace the native harness or require routing its inference through Engine Box. Tool-call-by-tool-call history is a separate integration decision; the current native CLI launch interface does not expose every internal tool call.

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
