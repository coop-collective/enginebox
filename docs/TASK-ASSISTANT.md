# Native task assistant

Enginebox queues task-assistant requests in its Cloudflare project authority. An explicitly configured node delegates each request to the existing native Enginebox agent-run API. That runtime launches the official Codex client in a private prepared workspace; this adapter does not supply an inference loop or share the native controller credential with the model, browser or game room.

Assistant requests manage project tasks through scoped MCP tools. They are metadata jobs with no Store generation and cannot serve as build, CI or merge evidence. Their capacity reservations and leases use the same node/project records as other fleet jobs. Native engine builds, logical LayerFS merging and artifact promotion retain their separate gates.

## Configure a native executor

1. Prepare an authorized private native Enginebox workspace with the owner's existing Codex connection. Use an isolated workspace for this project, never a live editor's writable checkout or another user's provider home.
2. Install Node 24+ and the Enginebox task MCP package in that workspace. Configure the task MCP server with an agent credential scoped to this project, and verify its tools against the actual fleet API. Keep the credential file private. It must never contain the fleet owner's credential. The task server exposes reversible task operations and cannot approve or ship.
3. Register a new agent node with `assistantProjects: ["demo"]` in addition to its normal project membership. Existing node enrollment is immutable; use a new node ID when changing its declared capability.
4. In the node's private JSON configuration add the native execution binding below. The native controller credential stays in this trusted node service; it is not copied into the provider workspace. `taskToolsConfigured` explicitly records completion of step 2, rather than assuming that any signed-in client already has the tools.

```json
{
  "assistantProjects": ["demo"],
  "nativeAssistant": {
    "endpoint": "https://private-native-runtime.example",
    "token": "<native-controller-credential>",
    "bindings": {
      "demo": {
        "host": "worker-one",
        "workspace": "ebox-project-assistant",
        "workdir": ".",
        "taskToolsConfigured": true
      }
    }
  }
}
```

The endpoint must be an origin without embedded credentials, paths or redirects. A deliberately local HTTP runtime additionally requires `allowHttp: true`. The public app cannot select the native endpoint, host, workspace, provider, timeout or resume identity. Each app turn starts a new bounded Codex run; cross-account native conversation resumption is not exposed. Codex reads the current project tasks through its tools.

Codex supports [stdio MCP registration](https://learn.chatgpt.com/docs/developer-commands#codex-mcp). Use the task server's documented explicit tool allowlist/approval policy for noninteractive runs. Native sign-in, model selection and provider credentials remain governed by the existing native runtime configuration. Default Compose nodes do not contain Codex or private provider credentials, so the app correctly reports the assistant unavailable until a configured executor connects.

## App API

Authenticated owner or project-scoped agent credentials can call these routes; storage nodes cannot. The app backend authenticates its user before proxying, keeps the node credential server-side, and binds polling to the requesting account.

| Route | Behavior |
|---|---|
| `GET /v1/projects/:project/assistant` | Reports configured/live executor availability and provider; no native endpoint or credentials |
| `POST /v1/projects/:project/assistant` | Accepts `{id, prompt}`; ID is stable for retries, prompt is at most 8,000 UTF-8 bytes; returns queued job identity |
| `GET /v1/projects/:project/assistant/:id` | Reports queued/assigned/completed/failed and the final assistant text or bounded error |

Reusing an ID with another prompt fails. Submission without a live capable node fails visibly. Existing accepted requests can be retried while a node is offline without creating another job. Nodes without this project's assistant capability never receive its assistant work.

## Recovery and qualification

The node writes an intent before native submission and saves the returned native run ID before polling. The existing native launch API does not provide a caller idempotency key: a lost submission response is therefore uncertain, not safe to retry. The node retains the fleet reservation and blocks further execution until an owner checks/stops that native executor, reconciles the assignment, and restarts the node. Process restart never silently resubmits an uncertain native request.

On fleet lease loss or shutdown, the adapter requests native cancellation and checks that the run is terminal. A cancellation acknowledgment alone does not release capacity. If confirmation fails, owner reconciliation is required. Completed attempts retain their saved result for acknowledgment retry. The model's text does not assert that code merged or an engine build passed; task and candidate records remain authoritative.

Tests exercise scoped availability, capable-node dispatch, immutable request identity, restart fencing, native API request/result mapping, lost-submit acknowledgment and confirmed cancellation. Native protocol tests use a controlled HTTP executor; they do not claim a newly authenticated live model run. The task MCP integration proof separately invokes the actual tools against the Worker. Real deployment additionally requires qualifying that exact prepared workspace, native provider connection and private network path.
