# Installing Engine Box

The Rust distribution uses a pinned release archive for each operating system and CPU architecture. Executable code lives in `packages/`. Python is not an installation dependency. The dashboard remains TypeScript/Vinext and requires Node.js 22.12 or newer.

The commands below describe the Rust installer in this branch. Use a version whose matching release assets have actually been published and qualified; an unpublished `0.3.0` archive is not available simply because the source version is `0.3.0`.

## Choose the machine's role

| Role | What is installed | Platform boundary |
| --- | --- | --- |
| `operator` | Native CLI, MCP and administrative tools | Linux, macOS, Windows release targets |
| `controller` | Native controller, stable update supervisor, dashboard and provider client installer | Same targets; Node required for the dashboard |
| `incus-host` | Linux node binaries and an explicit dependency/provisioning plan | Linux Incus/KVM; current VM bootstrap qualification is x86_64 |

A macOS or Windows controller manages separately provisioned Linux nodes. Installing it does not turn Docker Desktop, Windows GPUs or macOS GPUs into fleet capacity. Those resource backends require their own implementation and qualification. Windows operator tooling can connect to a qualified controller; native provider sign-in on a Windows controller remains unsupported until its process and credential flows are qualified. Manually installing a Windows provider CLI does not make that dashboard sign-in flow supported. Windows service/ACL code and PowerShell bootstrap also require Windows execution in the release matrix; a successful macOS build is not Windows machine validation.

## Terminal bootstrap

Download `install.sh` or `install.ps1` from the selected GitHub release, inspect it, and run the plan. A release version is required; there is no moving `latest` install or unattended downgrade.

```sh
sh install.sh --version 0.3.0 --role controller
sh install.sh --version 0.3.0 --role controller --apply --install-dependencies
```

```powershell
.\install.ps1 -Version 0.3.0 -Role controller
.\install.ps1 -Version 0.3.0 -Role controller -Apply -InstallDependencies
```

The default prefix is `$HOME/.local/share/enginebox` on Unix and `%LOCALAPPDATA%\EngineBox` on Windows. Override it with `--prefix` or `-Prefix`. Planning makes no changes. Applying downloads the exact platform archive, checks its SHA256 and the manifest SHA256, then calls the native installer to validate the entire archive before staging it. It activates the first release; it **does not restart an existing service**.

By default, checksums come from the exact release's `enginebox-VERSION-TARGET.sha256` over HTTPS. Every redirect must remain HTTPS. This provides authenticated transport and artifact integrity, not an independent code signature. Administrators can provide independent `--archive-sha256`/`--manifest-sha256` pins (`-ArchiveSha256`/`-ManifestSha256` on Windows). Custom download sources require both independent pins.

`--install-dependencies` explicitly runs the printed OS package-manager commands. Linux uses an existing apt or dnf; macOS uses an existing Homebrew; Windows uses Windows Package Manager. The installer does not silently add package repositories or install Homebrew. It rechecks the required versions afterward and stops if the distro package or PATH is insufficient. Incus installation never initializes storage, repartitions a disk or changes firewall policy. KVM availability and existing host configuration are separately inspected before provisioning.

## Configure a controller

After staging, native programs are in `PREFIX/releases/VERSION/bin/`. Keep this directory on the service PATH or use its absolute paths. Release directories are immutable; private state stays in `PREFIX/state` and `PREFIX/etc`.

1. Put the fleet's private TOML configuration in `PREFIX/etc/fleet.toml`.
2. Put a random API token of at least 32 characters in `PREFIX/etc/api-token`.
3. Create `PREFIX/etc/environment.json`, containing `ENGINEBOX_DASHBOARD_PASSWORD` (at least 16 characters), `ENGINEBOX_SESSION_SECRET` (at least 32), and optionally `ENGINEBOX_DASHBOARD_ORIGIN` and a service `PATH` including Node and SSH. Use distinct random secrets. These values remain constant across releases so browser sessions survive an upgrade.
4. Copy `PREFIX/etc/supervisor.example.json` to `PREFIX/etc/supervisor.json` and inspect its absolute paths, API/dashboard listener addresses and drain timeout. Only loopback listeners are accepted. An external HTTPS gateway remains an administrator-managed component.
5. Set private files to owner-only permissions. Unix private directories are `0700` and secrets `0600`. The Windows installer replaces the owned prefix/state directory DACLs with protected, current-user-only access, removing other explicit and inherited grants; use a private, user-owned prefix and keep credentials inside it.
6. On Linux/macOS, run `enginebox-agent-clients --state-dir PREFIX/state --plan`, then `enginebox-agent-clients --state-dir PREFIX/state --apply` as the unprivileged service user. This installs qualified official Codex, Claude and Cursor clients into controller state. On Windows this tool reports manual official-client installation guidance; managed provider installation is not yet qualified there. Connect accounts through Agents in the dashboard. Installation alone does not establish subscription access or successful inference.

The supervisor configuration is JSON:

```json
{
  "prefix": "/absolute/path/to/enginebox",
  "fleet_config": "/absolute/path/to/enginebox/etc/fleet.toml",
  "token_file": "/absolute/path/to/enginebox/etc/api-token",
  "environment_file": "/absolute/path/to/enginebox/etc/environment.json",
  "listen": "127.0.0.1:8765",
  "dashboard_listen": "127.0.0.1:3000",
  "drain_timeout_seconds": 300
}
```

Omit `dashboard_listen` to run only the native API. The environment file is parsed as JSON and never sourced as shell code. The service launches a standby controller from the active release, obtains exclusive state ownership, then serves traffic through the stable front door.

```sh
PREFIX/releases/0.3.0/bin/enginebox-supervisor --config PREFIX/etc/supervisor.json
```

For a persistent per-user service, inspect and then apply the service plan:

```sh
PREFIX/releases/0.3.0/bin/enginebox-install --prefix PREFIX service --config PREFIX/etc/supervisor.json
PREFIX/releases/0.3.0/bin/enginebox-install --prefix PREFIX service --config PREFIX/etc/supervisor.json --apply
```

Use real absolute paths in these examples. The service manager is systemd user units on Linux, a LaunchAgent on macOS, or a least-privilege Task Scheduler task on Windows. User services normally follow user login/session availability; dedicated Linux servers may need an administrator to enable user lingering, while Windows/macOS logout and sleep are not always-on server guarantees. Existing service definitions are never overwritten implicitly. Supervisor upgrades are separate planned maintenance; routine controller/dashboard updates use the existing supervisor.

## Install a dedicated Linux node

Install nodes into a root-owned prefix because the controller invokes the node helper through `sudo`. Do not expose binaries from a user-writable directory as privileged helpers.

```sh
sudo sh install.sh --version 0.3.0 --role incus-host --prefix /opt/enginebox-node --apply --install-dependencies
sudo /opt/enginebox-node/releases/0.3.0/bin/enginebox-install --prefix /opt/enginebox-node expose-host
sudo /opt/enginebox-node/releases/0.3.0/bin/enginebox-install --prefix /opt/enginebox-node expose-host --apply
```

This creates only an installer-owned `/usr/local/bin/enginebox-host` symlink after checking root ownership and non-writable ancestors. It refuses unrelated executables and links owned by another prefix. It does not grant sudo permissions, add users to Incus administrator groups, or modify an existing SSH policy.

Use `enginebox-host bootstrap init`, `doctor`, `plan` and `apply` to inspect and provision owned Incus resources from the host configuration. The Linux host's release supplies its Linux guest binaries; a Mac or Windows controller's executables are never uploaded into a Linux guest. Fleet credentials, host inventories, customer repositories and private checkpoints are not included in release archives.

## Build an archive from source

The native `enginebox-release` builder accepts already-built binaries, license notices and an optional standalone dashboard. Run workspace checks before building a distribution. This builder does not publish releases or assert hardware qualification.

```sh
cargo run -p enginebox-install --bin enginebox-release -- \
  --version 0.3.0 --target aarch64-apple-darwin \
  --bin-dir target/release --license LICENSE --notice NOTICE \
  --dashboard packages/dashboard/dist/standalone \
  --dashboard-start packages/dashboard/scripts/start.mjs \
  --output .state/releases/0.3.0/aarch64-apple-darwin
```

It emits `enginebox-VERSION-TARGET.tar.gz`, the matching JSON manifest and target-specific `.sha256` receipt. Required service, node, guest, CLI and MCP binaries are allowlisted; use repeated `--binary NAME` for additional tools. Dashboard dependencies must be materialized inside the standalone artifact: escaping or remaining symlinks are rejected. The output directory must not already exist.
