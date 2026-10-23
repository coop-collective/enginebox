# Historical Lore and Artifacts comparison

Owner decision, October 23, 2026: Lore is excluded from the current architecture.
The current design is [LayerFS game workspaces](../../docs/GAME-WORKSPACES.md)
and [project Store distribution](../../docs/STORE-DISTRIBUTION.md). This file
retains the October 19 comparison and its reproducibility data only.

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

The populated-cache checkout still took almost 39 seconds. These measurements are retained for comparison; they do not create an Engine Box dependency or an adoption plan.

Reproduction: [manual benchmark](README.md). Verified macOS archive digests:

```text
lore-v0.10.1-aarch64-apple-darwin.tar.gz
5e23a343becfefae57021bfb783d23bfafe02dbadd1d61acd6c0c9999b6efe55
loreserver-v0.10.1-aarch64-apple-darwin.tar.gz
d8fdcd42e3dcfc8f64e41acb08633b1e8370a6c152fdb6caa2cb0343863d1d61
```
