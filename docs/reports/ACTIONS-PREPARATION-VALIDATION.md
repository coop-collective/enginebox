# Actions preparation validation — October 25, 2026

This report qualifies components, **not a deployed GitHub Actions service or a
full-service performance/cost advantage**. All fixtures were disposable; existing workspaces,
fleet volumes and GitHub runners were preserved.

| Boundary | Evidence |
| --- | --- |
| Group admission/JIT | Local GitHub API fixture checks private repository/workflow restrictions, required work root, host-only verification token, actual assigned job differing from webhook demand, and owned cleanup |
| Start hook | Mount absence and missing host acknowledgement fail before cache environment export; repository/workflow mismatches fail |
| Pnpm recipe | Real pnpm 12.15.0 local tarball install, clean Git checkout, exact warm restore with zero downloads, Node/pnpm consumption, intentional failing test, unchanged approved baseline, corruption refusal and configuration/path/scope invalidation |
| Protected handoff | Disposable Linux read-only bind refuses writes; root helper rejects escaping/symlink/mismatched-policy targets; matching inode before/after proves a move without copying |
| Lifecycle policy | `deny` is explicit; `run` executes the project's install lifecycle and never substitutes a prepared tree |
| Fleet catalog | Owner approval, node denial, published-generation/record binding, immutable keys and revocation surviving Durable Object eviction |
| Fleet transfer | Publication then hydration into an empty node cache, warm local data reuse, wrong repository/missing scope refusal, revocation during transfer prevents activation |
| Native SDK | Linux arm64, upstream revision `968a5d0be0ea3e9456b06f9148b65523138e9490` built from a clean source archive with Rust 1.85.1; SQLite snapshot reopened with the product preparation bridge and its copied discovery record; missing record refused; independent branches preserve main and sibling data |

The native test is
[`preparation-sdk.mjs`](../../packages/enginebox-storage-layerfs/tests/preparation-sdk.mjs).
It requires a disposable Linux environment with empty `/state` and
`/layerfs-store`, the exact built SDK specified by `ENGINEBOX_TEST_SDK`, and
`ENGINEBOX_DISPOSABLE_PROOF=1`. The qualification run used new labeled Docker
volumes, bounded CPU/RAM, and the pinned native addon. Its initialized discovery
record demonstrates direct Store reopen; this does **not** exercise the production
Incus attachment of an installed SDK sandbox. Reported results:

```json
{"passed":true,"nativeStoreReopened":true,"productionPreparationBridge":true,"missingRecordRefused":true,"independentBranches":true,"incusAttachment":false,"liveGitHubJob":false}
```

The same pnpm/hook fixtures also run inside a separate Linux container with
`--network=none`, after installing exact pnpm 12.15.0 in disposable tool state.
This proves their offline recipe behavior; their filesystem is the container's
private filesystem, not an Incus LayerFS mount. The recipe and native Store tests
are separate boundaries, not an end-to-end runner result.

Repository validation uses the pinned workspace commands: frozen installation,
check, test, Rust lint, build and dashboard/site browser tests. Opt-in live tests
remain skipped without their dedicated fixtures/credentials.

Still required before service rollout: real GitHub App approval/revocation and JIT
registration; exact runner 2.337.0 (currently source-reviewed only); Incus
attachment and ordinary checkout on the prepared work root; successful/failing
workflow, cancellation/timeout/crash/drain; fresh-node activation followed by a
real build; native module/container/monorepo matrices as separate profiles.
End-to-end latency, downloaded bytes, disk growth, concurrent capacity,
and compute/storage/network/idle costs have not been measured. Warm package
counts or a local branch proof cannot establish superiority over GitHub-hosted
or equivalently cached conventional self-hosted runners.

The [dependency-stage comparison](../../research/actions-preparation/README.md)
records a narrower measured benefit and an explicit regression gate. It runs on
a Linux named volume with the production recipe/root helper, independently of the
SDK reopen test. Its timings exclude LayerFS mount/fork and VM provisioning, so
it must not be presented as an end-to-end LayerFS Actions benchmark.
