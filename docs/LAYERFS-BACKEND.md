# LayerFS checkout storage

`ebox storage layerfs` integrates the qualified LayerFS preview with Engine Box's Rust operator CLI. It imports an approved Git revision, creates isolated writable Docker workspaces, enforces configured CPU/RAM limits, runs ordinary commands, reuses immutable pnpm dependencies, and checkpoints/remounts file state at a stable `/project` path.

LayerFS is Engine Box's first-class checkout storage backend. The controller owns registered storage nodes and source aliases; authenticated API and MCP operations use that registry and the existing durable job queue. See [storage registration and API](STORAGE-API.md). Node execution currently requires Linux and qualified local artifacts. The legacy `experimental` configuration field remains readable for existing receipts but no longer enables or disables the feature.

File storage and VM memory remain distinct. Provider homes and credential delegation keep their existing private lifecycle. Native authenticated sessions on LayerFS mounts and production durability still need separate qualification. Incus attachment and recovery have explicit capability checks; first-class access does not mean every platform or failure mode is supported.

The [game workspace decision](GAME-WORKSPACES.md) keeps this container/LayerFS
combination as the foundation. It maps checkpointing,
fork reuse, conflicts and compute leases to the upstream core ideas.
[Distributed Store generations and local cache eviction](STORE-DISTRIBUTION.md)
extend this backend; they are not supplied by the current commands.

## Runtime and installation boundary

The Rust package is `packages/enginebox-storage-layerfs`. Rust owns configuration, import policy, local receipts, leases, admission, Docker resource limits and CLI behavior. An embedded JavaScript bridge calls the same upstream SDK used in qualification; it supplies no inference proxy, agent loop or orchestration. Each SDK operation runs in a short-lived trusted Docker helper. Agent commands run as the upstream sandbox's unprivileged UID 501/GID 20, without the Docker socket or readable backing Store.

The backend pins revision `968a5d0be0ea3e9456b06f9148b65523138e9490`. It checks the full SDK source digest and both configured native artifact hashes before execution. It requires an immutable local Docker image ID. The SDK and daemon are **not downloaded or installed by default**. Build the pinned artifacts using the [disposable research harness](../research/layerfs-validation/README.md), or explicitly provision those exact inputs on a dedicated Linux test node. This preview still has no supported upstream install; there is no advertised fleet-wide LayerFS installer.

Use local ext4, XFS or Btrfs for both Docker volumes and private SDK metadata. The pinned SDK requires Docker API **1.54**; `doctor` and import preflight reject incompatible daemons before creating state. Docker 29.1.3 reporting maximum API 1.52 is incompatible, even though it is a recent Docker release. The CLI does not upgrade a host daemon. Run it as root on a compatible node with its local Docker socket. A root with existing unrelated files is refused. One private state root is one administrator/trust boundary; projects get separate Store volumes. Root configuration, source revision, immutable artifact hashes and bridge contents are bound to local receipts. A changed binding is refused instead of silently migrating or downgrading a Store.

`docker_socket` defaults to `/var/run/docker.sock`. An explicitly provisioned daemon can use another absolute, root-owned local Unix socket with protected parent directories. Remote TCP endpoints and Docker Desktop's separate-kernel filesystem sharing are not supported by this node backend. The selected socket is part of the storage root's binding; it cannot be switched underneath existing workspaces.

Start from [the example configuration](../examples/layerfs.example.json), replacing paths, image ID and artifact hashes with the build receipts. `max_workspaces × cpus_per_workspace` and `max_workspaces × memory_mib_per_workspace` describe the workspace budget. A temporary SDK helper adds up to 1 CPU and 1 GiB per concurrent operation. Reserve host capacity for that overhead or use a VM with an explicit outer budget. This backend does not enroll a Docker node in the fleet scheduler.

```sh
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json doctor
ebox --host local storage layerfs preview --repo /srv/source/my-project --revision HEAD
```

`doctor` checks existing prerequisites; it does not install Docker, change networking or provision compute. Remote `--host` and `--fleet-host` values are refused rather than accidentally executing locally. Connect to the chosen node over SSH first.

## Publish a clean project and run work

Review the preview's file list, exact commit, byte count and `approval` digest. Import materializes **only the reviewed committed files** into LayerFS. Git objects exist only in temporary connector staging; `.git` is removed before the Store is initialized. The original commit ID remains in the provenance receipt. It excludes the source checkout's local Git configuration, hooks, credentials, remotes, untracked files, ignored dependencies and earlier history objects. Export attributes cannot omit or rewrite tracked files. Known credential paths, escaping symlink chains and submodules are refused. This path policy is not a general secret scanner: the administrator must review the tracked content and explicitly declare it suitable for reuse. Git LFS hydration, submodules and automatic history fetching are not supported by this first import path.

```sh
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json \
  import demo --repo /srv/source/my-project --revision COMMIT_FROM_PREVIEW \
  --approval DIGEST_FROM_PREVIEW --credential-free

sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json fork demo worker-a
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json fork demo worker-b
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json \
  exec worker-a --timeout 120 -- ls -la /project
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json \
  checkpoint worker-a --message 'Saved file changes'
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json remount worker-a
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json close worker-a
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json list
```

After preparing and checkpointing dependencies, `fork demo worker-c --from worker-a` forks that workspace's committed state, including installed files. The source must have a completed checkpoint in the same project and no interrupted operation; it may already be closed. Uncommitted changes are not copied. The default fork source remains the imported clean base. Selecting a prepared source is an explicit administrator action within the same trust boundary, not automatic publication of a user's workspace as a fleet template.

Every sandbox gets a separate bind mount at `/project`, pointing to its current LayerFS mount. Remount changes the underlying `/workspaces/N` location but restores the `/project` bind before accepting another command. Workspaces have independent leases: commands in different workspaces may overlap; concurrent operations on the same workspace are refused. Do not launch detached background workloads through this first command interface.

New workspaces use LayerFS checkpoints and branches, without a native Git checkout. Existing legacy imports containing `.git` are retained as-is and require explicit migration. GitHub import and optional outbound commits are handled by the separate [GitHub connector](GITHUB-SYNC.md).

`close` and `remount` refuse uncheckpointed changes. There is no implicit discard. The `main` and `promote` commands inspect and conditionally advance LayerFS main from a checkpointed, closed workspace; `promote --merge` first merges with current main. GitHub-linked projects use `github-promote` so enabled export is queued. Git reconciliation occurs only in a dedicated sync environment. `exec` does not automatically checkpoint; an orchestrator must explicitly commit at a safe task/command boundary before releasing compute. File checkpoints retain the upstream preview's durability and mtime limitations; they are not a backup or power-loss guarantee.

## Immutable dependency reuse

Configure both `pnpm_store` and `pnpm_metadata` to existing local Docker volumes containing one published cache generation. Seed that generation separately using pnpm 12.14.0, then stop all writers before readers use it. The backend mounts both volumes read-only and copies metadata to each sandbox's private writable cache.

```sh
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json deps worker-a
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json \
  exec worker-a -- env PNPM_CONFIG_CACHE_DIR=/tmp/enginebox-pnpm-cache \
  PNPM_CONFIG_STORE_DIR=/pnpm-store PNPM_CONFIG_FROZEN_STORE=true \
  PNPM_CONFIG_OFFLINE=true pnpm exec tsc --version
```

`deps` runs `pnpm install --frozen-store --offline --frozen-lockfile --ignore-scripts --package-import-method=copy`. It neither downloads missing packages nor enables lifecycle scripts. Cache misses return failure; they do not silently fall back to online installation. The general `exec` command still has networking, so this is pnpm's offline policy, not a network-isolation claim.

`--frozen-store` bypasses SQLite locks. Read-only mounts protect against these consumers writing the cache; they cannot prevent an external publisher from modifying it. Publish a new generation for future consumers rather than updating an active one. Large graphs, native dependency builds, lifecycle scripts and non-pnpm caches remain separate qualification work.

## Failure custody and retention

The CLI writes an in-progress receipt before each mutation. A caught transport or SDK failure marks the workspace uncertain. Killing the CLI can leave its recorded action in progress while the bounded Docker helper/command finishes. A later command refuses that workspace even after its original process ends: nothing is automatically replayed, discarded or claimed healthy. `list` reports durable local receipts, not a fresh daemon health probe.

`recover NAME` reconciles an interrupted `exec` or `deps` after the original bounded process finishes. Each new command records its exact helper identity before launch; the trusted helper saves its original SDK result and completion marker in private project state before returning. Recovery requires that completed result. An absent helper alone does not prove a delayed launch cannot still begin; missing, timed-out or ambiguous results remain blocked.

Recovery acquires the workspace lease, verifies the exact owned Store/container, refuses while a project SDK helper exists, and checks the container's kernel process and mount tables. Only the expected root init/daemon processes may remain; `/project` must still refer to the recorded FUSE mount. The authenticated SDK must confirm the original daemon instance, the expected branch, idle control state and no unresolved merge. Dirty file edits can stay mounted. The audit records the original saved exit code; it never runs the command again or invents an exit code. Helper result files can contain command output and remain private administrator state, subject to the same retention policy as the workspace.

```sh
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json recover worker-a
```

A dead daemon cannot satisfy that proof. If its container is already stopped and the workspace has a recorded completed file checkpoint, `restore-checkpoint NAME --acknowledge-uncheckpointed` creates a new sandbox and branch. Before accepting it, the backend checks the Store's actual branch head against the saved checkpoint, verifies the new mount, and reapplies CPU/RAM limits. The old stopped container, overlay and identities remain available for inspection. **Edits made after the last checkpoint are not recovered into the replacement.** This command never stops a running agent, deletes an old overlay, or retries an unknown storage mutation.

```sh
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json \
  restore-checkpoint worker-a --acknowledge-uncheckpointed
```

These are operator recovery commands, not an automatic startup reconciler or a power-loss guarantee. Partial create, checkpoint, remount, close and restoration operations remain blocked for inspection. A missing/corrupt Store, missing old container, changed checkpoint head, changed configuration or unknown legacy operation is also refused. The preview's SQLite settings still do not guarantee durable writes after host/power failure; preserving a receipt does not reconstruct lost data. Retained active/uncertain workspaces count against admission, and retained stopped containers consume disk until explicitly inspected and removed. Do not hand-edit receipts to ready or remove their locks while an operation might still be running.

The runtime reads existing schema-1 workspace receipts and writes schema 2 on mutation to preserve recovery custody. Older binaries reject schema 2; do not roll a storage root back to a binary that cannot read its receipts. Project/config bindings and the pinned SDK bridge are unchanged. Legacy interrupted commands without an exact helper completion receipt remain blocked.

Closing a workspace deletes its sandbox only after a clean unmount; committed history remains in its project Store. Closed names are not reused. Project deletion and history garbage collection are not automated. The disk reserve checks admission and operations against current free space; it is not a per-workspace write quota and cannot prevent a long command from filling a disk. Do not use this preview for irreplaceable edits or production tenants.

## Qualification

The opt-in test is [live_backend.rs](../packages/enginebox-storage-layerfs/tests/live_backend.rs). Run it only in a disposable Linux fixture with prepared SDK/image/cache inputs, a fresh private state root and exactly two workspace slots. A disposable VM uses the marker `/etc/enginebox-layerfs-validation`. A separately isolated task-owned daemon can instead use an explicit `ENGINEBOX_LAYERFS_MARKER` path and exact `ENGINEBOX_LAYERFS_MARKER_CONTENT`, as in the [recovery qualification](reports/LAYERFS-RECOVERY-VALIDATION.md); its resource isolation and cleanup must be provisioned beforehand. Existing developer workspaces and production daemon upgrades are not test setup.

The test requires `ENGINEBOX_LAYERFS_LIVE=1`, `ENGINEBOX_LAYERFS_LIVE_CONFIG`, `ENGINEBOX_LAYERFS_EBOX`, `ENGINEBOX_LAYERFS_SEED` (synthetic package/lockfile directory), and a fresh `ENGINEBOX_LAYERFS_REPORT` path. Build with the pinned root workspace and run the resulting Linux release test executable with `--exact real_checkout_lifecycle --ignored --nocapture` under an outer timeout. It leaves evidence and any interrupted workspace intact for ownership-checked cleanup after export.

Actual results and remaining limits are recorded in [the backend validation report](reports/LAYERFS-BACKEND-VALIDATION.md). The earlier [LayerFS storage measurements](reports/LAYERFS-VALIDATION.md) and [Rust VM startup benchmarks](reports/RUST-BOOT-BENCHMARKS.md) have different workloads and timing boundaries.
