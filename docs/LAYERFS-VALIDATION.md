# LayerFS storage and checkout validation

## Git-free Unreal follow-up, October 23, 2026

An independent disposable probe of upstream **v1.0.0**, commit
`2aaf2552ce21154928b9f9c83582e41a637f5910`, exercised real environment meshes,
textures and a map, one deliberate duplicate mesh, and 1,001 small generated
files. Total: **1,006 files / 802,045,960 bytes**, with no `.git` directory.
Original assets were mounted read-only; only private copies were changed.
This is a single local diagnostic on Linux ARM64 in Docker Desktop with
uncontrolled OS caches, not a cold-node or engine-start benchmark.

| Observed operation | Seconds |
| --- | ---: |
| Import into a new Store | 3.895 |
| Create two branch references, separately | 0.0058 / 0.0005 |
| Mount first workspace in its sandbox | 0.327 |
| Fully read and hash the first two views | 7.306 / 9.210 |
| Commit a 4 KiB overwrite in a 175.5 MB mesh, plus one text file | 1.233 |
| Merge the two branches' disjoint changes | 0.034 |
| SQLite online backup, integrity/foreign-key checks and local fsync | 1.144 |
| Mount from a copied backup in a fresh volume and sandbox | 0.927 |
| Fully read and hash all restored files | 6.359 |

Restore preparation additionally took 1.863 seconds including its container,
copy and checksum checks. It reused the local image/toolchain and project
discovery manifest; no network download was involved. The database after import
was 631,500,800 bytes; the deliberate duplicate affects this ratio. The binary
commit reported 8,310 newly stored object bytes and 8,149 metadata bytes, while
the WAL grew 127,840 bytes. These are different accounting boundaries. The edit
used repeated replacement bytes and a copy/rename save, not an Unreal editor
resave; the live overlay temporarily held a complete modified file.

Full SHA-256 comparisons passed for both initial views, the untouched sibling,
the merged/published view, and the backup restored into a separate Store volume.
Same-file branch changes were refused as a conflict. The SQLite backup passed
`integrity_check` and `foreign_key_check`; SDK history and content were readable.
No engine imported or rendered the modified mesh: this was storage validation.

The publication experiment directly distinguishes three behaviors:

- Separate branches and mounted workspaces stay pinned, including untouched
  files. A new fork from main sees a new publication; an existing fork does not.
- Publishing B from an old baseline after A replaced A's changes in main,
  including an unrelated file B never edited. LayerFS reported one overwritten
  layer and retained A's history. Publication is not automatic integration.
- Two workspaces on one branch do not see each other's edits immediately, but B's
  later commit merged A's non-conflicting committed change and updated B's view.

Merging B with the branch pinned to A's publication retained both changes. After
the deliberate overwrite, merging A with the now-descendant main did not undo
that overwrite; merging the original A and B branches recovered both. The first
assertion assumed otherwise and failed; its evidence remains in the raw logs.
A second harness attempt passed a raw layer ID to `branch.merge`, which accepts
branch names/main, and was refused. The corrected test used the already-pinned
branch. An initial Docker memory-limit update also failed before readback and
was corrected. No failed attempt was deleted or counted as a passing stage.

This corrects a possible overgeneralization from the older `.git` experiment:
**LayerFS does have chunked history and can merge Git-free changes.** The conflict
with independently changed `.git` internals does not establish that LayerFS
cannot replace Git inside an ephemeral workspace. The tested JavaScript SDK still
selects Disposable/WAL/synchronous=OFF, so this orderly restore is not a host
power-loss guarantee. The prior custom durable-profile research used a different
upstream revision and API and does not qualify this SDK path.

Probe sources are `research/storage-benchmarks/layerfs-unreal*`; sanitized results
are in `research/storage-benchmarks/layerfs-unreal-results.json`. Raw append-only
receipts, hashes and backup/discovery records remain in ignored
`.state/layerfs-unreal/`; temporary data/store/restore volumes and stopped
experiment containers were removed after export. No private asset or
credential is included in Git. These probes do not update Engine Box's existing
pinned preview backend.

## Original preview qualification

On October 19, 2026, the pinned LayerFS preview **did share a complete checkout and installed dependencies across independent writable mounts**. This validates the basic storage idea. It does not yet qualify LayerFS as Engine Box's persistent filesystem or as a shared store across its existing Incus VMs.

Upstream revision: `968a5d0be0ea3e9456b06f9148b65523138e9490`. The independent [research harness](../research/layerfs-validation/README.md) builds the actual upstream Rust daemon and Node addon. [Source-level integration boundaries](LAYERFS.md) cover durability, same-kernel storage access, restart recovery and retention. [Rust boot measurements](RUST-BOOT-BENCHMARKS.md) are a separate experiment; LayerFS forks are not VM boots.

## Workload and timing boundaries

The disposable Ubuntu 24.04 VM had 4 vCPU, 8 GiB RAM, a 40 GiB ext4 root and 10 GiB state capacity. Docker ran inside this VM. Two LayerFS sandboxes and the trusted SDK runner therefore shared one Linux kernel; command sandboxes had no access to the host Docker daemon. The Rust boot experiment finished before active LayerFS testing began.

The generated fixture contained 1,214 regular files, 261 directories and one symlink: a real Git repository, TypeScript 5.9.3 and its installed dependencies, compiled output, 512 small files and a deterministic 16 MiB high-entropy payload. Regular files totaled **63,954,424 bytes**; native allocated storage was **67,293,184 bytes** per full tree. This is a small reproducible coding fixture, not a large game repository or production workload.

Each row below is a **single diagnostic observation**, with uncontrolled warm caches. No host caches were dropped. Independent full-tree manifests checked every file's hash and mode and every symlink target in both native copies and both LayerFS mounts. Those audits took 810 / 832 ms on the mounts and ran outside mount timing; they warmed caches before the subsequent Git/Node command. There is no cold-read or p95 claim for this campaign.

| Operation | Observed wall time |
| --- | ---: |
| Full native byte copy, `cp --reflink=never -a`, two copies | 69.1 / 73.7 ms |
| Initial LayerFS project import | 337.1 ms |
| Create Docker sandboxes A / B | 558.6 / 326.8 ms |
| Create branch references A / B | 1.088 / 0.316 ms |
| Mount complete branches A / B | 3.35 / 3.94 ms |
| Git status + Node dependency metadata load + payload hash, after audits | 155 / 153 ms |
| Two concurrent workspace commits, combined wall time | 148.3 ms |

The ext4 guest refused `cp --reflink=always` with `Operation not supported`. This is a capability result, not a LayerFS failure or a comparison with Btrfs. Engine Box's existing host pool already supports copy-on-write snapshots; the Rust experiment copied its prepared VM snapshot in 92.8 ms once, with boot/preparation excluded. The VM snapshot and project fixture differ, so neither timing establishes a relative speedup over that existing backend.

## Storage accounting

Physical allocation includes the Store's main database, WAL, SHM and directory blocks. Live overlay allocation is separate and must be added; it is not hidden inside the upstream `stored` object-byte counter.

| Point | Shared Store allocation | Live overlay allocation |
| --- | ---: | ---: |
| Imported base, before branches | 44,519,424 B | 73,728 B, sandbox A's template |
| After two branch references | 44,548,096 B | 73,728 B, unchanged |
| After independent edits/builds, before commits | 44,556,288 B | A: 24,813,568 B; B: 233,472 B |
| After both commits | 45,228,032 B | A: 25,174,016 B; B: 233,472 B |
| Final orderly close/reopen and integrity check | 44,916,736 B | Sandboxes closed |

Two branch references added **28,672 bytes (28 KiB)** to the Store, with no second content tree. Each native byte copy added 67,293,184 allocated bytes. This proves cheap reference sharing for this input. It is not the total cost of running two agents: sandbox setup, overlays, package caches, images, source/import artifacts and process memory have their own costs.

The roughly 25 MB active overlay was significant. In this campaign pnpm reinstalled a dependency even though the imported tree was already installed. The final history also contains a deliberately failed reinstall followed by its deletion-state commit, so final Store size must not be described as a successfully reinstalled working tree. Upstream's `logical` counter sums historical states; its growing deduplication ratio is not a multiplier for present checkout capacity.

## Correctness results and observed limits

- Both mounted trees matched the original full manifests, including `.git`, dependencies and ignored build output. Direct Node `require('typescript/package.json')` used the existing package without an install; compiler execution was checked separately.
- Git status/commit, TypeScript compilation, atomic rename, relative symlink and in-mount hardlink operations succeeded. A hardlink from a separate read-only package volume returned `EXDEV`, as expected across filesystems.
- Branch A's edits were absent from B. Concurrent commits retained both branches' expected contents; orderly Store close/reopen returned those exact bytes. SQLite `integrity_check` returned `ok`.
- The command user could not read the raw Store (`EACCES`). Node's watcher observed rename/change events in this simple probe. Neither result is a comprehensive tenant-isolation or watcher qualification.
- A pure mtime change returned `UpToDate` from Commit. After remount, the old mtime returned while file contents remained identical. The tiny TypeScript project still reported up to date after its comment-only edit; we did **not** reproduce upstream's multi-project repeated-build scenario. The underlying mtime behavior is confirmed, and [upstream F-056](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/core/docs/issues/352/BUGS.md) remains a relevant separate report.
- Full filesystem branch merge refused five conflicts, all in `.git` metadata: commit message, binary index, two reflogs and the branch ref. The application edits were independent. LayerFS filesystem merging is therefore not a substitute for Git's own merge of independently committed agent work.
- `pnpm exec tsc --version` exited successfully but logged **downloaded 1** and an automatic install. It was not a no-install cache hit. The first explicit offline reinstall failed; native controls and the corrected cache recipe are recorded separately below.

The current source selects Disposable/WAL/synchronous=OFF. A successful `fsync` syscall in the probe, orderly reopening and a clean integrity check do **not** establish recovery after a crash or power loss. Forced daemon crash, host power loss, full disk, hostile tenants, cross-node sharing, authenticated Claude/Codex/Cursor sessions, sustained parallel load and Incus memory/filesystem checkpoint coordination were not qualified.

## Dependency-cache control

The original read-only-store reinstall was not enough to diagnose a LayerFS problem. Ordinary ext4 controls also failed: a fresh metadata cache produced `ERR_PNPM_NO_OFFLINE_META`, and a read-only SQLite store without its explicit immutable mode produced `ERR_PNPM_NO_OFFLINE_TARBALL`. Those failed receipts were retained.

A focused follow-up prepared an immutable package store **and** seeded pnpm's metadata cache, then gave each command user a private writable copy of that metadata cache. Native ext4 with networking disabled and LayerFS both succeeded with **reused 1, downloaded 0** using:

```sh
pnpm install --frozen-store --offline --frozen-lockfile \
  --ignore-scripts --package-import-method=copy --store-dir /pnpm-store
```

The metadata cache was explicitly selected with `PNPM_CONFIG_CACHE_DIR`. No package integrity or supply-chain checks were disabled. In the final fresh control C, reinstall plus dependency version/hash verification took **128.2 ms** (pnpm reported 69 ms); commit took **98.8 ms**, remount **3.73 ms**, and dependency verification **55.9 ms**. A remounted `pnpm exec tsc --version`, with offline/frozen-store settings, took **145.1 ms** and showed no installation output. These remain single diagnostic observations, not native-versus-FUSE throughput ratios. The native control disabled networking at Docker level; the FUSE case used pnpm's offline policy, not an independently disabled sandbox network.

The changed mount path still caused pnpm to update `/node_modules/.pnpm-workspace-state-v1.json`. In the preceding control B, clean unmount correctly refused the dirty workspace. The follow-up inspected that state and explicitly committed the metadata before closing; the reusable script now makes that commit explicit. Control C exercised the full corrected sequence, including the metadata commit, clean unmount/close and a successful SQLite integrity check. This is an integration requirement even when no package is downloaded. Both controls' receipts remain available.

The recipe must use an immutable published store generation: pnpm's `--frozen-store` bypasses SQLite locking, so another process must not write to that store while consumers use it. A read-only mount alone does not stop a different writer. Publish a new generation and switch future consumers instead of mutating the shared one. This follows [pnpm's documented frozen-store contract](https://github.com/pnpm/pnpm.io/blob/main/docs/settings/store.md#frozenstore). The probe used one package without lifecycle scripts; packages needing native builds, approved install scripts, other platforms and larger workspace graphs need separate qualification.

Run the focused control with `run-dependency-control.sh CASE FRESH_CONTROL_NAME` after a completed campaign. It writes fresh receipts beneath that campaign; it does not replace its initial results.

## Integration decision

Keep LayerFS as an optional candidate for **node-local, credential-free checkout and dependency state**. The experiment establishes fast file branching, real writable mounts and a working offline dependency-cache recipe. Keep the existing Incus/private-home persistence and VM memory-checkpoint path. Before a product adapter, require stable project paths, Git-aware integration, durability/recovery and retention decisions, plus a supported Incus mount boundary. Do not add a shared writable database across fleet VMs or claim native account/session resume from these filesystem tests.

## Reproducibility

The upstream archive hash was `0515f69c92ba1c5a049f3546c61249b355596743f1d7c0d7b39ca060e3eafb4e`. Builds used Rust 1.85.1, Node 24.21.0 and pnpm 12.14.0. The daemon SHA-256 was `fdbcb2aa497b4b65b119003eb8c5041c12ca0ff3b438db67b687bdf3cdb192fe`; the addon was `293cfccebb85263ecc79faa45b5ada92115f84cb33eb7994f5bf28508c931826`.

Original receipts remain outside Git in `.state/layerfs-evaluation/case-20261019-a/`, including the failed reflink/merge/reinstall outcomes, manifests, exact timings, allocation observations, Docker diagnostics and `frozen-store-control-c/receipts.jsonl`. Exact initial-fixture and closed-Store archives were exported with artifact hashes before cleanup. The source archive and full private host receipts remain in `.state/layerfs-evaluation/`. The package README documents reproduction and the ownership-checked transport. No upstream runtime changes were made to obtain these results.

The experiment-owned VM was deleted under the host lease after export; a final inventory confirmed it was absent. The final reusable scripts also refuse preexisting Docker volumes and verify new volumes' unique ownership labels. Those last guard changes received syntax and source review after live qualification; they were not rerun on a new VM.
