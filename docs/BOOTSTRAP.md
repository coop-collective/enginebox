# Bootstrap a native-agent workspace

The bootstrap creates one Ubuntu VM with official Codex, Cursor Agent and Claude Code clients. It uses a versioned TOML configuration, read-only inspection, ownership checks and a resumable apply. It does not sign in to providers or register CI runners.

The server needs Linux x86_64, usable `/dev/kvm`, the native `enginebox-host` release, SSH and noninteractive administrative access. The operator uses the native `ebox`/`enginebox-host` binaries and OpenSSH; Python is not required. Existing Incus deployments must provide Incus 6.0+, its stateful-migration capability and QEMU 8.2+. Automatic host package installation is explicitly limited to Ubuntu 24.04/26.04 repository packages. The guest is Ubuntu 24.04; other guest images are rejected before mutation. These are enforced target bounds, not a claim that every host/kernel/package combination has been tested.

## Configure, inspect and apply

First install a platform release on the operator and the Linux target using [INSTALL.md](INSTALL.md). On a dedicated target, expose the root-owned node helper through the installer's `expose-host` command. The SSH transport invokes `sudo -n enginebox-host`; the installer does not grant this administrative permission implicitly. Then run:

```sh
ebox init --output enginebox.toml
# Edit the SSH target, resource names and sizes before continuing.
ebox doctor --config enginebox.toml
ebox plan --config enginebox.toml
ebox apply --config enginebox.toml
```

`init` only creates a local file and refuses to overwrite one. `doctor` and `plan` inspect the host without writing files or changing services. They return JSON, with a failing exit status when blockers exist. `apply` rechecks the live host under an exclusive lock before changes. Repeating the same configuration converges from verified resources; a completed workspace is a no-op.

The [fresh-server example](../examples/fresh-server.toml) opts into package installation, a bounded Btrfs loop pool and a private NAT bridge. The [existing-server example](../examples/enginebox.example.toml) explicitly references an existing pool and managed bridge. Select a **new project**; an unrelated project, profile, pool or VM is never adopted just because its name matches.

For direct operation on the server, set `host.transport = "local"`. For SSH, `host.target` is an SSH configuration alias or `user@hostname`; existing SSH host-key checking and identities apply. Bootstrap never copies the operator's SSH keys into a guest.

## What apply changes

1. If Incus is absent and `host.install_packages=true`, install Incus/QEMU packages from the server's configured Ubuntu repositories. Check package candidates first; do not add repositories. Existing working Incus installations are reused.
2. Create only requested, missing owned resources: optional loop pool, optional managed bridge, private project/profile and VM.
3. Copy qualified Linux native helpers into the guest's `/usr/local/bin` and run the Rust `enginebox-host prepare-guest` entrypoint as guest root. It creates the configured ordinary user, installs selected official clients and prepares optional browser/desktop tools. The target's installed release supplies its guest binaries; Mac/Windows operator binaries are not copied into Linux guests.
4. Stop the new VM cleanly, capture the prepared disk snapshot and start it again. The response names the workspace, snapshot and environment variables for ordinary runtime access.

The native clients run directly as the guest user. With the controller installed, connect providers once in **Agents** and assigned workspaces receive native access automatically. A standalone bootstrap without a controller still uses each client's normal native sign-in in the private guest. Successful provisioning proves installation and preparation; it does not prove subscription inference or quota behavior.

The guest installer records resolved versions in `/opt/enginebox-runtime/installed-versions.json`. Core preparation defaults are maintained in the guest recipe; changed code is not silently reapplied to a completed workspace. Create a new named workspace/profile for changed resource requirements, and perform upgrades explicitly.

## Resource and ownership rules

Host inspection checks OS/architecture, KVM, daemon capabilities, logical CPUs, available memory, storage capacity, configured names and subnet overlaps. Saved memory gets its own `workspace.state_disk`; the configured bound must exceed guest RAM by at least 1 GiB. Pool capacity must cover the guest disk, saved state and disk reserve. Loop-pool checks use the Incus data filesystem, not merely the journal filesystem.

An `existing` pool or network is referenced without editing it. Existing networks currently must be Incus-managed bridges with IPv4/DHCP. Existing pools support Btrfs, ZFS, LVM and directory storage; a directory pool does not provide efficient copy-on-write forks. Development loops trade performance for convenient non-destructive setup. Bootstrap never repartitions or formats a host disk.

Owned resources carry `user.enginebox.owner`. The instance also records a configuration fingerprint and preparation completion. Drift, extra VM devices, unowned name collisions and profile changes block bootstrap rather than overwriting the user's work. The per-host lock prevents concurrent bootstrap operations; it does not lock out an administrator editing Incus independently.

No global NFS, UFW, nftables or Docker configuration is replaced. Existing rules must allow DHCP, DNS and guest egress on the selected bridge. A restrictive existing firewall can therefore block readiness/downloads; bootstrap reports the failure instead of opening unrelated host access. Shared NFS mounts and cache services are not configured by this bootstrap version; unsupported configuration sections are rejected.

## Recovery and snapshots

The default root-owned journal is `/var/lib/enginebox/bootstrap/<workspace>.json`. It contains operation status and bounded errors. A failed apply retains newly created resources for diagnosis and can be rerun with the same config. Live inventory is authoritative; the journal is not trusted as proof a command completed. There is no automatic destructive rollback or deletion command.

The reusable disk snapshot is named `prepared-<configuration-fingerprint-prefix>`. It is captured before provider/Git login and marked `user.enginebox.clean-base=true`. That marker is removed from the running source immediately after capture so future private snapshots do not inherit a clean-base claim. A final guard refuses known provider/Git/cloud credential files and nonempty browser profiles; it is an additional check, not a universal secret scanner. Only a newly bootstrap-owned VM is eligible. Never sign into an incomplete workspace while bootstrap is running or before recovery finishes. Provider credentials in private snapshots must remain within the owner's access boundary.

Pause retains RAM; hibernation writes memory to storage; a disk fork boots a fresh process tree. This installer configures stateful operation, but workload-specific suspend/restore and backups still require acceptance checks. Same-pool snapshots are not off-host backups, and external mounts are not captured by an instance snapshot.

## Module flags

| Flag | Meaning |
|---|---|
| `agents.install` | Select any subset of `codex`, `cursor`, `claude`; no proxy or custom harness. |
| `modules.browser` | Install Chromium and the pinned Playwright MCP recipe. Browser use works without a desktop. |
| `modules.desktop` | Install the optional virtual display and desktop services. |
| `modules.guest_computer_mcp` | Register guest computer tools; requires the desktop. |
| `modules.operator_mcp` | Records intended operator integration; never creates a public host API or copies operator authority into guests. |
| `modules.ci` | Must remain false in this version; runner integration is a separate module. |

Browser or computer-tool selection enables the required guest MCP runtime. Optional tools retain each native client's ordinary approval policy. Desktop services bind guest loopback; use authenticated access supported by the runtime rather than exposing an unauthenticated port.

## Offline verification

```sh
cargo test -p enginebox-core bootstrap
cargo test -p enginebox-core --test transport_contract
```

The Rust tests exercise malformed configuration, ownership/admission checks and transport behavior using controlled fixtures. They do not prove that a particular real host supports memory restore or that a provider subscription can complete inference. Live validation must use a newly named disposable workspace; never use a signed-in or actively edited workspace as a restoration fixture. Current Rust controller update qualification is recorded separately in [UPDATES.md](UPDATES.md); the first Python-to-Rust control-plane migration is not a rolling-update proof.
