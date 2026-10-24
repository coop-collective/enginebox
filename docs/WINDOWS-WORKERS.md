# Windows CI and agent machines

An installed Windows x64 worker connects outbound to the **same native controller
and HTTPS dashboard** as an existing bare-metal host. It needs no inbound port,
VM, GPU, GitHub runner token or controller administrator credential.

In the dashboard, open **Worker machines**, choose a name, CPU cores, memory in
GiB, disk allowance in GiB, and CI and/or native agents. Create and copy the install
command, then paste it into Windows PowerShell. The installer can also prompt for
the controller origin and registration code when launched without them.

The command verifies a versioned installer checksum. That installer verifies its
archive and executable, inspects available CPU/RAM/disk and existing installation
ownership, registers the machine, then installs missing Git, PowerShell and/or the
native Codex CLI with WinGet. Codex sign-in is a separate official local sign-in.
A machine is registered before tool installation; the dashboard reports workload
readiness separately. PowerShell 7.3+ is required for CI. Organization-enforced
script signing still requires an approved installer; the launcher changes only
its child process's execution policy.

## Resource allowance

- CPU means a share equivalent to that many logical processors, shared across the
  supervisor and its descendants. Windows Job Objects enforce a CPU rate cap.
- Memory is an aggregate committed-memory cap for the same process tree.
- Disk is a **workspace watchdog**, not a reserved partition or hard filesystem
  quota. The worker monitors its work directory and free volume space and stops
  work at the allowance or below 1 GiB free. Sampling can overshoot; files outside
  the work directory are not covered. Logs, retained artifacts and agent output
  need owner cleanup when the budget fills. No user data is automatically deleted.
- One CI or agent job runs at a time. Native jobs run as the installing Windows
  user. Use trusted owner projects; this is not isolation for hostile tenants.

Windows applies limits across nested jobs and terminates descendants when their
owning job handle closes. See [Microsoft's CPU rate control](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_cpu_rate_control_information)
and [Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects).

The initial budget is fixed for that identity. To change it, revoke the old
machine, uninstall its owned task, and create a fresh registration and installation
directory. The worker may choose a smaller initial budget than the invitation;
it cannot increase the owner's allowance.

## Identity and lifecycle

Registration codes expire after ten minutes and bind one machine-generated
identity. An interrupted registration can retry with that same identity before
expiry. The controller stores hashes, not reusable raw codes or machine tokens.
The copied command contains the one-time code; treat it as private until consumed
or expired. The node credential authorizes only that machine's heartbeat and work
lease endpoints. Browser requests cannot use those endpoints. The dashboard's
owner credential stays server-side.

The installer creates an owned, least-privilege scheduled task named **Enginebox
CI and Agents Node**, starts it now and at sign-in, and configures failure restarts.
This is an interactive-user task: it requires the user to remain signed in; it is
not an unattended Windows Service. Private identity and receipts are under
`%LOCALAPPDATA%\Enginebox\worker-node\private`; working data is in `work`.
The installer restricts that directory to the installing user's SID. Trusted
same-user native jobs remain capable of reading that user's files.

Revocation stops new assignments immediately; the local worker stops its owned
process tree when its next heartbeat is rejected. Lost leases, controller restart
and uncertain execution are fenced, never replayed. An owner must reconcile an
uncertain run after the live node reports its worker stopped. Controller handoff
allows heartbeat/results for already admitted work while draining.

## Job API and scope

Owner-authenticated `POST /v1/contributors/{id}/runs` takes a unique `request_id`,
`kind` (`ci` or `agent`), and `request`. Inspect the returned job through
`GET /v1/jobs/{id}`. Retrying identical content with that ID returns the same job;
changed content is rejected. Results are machine reports, not protected-branch
acceptance or shipping approval.

For CI, `request` is the existing native automation request documented in
[CI.md](CI.md). Its `request_id` must match the outer ID. Use
`runs-on: [windows, pwsh]`, `shell: pwsh`, and optionally an exact HTTPS checkout
SHA. Artifacts remain on the worker under `work/ci/artifacts/{request_id}`; this
release does not upload them automatically or register a GitHub Actions runner.

For agents, `request` uses the existing native run format, initially with
`provider: codex`, `prompt`, optional `model` and `timeout_seconds`. Runs create
fresh retained `work/agents/{request_id}` directories. Resume and automatic
LayerFS project materialization/acceptance are not qualified by this installer.
The official CLI owns the inference loop and local provider authentication.
Installation alone does not prove provider subscription access.

## Controller configuration and qualification

The deployed dashboard requires:

- `ENGINEBOX_NODE_INSTALLER_URL`: an exact versioned Enginebox GitHub release
  `install.ps1` URL.
- `ENGINEBOX_NODE_INSTALLER_SHA256`: the lowercase SHA-256 of that file.
- Optional `ENGINEBOX_NODE_PUBLIC_ORIGIN`: HTTPS origin used by installed nodes;
  defaults to the dashboard origin. Paths, queries and URL credentials are refused.

Without a configured installer, invitation creation returns a visible error and
does not consume an invitation slot. Deploy the matching native controller and
dashboard through the normal standby/drain/activate supervisor flow.

Tests cover identity replay/expiry, downward budgets, node-token scope, result
binding, fencing/reconciliation, owner-only command creation, browser rejection,
copy/readiness/revoke UI, Windows PowerShell 5.1 bootstrap checks, and a real Windows
supervisor executing a PowerShell CI artifact job and a fixture native Codex
process. The fixture agent proves process/auth-environment wiring, not real model
inference. Physical GPU qualification is separate; see [NATIVE-GPU-NODES.md](NATIVE-GPU-NODES.md).
