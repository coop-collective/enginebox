# Portable operators and optional resource hosts

Updated October 19, 2026. Engine Box's runtime and administrative tools are Rust binaries; the dashboard remains TypeScript/Vinext. The implemented workspace backend is Linux Incus VMs. A native Windows or macOS binary does not, by itself, make those machines compute nodes.

## Installation and current platform boundary

Use the version-pinned terminal installers in [INSTALL.md](INSTALL.md). Each release contains native controller, operator, MCP, node and guest tools for its target architecture. Python, a local Rust compiler and a Node package manager are not required to run those binaries. The optional dashboard needs Node.js 22.12+. Rust/Cargo and the root pnpm workspace are development/build tools.

| Capability | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Native operator CLI and fleet MCP | Implemented | Implemented | Implemented; Windows execution is a release qualification gate |
| Native controller/API and update supervisor | Implemented | Actual two-version process update/rollback qualified on Apple Silicon | Implemented; installation/service qualification remains required, and native provider sign-in is unsupported until qualified |
| Dashboard | Vinext/Node | Actual production Node dashboard used in update/cookie/asset qualification | Requires Node; Windows service execution remains a gate |
| Controller native-client installer | Official pinned CLI install workflow | Official pinned CLI install workflow | Reports manual official installation guidance; managed installer is not qualified |
| CPU/RAM/disk inventory | Native/system observations | Native/system observations | PowerShell/CIM observations, separately qualified on Windows |
| Linux Incus/KVM workspace provisioning | Current x86_64 host contract | Controls a separate Linux host over SSH | Controls a separate Linux host over SSH |
| Docker Desktop workspace contribution | Not implemented as a scheduling backend | Planned | Planned |
| Native Unity/Unreal execution, GPU assignment | Requires a separate qualified backend | Planned | Planned |

The Rust updater's actual macOS process test used a disposable private installation and an isolated second controller version. It did not enroll a laptop, run a Windows VM, assign GPUs or modify a live fleet. The v0.2 Python reference-host validation remains historical evidence for that implementation; it is not a substitute for fresh Rust host qualification. Read [UPDATES.md](UPDATES.md) for precise update and migration boundaries.

## Inspect a computer without enrolling it

```sh
enginebox-host --json
enginebox-host --docker --cpus 2 --memory-gib 3 --disk-gib 30 --json
# Equivalent when the complete release is installed together:
ebox host --json
```

`--storage-path PATH` selects the filesystem whose free space is observed. Inventory reports OS/architecture, logical CPUs, physical memory, disk space, visible GPUs and optional local Docker engine capacity. It does not read provider credentials, SSH keys or Docker login configuration. Docker inspection is opt-in, rejects remote contexts and may wake a local Docker Desktop engine.

Suggested allocation preserves at least half the observed CPU, RAM and free storage, and at least two logical CPUs, 4 GiB RAM and 20 GiB free disk for the computer's owner. An explicit request above that budget is rejected. Docker capacity is bounded separately by host and VM totals. These are planning observations, not resource reservations. A GPU name means visibility only; scheduling, device assignment, drivers and real workloads must be qualified separately.

## Enforced contribution ceilings on existing Linux nodes

The current Incus backend accepts paired limits in each private controller `[[hosts]]` entry:

```toml
contribution_cpu = 8
contribution_memory_gib = 16
```

Both are optional together. CPU must be an integer from 1–4096 and memory an integer from 1–16384 GiB. They cap aggregate configured allocations of active Engine Box-owned VMs, including frozen warm reserves and template preparation. Only confirmed stopped VMs release CPU/RAM admission capacity. Unrelated workloads still reduce physical capacity; contribution limits do not authorize oversubscription.

The same admission checks run before create, resume, warm-pool refill and template preparation. Claiming a frozen reserve reuses its accounted allocation. Lowering a cap blocks new work without stopping current workspaces. This is fleet admission plus per-VM limits, not a new host-wide cgroup or a restriction on an administrator using Incus directly. Controller configuration changes currently require loading a replacement controller; use the versioned service workflow only where its state/ownership contract applies.

The capacity API reports `contribution_cpu`, `contribution_memory_bytes`, `contribution_allocated_cpu` and `contribution_allocated_memory_bytes`; absent ceilings are `null`. Available capacity already reflects contribution and physical reserve constraints.

## Optional personal-computer contribution design

These capabilities remain design work:

- A trusted local resource service owns provisioning authority and joins an owner's fleet using a revocable machine identity. It advertises execution OS/architecture, allowed CPU/RAM/disk, device capabilities and compatible templates.
- The owner sets a local maximum contribution. The controller may request less and cannot remotely raise that ceiling. Enforce aggregate and per-workspace limits locally.
- Pausing contribution stops new assignments and drains active work. Laptop sleep/disconnect removes schedulable capacity; reconnect must reconcile existing sessions instead of launching duplicates.
- Docker on macOS/Windows supplies Linux workloads within Docker Desktop's VM. It does not supply native Apple/Windows toolchains or RAM-state restore merely by persisting a volume.
- Native Windows/macOS Unity, Unreal, Apple toolchain and GPU work needs a separate execution/isolation adapter and workload qualification. Expose capabilities explicitly so incompatible templates remain visibly blocked.

Do not use privileged Docker-in-Docker or mount the host Docker socket into agent workspaces as the default. The daemon connection belongs to the trusted provisioning service. Prefer ordinary non-root workspace users, bounded processes/CPU/RAM/storage and only authorized volumes. Each tenant's provider home, skills, memory, session logs and scratch files need a private persistence boundary; package/artifact caches require separate ownership and poison-resistance rules.

Before enabling enrollment, qualify a disposable workload end to end: install, inspect, apply the owner's limits, enroll, launch/resume a native harness, disconnect/reconnect without duplication, drain and uninstall owned resources. Add native GPU/image qualification only after the basic backend works. Until then, the installers expose operator/controller capabilities on personal computers and Linux Incus as the functioning compute backend.

## Release checks

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Release checks must also install the exact built artifact on each advertised operating system and execute its supported commands. The Rust release builder checks artifact shape and integrity; a successful archive build is not evidence that a Windows service, a new Linux machine or a GPU workload works. No package is automatically published by the installer or the build tool.
