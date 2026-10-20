# Engine Box

Prepared environments for native coding agents, on hardware you control.

Engine Box manages the environment around **Codex, Cursor Agent and Claude Code**: working files, installed tools, snapshots, shared caches and optional desktop/MCP access. You run the official clients and use their native sign-in flows. Agent planning and swarm orchestration stay with other tools.

The goal is less repeated setup and faster, trustworthy feedback from builds and tests. Self-hosted CI optimization is an optional module on the same foundation.

The [product site and developer guide](packages/site/README.md) run locally with `pnpm dev:site`. They explain the environment model, getting started, native agent state, templates, storage and the API. The [persistent development environment design](docs/DEV-ENVIRONMENTS.md) describes the next step: a core app that retains development data while agents work in disposable private forks.

## Status

**Early development: native Rust services and tools, a capacity-aware fleet API, Vinext dashboard and headless browser MCP.** See [Rust migration and qualification](docs/RUST-MIGRATION.md) for the current release boundary.

| Available in this repository | Planned |
|---|---|
| Configurable server bootstrap: `init`, `doctor`, `plan`, `apply` | Additional qualified host/guest combinations |
| Controller-synchronized templates, resource presets and prebooted fresh workspace pools | Additional image/recipe formats |
| Official client preparation and reusable clean VM snapshots | Shared mount/cache services and off-host backup workflows |
| Fleet REST API, operator MCP and authenticated Vinext dashboard | Automatic placement, public tenant authorization and billing |
| Native CLI launch/resume through operator MCP; persistent private workspace state | Movable tenant state volumes and authenticated multi-tenant service |
| Account usage cards with native Codex allowance reads and provider links | Claude/Cursor allowance ingestion and subscription renewal dates |
| Native packages, terminal installer plans, version pinning and supervised update/rollback | Enrolled Docker/Windows/macOS resource workers and GPU scheduling |
| Pause/resume, hibernate, disk/memory snapshots and forks | Optional GitHub Actions runner/cache module |
| Private Chromium profiles controlled through official Playwright MCP | Specialized game-engine and Cloudflare recipes |
| Controller-managed LayerFS checkout storage, prepared dependency forks and MCP operations | Qualified cross-platform storage and off-host recovery |

The first target is one trusted owner operating Linux/KVM servers. Connect Codex, Claude and Cursor in the controller’s **Agents** panel; compatible native credentials synchronize to assigned private workspaces. Rust qualification passed real Codex/Claude coding, exact conversation resume and browser MCP use. Cursor inference remains blocked by the test account’s provider quota. The reference fleet now runs Rust 0.3.1-live.0 with account state preserved; Codex/Claude coding and exact resume also passed through the deployed operator MCP. Native inference on LayerFS attachments remains unqualified. Hostile tenancy, cross-host memory migration and a portable CI module are not validated. Reusable bases and unassigned warm reserves stay credential-free.

[Reference-host validation](docs/VALIDATION.md) records fresh workspace creation, independent clones, RAM continuity, real Chromium control and operation timings.

## Bootstrap a server

Use the [terminal installer](docs/INSTALL.md) for release binaries. Admin/operator tools can run on Linux, macOS and Windows; the current compute backend needs a Linux server with usable KVM, Incus and administrative access. The controller and Linux node have separate installation roles. Existing Incus resources can be referenced explicitly; the [fresh-server example](examples/fresh-server.toml) opts into package installation and bounded development storage.

To build from source, install Rust, Node 22.12+ and the pinned pnpm version, then use the root workspace. Python is not a runtime prerequisite.

```sh
git clone https://github.com/coop-collective/enginebox.git
cd enginebox
pnpm install --frozen-lockfile
pnpm build
target/release/ebox init --output enginebox.toml
# Edit enginebox.toml for your host, storage and networking.
target/release/ebox doctor --config enginebox.toml
target/release/ebox plan --config enginebox.toml
target/release/ebox apply --config enginebox.toml
```

Inside a prepared guest, launch `codex`, `agent` or `claude` directly. Provider subscriptions and limits remain with the provider; Engine Box does not pool credentials or resell inference.

## Manage workspaces

The [fleet service](docs/FLEET.md) clones configured clean snapshots, checks CPU/RAM/storage reserves and serializes mutations per host. Installed tools and dependencies are inherited through storage-level copies; repeated forks do not rerun the installers. Frozen VMs retain their RAM allocation. Stopped and hibernated VMs release compute but keep disk commitments.

The [Vinext dashboard](docs/DASHBOARD.md) provides inventory, creation, lifecycle controls and template installation. [Portable template manifests](docs/TEMPLATES.md) define Minimum/Recommended resources and install independently on each host. Optional [warm pools](docs/WARM-POOLS.md) preboot fresh workspaces so allocation can resume a ready VM instead of waiting for a cold boot. Its server holds the API token; the browser authenticates with a separate owner password. The [operator MCP](docs/FLEET.md) exposes the same fleet operations to local tools. Both use the same backend and admission checks.

[Browser MCP](docs/BROWSER.md) gives each native CLI its own persistent headless Chromium profile. Agents can navigate, inspect pages, type, click and take screenshots. It controls the workspace browser; personal laptop browser sessions are not imported.

The runtime targets an Incus project named `enginebox` by default, with `ebox-` prefixed VMs and an `agent` user (UID/GID 1001), `/home/agent`, `/workspace` and optional display `:99`. See [runtime configuration](docs/RUNTIME.md) for overrides and capability boundaries. The operator needs administrative Incus access through noninteractive `sudo`; this is a trusted-owner tool.

## Design and implementation

- [Persistent development environments and private data forks](docs/DEV-ENVIRONMENTS.md)
- [Terminal installation and platform roles](docs/INSTALL.md)
- [Versioned updates, draining and rollback](docs/UPDATES.md)
- [Rust workspace and migration qualification](docs/RUST-MIGRATION.md)
- [LayerFS workspace storage evaluation](docs/LAYERFS.md)
- [LayerFS checkout storage and measured validation](docs/LAYERFS-BACKEND.md)
- [Interrupted-command and daemon recovery qualification](docs/LAYERFS-RECOVERY-VALIDATION.md)
- [LayerFS checkout attachment to Incus](docs/LAYERFS-INCUS.md)
- [Storage registration, controller API and MCP](docs/STORAGE-API.md)
- [Lore, Cloudflare Artifacts and measured game-project storage](docs/LORE-STORAGE.md)
- [Architecture and portable bootstrap design](docs/DESIGN.md)
- [Implementation milestones and acceptance criteria](docs/IMPLEMENTATION-PLAN.md)
- [Native client installation and authentication](docs/NATIVE-CLIENTS.md)
- [Persistent private agent state and native session launching](docs/TENANT-AGENT-STATE.md)
- [Portable host capabilities and platform qualification](docs/PORTABLE-HOSTS.md)
- [Provider allowance and billing visibility](docs/AGENT-USAGE.md)
- [License and commercial boundary](docs/LICENSING.md)
- [Server bootstrap](docs/BOOTSTRAP.md)
- [Fleet API and MCP](docs/FLEET.md)
- [Template catalog and resource requirements](docs/TEMPLATES.md)
- [Warm workspace pools](docs/WARM-POOLS.md)
- [Vinext dashboard](docs/DASHBOARD.md)
- [Headless browser control](docs/BROWSER.md)
- [Runtime operations and configuration](docs/RUNTIME.md)
- [Research experiments and storage benchmarks](research/README.md)
- [Contributing](CONTRIBUTING.md)

The first host target is Linux x86_64 with KVM; the reference prototype used Ubuntu 26.04, Incus 6.0.5 and an Ubuntu 24.04 guest. Other supported combinations will be added after testing. A dedicated server or a VM exposing hardware virtualization can be a candidate; macOS can operate the server over SSH.

Personal computers are an optional resource-contribution track. Owners choose how much CPU and memory to contribute. The current Linux fleet enforces configured contribution ceilings in admission in addition to physical reserves and per-VM limits. macOS/Windows Docker and native workers are not yet enrollment targets; `enginebox-host` reports hardware and planning budgets without installing a worker or allocating resources. See the [platform matrix and installation gates](docs/PORTABLE-HOSTS.md).

## Source-available and commercial boundary

This repository contains the source-available environment foundation. Personal and internal company self-hosting are permitted under the community license. Commercial hosting, selling access and commercial redistribution require a separate agreement. Reusable optional modules, including CI integration, will live here as they are implemented. Managed hosting and proprietary workspace recipes can be separate offerings. Third-party clients and engines retain their own licenses; they are not bundled or relicensed here.

New versions use the [Sustainable Use License 1.0](LICENSE), originally published by n8n. This is not OSI open source. Previously published Apache-2.0 code retains its existing rights; see the [licensing boundary](docs/LICENSING.md).
