# Experimental node-local checkout backend

`ebox storage layerfs` integrates the qualified LayerFS preview with Engine Box's Rust operator CLI. It imports an approved Git revision, creates isolated writable Docker workspaces, enforces configured CPU/RAM limits, runs ordinary commands, reuses immutable pnpm dependencies, and checkpoints/remounts file state at a stable `/project` path.

This is an **opt-in Linux administrator backend**, not the default Incus workspace filesystem. It does not attach storage to fleet VMs, move native agent homes, delegate credentials, expose new controller/MCP endpoints, or checkpoint RAM. The existing Incus lifecycle and private account state are independent. Authenticated native agent sessions on these mounts remain unqualified.

## Runtime and installation boundary

The Rust package is `packages/enginebox-storage-layerfs`. Rust owns configuration, import policy, local receipts, leases, admission, Docker resource limits and CLI behavior. An embedded JavaScript bridge calls the same upstream SDK used in qualification; it supplies no inference proxy, agent loop or orchestration. Each SDK operation runs in a short-lived trusted Docker helper. Agent commands run as the upstream sandbox's unprivileged UID 501/GID 20, without the Docker socket or readable backing Store.

The backend pins revision `968a5d0be0ea3e9456b06f9148b65523138e9490`. It checks the full SDK source digest and both configured native artifact hashes before execution. It requires an immutable local Docker image ID. The SDK and daemon are **not downloaded or installed by default**. Build the pinned artifacts using the [disposable validation package](../packages/layerfs-validation/README.md), or explicitly provision those exact inputs on a dedicated Linux test node. This preview still has no supported upstream install; there is no advertised fleet-wide LayerFS installer.

Use local ext4, XFS or Btrfs for both Docker volumes and private SDK metadata. Run the CLI as root on that node with its local Docker socket. A root with existing unrelated files is refused. One private state root is one administrator/trust boundary; projects get separate Store volumes. Root configuration, source revision, immutable artifact hashes and bridge contents are bound to local receipts. A changed binding is refused instead of silently migrating or downgrading a Store.

Start from [the example configuration](../examples/layerfs.example.json), replacing paths, image ID and artifact hashes with the build receipts. `max_workspaces × cpus_per_workspace` and `max_workspaces × memory_mib_per_workspace` describe the workspace budget. A temporary SDK helper adds up to 1 CPU and 1 GiB per concurrent operation. Reserve host capacity for that overhead or use a VM with an explicit outer budget. This backend does not enroll a Docker node in the fleet scheduler.

```sh
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json doctor
ebox --host local storage layerfs preview --repo /srv/source/my-project --revision HEAD
```

`doctor` checks existing prerequisites; it does not install Docker, change networking or provision compute. Remote `--host` and `--fleet-host` values are refused rather than accidentally executing locally. Connect to the chosen node over SSH first.

## Publish a clean project and run work

Review the preview's file list, exact commit, byte count and `approval` digest. Import takes **only that Git commit and its tree objects**, preserving the original commit ID in a clean shallow repository. It excludes the source checkout's local Git configuration, hooks, credentials, remotes, untracked files, ignored dependencies and earlier history objects. Export attributes cannot omit or rewrite tracked files. Known credential paths, escaping symlink chains and submodules are refused. This path policy is not a general secret scanner: the administrator must review the tracked content and explicitly declare it suitable for reuse. Git LFS hydration, submodules and automatic history fetching are not supported by this first import path.

```sh
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json \
  import demo --repo /srv/source/my-project --revision COMMIT_FROM_PREVIEW \
  --approval DIGEST_FROM_PREVIEW --credential-free

sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json fork demo worker-a
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json fork demo worker-b
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json \
  exec worker-a --timeout 120 -- git status --short
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json \
  checkpoint worker-a --message 'Saved file changes'
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json remount worker-a
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json close worker-a
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json list
```

After preparing and checkpointing dependencies, `fork demo worker-c --from worker-a` forks that workspace's committed state, including installed files. The source must have a completed checkpoint in the same project and no interrupted operation; it may already be closed. Uncommitted changes are not copied. The default fork source remains the imported clean base. Selecting a prepared source is an explicit administrator action within the same trust boundary, not automatic publication of a user's workspace as a fleet template.

Every sandbox gets a separate bind mount at `/project`, pointing to its current LayerFS mount. Remount changes the underlying `/workspaces/N` location but restores the `/project` bind before accepting another command. Workspaces have independent leases: commands in different workspaces may overlap; concurrent operations on the same workspace are refused. Do not launch detached background workloads through this first command interface.

The source checkout's local Git author settings and remotes are deliberately not imported. Configure the desired author identity in your image or workspace before committing. Agent commits retain the imported commit as their normal Git ancestor.

`close` and `remount` refuse uncheckpointed changes. There is no implicit discard or filesystem branch-merge command. Use Git's own integration semantics for future agent changes; whole-tree LayerFS merging of `.git` metadata is not Git merging. File checkpoints retain the upstream preview's durability and mtime limitations; they are not a backup or power-loss guarantee.

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

Inspect the exact private project/workspace receipts and their recorded container/volume identities before recovery. Recovery after daemon/host restart, automatic reconciliation and an administrative repair command are not implemented. Retained in-progress/uncertain workspaces count against admission. Do not hand-edit them to ready or remove their locks while an operation might still be running. Preserve their data for inspection.

Closing a workspace deletes its sandbox only after a clean unmount; committed history remains in its project Store. Closed names are not reused. Project deletion and history garbage collection are not automated. The disk reserve checks admission and operations against current free space; it is not a per-workspace write quota and cannot prevent a long command from filling a disk. Do not use this preview for irreplaceable edits or production tenants.

## Qualification

The opt-in test is [live_backend.rs](../packages/enginebox-storage-layerfs/tests/live_backend.rs). Run it only inside a new disposable Linux VM with the marker `/etc/enginebox-layerfs-validation`, prepared SDK/image/cache inputs, a fresh private state root and exactly two workspace slots. It requires `ENGINEBOX_LAYERFS_LIVE=1`, `ENGINEBOX_LAYERFS_LIVE_CONFIG`, `ENGINEBOX_LAYERFS_EBOX`, `ENGINEBOX_LAYERFS_SEED` (synthetic package/lockfile directory), and a fresh `ENGINEBOX_LAYERFS_REPORT` path. Build with the pinned root workspace and run the resulting Linux release test executable with `--exact real_checkout_lifecycle --ignored --nocapture` under an outer timeout. This test leaves evidence and any interrupted workspace intact for ownership-checked VM cleanup after export.

Actual results and remaining limits are recorded in [the backend validation report](LAYERFS-BACKEND-VALIDATION.md). The earlier [LayerFS storage measurements](LAYERFS-VALIDATION.md) and [Rust VM startup benchmarks](RUST-BOOT-BENCHMARKS.md) have different workloads and timing boundaries.
