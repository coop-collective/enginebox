# LayerFS operator and checkpoint recovery qualification

Status: implemented and exercised on **2026-10-19**, following the initial [backend qualification](LAYERFS-BACKEND-VALIDATION.md). This report covers operator interruption and loss of one sandbox daemon. It does not establish host power-loss durability or recovery of uncheckpointed edits after daemon loss.

## Final run

The extended [live lifecycle test](../packages/enginebox-storage-layerfs/tests/live_backend.rs), campaign D, passed in **33.63 seconds**. It used a new private state root, synthetic Git source, an immutable TypeScript 5.9.3 dependency cache and two workspace slots. Each sandbox was limited to 1 CPU and 1 GiB RAM; each temporary SDK helper to 1 CPU and 1 GiB.

The Linux host was Ubuntu 26.04. Qualification used a task-owned Docker 29.5.3 daemon, API 1.54, with separate data/socket/network state inside a container limited to 4 CPUs and 6 GiB. The existing host Docker daemon was not upgraded or reconfigured. The selected Unix socket was supplied explicitly in node configuration. This test shared its outer resource budget with the separate Incus attachment experiment; the storage projects and roots were separate.

| Operation | Observed time |
| --- | ---: |
| First two fresh sandbox forks | 1.306 s / 1.252 s |
| Offline dependency install, reused 1 / downloaded 0 | 0.881 s |
| New sandbox from a prepared dependency checkpoint | 1.346 s |
| Two simultaneous three-second commands | 3.629 s total |
| Recover the completed original command after killing its CLI | 0.647 s |
| Create and verify a replacement after sandbox SIGKILL | 1.749 s |

These are individual wall-clock observations, including CLI/helper overhead, with cached images and prepared dependencies. They are not VM boot times, p95 estimates, large-project benchmarks, or inference measurements.

## What the failures proved

The test killed the operator CLI after observing its command's first write. While the original helper was still running, both ordinary re-execution and recovery were refused. The original command then finished independently. Recovery required the exact helper's saved successful completion result, the original authenticated daemon, matching branch/mount identity, and a kernel process census containing only root init and the daemon. It restored the ready state, retained dirty file changes, reported the original exit code 0 and preserved the audit trail. The file contained one write, not a replay.

Next, the test created a completed file checkpoint, wrote another uncheckpointed file, and confirmed that checkpoint restoration refused to stop the running sandbox. It sent SIGKILL to that exact disposable container. Ordinary recovery refused the stopped daemon. Explicit checkpoint restoration created a new branch/container, verified its actual LayerFS head against the saved checkpoint, restored `/project`, and reapplied resource limits.

The replacement contained the checkpointed files and ran the inherited TypeScript compiler without an install. The later uncheckpointed file was absent, as the command's acknowledgement states. The old container remained stopped and inspectable; its **69,632-byte overlay database** was exported as forensic evidence. That export is not a claim that the old overlay can be mounted or that uncheckpointed edits are recoverable.

The run also retained the earlier checks for Git ancestry, private branch writes, capacity refusal, same-workspace locking, dirty-close refusal, stable remounts and immutable dependency reuse.

## Corrections derived from qualification

- The pinned SDK sends Docker API 1.54 requests. The existing host daemon reported API 1.44–1.52. `doctor` and import preflight now reject that incompatibility before creating storage state. Qualification used a separate compatible daemon.
- Loading the same runtime image into the separate daemon's classic image store produced a different local image ID from the host's containerd image store. Campaign B correctly refused the stale ID during read-only preflight. Its private state root was never created. Later campaigns used the destination daemon's inspected immutable ID.
- Campaign C successfully reconciled the interrupted command, then exposed an overly strict checkpoint validator before restoration mutated anything. A LayerFS `CommitId` is a 33-byte typed identity: tag `0x12`, followed by 32 bytes. It is not a plain 32-byte artifact hash. The validator and regression tests now enforce that exact format. Campaign D exercised the corrected validator against an actual saved checkpoint.

Focused tests also cover delayed/absent helper launches, timed-out or ambiguous helper results, partial storage operations, moved branch heads, unresolved daemon states, foreign mount identities, unexpected processes and missing checkpoint records. A missing helper by itself is never evidence that an old command cannot start later.

## Provenance and state compatibility

The SDK source remains pinned to `968a5d0be0ea3e9456b06f9148b65523138e9490`, with the unchanged full source digest checked by the backend. No upstream source or Docker API rewriting was used. The embedded SDK bridge is unchanged.

| Final run input | SHA-256 |
| --- | --- |
| Runtime image ID | `e19fdeebd1b283b6dd68e180823dc82d8eb86e67acada5b039874088d8f1dc68` |
| LayerFS daemon | `af190f60f25ed1bcf0ee3126c9645db4f7032284e6bbe7201ea16c90bfe298b1` |
| Native addon | `2a8427e8dfd3f5800a9f1c191c834d57a3703e8d5dbc71144d3b0386951797ac` |
| Linux release `ebox` | `daab353f4430cd5fddac4479d8cd103fee94dbd7a71d119e36815cc741782068` |
| Linux release test executable | `0a83f7f5631ebbe4feef2a552a0737ba170c7b79ed733c7b96abdbf3dc955eaf` |

Engine Box was built with Rust 1.96.0. Local and Linux storage unit/source-policy tests and strict Clippy passed. The actual Linux release executable ran the disposable integration test.

Workspace mutations now write receipt schema 2; the runtime accepts schema 1 and 2. Older binaries refuse schema 2 instead of silently erasing recovery history or ignoring attachment leases. Attachment upgrades the workspace receipt before publishing an export. Existing project/config bindings remain compatible, including omission of the default Docker socket from serialized bindings. Legacy interrupted commands lacking exact helper-completion evidence remain blocked.

## Evidence and cleanup boundary

Raw reports from the failed and successful campaigns, final workspace/project receipts and the retained overlay were exported to ignored private operator storage before cleanup. They are not committed to Git. The test closes its replacement workspaces but deliberately retains the stopped originals for ownership-checked cleanup by the shared qualification fixture's owner. Production services and developer workspaces were not used as test fixtures.

The upstream SQLite settings still do not guarantee durable writes after power loss. Missing or corrupt Stores, changed checkpoint heads, partial checkpoint/remount/create/close operations and missing original containers remain blocked. There is no automatic boot-time replay, lost-data reconstruction, old-overlay garbage collection or host-restart qualification in this report.
