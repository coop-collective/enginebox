# Engine Box

Prepared environments for native coding agents, on hardware you control.

Engine Box manages the environment around **Codex, Cursor Agent and Claude Code**: working files, installed tools, snapshots, shared caches and optional desktop/MCP access. You run the official clients and use their native sign-in flows. Agent planning and swarm orchestration stay with other tools.

The goal is less repeated setup and faster, trustworthy feedback from builds and tests. Self-hosted CI optimization is an optional module on the same foundation.

## Status

**Early development: working server bootstrap, capacity-aware fleet API, Vinext dashboard and headless browser MCP.**

| Available in this repository | Planned |
|---|---|
| Configurable server bootstrap: `init`, `doctor`, `plan`, `apply` | Independently qualified fresh-host installs and upgrade/rollback |
| Official client preparation and reusable clean VM snapshots | Shared mount/cache services and off-host backup workflows |
| Fleet REST API, operator MCP and authenticated Vinext dashboard | Automatic placement, tenancy and billing |
| Pause/resume, hibernate, disk/memory snapshots and forks | Optional GitHub Actions runner/cache module |
| Private Chromium profiles controlled through official Playwright MCP | Specialized game-engine and Cloudflare recipes |

The first target is one trusted owner operating Linux/KVM servers. Native subscription login/inference, hostile tenancy, cross-host memory migration and a portable CI module are not validated capabilities. Provider and browser accounts stay inside private workspaces; reusable bases are prepared before login.

[Reference-host validation](docs/VALIDATION.md) records fresh workspace creation, independent clones, RAM continuity, real Chromium control and operation timings.

## Bootstrap a server

Prerequisites: Python 3.11+, `uv`, OpenSSH and a Linux x86_64 server with usable KVM and administrative access. Start with the [bootstrap guide](docs/BOOTSTRAP.md). Existing Incus resources can be referenced explicitly; the [fresh-server example](examples/fresh-server.toml) opts into package installation and bounded development storage.

```sh
git clone https://github.com/coop-collective/enginebox.git
cd enginebox
uv sync --project runtime --locked
runtime/.venv/bin/ebox init --output enginebox.toml
# Edit enginebox.toml for your host, storage and networking.
runtime/.venv/bin/ebox doctor --config enginebox.toml
runtime/.venv/bin/ebox plan --config enginebox.toml
runtime/.venv/bin/ebox apply --config enginebox.toml
```

Inside a prepared guest, launch `codex`, `agent` or `claude` directly. Provider subscriptions and limits remain with the provider; Engine Box does not pool credentials or resell inference.

## Manage workspaces

The [fleet service](docs/FLEET.md) clones configured clean snapshots, checks CPU/RAM/storage reserves and serializes mutations per host. Installed tools and dependencies are inherited through storage-level copies; repeated forks do not rerun the installers. Frozen VMs retain their RAM allocation. Stopped and hibernated VMs release compute but keep disk commitments.

The [Vinext dashboard](docs/DASHBOARD.md) provides inventory, creation and lifecycle controls. Its server holds the API token; the browser authenticates with a separate owner password. The [operator MCP](docs/FLEET.md) exposes the same fleet operations to local tools. Both use the same backend and admission checks.

[Browser MCP](docs/BROWSER.md) gives each native CLI its own persistent headless Chromium profile. Agents can navigate, inspect pages, type, click and take screenshots. It controls the workspace browser; personal laptop browser sessions are not imported.

The runtime targets an Incus project named `enginebox` by default, with `ebox-` prefixed VMs and an `agent` user (UID/GID 1001), `/home/agent`, `/workspace` and optional display `:99`. See [runtime configuration](docs/RUNTIME.md) for overrides and capability boundaries. The operator needs administrative Incus access through noninteractive `sudo`; this is a trusted-owner tool.

## Design and implementation

- [Architecture and portable bootstrap design](docs/DESIGN.md)
- [Implementation milestones and acceptance criteria](docs/IMPLEMENTATION-PLAN.md)
- [Native client installation and authentication](docs/NATIVE-CLIENTS.md)
- [Server bootstrap](docs/BOOTSTRAP.md)
- [Fleet API and MCP](docs/FLEET.md)
- [Vinext dashboard](docs/DASHBOARD.md)
- [Headless browser control](docs/BROWSER.md)
- [Runtime operations and configuration](docs/RUNTIME.md)
- [Contributing](CONTRIBUTING.md)

The first host target is Linux x86_64 with KVM; the reference prototype used Ubuntu 26.04, Incus 6.0.5 and an Ubuntu 24.04 guest. Other supported combinations will be added after testing. A dedicated server or a VM exposing hardware virtualization can be a candidate; macOS can operate the server over SSH.

## Open-source and commercial boundary

This repository contains the open-source environment foundation. Reusable optional modules, including CI integration, will live here as they are implemented. Self-hosting remains useful on its own. Managed hosting and proprietary, maintained workspace recipes can be separate offerings. Third-party clients and engines retain their own licenses; they are not bundled or relicensed here.

Licensed under [Apache-2.0](LICENSE).
