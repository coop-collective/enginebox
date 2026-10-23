# Storage qualification

The active design uses [LayerFS game workspaces](../../docs/GAME-WORKSPACES.md)
and [Store distribution](../../docs/STORE-DISTRIBUTION.md). The experiments below
measure storage and recovery without changing the product runtime contract.

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
implement LayerFS transfer, server provisioning or fleet admission. Provide private
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

## Immutable LayerFS snapshot distribution

`layerfs-distribution.py` exercises desync v1.1.5 against an existing consistent
SQLite snapshot. It reads the original, publishes chunks to a fresh private local
directory and restores through a loopback HTTP server into temporary output files.
The worker cache and origin are distinct directories. Successful restores pass
whole-file SHA-256, SQLite integrity and foreign-key checks; only then does the
harness remove its own restored file. It leaves the original and private evidence
intact. No live Store, production server or active workspace is changed.

Provision the pinned release separately and verify its release digest. The tested
`desync_1.1.5_darwin_arm64.tar.gz` SHA-256 was
`dcbbaa2c6b6960ccc1b9f6c8095dd724f6e74bc7e6f9a6372219d6754f2b6717`.
Python 3.11+ is required. Use a fresh ignored output directory with at least six
times the input size plus 1 GiB free; input must be a closed consistent snapshot
without WAL/SHM/journal sidecars, never an active Store file.

```sh
python3 research/storage-benchmarks/layerfs-distribution.py \
  --snapshot PRIVATE_CONSISTENT_SNAPSHOT \
  --desync VERIFIED_DESYNC_BINARY \
  --output .state/FRESH_PRIVATE_RUN
```

The [recorded observation](layerfs-distribution-results.json) used a 631,517,184-byte
LayerFS v1.0.0 snapshot from the earlier asset probe. Chunk publication took 6.35 s.
First restore took 11.77 s and fetched 630,104,188 payload bytes; a fresh restore
with a populated cache took 3.42 s with zero HTTP requests. After corrupting one
test-cache chunk, explicit repair-disabled extraction failed. Repair fetched one
19,059-byte chunk and restored the exact database. With the origin stopped, the
full cached restore also passed, in 2.84 s. Independent verification took another
0.81–1.38 s per successful restore. Payload counters exclude HTTP/TCP overhead.

The initial HTTP/1.0 fixture hit a connect timeout. Its failed logs remain private;
the fresh successful fixture uses HTTP/1.1, a larger accept queue and TCP_NODELAY.
This is not an isolated diagnosis of which change fixed the timeout. The binary's
`--cache-repair` flag is explicit in both corrupt-cache cases; do not infer its
behavior from an unpinned documentation page.

This proves snapshot-byte distribution and cache behavior on loopback. It does
not prove WAN throughput, changed-generation deduplication, Linux cross-host
LayerFS reopening, game startup, active-project eviction or the 11 GB fixture.
The server binds only loopback and stops in the finalizer. Commands have a
180-second deadline and use four workers. Private output can be removed only
after exporting receipts and confirming its exact owner marker; this is not a
general cache-deletion command.
