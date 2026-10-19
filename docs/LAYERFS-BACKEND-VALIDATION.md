# LayerFS CLI backend qualification

Recorded 2026-10-19. The opt-in Rust `ebox storage layerfs` backend passed a real Docker/LayerFS lifecycle test inside a new disposable Linux VM. This qualifies the [node-local administrator workflow](LAYERFS-BACKEND.md), not fleet VM attachment, production storage durability or authenticated agent sessions.

## Environment and provenance

| Input | Qualified configuration |
|---|---|
| Guest | Ubuntu 24.04.5 LTS, Linux 6.8.0-147-generic, x86_64 |
| Outer VM budget | 4 vCPU, 8 GiB RAM, 40 GiB root disk, ext4 |
| Docker | 29.9.1, local daemon inside the disposable VM |
| Backend budget | Two concurrent workspaces, each 1 CPU and 1 GiB RAM; 4 GiB free-disk reserve |
| SDK helpers | Short-lived containers, each capped at 1 CPU and 1 GiB RAM |
| LayerFS | Revision `968a5d0be0ea3e9456b06f9148b65523138e9490` |
| Toolchains | Engine Box Rust 1.96.0; upstream Rust 1.85.1; Node 24.21.0; pnpm 12.14.0 |
| Source fixture | Four tracked files, 5,242 bytes; one TypeScript 5.9.3 dependency |
| Dependency inputs | Pre-seeded immutable pnpm store and metadata volumes, read-only for consumers |

The final run used Linux release binaries built from the reviewed source. Artifact SHA-256 values:

```text
ebox          f37701456bfff9d1f23b889cbe8900997a7a68b5e3638362d79bd0c7c69cf9e5
live_backend  9255980bb9f632f01bc8f44cf76f3e6dc6444906099480b83f5edaf7ab74edbb
daemon        fdbcb2aa497b4b65b119003eb8c5041c12ca0ff3b438db67b687bdf3cdb192fe
native addon  293cfccebb85263ecc79faa45b5ada92115f84cb33eb7994f5bf28508c931826
SDK sources   f571de4c12a0201d4daaed5ec86a1472303ca7b88ec73728195ca3be390deb02
```

The runtime checks the source digest and configured artifact hashes before executing operations. The SDK digest covers sorted relative filenames for `package.json` and every file under `src/`, each followed by NUL and the raw SHA-256 of its contents. The test image was pinned to local Docker ID `sha256:f331a73bd0d766f9f9ab4af42ae50d92c8661bfced97749d319ca3c6eb091b13`; this is a provenance receipt, not a published image or install recommendation.

## Observed behavior

- Preview/import rejected an incorrect approval digest. The imported checkout preserved its original Git commit ID; an ordinary Git commit made inside the sandbox retained that ID as its parent. Untracked `.env`, local Git authorization configuration and source hooks were excluded.
- Two independently writable branches used the stable `/project` path. Each branch saw its own edits and not its sibling's edits. The unprivileged command user could not read the raw Store or access a Docker socket. A third workspace was refused at the configured capacity.
- `deps` reported `reused 1, downloaded 0`. After checkpoint/remount, `pnpm exec tsc --version` returned `Version 5.9.3` without reinstalling, and the following checkpoint reported `UpToDate`.
- A new workspace forked from the closed prepared checkpoint executed `node node_modules/typescript/bin/tsc --version` directly. No dependency installation command ran in that new workspace.
- Two commands that each slept three seconds overlapped across separate workspaces. A checkpoint targeting an already busy workspace was refused by its lease.
- Closing a dirty workspace was refused. Clean close completed after checkpointing and removed the sandbox while retaining project history.
- The test killed a real CLI process after its command wrote a marker. A retry was refused; the original bounded command finished, the marker appeared exactly once, and the durable receipt remained `exec` for inspection. This establishes refusal to replay an uncertain operation; it does not establish host/daemon crash recovery.

The ordinary source-policy tests also cover tracked credential paths, submodules, symlink-chain escape, Git replacement refs, export attributes and exclusion of earlier history objects. Remote host selection is rejected for these local-only CLI commands.

## Measured timings

These are individual observations from the final successful run, not percentile estimates or a large-repository benchmark. Timings include the Rust CLI, prerequisite checks, SDK helper startup and Docker operations inside the guest. They exclude SSH, VM provisioning, image builds and cache seeding. The guest, Docker image and dependency caches were already prepared; workspace containers were newly created.

| Operation | Elapsed |
|---|---:|
| Import the approved source commit | 1.833 s |
| Create two separate base workspaces, one at a time | 1.437 / 1.437 s |
| Offline dependency installation including helper overhead | 0.636 s |
| pnpm's reported installation time within that operation | 75 ms |
| Two concurrent three-second commands, combined wall time | 3.661 s |
| Checkpoint branch with installed dependencies | 0.667 s |
| Clean remount at `/project` | 1.234 s |
| New workspace from prepared checkpoint | 1.549 s |
| Execute inherited TypeScript compiler in that new workspace | 0.621 s |
| Complete integration test, including interruption scenario | 28.21 s |

Creating a Docker workspace includes more work than the [earlier millisecond branch-ref experiment](LAYERFS-VALIDATION.md). It also differs from the [0.512 s median frozen-VM resume](RUST-BOOT-BENCHMARKS.md). This backend does not yet keep prewarmed Docker sandboxes. These results do not show a Python-to-Rust speedup.

## Checks and cleanup

The root frozen dependency install, workspace check, tests, strict Rust Clippy and production build passed. After the final import-policy changes, focused storage/CLI tests and Clippy passed again, including all six source-policy cases. The Linux release integration executable was built and checked on Linux, then run with explicit disposable-VM opt-in. See [the reproduction contract](LAYERFS-BACKEND.md#qualification) and [live_backend.rs](../packages/enginebox-storage-layerfs/tests/live_backend.rs).

Private reports, configuration, operation receipts and binary hashes were exported before cleanup. The experiment-owned VM was deleted only after checking its exact ownership marker under the host lease; its internal containers and volumes were removed with it. Existing developer workspaces and production services were not changed.

## Remaining boundaries

The tested SDK preview still has the mtime and durability limitations recorded in the earlier storage report. Host or daemon restart recovery, garbage collection, hard disk quotas, native builds and large dependency graphs remain unqualified. Read-only consumers require an operator-enforced immutable cache generation; they cannot stop an unrelated publisher from changing it.

This is one administrator/trust boundary per storage root on Linux Docker. There is no Incus attachment, controller/MCP API integration, provider-home migration, native Windows/macOS storage execution, or hostile multi-tenant isolation claim. LayerFS file checkpoints do not preserve RAM. The adapter remains opt-in and does not change the installed fleet runtime.
