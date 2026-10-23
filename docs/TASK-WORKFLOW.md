# Project tasks and native agent tools

Enginebox owns durable task state and workflow enforcement. Gamejam stores its chat/community data and calls these APIs through an authenticated server adapter; its browser never receives fleet credentials. The same project API is exposed to native Codex through MCP. Moving a card records workflow state; it does not execute an agent, merge a branch, run an engine or bypass the candidate CI gate.

Tasks have `type: idea | issue` and these statuses:

| Status | App label | Meaning |
|---|---|---|
| `backlog` | Ideas & Issues | Work proposed, with optional immutable chat references |
| `in_progress` | In Progress | Work being prepared; actual execution appears separately in node/job activity |
| `testing` | Testing | Work awaiting evidence and/or owner review |
| `shipped` | Shipped | The exact task result was included in an accepted, fully validated candidate |

Every task includes its `project`, `id`, `title`, `description`, `type`, `status`, `revision`, creation/update timestamps and credential actors, optional `assignee`, chat `source`, immutable `review` policy, `resultGeneration`, `candidate` and `shippedGeneration`. New tasks start in backlog with revision 1. Chat references are reported provenance; the app must verify the referenced conversation/messages against its own authenticated records before promotion.

## API

All paths below are under `/v1/projects/:project`. Owner credentials and agent-node credentials scoped to that project can read tasks. Storage nodes cannot access the task APIs. Normal edits use the scoped agent/service credential; owner-only policy and approval must stay in a trusted service boundary separate from arbitrary project code.

| Request | Body or result |
|---|---|
| `GET /tasks?cursor=&limit=&status=` | `{tasks, nextCursor}`; cursor is the last task ID, limit 1–100 |
| `GET /tasks/:id` | One task |
| `POST /tasks` | `{id, title, type, description?, source?, review?}` |
| `POST /tasks/:id/update` | `{operationId, expectedRevision, patch:{title?, description?, type?, assignee?}}` |
| `POST /tasks/:id/move` | `{operationId, expectedRevision, status, generation?, candidate?}` |
| `GET /tasks/activity?cursor=&limit=` | `{events, nextCursor}`; cursor is the last numeric event sequence |
| `GET /activity` | `{jobs, nodes}` from actual durable job/node records, with commands, origins and other project membership omitted |

An identifier is 1–48 lowercase letters/digits/hyphens, beginning and ending with a letter or digit. Task ID `activity` is reserved. Titles are trimmed and limited to 200 characters, descriptions to 16,000. `source` is `{kind:"chat", conversation, messageIds}`; conversation/message IDs are 1–128 characters, with 1–32 unique message IDs. A source bundle can create only one task. `assignee` is a nullable identifier. A task list uses stable ID ordering; refresh the first page to see new IDs inserted before the current cursor.

Reusing a task ID with the same normalized creation request returns its current record. A different creation request for that ID, or another ID for the same source bundle, returns 409. Update/move operations require the last observed revision and a caller-generated operation ID. An identical retry by the same credential actor returns the original applied result; changed input/actor under the same operation ID returns 409. Concurrent edits cannot silently overwrite each other. All task, audit-event and operation-receipt writes share the Project Durable Object's synchronous SQLite transaction boundary.

## Review and shipping

The default immutable policy is `review:"required"`. Only the owner can create a task with `review:"automatic"`. Agents cannot lower that policy or approve their own work.

Moving into Testing may attach a published `generation` and a `candidate` containing `{id:task.id, generation:task.resultGeneration}` in its change membership. The task can enter Testing before evidence exists, but that does not make it shippable. Editing its requested title, description or type clears old evidence; an edited Testing task returns to In Progress. Returning to Backlog/In Progress also clears evidence.

Staging a candidate also records a server-derived fingerprint of each existing task's title, description and type. Attaching evidence, accepting that candidate and shipping its tasks require the same requirements. Retrying a candidate ID never refreshes its snapshot; changed requirements need a newly staged and validated candidate. Normal assignment/status/revision changes do not invalidate matching requirements. Once a durable acceptance is pending, content edits are rejected until it settles. Generic non-task changes remain supported, while older candidates without task snapshots cannot serve as task-shipping evidence.

For review-required work, the owner's explicit **Approve & ship** move must name the current task revision. Enginebox requires Testing state, accepted candidate evidence, and exact task/result membership. A raw accepted generation or an unrelated candidate is insufficient. The resulting immutable event records the owner credential identity, task revision and durable acceptance decision ID. App-specific end-user attribution remains in the app's authenticated audit trail; Enginebox does not trust caller-supplied identity headers. This version has no separate delegated-reviewer or standalone approval endpoint.

For automatic tasks already in Testing with matching evidence, candidate acceptance advances the task to Shipped after required full checks and durable acceptance succeed. The event records `policy:automatic` and the acceptance decision. A task changed while validation was running will not automatically ship under its old evidence. Shipped records are immutable; create follow-up work instead of rewriting an accepted task. These rules build on [the integration/CI gate](INTEGRATION-CI.md), including its frozen validation receipts.

## Give Codex scoped task tools

Install the pinned workspace dependencies and Node 24+ in the authorized private native workspace. Create a private, untracked configuration file:

```json
{
  "controller": "https://fleet.example",
  "token": "<project-scoped-agent-node-token>",
  "project": "demo"
}
```

Use an existing enrolled **agent** credential with only the intended project in its scope. The MCP server rejects owner credentials and verifies the server's node identity before its first task request. Its configuration and the agent's native provider credentials remain private; keep owner promotion credentials out of the agent's environment, task files and MCP configuration. Use HTTPS outside an isolated local development network.

Register the stdio server in that workspace's Codex configuration:

```sh
codex mcp add enginebox_tasks -- node /absolute/enginebox/packages/fleet-node/src/task-mcp.ts /private/task-mcp.json
codex mcp list
```

This uses the installed CLI's `mcp add` contract and the [official MCP configuration guidance](https://learn.chatgpt.com/docs/extend/mcp?surface=cli). Existing node configuration files containing `controller`, `token` and `projects` can be reused by supplying the project as the final argument. The controller URL must be an origin without credentials, a path, query or fragment. Project authority is always checked on the server.

For an authorized unattended native task runner using `approval_policy="never"`, explicitly permit only this scoped tool set in its private Codex configuration; otherwise a tool requiring interactive approval cannot run. The MCP configuration supports `enabled_tools`, `required` and `default_tools_approval_mode`:

```toml
[mcp_servers.enginebox_tasks]
command = "node"
args = ["/absolute/enginebox/packages/fleet-node/src/task-mcp.ts", "/private/task-mcp.json"]
required = true
enabled_tools = ["enginebox_tasks_list", "enginebox_tasks_get", "enginebox_tasks_create", "enginebox_tasks_update", "enginebox_tasks_move", "enginebox_tasks_activity", "enginebox_agents_activity"]
default_tools_approval_mode = "approve"
```

This approval setting applies only to the listed task server. It does not permit owner promotion, arbitrary fleet commands, shell commands or another MCP server. Keep this configuration in the selected private native workspace; do not edit a developer's existing global profile as part of starting the local fleet.

Tools are `enginebox_tasks_list`, `enginebox_tasks_get`, `enginebox_tasks_create`, `enginebox_tasks_update`, `enginebox_tasks_move`, `enginebox_tasks_activity` and `enginebox_agents_activity`. Codex can create ideas/issues, edit their contents, assign work and move it through Backlog/In Progress/Testing. It cannot call an owner shipping or acceptance tool. The harness still plans and reasons natively; Enginebox does not proxy inference or replace its loop.

The `TaskTools` client in `packages/fleet-node/src/task-mcp.ts` is reusable by a native adapter. The read-only Rust `ebox tasks` command still views native provider runs, while the project task service tracks roadmap work; those identities are not interchangeable. Native prompt/run dispatch must use the existing qualified native execution adapters and their resource leases, not infer that a task status means an agent ran.

## Verification and current limits

`pnpm check:fleet` and `pnpm test:fleet` cover authorization, immutable source/creation requests, optimistic concurrency, Durable Object eviction, audit pagination, changed requirements, accepted candidate membership, required review and automatic shipping. The isolated `pnpm fleet:smoke` launches the official MCP client and stdio server inside an enrolled node, creates/edits/moves a task against the actual Wrangler Worker, and confirms it through a separate HTTP read. It does not spend model tokens or modify a real native Codex profile.

This remains the trusted-owner development fleet. App authentication, public tenant isolation, retention of long task histories and the native assistant adapter need their own qualification. Automatic compatible-batch selection, logical LayerFS merge/import and engine startup are separate Enginebox capabilities; task CRUD and MCP tools do not implement them.
