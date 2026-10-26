# Native agent state and private sessions

Engine Box currently has **one controller owner**, authenticated by its dashboard
session/operator API token. It is not a public multi-tenant service. Every assigned
workspace belongs to that owner; the API cannot choose a different tenant or mount
an arbitrary home. Separate VMs provide the present isolation boundary. A second
person must not receive the operator token as a substitute for tenant authorization.

## Implemented: native runs and workspace persistence

The operator fleet MCP exposes `agent_launch` and `agent_run` for Codex, Claude
Code and Cursor Agent. They start the installed official CLI with its existing
private guest HOME and a directory inside the configured workspace. They do not
replace its agent loop or proxy model requests. Controller access delegation runs
before admission; refresh credentials stay on the controller.

```json
{
  "host_id": "primary",
  "name": "ebox-example",
  "provider": "claude",
  "prompt": "Read this project's instructions and summarize its structure.",
  "workdir": ".",
  "timeout_seconds": 120
}
```

Call `agent_launch` with that object; poll `agent_run` using its `run.id` and the
same host/workspace. Start another run with `resume_id` set to the returned
`native_session_id` to continue that exact conversation. IDs must be native UUIDs;
conversation names and filesystem paths are rejected. There is no implicit
"latest conversation" lookup, which could resume another parallel task.

Equivalent authenticated API endpoints are:

- `POST /v1/hosts/{host}/instances/{workspace}/agent-runs`
- `GET /v1/hosts/{host}/instances/{workspace}/agent-runs/{run_id}`

Launches reject reusable bases, unassigned/quarantined reserves, stopped VMs,
unknown hosts, and workspaces outside the registered storage pool. Relative
`workdir` values must stay inside the workspace after symlink resolution.
The request cannot replace HOME, executable, permissions flags, tenant identity or
any environment variable Engine Box controls; it may add per-run variables through
`env` (see below) and name an owner-managed [profile](PROFILES.md), which selects
turn limits, a permission profile from a fixed set, tools, plugins, skills and MCP
servers without ever naming a CLI flag. A model name is optional; plan/model eligibility and
charges remain provider decisions.

A bounded `--version` preflight verifies the expected native product before a
prompt is sent. This matters because unrelated products also install a binary
named `agent`. Claude runs with unattended permission prompts disabled; Codex and
Cursor retain their workspace sandbox. Missing approvals fail through the native
client rather than enabling a global permission bypass.

Accepted runs wait in a durable queue and are admitted by host capacity
(a per-host run limit plus measured CPU and memory headroom), FIFO per
principal and round-robin across principals; see [the run queue](QUEUE.md).
Each workspace/provider permits one writer at a time, covering
new conversations as well as explicit resumes. A guest file lock independently
protects native state if another controller process or a restarted controller
tries to launch into that same home. This lease covers Engine Box launches;
direct terminal processes do not participate automatically.

A supervised run's `timeout_seconds` is 1-86400 (default 120) and it returns at most 256 KiB of
combined native output. Timeout or excessive output terminates its process group and reports a
failed run. Lifecycle actions wait for an admitted run to finish. Conversation
history remains under the native home; it can be resumed in another bounded run.
This API does not yet offer interactive approvals, streaming or indefinite
sessions; cancellation and signed completion callbacks are available. Direct native terminals remain available for those workflows.

Private controller records use mode 0600 in a mode 0700 directory, keep at most
100 completed outputs, and never enter fleet inventory or lifecycle-job output.
They may contain generated code, prompts echoed by a provider, or other private
conversation text. Treat them like native chat history. A controller restart
marks incomplete runs failed and uncertain. A read-only guest lease probe must
confirm the old process is gone before another launch or lifecycle change is
allowed. A failed transport has the same recovery rule. An unreachable/frozen
guest fails closed and may require an explicit operator inspection outside this
API. The guest timeout remains independent of the controller process. The native
child inherits the writer lease: if the guest supervisor itself is force-killed,
that surviving child continues to hold it. The supervisor's timeout no longer
exists in that failure mode, so manual process inspection/cleanup may be needed.
This is deliberately fail-closed recovery, not a guarantee of automatic cleanup
after SIGKILL. A hostile native program can also deliberately close inherited
descriptors; this local lease is not a security boundary against a hostile guest.

## Scoped service tokens and per-run environment

Two primitives let a hosting owner hand narrow, auditable access to another
program without giving it the owner token. They do **not** make the controller a
multi-tenant service: there is still one owner, and the VM is still the isolation
boundary.

- **Scoped service tokens.** The owner mints a token (`POST /v1/service-tokens`)
  bound to explicit `(host_id, name)` workspaces and a subset of
  `agent_runs.start|get|list|cancel`, with a label and optional expiry. The
  controller stores only a SHA-256 digest under its private state
  (`service-tokens/`, reloaded on every activation like other owned state), shows
  the plaintext once and compares digests in constant time. A token cannot reach
  fleet, template, storage, CI, provider sign-in, controller-internal or token
  routes (403); a revoked or expired token is unauthenticated (401). It sees and
  cancels only runs on its own workspaces, including runs other principals started
  there. Every run's sidecar records `started_by` (`owner` or
  `service-token:<id>`); legacy runs read as `owner`. Revocation stops new
  requests; runs already admitted continue until they finish or the owner cancels
  them.
- **Per-run `env`.** A launch may add up to 32 validated variables for that one
  native process (rules in [FLEET.md](FLEET.md#per-run-environment)). Names that
  Engine Box controls — launcher variables, provider credentials/selectors,
  loader, proxy/TLS and start-up hooks — are refused, so a caller cannot redirect
  inference, replace delegated credentials or move the native home. Values are
  secrets: they never enter records, logs or job output, and only `env_keys` is
  recorded.

How this maps to the tenant design below: a workspace remains the unit of
authorization, and a service token is a *capability* over named workspaces, not
a tenant identity. Step 1 of the build order still requires real user identities
and server-side tenant membership; when that exists, a tenant's tokens should be
mintable only over workspaces that tenant owns, and scopes should name workspace
UUIDs rather than reusable names (a workspace deleted and recreated under the same
name is currently still in scope). `env` carries a caller's own per-run material
(for example a narrowly scoped repository token); it is not a substitute for the
controller vault, which continues to own provider refresh credentials.

## What state survives

Preserving the whole private home avoids an incomplete allowlist of changing
provider files. Engine Box does not reset that home between native runs. The
workspace disk also retains repository instructions, project skills, scratchnotes,
build outputs and other files the agent creates.

| Native client | State to retain | Current resume command |
| --- | --- | --- |
| Codex | `~/.codex`, user skills in `~/.agents`, project instructions/configuration | `codex exec resume SESSION_ID` |
| Claude Code | `~/.claude`, `~/.claude.json` when present, project `.claude` and `CLAUDE.md` | `claude -p --resume SESSION_ID` |
| Cursor Agent | `~/.cursor`, `~/.config/cursor`, project `.cursor` and instructions | `agent -p --resume SESSION_ID` |

Codex stores its user state under `CODEX_HOME` (normally `~/.codex`). Configuration,
history and file-backed auth belong there; an OS keychain may separately hold
credentials. The launcher preserves the native default home and strips inherited
operator `CODEX_HOME`/API-key overrides. See
[Codex configuration and state](https://learn.chatgpt.com/docs/config-file/config-advanced),
[Codex noninteractive resume](https://learn.chatgpt.com/docs/non-interactive-mode).

Claude's auto memory is normally under
`~/.claude/projects/<project>/memory/`. Its project identity can depend on the
repository/path; moving checkouts must preserve that identity or explicitly
migrate it. Memory files and conversation transcripts have different retention
behavior. Mounting a memory file alone does not preserve sessions, settings,
skills and other native state. See
[Claude memory](https://code.claude.com/docs/en/memory) and
[Claude CLI](https://code.claude.com/docs/en/cli-reference).

Cursor documents exact chat-ID resume, global CLI configuration, and project MCP
configuration. Its complete on-disk chat store is not treated as a stable public
format here: preserve the private home instead of parsing or merging that store.
See [Cursor parameters](https://cursor.com/docs/cli/reference/parameters) and
[Cursor configuration](https://cursor.com/docs/cli/reference/configuration).

Native conversation resume reloads saved context. VM hibernation retains the
process's RAM as well. Neither means sharing model inference/KV caches across
tenants. Provider-side prompt caching is independent of these filesystem mounts.

## Next: movable tenant state volumes — design, not implemented

The desired unit is an opaque, authorization-bound tenant state volume, not a
host-global `.claude`/`.codex` mount. The storage/control-plane relationship is:

| Data | Scope and mounting rule |
| --- | --- |
| OAuth refresh credentials | Controller vault, keyed by authenticated tenant/provider; never in templates or general guest mounts |
| Native homes, personal skills, settings, chat stores | Private tenant state volume; mount only into authorized tenant VMs, at stable guest paths |
| Project memory, scratchnotes, repository-local configuration | Tenant + project, stable project directory; no shared writable cross-tenant state |
| Public package artifacts and credential-free templates | Shareable immutable/read-only cache; private package artifacts require tenant-scoped caches |
| Private checkpoints, browser cookies and RAM | Same tenant authorization and encryption policy as the original workspace; never promoted to a public base |

Build this in the following order:

1. Add actual user identities, tenant/workspace membership and scoped capabilities.
   Resolve the tenant from authenticated server-side ownership, never an arbitrary
   `tenant_id` supplied by a prompt or MCP call. Associate workspace UUIDs and
   state-volume IDs with that ownership before exposing shared controllers.
2. Attach a tenant-owned persistent volume to a dedicated VM, retaining a stable
   home and project path. A mount must not expose the parent directory containing
   other tenants' volumes. Revocation must prevent new mounts and credential
   delegation, including after node registration or restart.
3. Implement a fenced storage lease before moving that home between nodes. Start
   with **one active mount writer per native home/provider**. A naive writable
   network mount of a provider's databases into many VMs is not a qualified
   concurrency strategy. The current local guest lock is insufficient for
   distributed storage, split brain or a partitioned node.
4. Give parallel agents separate conversation/session stores and private writable
   overlays. Distribute immutable skill/config bundles; synchronize selected
   project memory through an explicit merge/version policy. Do not merge SQLite
   files, token caches or entire native homes with last-write-wins copying.
5. Add tenant-scoped export/import, backup, retention and deletion. Test that
   another tenant cannot list, mount, restore, resume, read logs from or receive
   credentials for the first tenant's data, including hostile symlinks and stale
   workspace names.

The current code does **not** synchronize an owner's memory between different
workspaces or attach portable tenant volumes. Reusing two native sessions in one
workspace is implemented; cross-workspace continuity is the next storage feature.

## Verification

Rust regression tests under `packages/enginebox-agents/tests/` launch disposable executable fixtures named `codex`, `claude` and `agent`. They cover native prompt/resume arguments, private state, stripped credentials/environment, same-provider writer exclusion, symlink/path boundaries, bounded output, timeout/descendant cleanup and retained leases. They do not contact providers or read personal credentials. `tests/run_env.rs` covers `env` validation, delivery to only the native run, output scrubbing and value-free records; controller tests cover service-token scope, operation, revocation, expiry and run visibility.

The v0.2 release separately passed real operator MCP and Codex/Claude inference with exact-session resume, conversation-only recall and checked file edits. Cursor reached its provider with the native sandbox enabled but quota blocked inference/resume. Those results do not automatically qualify the Rust port. Current Rust update tests preserve durable jobs and a private Claude scratchnote across actual controller upgrade/rollback. See [validation](reports/VALIDATION.md) and [updates](UPDATES.md) for the individual acceptance records.
