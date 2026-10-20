# Lore and Cloudflare Artifacts for game workspaces

Assessment checked **2026-10-19**. Comparator: **Cloudflare Artifacts, the Git repository service**, not GitHub Actions artifacts, an archive download, or Git LFS.

## Recommendation

Make room for Lore as a first-class **source and asset provider**, while keeping LayerFS responsible for prepared local workspaces. This is an architecture recommendation supported by a local Lore qualification; Engine Box does not yet register, synchronize or import Lore repositories.

| Layer | Responsibility | Current boundary |
|---|---|---|
| Git / Cloudflare Artifacts | Code revisions and ordinary Git workflows | Artifacts cannot hold the complete tested game project under its published limits |
| Lore | Versioned large assets, content-addressed distribution and shared host cache | Local CLI/server and shared-cache workflow verified below; fleet integration proposed |
| LayerFS | Prepared files/dependencies and independent writable agent forks on a host | Existing Engine Box backend; Lore ingestion still needs an adapter |
| R2 | Possible remote object-storage tier | No Lore-to-R2 integration or remote benchmark has been qualified |

Do not make every project adopt Lore or replace the native coding harnesses. Preserve Git where agents expect it. Choose one authoritative history for each file set: a whole Lore project or an explicitly separated code/assets arrangement. Concurrent Git and Lore ownership of the same mutable files needs a synchronization contract before it can be supported.

## Artifacts capacity

Cloudflare currently documents **1 GB maximum storage per repository** and **32 MB per file/blob**. Its 1 TB account limit does not raise those per-repository limits. [Published limits](https://developers.cloudflare.com/artifacts/platform/limits/).

Cloudflare describes repository data in Durable Object SQLite, with snapshots in R2. R2 backing does not mean arbitrary R2 object sizes are accepted through the Artifacts Git API. [Artifacts architecture](https://blog.cloudflare.com/artifacts-git-for-agents-beta/).

The tested Unity content is 3,960,409,930 bytes across 11,556 files. Sixteen files exceed even 32 MiB; the largest is 193,292,137 bytes. The individual-file limit alone rules out an ordinary complete import. Repository storage accounting may differ from working-tree size, but that does not remove this blob-limit failure. No attempt was made to upload the private project to Artifacts.

[ArtifactFS](https://developers.cloudflare.com/artifacts/guides/artifact-fs/) is a separate lazy-checkout mechanism. It can reduce initial hydration; it does not remove the backing repository service's upload limits. Splitting assets into R2 and keeping pointers in Git would be a separate asset transport/versioning system, not a direct Artifacts capability established by this evaluation.

## Local Lore qualification

Pinned [EpicGames/lore v0.10.1](https://github.com/EpicGames/lore/releases/tag/v0.10.1), source `95a9a834f18402cbacf43d43518c0ac771e029eb`; native CLI reports `0.10.1+1476-lore_v0.10.1__urc_main`. macOS 26.7.1, Apple Silicon Mac17,7, 128 GiB RAM, 18 logical CPUs; Lore limited to four worker threads. This is one local loopback observation with ordinary OS caches and other host activity, not an internet, R2 or controlled cold-disk benchmark.

Fixture: an existing Unity project's `Assets`, `Packages` and `ProjectSettings`, copied into private test directories. No `Library`, editor startup, game build, engine source distribution or large Unreal dataset was included. Both full clones, the changed clone and the untouched original passed complete SHA-256 comparisons.

| Operation | Measured wall time |
|---|---:|
| Initial online commit and branch push | 30.45 s |
| Complete checkout, empty Lore shared cache | 47.37 s |
| Complete checkout, populated shared cache | 38.89 s |
| Commit and push a two-file small delta | 0.276 s |
| Sync that delta to an existing checkout | 0.157 s |

Initial staging took another 0.57 s. Online commit uploads content; the subsequent 0.095 s branch push is **not** a 3.96 GB transfer. The delta overwrote 4 KiB inside one TIFF and appended a short comment to the copied settings file. It represents a small binary edit, not re-encoding a whole texture or validating its format.

| Stored file-length sum | Bytes |
|---|---:|
| Server after first publication | 1,959,959,542 |
| Shared cache after first checkout | 1,960,337,910 |
| Same shared cache after second checkout | 1,961,309,121 |
| Additional server storage after the delta | 142,728 |

The second cache grew by 971,211 bytes instead of another complete payload. Each materialized working tree still contains roughly 3.96 GB. These values are sums of stored file lengths, not network counters, resident RAM or allocated physical disk blocks. The observed compression/deduplication ratio is specific to this fixture.

The warm checkout is useful but still takes almost 39 seconds. That supports preparing a host checkout once and qualifying LayerFS forks over it. Copying Lore content into LayerFS can retain content in both stores; budget and measure that duplication rather than assuming shared chunk formats or direct backing-store compatibility.

Reproduction: [manual benchmark](../research/storage-benchmarks/README.md). Verified macOS archive digests:

```text
lore-v0.10.1-aarch64-apple-darwin.tar.gz
5e23a343becfefae57021bfb783d23bfafe02dbadd1d61acd6c0c9999b6efe55
loreserver-v0.10.1-aarch64-apple-darwin.tar.gz
d8fdcd42e3dcfc8f64e41acb08633b1e8370a6c152fdb6caa2cb0343863d1d61
```

## R2 and adoption gates

Lore separates its version-control layer from its content-addressed storage API, so adopting its storage does not inherently require replacing every Git workflow. That is a possible integration direction, not an Engine Box feature already delivered. [Lore system design](https://epicgames.github.io/lore/explanation/system-design/).

The pinned AWS backend uses **S3 plus DynamoDB**, including metadata and fragment lifecycle state. Configurable S3 endpoints alone do not establish an R2-only deployment. R2 would require testing the object API and providing compatible metadata/coordination storage or a new backend. [Pinned AWS implementation](https://github.com/EpicGames/lore/blob/95a9a834f18402cbacf43d43518c0ac771e029eb/lore-aws/src/store/immutable_store.rs).

Lore is pre-1.0; upstream warns about API and storage-format changes. Do not base instant workspace promises on its partially released SWFS implementation. [Upstream status](https://github.com/EpicGames/lore).

Before making Lore a default game storage provider, qualify:

1. Immutable revision ingestion into a credential-free prepared LayerFS base, with checksums, cancellation and disk admission.
2. Native agent edits and engine builds on those mounts, including save/rename semantics and large binary modifications.
3. Concurrent clients, tenant permissions, file conflicts/locking, interrupted transfer, backups and recovery on the dedicated host.
4. Much larger Unreal/Unity fixtures, repeated latency measurements and actual network byte counts across hosts.
5. A pinned upgrade/rollback policy and a separately tested R2-backed architecture if remote object storage is needed.

The practical first step is a pinned self-hosted Lore service and per-trust-boundary host cache feeding LayerFS. Cloudflare Artifacts remains an option for code repositories that fit its limits.
