# Engine Box

Prepared environments for native coding agents, on hardware you control.

Engine Box manages the environment around **Codex, Cursor Agent and Claude Code**: working files, installed tools, snapshots, shared caches and optional desktop/MCP access. You run the official clients and use their native sign-in flows. Agent planning and swarm orchestration stay with other tools.

The goal is less repeated setup and faster, trustworthy feedback from builds and tests. Self-hosted CI optimization is an optional module on the same foundation.

## Status

**Early development: a reusable runtime and implementation design, not yet a general-purpose server installer.**

| Available in this repository | Planned |
|---|---|
| Incus environment controls over SSH or locally | Configurable, repeatable server bootstrap with `doctor`, `plan` and `apply` |
| Pause, hibernate, disk/memory snapshots, forks and restore | Versioned preparation recipes for the three native clients |
| Operator MCP tools and restricted guest desktop tools | Safe desktop provisioning, cache/mount setup and backup workflows |
| Offline runtime tests | Optional GitHub Actions runner/cache module |

The original private-host prototype installed all three clients and passed 28 environment, mounted-Git and desktop checks. Those results apply to that prototype; this extraction needs its own disposable-VM validation. Native subscription login/inference, portable bootstrap and public tenancy have not been validated here.

## Try the runtime

Prerequisites: Python 3.11+, `uv`, OpenSSH, and a prepared Linux Incus VM with the guest conventions below. This checkout does not install Incus or create a VM yet.

```sh
git clone https://github.com/coop-collective/enginebox.git
cd enginebox
uv sync --project runtime --locked
export ENGINEBOX_HOST=my-server # your SSH config alias, or local
runtime/.venv/bin/ebox list
runtime/.venv/bin/ebox shell ebox-dev
```

Inside a prepared guest, launch `codex`, `agent` or `claude` directly. Provider subscriptions and limits remain with the provider; Engine Box does not pool credentials or resell inference.

The runtime targets an Incus project named `enginebox` by default, with `ebox-` prefixed VMs and an `agent` user (UID/GID 1001), `/home/agent`, `/workspace` and optional display `:99`. See [runtime configuration](docs/RUNTIME.md) for overrides and capability boundaries. The operator needs administrative Incus access through noninteractive `sudo`; this is a trusted-owner tool.

## Design and implementation

- [Architecture and portable bootstrap design](docs/DESIGN.md)
- [Implementation milestones and acceptance criteria](docs/IMPLEMENTATION-PLAN.md)
- [Native client installation and authentication](docs/NATIVE-CLIENTS.md)
- [Runtime operations and configuration](docs/RUNTIME.md)
- [Contributing](CONTRIBUTING.md)

The first host target is Linux x86_64 with KVM; the reference prototype used Ubuntu 26.04, Incus 6.0.5 and an Ubuntu 24.04 guest. Other supported combinations will be added after testing. A dedicated server or a VM exposing hardware virtualization can be a candidate; macOS can operate the server over SSH.

## Open-source and commercial boundary

This repository contains the open-source environment foundation. Reusable optional modules, including CI integration, will live here as they are implemented. Self-hosting remains useful on its own. Managed hosting and proprietary, maintained workspace recipes can be separate offerings. Third-party clients and engines retain their own licenses; they are not bundled or relicensed here.

Licensed under [Apache-2.0](LICENSE).
