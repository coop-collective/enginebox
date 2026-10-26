# ACP and fleet tasks

Engine Box exposes its existing native agent runs through **Agent Client Protocol
v1** over stdio. An editor or another ACP client can start a conversation in an
assigned remote workspace, send prompts, cancel a turn and continue the exact
native conversation. The provider CLI still owns its agent loop, tools, model
access, files and private memory. Engine Box does not proxy inference.

This is an operator interface for the controller's trusted owner, like the fleet
MCP server. Never put the operator API token in a guest or give it to an untrusted
ACP client. It controls the whole configured fleet; it is not a tenant-scoped
credential.

## Connect an ACP client

Connect Claude, Codex or Cursor through the controller's Agents panel first, then
allocate a private workspace. Use the controller's private API over an SSH tunnel
or HTTPS, not a public dashboard URL which may expose only browser routes:

```sh
export ENGINEBOX_API_URL=http://127.0.0.1:8765
export ENGINEBOX_API_TOKEN_FILE=/private/path/enginebox-api-token

ebox --fleet-host node-one acp \
  --workspace ebox-project \
  --provider claude
```

Configure that executable and argument list as an external agent in your ACP
client. `--provider` accepts `claude`, `codex` or `cursor`. `--model` passes a
native model identifier to that provider. `--timeout` sets the existing bounded
native turn limit, from 1 to 300 seconds, default 120. Prompts may consume the
connected provider's allowance.

By default, ACP `cwd` must be an absolute path beneath the guest's `/workspace`.
Use `--workspace-root` when the template has a different guest workspace root.
For an editor opened on a local checkout, add:

```sh
ebox --fleet-host node-one acp \
  --workspace ebox-project --provider codex \
  --local-root /Users/example/projects/project
```

The local root maps to the guest workspace root, and local subdirectories map to
the corresponding remote subdirectories. This maps paths; it does **not** upload
local files or synchronize checkouts. The remote checkout must already contain
the code you intend to edit. Paths outside the configured root are rejected.

## Protocol contract

The adapter uses newline-delimited JSON-RPC 2.0, with only ACP messages on stdout.
Its supported methods are:

| Method | Behavior |
| --- | --- |
| `initialize` | Negotiates protocol version 1 and advertises the supported capabilities. |
| `session/new` | Creates an isolated conversation binding to a host, workspace, provider and directory. |
| `session/prompt` | Starts a native run, emits run progress through `session/update`, then returns `end_turn` or `cancelled`. |
| `session/cancel` | Notification requesting termination of that session's active run. |
| `_enginebox/agent_runs` | Engine Box extension returning the same task summaries as `ebox tasks --json`. |

The following standard request creates a session using the command-line target:

```json
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/workspace","mcpServers":[]}}
```

An operator client can route separate sessions to different providers or assigned
workspaces using the advertised `sessionTarget` extension:

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "session/new",
  "params": {
    "cwd": "/workspace",
    "mcpServers": [],
    "_meta": {
      "enginebox": {
        "hostId": "node-two",
        "workspace": "ebox-other-project",
        "provider": "cursor"
      }
    }
  }
}
```

Those targets are immutable for the conversation. Normal controller workspace
authorization, capacity and writer leases still apply. Different sessions can
execute in parallel; only one run per provider per workspace can own the native
home at once. Engine Box admits native tasks by host capacity
from a durable queue ([QUEUE.md](QUEUE.md)). ACP allows up to 64 conversation bindings and 16
concurrent requests per connection.

A follow-up prompt uses the exact `native_session_id` returned by the previous
run. It never substitutes “latest session.” If the provider does not return a
usable session ID, or a run's execution is uncertain, the adapter refuses an
unsafe continuation and directs you to inspect the run. Conversation bindings
live for the ACP process lifetime: `loadSession` is explicitly false, and
`session/load` is not implemented. Native files and conversations remain in the
private workspace after the adapter exits.

The first version accepts text prompts only, up to 12,000 UTF-8 bytes. It rejects
unsupported image/audio/resource blocks and client-supplied MCP server lists
instead of silently dropping them. Configure MCP servers through the normal
native workspace configuration. It does not implement client file/terminal RPC,
interactive permission requests, agent plans, session modes or ACP v2.
Execution follows the controller's existing preauthorized native CLI policy;
the adapter does not claim that an editor approval dialog guards each guest tool.

Run status updates arrive while the task is active. Native assistant text and
provider-reported token counters arrive when the bounded native run completes;
the existing guest execution API does not stream individual tool events or
tokens. The adapter sends one aggregate native-run tool activity, not fabricated
provider tool calls. Turn results include `_meta.enginebox` with the run ID,
provider, elapsed time, native session ID and usage when available. The usage object carries an explicit `scope` (`run`, `session_total` or `unknown`);
these are not ACP context-window counters and are not sent as `usage_update` data.

## Watch the fleet from a terminal

```sh
ebox tasks
ebox tasks --watch
ebox tasks --active --provider claude
ebox tasks --json
ebox tasks --watch --json --interval 5
```

The table shows active tasks first, then recent completed tasks, across every
registered host. Columns include provider, host/workspace, prompt preview,
status, elapsed execution time, input/output tokens and cached input tokens.
`--json` returns full run IDs and one JSON object per refresh when watching.
Redirected table output appends snapshots; terminal watch mode refreshes in place.

Task summaries use the authenticated `GET /v1/agent-runs` endpoint. The list is
bounded and reports `total` and `truncated`; normal history retention remains 100
completed/native records. Prompt previews stay in private controller state, like
the run output. They are limited to 240 characters and control characters are
removed. Do not put secrets in task prompts.

Elapsed time starts at controller dispatch, includes guest/native startup and
execution, and excludes queue wait. It is not first-token latency. Older records
without a start measurement show an unknown value. The **SCOPE** column distinguishes
cumulative session counters from one run's counters:

- Codex uses the latest native `turn.completed` report with `scope: session_total`.
  A resumed conversation includes previous turns; do not sum its task rows.
- Claude uses final `result.usage` counters with `scope: run`, including cache
  input. Its session-cumulative `modelUsage` is not substituted when `usage` is absent.
- Cursor usage remains unknown when native output does not expose documented
  counters. Unknown values display `—`, never a made-up zero.

Cached input is a subset of input, not an extra cost to add again. No deltas are
inferred across runs. Counter scope was checked against Codex 0.163.0 and Claude
Code 2.1.302 native output; provider changes still need qualification. These
measurements are not subscription balance or billing estimates.

Use the existing native launch tool to start jobs and retain their exact IDs:

```sh
ebox tool agent_launch <<'JSON'
{"host_id":"node-one","name":"ebox-project","provider":"claude","prompt":"Run the project checks and report failures."}
JSON

ebox tool agent_cancel <<'JSON'
{"host_id":"node-one","name":"ebox-project","run_id":"0123456789abcdef0123456789abcdef"}
JSON
```

`agent_runs`, `agent_cancel` and `agent_run` are also available through the fleet
MCP server. Cancellation is a request, not an immediate success claim. Poll the
run until it reports `cancelled`; `cancel_requested` can be true while the guest
is still terminating its process group. Other providers and runs are untouched.
Cancellation works through a run-specific guest control file and the supervised
native process group, not by killing an SSH connection. Upgrade the controller
and guest helper together before relying on cancellation. Older guest helpers
cannot confirm it and errors remain visible.

On ACP disconnect, the adapter requests cancellation of its active runs and
waits for bounded confirmation. A transport failure can leave execution
uncertain; inspect `ebox tasks` before retrying. Existing private workspace state
and provider memory are retained. Cancellation never pauses or destroys a VM.

## Compatibility and validation

New task metadata is stored in private `agent-run-details` sidecars. Original
run records keep their existing schema so a retained older controller can read
them during rollback. An older controller displays a confirmed cancellation as
failed and does not provide the new list/cancel endpoints.

The regression suite exercises the shipped `ebox` stdio process against an
authenticated fixture controller, including parallel agent types, independent
cancellation, immediate exact-session continuation, invalid input, disconnect
cleanup and task-list JSON. Guest supervision tests use fake native executables
to verify process termination and lease release. These tests do not certify an
editor integration or successful live provider inference; those require a
qualified deployment and connected accounts.

References checked October 23, 2026: [ACP v1 initialization](https://agentclientprotocol.com/protocol/v1/initialization),
[session setup](https://agentclientprotocol.com/protocol/v1/session-setup),
[prompt turns](https://agentclientprotocol.com/protocol/v1/prompt-turn),
[transports](https://agentclientprotocol.com/protocol/v1/transports) and
[extensions](https://agentclientprotocol.com/protocol/v1/extensibility).
[Cursor also has native `agent acp`](https://cursor.com/docs/cli/acp); this adapter
provides one remote fleet interface over Engine Box's existing official harness
runs and does not replace or proxy Cursor's native agent loop.
