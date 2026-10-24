# CI and non-agent automation runners

The optional `enginebox-ci` package adds shell automations, GitHub Actions dispatch,
native GitHub runner registration, and a dedicated build-node registry. The core
runtime does not depend on this package. There is no Beanstalk service dependency.
Engine Box's merge/LayerFS work can call the same trigger API as an operator.
This change supplies that boundary; it does not implement a merge engine or a
LayerFS merge-event listener.

This native CI module is not yet an adapter for the Cloudflare project's
protected checks. Shell/GitHub runner results cannot satisfy those checks or
publish protected build artifacts. The qualified fleet integration and engine
build path is described in [INTEGRATION-CI.md](INTEGRATION-CI.md).

## Execution and resource decision

**Use dedicated CI nodes by default.** Builds must not compete with interactive
agent workspaces. CI inventory is separate from the Incus agent-host inventory.
Registration and placement reject an agent host's SSH hostname, even with a
different SSH username; a later fleet configuration change also fences that CI
node. Use canonical hostnames: DNS aliases/IP aliases and VMs sharing physical
hardware are not detected by string comparison. Provision these workers on
separate capacity, rather than relying on labels to isolate resources.

One automation executes at a time per CI target. A node-local file lock also
excludes a concurrent Engine Box-managed GitHub runner. Jobs on different nodes
can run concurrently. CI has its own four execution slots and 32 pending-job
limit, separate from workspace lifecycle operations, so a full CI queue does not
block agent create/resume admission. Registering two SSH usernames for one hostname does not add capacity.
Template installation checks available CPU, RAM (including Linux cgroup memory)
and disk before changing a worker. This is a point-in-time admission check, not
an enforced CPU/RAM budget, disk quota or hypervisor reservation.
Shared agent/CI hosts require a future implementation of enforceable reservations
and workload isolation; they are not an opt-in switch in this slice.

## Prepare and register a worker

Each worker is an already provisioned Linux, macOS or Windows OS with `ebox`, Git,
and Bash or PowerShell 7.3+. Supply its build toolchain manually or enable a
managed toolchain template as described below. A worker may itself be a VM.
The `image` field identifies the installed recipe/revision. It does not provision
or emulate an OS. macOS jobs require macOS workers and Windows jobs require
Windows workers; the executable rejects an OS or architecture mismatch.

Use the manifests under [examples/ci/nodes](../examples/ci/nodes), edit their SSH
targets and image/toolchain labels, and register through the controller:

```sh
ebox ci inspect                     # run on the worker to report native OS/arch
ebox ci register --manifest mac-worker.json
ebox ci nodes
```

The operator CLI uses the existing `ENGINEBOX_API_URL` and
`ENGINEBOX_API_TOKEN_FILE`/`ENGINEBOX_API_TOKEN`. The worker's noninteractive SSH
environment needs `ebox` on PATH and an absolute `ENGINEBOX_CI_ROOT`, owned by its
dedicated build user. On Unix this directory must have mode 0700. On Windows use
a private directory whose ACL restricts it to the build account and administrators.
Use pre-approved SSH host keys; the controller never accepts unknown keys.
The special `target: "local"` is for disposable local verification and is also
rejected when the fleet already uses a local agent host.

`runs-on` matches **all** labels, case-insensitively: `self-hosted`, `enginebox`,
OS, architecture, `image:<revision>` and explicit capabilities such as `xcode`,
`android-sdk`, `jdk-17`, `cmake`, `msvc`, or `pwsh`. If multiple nodes match, specify
`job.node`; there is no load-aware placement yet. Mark a manifest `disabled: true`
and re-register it to prevent new placements. Queued jobs recheck registration
before starting; a disabled or changed node fails that queued attempt. Running
jobs continue.

The catalog reports `health: not_probed` and `capabilities_verified: false`.
An operator's label is not a successful build qualification. An unreachable or
misconfigured worker fails its job; it is never silently replaced with an agent host.

## Enable and synchronize CI toolchain templates

The owner registers a versioned toolchain manifest, then enables it. Enabling
queues installation on every compatible, enabled dedicated CI node, including
nodes registered later. Compatibility requires the exact native OS and
architecture. The controller checks desired state on registration/change and
periodically while active; a standby or draining controller starts no new work.
Installations use the CI queue and the same per-target and worker file locks as
builds, keeping agent queue capacity separate. An independently running Actions
listener that already owns the worker lock causes a visible busy failure; retry
after that listener exits.

```sh
ebox ci template-register --manifest examples/ci/templates/node24-linux-x64.json
ebox ci template-enable node24-linux-x64
ebox ci templates
ebox ci template-retry node24-linux-x64 --node linux-builder
ebox ci template-disable node24-linux-x64
```

The dashboard's **Templates → CI toolchain templates** panel has the same JSON
import, enable/disable, per-node status and retry controls. New recipes start
disabled. Changed recipes require a higher revision; an enabled recipe stays
enabled and synchronizes the update automatically. Identical registration and
enable requests are idempotent. Recipes and their scripts are owner-authored
trusted code; do not import scripts you have not reviewed.

Recipes install into private, immutable-version directories under
`ENGINEBOX_CI_ROOT/templates/<id>/<manifest-digest>/<attempt>`. HTTPS assets require
SHA-256 and download size bounds. All downloads finish and verify before the
installer runs. Scripts run as the dedicated worker account with a private HOME
and a restricted environment; they receive `ENGINEBOX_TOOLCHAIN` and
`ENGINEBOX_ASSETS_DIR`. Nothing installs through sudo or modifies a global PATH.
These are native trusted processes, so host/VM isolation still supplies security
and resource enforcement. Declared download hashes do not restrict network calls
made by an owner-authored installer itself.

Every declared capability needs an executable probe with exact expected stdout.
Only successful probes produce a receipt tied to the full manifest and node
registration. The CI template catalog reports `ready` and
`capabilities_verified: true` only for that matching receipt. Require it in an
automation with `job.template: "node24-linux-x64"`; `runs-on` then matches the
template's verified capabilities and image, plus the node's actual OS and
architecture. Plans include the selected template revision and manifest digest.
Queued jobs recheck the binding, and every managed build re-runs the probes under
the worker lock before checkout/execution. Jobs without `job.template` retain
the manually prepared-node behavior. GitHub Actions dispatch and independently
launched listeners do not consume managed template bindings automatically.

Minimum CPU, available RAM and free disk are checked before installation, with a
256 MiB disk reserve beyond the recipe minimum. Budget for both compressed assets
and expanded files. These checks do not reserve capacity against unrelated host
processes; scripts run within the configured 1–120 minute deadline and bounded
output. Capacity, download, installer and verification failures remain visible
per node. Bounded diagnostic details stay in private `*.error.json`/`error.json`
files on that worker; the API exposes phase codes instead of script output.

Periodic reconciliation never retries a failure. Inspect the worker before
explicitly retrying or changing its desired configuration; each new attempt uses
a fresh identity. A controller restart marks
uncertain queued/installing attempts failed and re-verifies earlier ready
installations without replaying their installers. Disabling a template stops
new placements and queued starts, and lets running jobs finish. Disabling,
upgrading and retrying retain old files; cleanup is an operator maintenance task.

Both controller and worker must have this version of `ebox`; `ebox ci inspect`
reports `template_installation: 1`. The existing forced SSH command
`ebox ci execute` handles the bounded installation protocol. Older workers fail
visibly and need upgrading. Template installation does not create OS images,
accept licenses, install signing identities or register GitHub Actions listeners.
The [pinned example recipes](../examples/ci/templates) install Node on Linux,
macOS and Windows; Xcode, Android SDK, JDK and MSVC prerequisites for the existing
platform builds remain owner-prepared or require corresponding owner recipes.

## Trigger format and merge handoff

Both runners use a versioned JSON envelope:

```json
{
  "schema_version": 1,
  "request_id": "merge-0123456789-linux",
  "event": "merge",
  "sha": "0123456789012345678901234567890123456789",
  "job": {
    "runner": "automation",
    "runs-on": ["enginebox", "linux", "cmake"],
    "checkout": {"url": "https://github.com/your-org/your-app.git"},
    "shell": "bash",
    "run": "cmake -S . -B build && cmake --build build",
    "artifacts": ["build"],
    "timeout-minutes": 60
  }
}
```

`event` is `merge`, `workflow_dispatch`, or `schedule`. These values identify the
caller; the API does not create schedules or watch repositories. A merge publisher
should submit only after it has committed/published the intended merge result,
using its exact SHA and a stable identity per event/job. Multiple jobs get separate
identities. Deliver retries with identical content. Unknown fields are rejected.

```sh
ebox ci plan --request build.json
ebox ci trigger --request build.json
ebox ci job JOB_ID
```

The request identity is journaled before any external effect. CI records live in
`<state_dir>/ci-jobs`, separate from ordinary job history so an older controller
release cannot prune them during rollback. Both queues participate in draining. Duplicate delivery
returns the existing job; changing content under the same ID is rejected. A
controller restart marks unfinished work failed and **never replays it**. SSH loss
or an HTTP timeout may leave an external operation running: inspect its outcome
before intentionally submitting a different identity. This is at-most-once
submission, not an exactly-once external execution guarantee. Keyed records are
retained, with a 10,000-request bound; archive state during controlled maintenance
before reaching that limit. Keeping an external event-delivery ledger is required
if records are deliberately archived.

## Shell automations

Shell jobs are ordinary build programs, with no prompt, model, provider account or
agent loop. They run in a new private temporary workspace/home. Optional checkout
fetches exactly the requested SHA over HTTPS and checks it before running. Public
or otherwise unauthenticated repositories work; private Git credential delegation
is not implemented. LayerFS checkouts are not mounted implicitly.

The script receives `CI`, `ENGINEBOX_SHA`, `ENGINEBOX_EVENT`, and
`ENGINEBOX_REQUEST_ID`. The child inherits PATH and explicitly allowed OS/toolchain
locations, not controller/provider/GitHub tokens. These are trusted native build
workers, not a sandbox for hostile scripts; filesystem and network isolation must
be provided by their VM/host. Use credential-free, dedicated build accounts.

Bash fails on pipeline errors. PowerShell templates check native-command exit
codes. A nonzero exit, timeout, missing declared artifact, or output overflow
fails the job. Execution is bounded to 1–360 minutes, including checkout. Success
returns at most 16,384 stdout characters; failure diagnostics are currently
generic, with no streaming log API or running-shell cancellation endpoint.

Artifacts are literal relative files/directories, without globbing, traversal,
overlap or symlinks. Successful jobs retain only these paths under
`ENGINEBOX_CI_ROOT/artifacts/<request_id>` (up to 100,000 entries/10 GiB per run).
Retrieve them through the node's existing SSH access. There is no artifact download
API, automatic retention cleanup, signing-secret injection or compatible cache
service yet. Maintain disk capacity/retention on dedicated workers. Other temporary
workspace files are removed after execution.

## GitHub Actions

Copy the selected [workflow templates](../examples/ci/workflows) into the target
repository's `.github/workflows/` directory and commit them on its default branch.
They accept the required `enginebox_sha` input and check out that source commit.
Edit the project-specific commands before use:

| Template | Build | Default GitHub runner | Native capabilities |
|---|---|---|---|
| iOS | Unsigned Release simulator app; edit Xcode project/scheme | macos-15 | macOS, Xcode |
| Android | Debug APK and unit tests; project Gradle wrapper | ubuntu-24.04 | Linux, Android SDK, JDK 17 |
| Windows | CMake Release build, CTest, install artifacts | windows-2025 | Windows, CMake, MSVC, PowerShell |
| Linux | CMake Release build, CTest, install artifacts | ubuntu-24.04 | Linux, CMake, compiler |

Matching Engine Box shell requests live under
[examples/ci/automations](../examples/ci/automations). Replace the zero SHA,
repository, request ID, and project-specific paths in every example. iOS device
signing, store upload and Android release signing are intentionally not configured.

A GitHub request uses the same envelope, with this job:

```json
{
  "runner": "github-actions",
  "repository": "your-org/your-app",
  "workflow": "ios.yml",
  "ref": "main",
  "inputs": {}
}
```

Set `ENGINEBOX_CI_GITHUB_TOKEN` only in the controller service environment, using
a repository-scoped credential with Actions write permission. Tokens never appear
in request JSON, plans or durable results. GitHub resolves `ref` for the workflow
definition; Engine Box supplies the exact source SHA as `inputs.enginebox_sha`.
The caller cannot override this input.

The API uses GitHub REST version `2026-03-10`. It accepts both the current 200
response with a workflow run ID and the older 204 response without one. It does
not guess a run from a timestamp or automatically retry an uncertain dispatch.
The controller job succeeding means the trigger was accepted, **not that the
workflow passed**. Read its actual conclusion:

```sh
ebox ci github-status your-org/your-app RUN_ID
ebox ci github-cancel your-org/your-app RUN_ID
```

If GitHub returns no run ID, inspect that repository's Actions page. Workflow
status is queried on demand, not automatically reconciled into a merge gate.

For a dedicated self-hosted Engine Box worker, install a current official Actions
runner archive in a fresh private directory. Run locally on that worker:

```sh
# Supply a transient repository registration token via the process environment.
ebox ci github-runner --manifest mac-worker.json \
  --directory /srv/actions-runner --repository your-org/your-app
```

`ENGINEBOX_CI_REGISTRATION_TOKEN` is passed to the official listener's environment,
never its argv or job journal. Existing registrations are refused. Registration
uses `--ephemeral --disableupdate`: one job, then disposal/reprovisioning of the
runner installation by its owner. Keep the runner version current; Engine Box
does not download or silently update it. An interrupted registration/listener can
leave a registration behind; inspect/remove it using GitHub's runner controls.
The listener is bounded to seven hours including queue wait. Its exit does not
determine the workflow's conclusion.

Set a repository variable such as `ENGINEBOX_IOS_RUNNER` to a JSON label array:

```json
["self-hosted", "enginebox", "macos", "arm64", "image:macos-15-xcode", "xcode"]
```

The other variables are `ENGINEBOX_ANDROID_RUNNER`, `ENGINEBOX_WINDOWS_RUNNER` and
`ENGINEBOX_LINUX_RUNNER`. GitHub performs native Actions scheduling, status, logs,
cancellation and artifact handling. Engine Box does not emulate Actions using a
shell/YAML interpreter. Workflow labels must select dedicated CI capacity too;
the controller cannot police unrelated GitHub runner registrations.

## API and verification

All endpoints use the existing owner authentication and controller drain gate.

| Endpoint | Purpose |
|---|---|
| `GET /v1/ci` | Capabilities and runner catalog |
| `POST /v1/ci/nodes` | Register/update a native node manifest |
| `GET /v1/ci/templates` | Desired toolchains and per-node readiness/failures |
| `POST /v1/ci/templates` | Register `{ "manifest": ... }`, initially disabled |
| `POST /v1/ci/templates/{id}/enable` or `/disable` | Set desired enablement with `{}` |
| `POST /v1/ci/templates/{id}/retry` | Retry a failed installation with `{ "node": "id" }` |
| `POST /v1/ci/plan` | Validate and resolve a request |
| `POST /v1/ci/runs` | Submit an idempotent trigger; returns existing fleet job |
| `GET /v1/jobs/{id}` | Durable trigger/automation result |
| `GET /v1/ci/github/{owner}/{repo}/runs/{id}` | Actual workflow status/conclusion |
| `POST /v1/ci/github/{owner}/{repo}/runs/{id}/cancel` | Request GitHub cancellation |

Operator MCP adds `ci_nodes`, `ci_register`, `ci_plan`, `ci_trigger`, with
`fleet_job` for results. Toolchains add `ci_templates`, `ci_template_register`,
`ci_template_enable` (boolean `enabled`) and `ci_template_retry`.
Node execution uses the bounded `ebox ci execute` stdin
protocol through SSH. CI does not change native provider account delegation.

Tests cover matching, dedicated-host fencing, CI queue isolation, native shell success/failure,
artifact retention/escapes, request deduplication/restart, HTTP authentication and
draining, pinned toolchain install/probe/build/retry/restart, later-node
synchronization, dashboard controls, and mock GitHub dispatch/status/cancel responses. `actionlint` checks the
four workflows. The native verification matrix runs worker/controller tests on
Linux, macOS and Windows, and explicitly downloads the matching pinned Node
recipe into a disposable worker to verify installation, reuse and an actual build
artifact. Run that network qualification locally with
`pnpm test:ci-templates` (opts into network downloads).
Signed mobile builds, official runner registration, OS image provisioning and
cache benchmarks remain separate qualification; toolchain installation tests do
not establish those capabilities or a successful mobile store release.

Protocol references: [GitHub workflow dispatch](https://docs.github.com/en/rest/actions/workflows#create-a-workflow-dispatch-event),
[self-hosted runner routing and ephemeral lifecycle](https://docs.github.com/en/actions/reference/runners/self-hosted-runners),
and [official listener command/environment handling](https://github.com/actions/runner/blob/main/src/Runner.Listener/CommandSettings.cs).
