# Engine Box

Prepared environments for native coding agents, on hardware you control.

Engine Box is a source-available (not open-source; see [License](#license-and-pricing)) toolkit that manages the environment around **Codex, Cursor Agent and Claude Code**: working files, installed tools, snapshots, shared caches and optional desktop/MCP access. You run the official clients and use their native sign-in flows. Native clients keep their planning loops; Engine Box owns the reusable task, workspace, integration and validation workflow that apps such as Gamejam consume.

The goal is less repeated setup and faster, trustworthy feedback from builds and tests. Self-hosted CI optimization is an optional module on the same foundation.

Terms used below: **`ebox`** is the Engine Box command-line tool; a **fleet** is the set of machines (nodes) one owner manages through a single controller; **Vinext** is a Vite-based runtime for Next.js apps on Cloudflare Workers, used for the dashboard.

The [product site and developer guide](packages/site/README.md) run locally with `pnpm dev:site`; all documentation is indexed in [docs/README.md](docs/README.md). The [persistent development environment design](docs/DEV-ENVIRONMENTS.md) describes the next step: a core app that retains development data while agents work in disposable private forks.

## Quick start

### Run a local fleet (no Cloudflare account)

Use Node 24+, pnpm 12.15.0 and Docker Compose with Linux containers on Linux, macOS or Windows (Docker Desktop/WSL2). No Cloudflare account is needed:

```sh
pnpm install --frozen-lockfile
pnpm fleet:dev --storage
pnpm fleet:fixture
pnpm fleet:status
pnpm fleet:smoke
pnpm fleet:down
```

This starts Wrangler with persistent local Durable Objects/R2, two agent nodes and an optional storage cache. Source edits reload automatically; database state stays on Linux named volumes. Health is at **http://127.0.0.1:8799/health**. Omit `--storage` for R2-only shared storage. `fleet:down` preserves development data; the smoke test creates and removes its own isolated fleet.

See [local development, debugging, recovery and current limits](docs/LOCAL-FLEET.md). The new stack implements generation publication, verification, cache reads, fallback and job leases. Existing native/provider/engine services retain their separate launchers; `pnpm dev` still starts only the dashboard. Automatic restored-Store activation and production controller migration remain explicit release gates.

### Bootstrap a server

Use the [terminal installer](docs/INSTALL.md) for release binaries. Admin/operator tools can run on Linux, macOS and Windows; the current compute backend needs a Linux server with usable KVM, Incus and administrative access. The controller and Linux node have separate installation roles. Existing Incus resources can be referenced explicitly; the [fresh-server example](examples/fresh-server.toml) opts into package installation and bounded development storage.

To build from source, install Rust, Node 24+ and the pinned pnpm version, then use the root workspace. Python is not a runtime prerequisite.

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

## Architecture

```text
 Owner ── Vinext dashboard ─┐
 Operator MCP / ebox CLI ───┼─► Controller (fleet API; target: Cloudflare Workers + Durable Objects)
 ACP clients ───────────────┘          │  capacity-aware placement, templates, leases
                                       │  durable storage: R2 generations (+ optional storage-node caches)
              ┌────────────────────────┼────────────────────────┐
              ▼                        ▼                        ▼
        Node (Linux/KVM)         Node (Linux/KVM)         Node ...
        Incus VMs + LayerFS      Incus VMs + LayerFS
              │
              ▼
        Workspace (private VM)
        LayerFS project files · persistent agent state · headless Chromium MCP
        native CLIs: Codex · Claude Code · Cursor Agent
```

The controller authorizes and places work; nodes run isolated workspaces; each workspace runs the official provider CLIs unmodified. The Cloudflare-hosted controller is the target design; today a native Rust controller manages nodes. See [DESIGN.md](docs/DESIGN.md) for the full component diagram and migration boundary.

## Status

**Early development: native Rust services and tools, a capacity-aware fleet API, Vinext dashboard and headless browser MCP.** See [Rust migration and qualification](docs/RUST-MIGRATION.md) for the current release boundary.

The target [fleet control plane uses Cloudflare Workers and Durable Objects](docs/DESIGN.md#required-cloudflare-fleet-control-plane),
deployed in production or run with local Wrangler for development/tests.
Production bare-metal servers are managed nodes; this migration is planned, not deployed.
A dedicated storage node is optional. Single-node project storage can stay local;
multi-node project execution requires shared storage, which R2 directly can provide.

| Available in this repository | Planned |
|---|---|
| Configurable server bootstrap: `init`, `doctor`, `plan`, `apply` | Additional qualified host/guest combinations |
| Controller-synchronized templates, resource presets and prebooted fresh workspace pools | Additional image/recipe formats |
| Official client preparation and reusable clean VM snapshots | Shared mount/cache services and off-host backup workflows |
| Fleet REST API, operator MCP and authenticated Vinext dashboard | Automatic placement, public tenant authorization and billing |
| Native CLI launch/resume through operator MCP; persistent private workspace state | Movable tenant state volumes and authenticated multi-tenant service |
| Account usage cards with native Codex allowance reads and provider links | Claude/Cursor allowance ingestion and subscription renewal dates |
| Native packages, terminal installer plans, version pinning and supervised update/rollback | Enrolled Docker/Windows/macOS resource workers and GPU scheduling |
| Pause/resume, hibernate, disk/memory snapshots and forks | Qualified CI image provisioning and compatible persistent build caches |
| Optional CI triggers, dedicated native build-node registry and GitHub Actions runner adapter | Live cross-platform CI/runner qualification and signing integrations |
| Private Chromium profiles controlled through official Playwright MCP | Specialized game-engine and Cloudflare recipes |
| Controller-managed LayerFS storage, GitHub import/export with conflict-agent handoff, prepared dependency forks and MCP operations | Qualified cross-platform storage and off-host recovery |

The first target is one trusted owner operating Linux/KVM servers. Connect Codex, Claude and Cursor in the controller’s **Agents** panel; compatible native credentials synchronize to assigned private workspaces. The reference fleet now runs Rust 0.3.1-live.1 with native-dev revision 5. All three providers passed delegated MCP launches and exact conversation resume; Codex also passed ACP continuation. Existing private workspace boots and recorded native histories were preserved. The [timing report](docs/reports/REMOTE-AGENT-TIMING-2026-10-23.md) records the small sample, an unresolved admission rejection and the blocked Cursor Cloud comparison. Native inference on LayerFS attachments remains unqualified. Hostile tenancy, cross-host memory migration and a portable CI module are not validated. Reusable bases and unassigned warm reserves stay credential-free.

[Reference-host validation](docs/reports/VALIDATION.md) records fresh workspace creation, independent clones, RAM continuity, real Chromium control and operation timings.

## Manage workspaces

The [fleet service](docs/FLEET.md) clones configured clean snapshots, checks CPU/RAM/storage reserves and serializes mutations per host. Installed tools and dependencies are inherited through storage-level copies; repeated forks do not rerun the installers. Frozen VMs retain their RAM allocation. Stopped and hibernated VMs release compute but keep disk commitments.

The [Vinext dashboard](docs/DASHBOARD.md) provides inventory, creation, lifecycle controls and template installation. [Portable template manifests](docs/TEMPLATES.md) define Minimum/Recommended resources and install independently on each host. Optional [warm pools](docs/WARM-POOLS.md) preboot fresh workspaces so allocation can resume a ready VM instead of waiting for a cold boot. Its server holds the API token; the browser authenticates with a separate owner password. The [operator MCP](docs/FLEET.md) exposes the same fleet operations to local tools. Both use the same backend and admission checks.

The [ACP adapter and fleet task CLI](docs/ACP.md) connect ACP clients to native remote conversations. `ebox tasks --watch` shows parallel jobs across agent types, their tasks, elapsed time and reported usage. `ebox acp` supports prompts, exact-session continuation and targeted cancellation through the same controller.

The optional [CI module](docs/CI.md) triggers shell automations and GitHub Actions without an agent loop. Dedicated build nodes register OS, architecture, image and toolchain labels; the controller rejects configured agent hosts as CI targets. iOS, Android, Windows and Linux templates support the same merge-event handoff. Native image provisioning and live build qualification remain separate from this experimental execution support.


[Browser MCP](docs/BROWSER.md) gives each native CLI its own persistent headless Chromium profile. Agents can navigate, inspect pages, type, click and take screenshots. It controls the workspace browser; personal laptop browser sessions are not imported.

The runtime targets an Incus project named `enginebox` by default, with `ebox-` prefixed VMs and an `agent` user (UID/GID 1001), `/home/agent`, `/workspace` and optional display `:99`. See [runtime configuration](docs/RUNTIME.md) for overrides and capability boundaries. The operator needs administrative Incus access through noninteractive `sudo`; this is a trusted-owner tool.

## Design and implementation

Full index: [docs/README.md](docs/README.md).


- [Persistent development environments and private data forks](docs/DEV-ENVIRONMENTS.md)
- [Live services and HTTP/WebSocket/TCP/UDP exposure](docs/ENVIRONMENT-SERVICES.md)
- [Terminal installation and platform roles](docs/INSTALL.md)
- [Versioned updates, draining and rollback](docs/UPDATES.md)
- [Rust workspace and migration qualification](docs/RUST-MIGRATION.md)
- [LayerFS workspace storage evaluation](docs/LAYERFS.md)
- [LayerFS checkout storage and measured validation](docs/LAYERFS-BACKEND.md)
- [Interrupted-command and daemon recovery qualification](docs/reports/LAYERFS-RECOVERY-VALIDATION.md)
- [LayerFS checkout attachment to Incus](docs/LAYERFS-INCUS.md)
- [Storage registration, controller API and MCP](docs/STORAGE-API.md)
- [GitHub import, optional sync and specialized conflict environments](docs/GITHUB-SYNC.md)
- [Local Cloudflare fleet, shared Store transport and debugging](docs/LOCAL-FLEET.md)
- [Enginebox integration, batching and protected CI evidence](docs/INTEGRATION-CI.md)
- [Logical LayerFS integration executor and operator queue](docs/LOGICAL-INTEGRATION.md)
- [Native Codex task-assistant setup and recovery](docs/TASK-ASSISTANT.md)
- [Exact-source engine builds, playable artifacts and caches](docs/ENGINE-BUILDS.md)
- [Project roadmap tasks and scoped native Codex tools](docs/TASK-WORKFLOW.md)
- [Build artifacts, batch review and playtest evidence](docs/PLAYTEST-EVIDENCE.md)
- [LayerFS game workspaces, container integration and large-project measurements](docs/GAME-WORKSPACES.md)
- [Project Store distribution, worker NVMe caches and safe eviction](docs/STORE-DISTRIBUTION.md)
- [Architecture and portable bootstrap design](docs/DESIGN.md)
- [Implementation milestones and acceptance criteria](docs/IMPLEMENTATION-PLAN.md)
- [Native client installation and authentication](docs/NATIVE-CLIENTS.md)
- [Persistent private agent state and native session launching](docs/TENANT-AGENT-STATE.md)
- [Portable host capabilities and platform qualification](docs/PORTABLE-HOSTS.md)
- [Provider allowance and billing visibility](docs/AGENT-USAGE.md)
- [License, pricing and nodes](docs/LICENSING.md)
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

## License and pricing

Engine Box is source-available under the [Business Source License 1.1](LICENSE). It is not OSI open source.

- **Free:** personal, non-commercial and non-production use (testing, evaluation, development).
- **$10 per node per month:** production use by or on behalf of a business. A node is each machine registered with a controller to run workspaces; the controller itself does not count unless it also runs workspaces. Buy a license on the [pricing page](https://enginebox.coopcollective.dev/pricing/).
- **Separate agreement:** offering Engine Box to third parties as a hosted or managed service.
- Each version converts to Apache-2.0 four years after its first public release.

Earlier versions keep the licenses they were published under (Apache-2.0 through commit `19f834a`, then a prior source-available license). See [docs/LICENSING.md](docs/LICENSING.md) and [NOTICE](NOTICE). Reusable optional modules, including CI integration, will live here as they are implemented. Managed hosting is planned and not available. Third-party clients and engines retain their own licenses; they are not bundled or relicensed here.

GitHub account connections, private repository selection and the optional native Actions runner service are described in [GitHub Actions](docs/GITHUB-ACTIONS.md), with [Kitbash secret setup](docs/GITHUB-SECRETS.md).
