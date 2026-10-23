# Storage qualification

`lore.mjs` measures a private Unity content copy against an owned loopback Lore server. It does **not** measure Cloudflare Artifacts, R2 or GitHub. See [results and storage recommendation](../../docs/LORE-STORAGE.md).

Use Node 22.12+ and the verified native Lore **v0.10.1** CLI and server release for your platform. Extract both binaries into one directory, check their release SHA-256 digests, and make them executable. No server or CLI is installed by this script.

```sh
node research/storage-benchmarks/lore.mjs \
  /path/to/unity-project \
  .state/lore/fresh-run \
  /path/to/verified-lore-binaries
```

The output must not exist and must be outside the source. `Assets`, `Packages` and `ProjectSettings` are copied; symlinks and special files are refused. The workload requires a TIFF larger than 1 MiB and `ProjectSettings/ProjectVersion.txt`. It changes 4 KiB of that TIFF and appends a comment to the private settings copy. These deliberately artificial edits test storage, not Unity asset validity. The original tree is checked again at the end.

The fixture must be below 20 GiB. Allow six times its content size plus 4 GiB free disk; do not depend on filesystem cloning to satisfy capacity. Ports 43187 (TCP/UDP) and 43189 (TCP) must be free. The server binds loopback only and is stopped in the finalizer. Each command has a 15-minute timeout. No source content is uploaded to an external service.

`report.json` contains measured wall times, exact content counts and summed lengths of store files (not allocated blocks). SHA-256 checks validate both complete clones and the incremental sync. Timings exclude these verification passes. Commands use four Lore worker threads and `--sync-data`. The second checkout uses the same explicitly selected shared store with `--cache`; OS page caches are not flushed. Online Lore commit uploads content, so include **commit plus push** when reporting initial publication.

Private logs, server data and working copies remain under the output for review; they may contain project paths and contents and must not be committed. `LORE_GLOBAL_PATH` isolates the registry and configuration under that output as well. The shared store is explicitly selected, with `--make-default false`. After inspection, remove only the run's owned output if desired. No global cleanup is performed.

This is a manually invoked qualification tool, not a CI performance assertion. One observation cannot establish latency percentiles, remote throughput, production durability, engine build correctness or results for a much larger Unreal project.

## Git-free LayerFS Unreal probe

`layerfs-unreal-results.json` and [the validation notes](../../docs/LAYERFS-VALIDATION.md)
record a separate v1.0.0 experiment. This does not exercise the product backend or
run an Unreal editor. The 4 KiB mutation is deliberately a storage-only binary edit.

The bounded runner uses an explicit disposable namespace ending in `20261023`.
Before a new campaign, require its data/store volumes and source/output directory
to be absent; do not reuse somebody else's volumes. Build the supplied Dockerfile
as `enginebox-layerfs-unreal:20261023`. Place upstream commit
`2aaf2552ce21154928b9f9c83582e41a637f5910` at
`.state/layerfs-unreal/source`. Create these local Docker volumes with label
`enginebox.experiment=layerfs-unreal-20261023`:

- `ebox-lfu-data-20261023`
- `ebox-lfu-store-20261023`
- `ebox-lfu-target-20261023`
- `ebox-lfu-cargo-20261023`

In the supplied build image, mount that source read-only at `/src`, the target
volume at `/artifacts` and the Cargo cache at `/usr/local/cargo/registry`. From
`/src/core`, run `CARGO_TARGET_DIR=/artifacts cargo build --locked --release -p
layerfs-daemon -p layerfs-node`, then copy `release/liblayerfs_node.so` to
`release/layerfs.node` within the target volume. Keep the matching SDK, daemon
and addon together. The runner requires Docker API 1.54, local Linux volumes,
the Docker socket for its trusted SDK client, and Docker Desktop's
`host.docker.internal` connectivity. It is not an untrusted-agent launch recipe.

Set `UNREAL_PROBE_SOURCE` to a private environment asset directory containing
`Geometry/`, `Textures/` and a map. The original is mounted read-only. Invoke
`python3 research/storage-benchmarks/layerfs-unreal-run.py STAGE`, in this order,
inspecting each result before continuing:

```text
prepare
init
forks
read
edit
merge
backup
stale-publication
publication-recovery
shared-branch
restore-prepare
restore-read
```

Each stage has a deadline and preserves append-only invocation receipts and
failures in ignored `.state/layerfs-unreal/`. Do not blindly rerun a mutation:
inspect its recorded state first. `restore-prepare` creates its own fresh labeled
destination volume, and restore uses a separate discovery home and sandbox.
Export the private report, fixture hashes, database backup and `project.record`
before removing only the experiment-owned stopped containers and scratch volumes.
The original campaign did so; its backup remains locally ignored and its temporary
data/store/restore volumes were removed. Toolchain caches were retained.

## Full Unreal content and native cache follow-up

`unreal-cold-load.py` prepares a private APFS clone of an existing UE 5.8 project,
verifies every Content file with SHA-256, and runs a bounded native DDC-fill
commandlet against an empty private filesystem cache. Supply the source and a
fresh ignored output directory; real project names and manifests stay private.

```sh
python3 research/storage-benchmarks/unreal-cold-load.py prepare PRIVATE_PROJECT PRIVATE_CASE
python3 research/storage-benchmarks/unreal-cold-load.py fill PRIVATE_CASE cold
python3 research/storage-benchmarks/unreal-cold-load.py fill PRIVATE_CASE warm
python3 research/storage-benchmarks/unreal-cold-load.py fill PRIVATE_CASE restored
```

Only run warm/restored after a successful cold cache fill. The fixture map is the
European Hornbeam asset zoo; select an applicable map deliberately for a different
fixture. Installed native binaries are retained; this does not measure a clean
C++ compilation. The default is a 900-second deadline and 80 GiB free-disk floor.
No host cache is purged. `restored` uses a new local path and APFS clones, never
a network transfer. `UNREAL_PROBE_ENGINE_PAK=1` adds the installed engine's
read-only DDC pack, and `UNREAL_PROBE_DISABLE_APP_NAP=1` passes a per-process
macOS diagnostic option without modifying defaults or the installed engine.
The first invocation persists those settings for subsequent warm/restored runs.
Use a fresh case for a changed treatment. Keep stopped/failed receipts and do not
call an incomplete cache fill a successful warm-cache qualification.
`UNREAL_PROBE_NULL_RHI=1` is a separate headless treatment: it omits rendering and
cannot establish GPU shader readiness or first playable frame, even if it fills
other target-platform derived data successfully.

`unreal-server-restore.py` is a separate **archive baseline**, run on an existing
storage host against a newly admitted disposable Engine Box VM. It does not
implement Lore, LayerFS, server provisioning or fleet admission. Provide private
`--vm`, `--project`, `--uuid`, `--owner` and `--store` arguments. The store must be
an experiment-owned directory under `/srv/enginebox/assets`, with `owner`,
`manifest.json` and a complete `content.tar.zst`. The archive contains only the
verified `EuropeanHornbeam/` content tree, not agent homes or credentials.

The harness validates the VM UUID/owner, asks Linux to evict only its own archive
from file cache, then times extraction into two fresh guest directories and
guest fsync separately from full-file hash verification. Cache eviction is
advisory and device caches remain uncontrolled. Repeated extraction is not a warm
engine or an in-memory workspace resume. Guest extraction has its own timeout;
all operations preserve a 2 GiB free-disk reserve. Export receipts before deleting
only the exact experiment-owned VM and archive directory under the host lease.

## Checksum concurrency and streaming comparison

`unreal-verified-restore.py` compares full-file SHA-256 read-back with one and
four workers, and verification during extraction. It uses the same private
archive/manifest contract as the baseline, a fresh admitted VM and a fresh
`--output` directory beneath `/srv/enginebox/assets`. Pass `--vm`, `--project`,
`--uuid`, `--owner`, `--store` and `--output`; place `verified-restore.py` beside
the runner. Python 3.11+, zstd and Incus are required. Preflight reserves two
complete copies plus 2 GiB of usable guest space. Preserve the append-only
receipts before removing only the exact owned VM under the host operation lease.

The [results](unreal-verified-restore-results.json) favor bounded parallel
read-back: 2.59–2.84 s versus 9.21–10.86 s serially for 11.17 GB. All eight audits
passed. Extraction plus guest sync took 28.05 s. The first streaming extractor
took 64.62 s, including per-file fsync; it is **not a speedup**. Serial Python
hashing, extraction and a different durability strategy are confounded here;
there is no attribution of the slowdown to a single cause. Do not enable this
streaming implementation as an optimization based on these results.

Audits alternate worker counts on each of two restored trees, with the second
tree starting with four workers. Page/device caches are uncontrolled; archive
eviction is advisory. These are small-sample, same-host asset measurements, not
cold physical-disk verification, a remote cache service, editor startup or a
production latency percentile. Read-back goes through the filesystem and can
hit page cache; it is not an independent hardware/media integrity scrub.

The streaming helper requires a **trusted** manifest, fresh private destination
and private ancestor directories. It rejects unexpected/missing/duplicate files,
wrong sizes/hashes, traversal and links; preserves no executable modes or owners;
and exposes the complete read-only tree only after validation and fsync. This is
an asset-only format, not a general repository restore or an authentication API.
The controller must also require successful producer/decompressor exit before
granting a consumer access. A failed producer can leave a verified tree that is
still ineligible for use; no engine is launched by this harness.

`content_verified` means incoming bytes matched the manifest. `readback_verified`
means a separate pass read stored files and matched them. They are different
receipts. Read-only modes assume a trusted owner; they do not isolate hostile
same-user/root writers. Audits require an immutable base. Engine edits belong in
a private writable fork. Promotion and startup policies must not be inferred
from file creation or an incomplete pipeline.

Run the integrity/publication regressions locally:

```sh
python3 -m unittest discover -s research/storage-benchmarks -p test_verified_restore.py -v
```

These cover corrupt input/stored bytes, incomplete or unexpected content,
truncation, existing-workspace protection, path/link rejection, and publication
only after the whole input is consumed. They do not qualify crash consistency,
malicious concurrent writers, cross-platform behavior or engine startup overlap.
