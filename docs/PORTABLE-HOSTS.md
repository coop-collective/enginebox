# Portable resource hosts and installation gates

Updated October 19, 2026. **The working workspace backend is Linux Incus VMs.**
macOS and Windows resource contribution, Docker workspace lifecycle and GPU
placement are design work, not enabled backends. A Docker image alone would not
make the Incus runtime portable.

## Available now

The runtime package includes `enginebox-host`, a read-only inventory command:

```sh
enginebox-host --json
enginebox-host --docker --cpus 2 --memory-gib 3 --disk-gib 30 --json
```

From a source checkout, the same command is available without installation:

```sh
python3 runtime/host_capabilities.py --docker --json
```

It reports OS/architecture, logical CPUs, physical memory, free space on an
existing selected filesystem, visible GPUs, and optional local Docker engine
capacity. `--storage-path` selects that filesystem; its path is not included in
the report. It does not read provider homes, Docker login configuration or account
tokens. Docker inspection is opt-in; remote contexts are refused so another
server's capacity cannot be mistaken for laptop resources. Read-only engine
queries can wake Docker Desktop from its own resource-saving state.

Allocation suggestions preserve at least half the observed CPU, memory and free
disk, and at least two logical CPUs, 4 GiB RAM and 20 GiB free disk for the owner.
Explicit requests above that budget fail with exit status 2. Docker gets a
separate budget bounded by both the host and Docker VM totals. These are
conservative planning budgets, **not reservations or enforced limits**. Current
workload usage, Docker volume capacity, template minima and remaining fleet
commitments still need admission checks before provisioning. GPU visibility
always reports `schedulable: false` until a separate workload qualification exists.

The existing Linux fleet backend also supports **enforced admission ceilings**
in each private `[[hosts]]` configuration, separate from these inventory hints:

```toml
contribution_cpu = 8
contribution_memory_gib = 16
```

Both fields are optional together; omit both to retain the existing capacity
policy. CPU must be an integer from 1–4096 and memory from 1–16384 GiB. These
ceilings limit the aggregate configured allocations of active Engine Box-owned
VMs across projects, including frozen warm reserves and owned template preparation
VMs. Only a confirmed stopped VM releases its CPU/RAM budget. Unrelated Incus
workloads consume physical capacity but not the chosen contribution allowance.
Physical host reserves, observed available RAM, disk commitments and instance
limits remain independent constraints; larger contribution values never enable
oversubscription.

The controller checks the same ceilings before creating workspaces, resuming
stopped workspaces, refilling warm pools and preparing templates. Claiming an
already-frozen warm workspace reuses its accounted allocation. Lowering a cap
blocks additional work without stopping existing VMs. This is fleet admission
plus per-VM limits, not a new host-wide cgroup or a restriction on an administrator
running Incus directly. Configuration changes currently require a controller
restart; no existing private host configuration is modified automatically.

The fleet capacity response exposes `contribution_cpu`,
`contribution_memory_bytes`, `contribution_allocated_cpu` and
`contribution_allocated_memory_bytes`; absent ceilings are `null`.
`cpu_available`, `memory_budget_available_bytes` and `max_additional` already
reflect both contribution and physical capacity constraints.

| Platform or feature | Current implementation | Validation status |
|---|---|---|
| Linux x86_64, Incus/KVM | Existing bootstrap, fleet, templates, native agents, VM lifecycle | Reference host live-tested; fresh independent host installation remains a release gate |
| macOS arm64 operator | Python runtime and portable inventory | Clean wheel installation/imports/CLI checks on Python 3.14.6; inventory on actual Apple Silicon Mac; local Docker Linux/aarch64 totals correctly kept separate |
| Docker Desktop workspace node | Proposed second workspace backend | Inventory of local Docker Engine 29.3.0 tested; no workspace launch/enrollment implementation |
| Windows host inventory | PowerShell CIM CPU/memory/GPU inspection | Parser/error-path fixtures only; no physical Windows validation yet |
| Windows native workspace/GPU service | Proposed separate backend | No implementation or Windows image qualification |
| macOS native workspace/GPU service | Proposed separate backend | No implementation or macOS image qualification |
| Linux/Windows GPU execution | Capability design only | No device assignment, exclusivity, driver or real workload acceptance |
| Host installer packages | Existing Linux installer; test-gated Python wheel/sdist build | Same wheel installed in clean macOS arm64 and Linux x86_64 environments; machine installation remains a separate gate |

## Build packages only after checks pass

Use a Python 3.11+ interpreter and an installed `uv`:

```sh
python3 scripts/build-tested-package.py --out-dir .state/packages/review-build
```

The destination must not exist. The script runs the locked runtime offline suite
and installer suite, builds a source distribution and wheel, checks declared
modules and license notices, installs hashed locked dependencies and the wheel
into a fresh temporary virtual environment, exercises CLI entrypoints and imports
every declared module. It runs the smoke checks outside the source tree with
isolated imports. It rejects package-source changes during the run. Only then
does it copy artifacts and a `validation.json` receipt to the requested location.
The receipt identifies the tested platform, package source fingerprint and
artifact hashes; it explicitly records that host installation was not tested.

These are runtime/operator artifacts. They do not bundle the dashboard, an Incus
daemon, a Docker daemon, provider binaries or a Windows/macOS resource service.
The script neither publishes packages nor installs system services. Use the
existing [bootstrap](BOOTSTRAP.md) and [dashboard](DASHBOARD.md) procedures for
the current Linux host/control plane. Installing the wheel is not evidence that
a laptop can join the fleet as a resource host.

For a locally reviewed wheel, installation into an isolated operator environment
is conventional:

```sh
uv venv .state/operator
uv pip install --python .state/operator/bin/python --require-hashes -r runtime/requirements.lock
uv pip install --python .state/operator/bin/python --no-deps .state/packages/review-build/enginebox_runtime-VERSION-py3-none-any.whl
.state/operator/bin/enginebox-host --json
```

Replace `VERSION` with the artifact's actual version. Windows uses
`.state/operator/Scripts/python.exe` and `Scripts/enginebox-host.exe`; that route
still needs a real Windows installation test before it is offered as supported.
The existing Linux `ebox doctor`, `plan` and `apply` process remains the gate for
host changes; a successful Python install does not bypass it.

## Backend design

The controller should negotiate explicit backend capabilities, not infer them
from the operating system or a green Docker check. A host advertises execution
OS/architecture, admitted CPU/RAM/storage, devices, lifecycle semantics and
validated templates. A template declares compatible architectures and required
capabilities. The controller can continue distributing desired template state
to every enrolled node while reporting incompatible nodes as blocked, instead
of pretending every image can run everywhere.

| Backend | Isolation and intended workload | Persistence and lifecycle |
|---|---|---|
| `incus_linux_vm` | Existing private Linux VMs on qualified KVM hosts | Existing disk snapshots; RAM restore only for tested compatible devices |
| `docker_workspace` (planned) | Linux coding/build workloads inside the Docker Desktop Linux VM or a qualified Linux container host | Private persistent tenant/workspace volumes; stop/start and provider session resume; no claim of RAM hibernation |
| `native_windows` (planned) | Native Windows Unity/Unreal tools and Windows GPU drivers | Dedicated Windows account/workspace, Job Object process limits, explicit tenant authority; native processes or separately qualified Hyper-V VM |
| `native_macos` (planned) | Native Apple toolchains and Metal-dependent workloads | Dedicated macOS account/workspace or separately qualified Virtualization.framework guest; explicit tenant authority |

Implement the second backend as a small execution/lifecycle adapter after its
first disposable workload succeeds; do not build a generic plugin framework
first. The current runtime's hardcoded Incus operations must be moved behind
that adapter before Docker admission is enabled. Capabilities must distinguish
process pause, disk restart, conversation resume and RAM restore.

A small local resource service should own the Docker daemon connection or native
platform operations. Agents receive only their authorized workspace and tenant
state. The service should initiate an authenticated outbound connection to the
controller, use revocable node identity and reject commands for a different
owner. A laptop sleep/disconnect drains scheduling and leaves durable state;
reconnection must not launch a duplicate session. Enrollment, mutual node
authentication and this service are not implemented yet.

The intended enrollment flow is local and opt-in: inspect the computer, show its
available resources, let the owner choose a CPU and memory allowance, then join
their controller. Store that allowance on the contributing computer as an upper
bound; the controller may request less but cannot raise it remotely. The local
service must enforce the aggregate budget across all its workspaces as well as
each workspace's limits. Show contributed versus allocated resources in the
dashboard. Provide a local pause-contribution control that stops new assignments
and drains active work, plus a separate explicit stop action. An offline or
sleeping laptop is unavailable capacity, not a failed dedicated server. GPUs and
native engine execution are separate opt-ins after qualification.

## Docker and personal-machine isolation

Do not use privileged Docker-in-Docker or bind the host Docker socket into an
agent container as the default installation. The Docker API can create host
mounts and therefore grants substantial host authority. Keep that authority in
the trusted local resource service with an allowlisted provisioning interface.
On Linux, consider a qualified rootless daemon; on macOS/Windows, Docker Desktop
adds a Linux VM boundary. This is a design choice for personal trusted-owner
workloads, not a claim of hostile-tenant isolation. [Docker Engine security](https://docs.docker.com/engine/security/),
[rootless mode](https://docs.docker.com/engine/security/rootless/),
[Docker Desktop Mac permissions](https://docs.docker.com/desktop/setup/install/mac-permission-requirements/)

Planned workspace defaults: non-root agent user, no privileged mode, no daemon
socket, no host PID/network namespaces, dropped capabilities, no new privileges,
bounded process/CPU/RAM/disk use, and only explicitly authorized volumes. Keep
private agent state on a tenant volume, project writes on a workspace volume,
and compatible package downloads in trust-separated caches. Do not mount the
laptop's entire home directory. Building containers should use a separately
qualified build service inside the same owner boundary rather than exposing
the personal Docker daemon to arbitrary prompts.

Docker Desktop CPU/RAM limits and WSL limits are separate from per-workspace
quotas. WSL `.wslconfig` applies across distributions; an installer must show the
impact and must not silently change it. Docker Desktop resource-saving wakeup
can also add latency. [Docker resource settings](https://docs.docker.com/desktop/settings-and-maintenance/settings/),
[Microsoft WSL configuration](https://learn.microsoft.com/en-us/windows/wsl/wsl-config)

Enhanced Container Isolation is a Docker Business feature, not a universally
available baseline. Docker documents additional WSL shared-kernel limitations
and stronger separation with Hyper-V; the GPU path may require a different
backend choice. Do not advertise one configuration as both universally secure
and universally GPU-compatible. [Docker ECI limitations](https://docs.docker.com/desktop/enterprise/hardened-desktop/enhanced-container-isolation/limitations/)

## GPU and operating-system image constraints

Docker documents `--gpus` passthrough for Linux containers on Windows through
WSL 2 with supported NVIDIA hardware/drivers. This does not make a Linux
container a native Windows Unity/Unreal editor or provide general Metal
passthrough on a Mac. [Docker Desktop GPU support](https://docs.docker.com/desktop/features/gpu/)

The proposed fleet GPU contract records vendor, model, API family
(CUDA/DirectX/Metal/Vulkan), memory when reliable, driver/runtime versions,
exclusive/shared mode, stable device identity and tested workload recipe.
Initially lease an entire device exclusively. Never schedule from device count
alone; never infer VRAM from system RAM, and do not promise GPU RAM snapshots.
Run a real workload before marking a device schedulable. Incus provides
physical, mediated and SR-IOV GPU device types, but device compatibility and
memory-state restore need their own qualification. [Incus GPU devices](https://linuxcontainers.org/incus/docs/main/reference/devices_gpu/)

Windows and macOS images require their own drivers, setup recipes, engine
entitlements and operating-system licensing review. A macOS VM is a separate
Apple-host virtualization path, not a Docker Linux image. Apple's supported
Virtualization.framework APIs are the starting point for Apple Silicon guests;
we have not tested a macOS guest or GPU workload. [Apple macOS virtualization](https://developer.apple.com/documentation/virtualization/running-macos-in-a-virtual-machine-on-apple-silicon)

## Required implementation and release gates

1. **Runtime package:** build from frozen source, verify notices and contents,
   install into clean environments on actual Linux and macOS, and record exact
   artifact hashes. Repeat on real Windows before claiming Windows operator
   installation. Package publication is a separate explicit release action.
2. **Docker workspace proof:** create one disposable Linux workspace on a local
   Docker context without a socket mount or privileged mode; run all selected
   native clients, browser MCP, tenant state restart/resume and resource-limit
   checks. Test macOS arm64 and Windows WSL 2 separately. Verify unrelated
   containers/files are untouched and tenant A cannot open tenant B's volumes.
3. **Backend integration:** add capability negotiation, template compatibility,
   admission, workspace creation, deletion and stop/start through the fleet API.
   Add per-device leases only after GPU execution tests pass. Never translate a
   VM `hibernate` request into container stop without exposing that difference.
4. **Resource service:** authenticated enrollment, reconnect/duplicate-job
   protection, owner-approved resource budgets, draining, sleep/reboot recovery,
   upgrades and uninstall preserving private data. Test on independent machines.
5. **Installer packages:** sign/version the tested Linux service package and
   later macOS/Windows packages; show a preflight/plan before changes. Install,
   reinstall, upgrade, rollback and uninstall on clean machines, with a second
   unrelated workload present. No one-click resource-host promise before this.
6. **Native engine hosts:** qualify Windows native/Hyper-V and macOS native/VM
   execution separately, then test a real Unity/Unreal build or editor operation,
   license handling, GPU lease release, cancellation and crash recovery. Native
   user accounts are not equivalent to hostile-tenant VM isolation.
