# Windows WSL template nodes

The Windows setup runs the Rust node inside a dedicated **Enginebox WSL2**
distribution. Windows needs WSL and the small installer/startup task; Docker,
LayerFS, Node's SDK helper and development tools live in Linux. The preinstalled
`native-dev` sandbox image includes Node 24, pnpm, Git, Python, Codex and Claude.
No provider account is signed in or included in the reusable image.

## Install and register

Open **Worker machines** in the dashboard and create an install command. Paste
that one line into Windows PowerShell. The release installer and Linux image are
SHA-256 pinned. The same installer can run without a code: it defaults to
`https://enginebox.coopcollective.dev` and asks for a registration code after setup.

The installer asks for:

- Controller HTTPS origin, with the supplied/default host prefilled.
- CPU allowance: up to 4 cores or half the host by default, minimum 1.
- Memory allowance: up to 8 GiB or half the host by default, minimum 4 GiB.
- Linux virtual disk maximum: up to 50 GiB or half the available disk by default,
  minimum 20 GiB. This includes Linux, templates and workspace storage.

Resources are selected on the PC, not in the dashboard. A registration code
expires after one hour and authorizes exactly one node; that node registers its chosen resource
allowance. Older API clients may supply an explicit owner ceiling.

The installer updates WSL to at least 2.5.7 when needed. Enabling WSL can trigger
Windows UAC and require a reboot. It saves non-secret setup state and schedules
resumption under the original Windows user. It does not force a reboot. A code
that expires while the image downloads must be replaced with a fresh code from
the dashboard; there is a separate registration-code field for an installer that
is already open. The installed distribution is retained.

It refuses an unrelated distribution named Enginebox, unrelated scheduled tasks,
symlink/junction installation paths and changes to an existing identity's
controller or budget. Existing Ubuntu distributions, Docker Desktop, global
`.wslconfig`, default WSL distribution and host development tools are preserved.
Only the new distribution disables Windows drive automounting and Win32 interop.

## Resource and access boundaries

The distribution is installed with its own `--vhd-size` maximum. A cgroup v2
`enginebox.slice` caps aggregate CPU and memory across the trusted worker,
private Docker daemon, SDK helpers and workspace containers. Existing global WSL
limits still constrain it; setup rejects a selected allowance larger than the
Linux VM can supply. It does not change those global settings automatically.

The worker's Docker daemon has a dedicated socket and data root. Workspace
commands execute through the pinned LayerFS backend as its unprivileged user.
They receive their own project mount and container-local provider home, without
Windows drive mounts, node registration state, the backing Store or the Docker
socket. Provider authentication belongs in the authorized workspace. Closing or
replacing a sandbox does not preserve its container-local provider home; durable
provider delegation for this backend still needs integration. No host provider
home is imported or shared between jobs.

The Windows owner/administrator still controls the machine. WSL distributions
share a Linux kernel, and this is not qualification for mutually hostile tenants.
Native Windows/macOS toolchains and native GPU rendering are separate backends;
a Linux template does not make those workloads compatible.

## Readiness and execution

Before reporting template readiness, setup runs a real LayerFS proof: Linux
filesystem and artifact checks, two independent forks, unprivileged commands,
refusal of host credentials/Store/socket access, independent provider homes, and
checkpoint/remount content verification. Provider CLI version checks are included;
no live model inference is claimed by those checks.

The initial template has a credential-free `welcome` project and supports two
workspace containers within the aggregate allowance. The owner API queues:

```json
{
  "request_id": "unique-operation-id",
  "kind": "workspace",
  "request": {
    "operation": "fork",
    "project": "welcome",
    "workspace": "example"
  }
}
```

Send this to `POST /v1/contributors/{node}/runs` using owner authentication.
`exec`, `checkpoint`, `remount` and `close` operate on the named workspace. `exec`
requires an argument array and may specify `timeout_seconds` (1–3600):

```json
{
  "request_id": "another-unique-operation-id",
  "kind": "workspace",
  "request": {
    "operation": "exec",
    "workspace": "example",
    "argv": ["bash", "-c", "node --version && codex --version && claude --version"],
    "timeout_seconds": 60
  }
}
```

The service binding and node credential remain scoped; the node polls outbound.
Lost leases and revocation stop its dedicated Docker runtime, including detached
workspace processes. Interrupted operations are retained for reconciliation and
never replayed automatically. The legacy direct-host `agent` and `ci` requests
are not accepted by the Linux executor. CI commands can execute inside an
explicit workspace; the older Windows shell automation format is not silently
translated into Linux shell commands.

Additional project imports use the existing reviewed LayerFS import workflow
inside the trusted distro. Automatic migration of existing controller projects,
remote template catalog synchronization and provider delegation are not supplied
by successful machine registration. Do not advertise those paths as qualified
until they have their own end-to-end proof.

## Build and test

`packages/enginebox-install/wsl/build.sh OUTPUT` builds the default template,
compiles the pinned LayerFS SDK/daemon and Rust executables, and exports a
credential-free rootfs. `package.py OUTPUT TAG` produces the versioned installer
and hashes. The Windows WSL workflow parses/tests the installer with Windows
PowerShell 5.1 and attempts a real WSL import plus LayerFS proof. A runner that
cannot enable virtualization/reboot fails the qualification step; it is not a
passing WSL test.

The Windows startup task runs under the installing user at sign-in; it is not a
SYSTEM service that starts before anyone signs in. WSL runtime services run
inside the dedicated distribution.

The `Publish qualified WSL installer preview` workflow accepts a successful
qualification run. It verifies that runtime sources are identical, tests the
current installer on Windows, and publishes an immutable preview with hashes
and separate image/installer provenance.

The installer currently targets Windows x64. Do not replace the deployed
installer pin until its artifact build and applicable runtime checks pass.

References: [WSL commands](https://learn.microsoft.com/en-us/windows/wsl/basic-commands),
[per-distribution versus global configuration](https://learn.microsoft.com/en-us/windows/wsl/wsl-config),
[WSL command flags](https://github.com/microsoft/WSL/blob/master/src/windows/inc/wsl.h),
[LayerFS platform contract](https://layerfs.ai/docs/platform/).
