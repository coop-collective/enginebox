# Documentation index

Engine Box is in early development; each guide states what is implemented, qualified or still planned. Start with the [project README](../README.md).

## Get started

- [Installing Engine Box](INSTALL.md): terminal installer, controller and node roles, version pinning.
- [Local Cloudflare fleet](LOCAL-FLEET.md): run the fleet API, nodes and storage locally with Wrangler and Docker.
- [Server bootstrap](BOOTSTRAP.md): inspect, plan and prepare a Linux/KVM host and a credential-free workspace base.
- [Native agent clients](NATIVE-CLIENTS.md): installing and signing into Codex, Claude Code and Cursor Agent.
- [Portable hosts](PORTABLE-HOSTS.md): which operating systems can operate or contribute resources.
- [Licensing](LICENSING.md): the Sustainable Use License boundary and the historical Apache-2.0 grant.

## Guides and reference

- [Runtime](RUNTIME.md): current runtime configuration and capability boundaries.
- [Fleet API and MCP](FLEET.md): REST and operator MCP operations.
- [Dashboard](DASHBOARD.md): the authenticated Vinext fleet dashboard.
- [Templates](TEMPLATES.md): portable workspace templates and resource presets.
- [Warm pools](WARM-POOLS.md): prebooted fresh workspaces.
- [Browser access](BROWSER.md): per-workspace headless Chromium over Playwright MCP.
- [ACP and fleet tasks](ACP.md): ACP adapter and the `ebox tasks` CLI.
- [Native agent state](TENANT-AGENT-STATE.md): private sessions and native run launching/resume.
- [Agent usage](AGENT-USAGE.md): provider allowance and billing visibility.
- [Project tasks](TASK-WORKFLOW.md) and [task assistant](TASK-ASSISTANT.md): roadmap tasks and native assistant bindings.
- [Integration and CI](INTEGRATION-CI.md): batching, candidate validation and protected CI evidence.
- [Updates and rollback](UPDATES.md): pinned releases, draining and rollback.
- [Codex sandbox](CODEX-SANDBOX.md) and [Cursor sandbox](CURSOR-SANDBOX.md): Ubuntu 24.04 sandbox requirements.
- [Storage API](STORAGE-API.md): controller checkout storage API and MCP.
- [LayerFS checkout storage](LAYERFS-BACKEND.md), [Incus attachment](LAYERFS-INCUS.md) and [stopped workspace preparation](LAYERFS-ALLOCATION.md).
- [GitHub sync](GITHUB-SYNC.md): optional import/export and conflict environments.

## Design

- [Architecture and fleet design](DESIGN.md): components, Cloudflare control plane, storage and lifecycle.
- [Implementation plan](IMPLEMENTATION-PLAN.md): milestones and acceptance criteria.
- [Persistent development environments](DEV-ENVIRONMENTS.md): core app plus disposable private data forks.
- [Rust migration](RUST-MIGRATION.md): workspace migration and release boundary.
- [LayerFS evaluation](LAYERFS.md), [game workspaces](GAME-WORKSPACES.md) and [Store distribution](STORE-DISTRIBUTION.md): project storage direction.

## Reports

Dated validation logs and measurements; they record what was tested at the time, not current guarantees.

- [Reference-host validation](reports/VALIDATION.md)
- [Remote agent timing, 2026-10-23](reports/REMOTE-AGENT-TIMING-2026-10-23.md)
- [Rust native agent qualification](reports/RUST-NATIVE-QUALIFICATION.md)
- [Rust VM boot benchmarks](reports/RUST-BOOT-BENCHMARKS.md)
- [Rust controller metadata qualification](reports/RUST-STATE-QUALIFICATION.md)
- [LayerFS storage and checkout validation](reports/LAYERFS-VALIDATION.md)
- [LayerFS CLI backend qualification](reports/LAYERFS-BACKEND-VALIDATION.md)
- [LayerFS recovery qualification](reports/LAYERFS-RECOVERY-VALIDATION.md)
