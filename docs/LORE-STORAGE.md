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

## Engine workflow follow-up, October 23, 2026

The owner's proposed dedicated storage server, warm node caches and lightweight
agent workspaces fit this split. The [Git-free Unreal probe](LAYERFS-VALIDATION.md)
also establishes that LayerFS has usable local history and merging; its role is
not limited by an absence of history. The remaining choice is which system owns
recoverable cross-node project history, retention and distribution. Keep that
authority singular. The proposed default is Lore for accepted project revisions,
with LayerFS branches as temporary local workspaces. This is not implemented.

Inspecting Gitstalk main `785b66da` confirmed an existing integration pattern worth
adapting: parallel speculative preparation/checks, short serialized sprout updates,
asynchronous validation of immutable heads, and promotion of a validated prefix.
`v2-validator.ts` supports debouncing nearby landings into a shared validation;
`v2-check-reuse.ts` reuses a full passing check on the identical commit. The
debounce and evidence-promotion options are disabled by default in run config;
exact-commit full-check reuse defaults on. These are source findings, not a claim
that every option is deployed or enabled on a hosted repository.

The proposed Engine Box workflow is:

1. Agents edit private workspaces concurrently, usually without an engine process.
   A task submission records its base revision, exact changed content and intent.
2. The integration service combines compatible submissions into a sprout candidate.
   It preserves task dependencies and limits how much unvalidated work accumulates.
   Changes continue arriving while a verifier works on a frozen candidate.
3. A bounded engine pool performs the required compile/import, asset validation
   and playable smoke checks. One candidate build can cover several tasks, whose
   preview links record that same candidate revision. Isolated previews are a
   separate on-demand request and consume the same bounded capacity.
4. Promote only the checked candidate and its artifact to the validated/live
   channel, with a single publication authority and an expected previous revision.
   A newer sprout head is not covered by an older passing check. Failures keep the
   current live channel and return conflicting work for repair.
5. Refresh idle or checkpointed agent workspaces explicitly from an accepted
   revision. Updating files beneath an engine that is saving a project is not the
   default. LayerFS `branch.publish` alone cannot implement this workflow.

This preserves the integration policy without requiring Git to own every local
workspace. The Gitstalk implementation itself currently uses Git operations and
SHAs; a Lore/LayerFS adapter needs explicit revision, merge and publication
contracts. It is not a configuration-only substitution.

Gitstalk's optional test skipper was merged in PR #32. It selects Node test files
using restricted JS/TS import dependencies; the analyzer caps inputs at 16 MiB and
10,000 files/directories. It therefore does not cover Unreal or Unity assets.
Selected results cannot stand in for full validation or seed full-check reuse.
Its recorded paired benchmark showed a 1.11× median paired speedup in one CPU-heavy
scenario but lost to exact-commit reuse in every pair of that scenario: 9.21 s off versus 13.50 s on
for the separate medians. Reuse and skipping must be measured together.

For engines, begin with exact-input reuse and candidate batching. An engine test
selection adapter would need code/module dependencies, asset dependencies, importer
and build settings, engine/plugin versions, platform and check definitions. Unreal
[Asset Registry dependencies](https://dev.epicgames.com/documentation/unreal-engine/API/Runtime/AssetRegistry/IAssetRegistry/GetDependencies)
and Unity's
[asset dependency API](https://docs.unity3d.com/6000.0/Documentation/ScriptReference/AssetDatabase.GetDependencies.html)
can supply part of that graph; neither alone proves complete runtime dependency
coverage. Unknown/dynamic dependencies require broader checks. A shared material
can affect maps that no agent edited directly. The current Gitstalk flake heuristic
can treat different failing reruns as green; do not carry that heuristic into the
engine's final playable acceptance gate without a separate explicit policy.

For storage, a warm node forks an already prepared revision; a cold node fetches
missing Lore fragments, materializes the required view, prepares/imports its local
workspace, then starts the engine. Each stage needs its own timing. Lore's
[shared store](https://epicgames.github.io/lore/how-to/run-parallel-instances-over-a-shared-store/)
shares downloaded content across local instances, but its
[lazy-checkout VFS remains in progress](https://github.com/EpicGames/lore/blob/main/docs/roadmap.md).
Do not promise that a cold node can skip all materialization today. Lore and
LayerFS do not share a compatible chunk store automatically; retain that import
cost in cold-start and capacity measurements.

Keep live SQLite stores local to their supported Linux kernel/filesystem. The
mesh should exchange verified immutable content and revision manifests, with
optional peer cache sources scoped to authorized projects. Peers are cache
providers, not independent authorities for the accepted head. Back up canonical
metadata together with reachable content to a separate failure domain; a storage
server and its backup directory on the same machine are not independent recovery.
Keep generated shader/import/build caches separate and disposable, with explicit
compatibility keys. None of this cross-node flow has been deployed or benchmarked
by the local storage probe.

## Initial deployment and generated caches

The owner approved using the existing Engine Box host for both execution and
storage during qualification. Separate OVH storage and GPU machines are a later
deployment choice, not a prerequisite for the prototype. Keep the service and
cache interfaces separable now, and label same-host measurements accordingly.
The current test host exposes only a management display adapter, so rendered
Unreal startup is measured separately on the local Mac, not claimed on that host.

A prepared workspace should reference an accepted source revision and a separate
cache manifest keyed by engine build, compiler/SDK, target OS/architecture/RHI, plugins,
import/build settings and relevant inputs. Preserve Unity's imported Library
and Unreal's reusable derived/build data in their correct storage scopes. The
Gamejam Unreal prototype currently reuses artifact-local binaries by build hash,
but its source hash excludes Saved/Intermediate/DerivedDataCache and the runtime
uses a host-level Zen DDC. Copying just the project does not back up that Zen cache.

Prepared VM disks or explicit cache bundles can avoid redoing compatible imports
without running a shared cache service. A local filesystem DDC is one testable
option; a Zen database needs its supported export/quiescence procedure, not a
copy of live database files. Keep installed engine DDC packs in the base image.
Treat project-local caches, host-level caches, driver/pipeline caches and process
RAM as distinct. macOS Metal cache/binary reuse does not establish Linux Vulkan
compatibility. Cache misses must rebuild correctly; source history remains the
authoritative recoverable content.

Measure source download, materialization, cache hydration, import/shader work,
engine readiness and first playable frame separately. Engine-ready alone is not
first playable frame. Cold tests must state whether installed engine DDC packs
are available, and whether only the project cache or every DDC tier starts empty.
Shared cache services remain an optimization to evaluate once multiple hosts
need incremental reuse; they are not required merely to preserve a prepared
single-host workspace. Do not advertise instantaneous cold restore from a local
copy-on-write clone result.

## Same-host Unreal storage baseline, October 23

The [machine-readable report](../research/storage-benchmarks/unreal-cold-load-results.json)
records a real 11,168,790,731-byte Unreal environment with 379 files, including
files up to 228,269,877 bytes. Both restored copies passed full SHA-256 manifests.
This is an archive/materialization baseline inside an admitted disposable Engine
Box VM, not a Lore or LayerFS transport implementation.

| Stage | Single observation |
|---|---:|
| Cold base VM readiness reported by Engine Box | 15.03 s |
| Fresh restore from already-seeded storage host, including guest fsync | 27.40 s |
| Verify every restored file | 10.25 s |
| Repeat extraction into another fresh directory, including fsync | 26.46 s |
| Verify every file in the second copy | 10.76 s |

Only the owned archive was advised out of the host's file cache before the first
restore; OS/device caches were otherwise uncontrolled. The repeat still writes
all content and is not a warm workspace fork or editor resume. Local archive
hydration was much faster than WAN seeding: the initial sequential transfer was
changed after 1,314 seconds, retaining a checksum-verified 5.97 GB prefix. Four
connections then transferred the remaining 5.16 GB in 643 seconds. This changed
treatment is not a controlled throughput comparison. The full archive checksum
matched after assembly. Zstd level 1 reduced 11.17 GB to only 11.12 GB; these
already-compressed assets need reuse/deltas, not an assumption of large archive
compression savings.

The guest had 4 vCPU and 8 GiB RAM. Its Incus root device was configured as 80 GiB,
but the mounted guest filesystem was only 31,043,616,768 bytes. The two copies
fit with the required free-space reserve; this exposes a capacity-readiness gap
to fix before allocating larger game projects. Verify usable guest filesystem
capacity, not just the declared volume size.

Private source, manifests and logs remain outside Git. The disposable VM and
duplicate upload fragments were removed. The verified 11.12 GB archive remains
privately on the existing storage host to avoid another WAN seed for follow-up
tests; it is a reusable fixture cache, not an independent backup. Rendered
startup, complete shader-cache preparation, and restored-cache reuse remain
unqualified and must not be inferred from these file-copy timings.

The native probe copied and hashed the full 20.66 GB Content tree, while targeting
the environment's asset-zoo map for cache preparation. Render-capable DDC runs
hit a 900-second deadline during initial shader work. A separate headless run
exposed an incorrect `/Game/...` map argument in the harness: this commandlet
expects a relative Content filename. The corrected headless run selected the
map but still hit its 300-second limit before processing it. UE 5.8's commandlet
explicitly compiles target-platform global shaders before loading packages,
including under NullRHI. Headless does not eliminate that preparation cost.

All attempts, including the map-argument failure and deliberate diagnostic stop,
remain in the report. The harness now requires the map's processing log as well
as successful exit; an empty successful commandlet cannot count as qualification.
No cold-fill success, warm-fill speedup, generated-cache restore benefit or
first-playable-frame timing was established. Complete those measurements on the
intended engine/toolchain/platform before promising fast editor startup.

## Checksum follow-up and startup overlap

The [October 23 comparison](../research/storage-benchmarks/unreal-verified-restore-results.json)
reused the already-seeded 11.17 GB environment in a fresh 4-vCPU, 8-GiB disposable
VM. One-worker read-back took 9.21–10.86 s; four workers took 2.59–2.84 s. All eight
full-file SHA-256 audits passed. Across these observations, median verification
time fell from 10.46 s to 2.72 s (about 3.8×). This supports a **bounded parallel
verifier**, not removing integrity checks. Cache state is uncontrolled and this
small sample does not establish production percentiles or physical-media checks.

Extraction plus guest sync took 28.05 s. Adding the first serial verification
gave 37.26 s; adding the first four-worker audit would give about 30.64 s. The
latter is a component-time estimate, not a separately measured end-to-end run.
Thus the checksum gain removes roughly seven seconds here, not three quarters
of total restore time. The initial inline SHA-256 extractor took 64.62 s and
passed integrity checks, but was slower; serial extraction/hashing and per-file
fsync need separate investigation before recommending that implementation.

For the product integration, overlap independent preparation first:

1. Boot/admit the VM and prepare trusted engine binaries or reusable cache layers
   while source content is fetched and verified. Project scripts/plugins stay
   behind the project integrity gate. Reusing a prewarmed process is engine-
   specific and still needs qualification.
2. Verify a complete immutable base once for its revision and storage generation,
   then let authorized agents fork it. Do not run 100 identical full scans merely
   because 100 agents start. Invalidation, corruption or untrusted cache sources
   require revalidation; cache receipts must not outlive their content identity.
3. After every required file and the producer exit pass, allow project startup.
   A later independent audit may overlap startup only under an explicit policy
   that accepts ingress verification first. Audit the immutable base while the
   engine writes its own fork. A failed audit invalidates dependent work and
   prevents acceptance/promotion.
4. To load a project before *all* files arrive, build a filesystem gate that checks
   each chunk/file before the engine can read it. Ordinary extraction plus a
   background checksum task does not provide that guarantee. Neither this
   research helper nor the deployed provider implements that gate.

The measured recommendation is four-worker read-back before project access,
with host-wide limits to avoid competing scans. No actual engine-start overlap,
first-frame improvement or product provider change was demonstrated by this
checksum experiment. Startup/cache qualification remains an independent gate.
