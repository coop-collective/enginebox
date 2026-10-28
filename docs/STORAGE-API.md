# Controller checkout storage

LayerFS is a built-in controller API and fleet MCP capability. An administrator registers the qualified Linux nodes and repository locations once; callers then operate on IDs. Node prerequisites, resource budgets, source approval and recovery checks still apply. The controller does not install the upstream preview or silently select an unqualified filesystem.

These endpoints manage the LayerFS checkout sandboxes. They do not yet attach them to Incus VMs, allocate native agent sessions, move provider homes or publish private workspaces as templates. The fleet's existing Incus workspace lifecycle remains separate.

The node-administrator Incus adapter requires this generation of attachment-aware fleet and node tools. Cleanly detach every checkout before rolling a controller back to older binaries: schema-2 LayerFS receipts protect storage CLI operations, while older fleet lifecycle code ignores the VM markers. Enforcing the minimum compatible controller/node generation against active attachments is a release gate before automatic attachment or an attachment API. See [update compatibility](UPDATES.md#layerfs-attachment-compatibility).

## Register a node

Place `storage/config.json` inside the private controller state directory, owned by the controller service account and readable only by that account. The `storage` directory must also be private. Paths below refer to the **registered Linux node**, not the controller computer:

```json
{
  "schema_version": 1,
  "nodes": [
    {
      "host_id": "worker",
      "executable": "/opt/enginebox/current/bin/ebox",
      "config": "/etc/enginebox/layerfs.json",
      "projects": [
        {"id": "game", "repository": "/srv/repositories/game"}
      ]
    }
  ]
}
```

`host_id` must already exist in the fleet configuration. The executable must be this release's `ebox`, and the node configuration must refer to qualified local SDK artifacts, image and storage budgets. For local aliases, prepare the repository on the node beforehand. The GitHub connector can fetch an `owner/repository` directly without registering a local repository path. The controller never accepts arbitrary repository paths, executable paths, SSH targets or shell commands from an API/MCP request.

The registry is loaded under exclusive controller ownership during activation. Use the normal drain/activation upgrade process to apply changes. An absent registry produces an empty storage catalog; malformed registrations fail activation. Node additions are not automatically replicated. Local alias imports identify one approved base. GitHub imports become LayerFS-owned projects: subsequent source integration uses LayerFS main promotion and the optional GitHub reconciliation workflow, rather than replacing the imported Store.

The API catalog hides node paths and reports `registered`, which means enrollment, not verified health. Queue `inspect` to check current prerequisites and `list` to read workspace receipts. Receipts retain interrupted operations and are not a live daemon health observation.

## API

All routes use the existing authenticated controller boundary. Browser clients must use the dashboard proxy; guests must never receive the operator token.

`GET /v1/storage` returns the registered nodes, project aliases and supported operations.

`POST /v1/storage/hosts/{host_id}/{operation}` accepts the following JSON bodies:

| Operation | Body | Result |
| --- | --- | --- |
| `inspect` | `{}` | Check immutable artifacts, Docker and filesystem prerequisites. |
| `list` | `{}` | Read durable workspace receipts, including blocked operations. |
| `preview` | `{"project":"game","revision":"HEAD"}` | Review tracked files, exact commit, byte count and approval digest. |
| `import` | `{"project":"game","revision":"EXACT_COMMIT","approval":"DIGEST","credential_free":true}` | Import only the reviewed commit and its tree into that project's Store. |
| `fork` | `{"project":"game","name":"agent-a"}` | Create a separate writable checkout from the imported base. |
| `fork` | `{"project":"game","name":"agent-b","from":"prepared"}` | Reuse that project's completed prepared checkpoint, including installed files. |
| `deps` | `{"name":"agent-a"}` | Install from the immutable configured pnpm generation, offline and without lifecycle scripts. |
| `checkpoint` | `{"name":"agent-a","message":"Prepared dependencies"}` | Save file state; message is optional. |
| `remount` | `{"name":"agent-a"}` | Remount a clean checkout at its stable path. |
| `close` | `{"name":"agent-a"}` | Close a clean sandbox; uncheckpointed edits are refused. |
| `recover` | `{"name":"agent-a"}` | Examine an interrupted command against current node evidence; return it to ready only if the node can prove the safe conditions. |

Every POST returns HTTP 202 with a durable `job`. Poll `GET /v1/jobs/{id}`. On success, the node response appears in `job.result.result`; `job.result` also includes the backend, host ID and operation. Operations use the existing bounded controller queue and drain barrier. Dropping an HTTP request does not release ownership while admitted work continues. A failed or interrupted job is never automatically retried.

Node results are limited to 3 MiB before they enter controller job storage. An oversized preview fails explicitly; review it directly on the node rather than relying on a truncated file list.

Import requires the exact lowercase 40- or 64-character Git commit ID and 64-character approval digest returned by the preview. The explicit `credential_free` assertion covers the reviewed tracked content; known secret-path rejection is not a general secret scanner. Each API argument remains an argument through SSH quoting. The node CLI runs with a clean environment and explicit local host selection.

There is no API for general `exec`, arbitrary cache preparation, forced discard, filesystem merge, project deletion or `restore-checkpoint`. The latter remains an explicit node-administrator operation with acknowledgement of uncheckpointed edits; normal recovery never replays the original command or creates a replacement checkout.

## MCP and terminal

The dashboard's **Workspaces → Source and storage** controls use the same API. Within a workspace, project selectors and checkout receipts are limited to that project; the all-workspace setup view remains available for new imports and legacy `?panel=storage` links. The controls show enrolled nodes and their receipts, lets an operator review the exact source commit before attesting and importing, and forks from the base or a completed prepared checkpoint. Checkout actions track independent jobs, so work on one checkout does not disable its siblings. Status polling stops after 15 minutes or an observation failure; the UI can check an existing job again without resubmitting its command. Leaving the page stops observation, not the node operation.

The built-in `enginebox-fleet-mcp` server exposes six local storage tools:

- `storage_catalog`
- `storage_inspect` (`operation` is `list` or `inspect`)
- `storage_preview`
- `storage_import`
- `storage_fork`
- `storage_action` (`deps`, `checkpoint`, `remount`, `close`, `recover`)

All return the same API results. Poll the existing `fleet_job` tool. `storage_preview` defaults to `HEAD`; imports still require the exact approved commit.

The operator terminal uses the same tools with JSON from stdin:

```sh
printf '%s\n' '{"host_id":"worker","project":"game"}' | ebox tool storage_preview
printf '%s\n' '{"host_id":"worker","project":"game","name":"agent-a"}' | ebox tool storage_fork
printf '%s\n' '{"host_id":"worker","operation":"list"}' | ebox tool storage_inspect
```

See [LayerFS backend operation](LAYERFS-BACKEND.md) for source import, immutable caches and node-local configuration, and the [backend validation results](reports/LAYERFS-BACKEND-VALIDATION.md) for the earlier live storage qualification.

## Verification boundary

Controller and MCP tests exercise strict registration/request validation, exact import approval, absence of shell/path escape hatches, failed node results, durable jobs and draining during a storage operation. The node executor is substituted in controller route tests; those tests do not claim a deployed remote controller or production host migration.

Core Incus transport tests additionally refuse hibernation, VM snapshots, restores and copies when a LayerFS attachment marker is present. Bootstrap preparation and template publication also reject marked VMs or snapshots, including markers left by a partial attachment. Old snapshots and reusable templates containing such a marker are refused. An external checkout is not captured by a VM's disk/RAM snapshot, and its file checkpoint must remain a separate operation.

## GitHub connections and LayerFS main

See [GitHub architecture and operation](GITHUB-SYNC.md). These operations use the
same authenticated node job boundary and do not accept filesystem paths, tokens,
Git URLs outside github.com, shell commands or arbitrary push refspecs.

| Operation | Body | Behavior |
| --- | --- | --- |
| `github-preview` | `{"project":"app","repository":"owner/repo","branch":"main"}` | Fetch and review the exact source. Branch is optional on initial import. |
| `github-import` | `{"project":"app","revision":"EXACT_COMMIT","approval":"DIGEST","credential_free":true}` | Import Git-free files into LayerFS; idempotent for the reviewed initial commit. |
| `github-fork` | `{"project":"app","name":"agent-a"}` | Fork LayerFS main; optional `from` selects a prepared checkpoint in that Store. |
| `github-settings` | `{"project":"app"}` | Observe accepted main revision/sequence and delivery state. Add `enabled: true/false` to configure outbound sync. |
| `github-promote` | `{"project":"app","name":"agent-a","expected_main":"REVISION","merge":true}` | Merge the closed checkpoint with main and conditionally publish it. `merge: false` explicitly promotes that branch's complete tree. Optional `message`. |
| `github-delivery` | `{"project":"app","action":"export"}` | Queue the current accepted main if enabled; ordinary task checkpoints are not export events. |
| `github-delivery` | `{"project":"app","action":"deliver","attempt":"sync-ID"}` | Continue a durable attempt; lost push replies reuse its exact commit. |
| `github-delivery` | `{"project":"app","action":"agent-task","attempt":"sync-ID"}` | Provision/inspect an isolated Git reconciliation container and return its specialized-agent task. |
| `github-delivery` | `{"project":"app","action":"resolve","attempt":"sync-ID"}` | Accept a stopped agent environment's clean resolution into LayerFS, then conditionally deliver to GitHub. |
| `github-delivery` | `{"project":"app","action":"retry","attempt":"sync-ID"}` | Retain a stale/failed-preparation attempt and reconcile again from accepted files. |

The corresponding MCP tools are `storage_github_preview`, `storage_github_import`,
`storage_github_fork`, `storage_github_settings`, `storage_github_promote` and
`storage_github_delivery`. Poll `fleet_job` as for other storage operations.

An administrator can add `github_token_file` to a registered node. It names a
node-local service-owned mode-0600 JSON file containing `{"token":"..."}`.
Parent storage state stays private. Use repository-scoped read access for imports
or contents read/write for enabled delivery. Credentials are passed through an
isolated Git process environment, never argv, a remote URL, a project, a sync
container or a job response. Public imports need no token. Auth/rate/network
errors remain visible and do not roll back accepted LayerFS main.

Main promotion must use a closed workspace with a completed LayerFS checkpoint.
The current adapter's publication lock and durable outbox are local-owner
coordination, not distributed fencing. The in-progress distributed controller
must invoke this workflow only after accepting an immutable generation under its
lease/durability policy. Native agent scheduling/provider assignment remains the
executor's responsibility; the connector supplies the bounded environment and
exact-input task, and refuses active or unresolved agent results.
