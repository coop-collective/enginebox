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

`host_id` must already exist in the fleet configuration. The executable must be this release's `ebox`, and the node configuration must refer to qualified local SDK artifacts, image and storage budgets. Prepare or fetch the repository on that node beforehand. The controller never accepts arbitrary repository paths, executable paths, SSH targets or shell commands from an API/MCP request.

The registry is loaded under exclusive controller ownership during activation. Use the normal drain/activation upgrade process to apply changes. An absent registry produces an empty storage catalog; malformed registrations fail activation. Node additions and newer Git revisions are not automatically fetched or replicated in this release. Project IDs currently identify one approved imported base; updating an existing base needs a separate versioned publication workflow.

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

The dashboard's **Storage** sidebar uses the same API. It shows enrolled nodes and their receipts, lets an operator review the exact source commit before attesting and importing, and forks from the base or a completed prepared checkpoint. Checkout actions track independent jobs, so work on one checkout does not disable its siblings. Status polling stops after 15 minutes or an observation failure; the UI can check an existing job again without resubmitting its command. Leaving the page stops observation, not the node operation.

The built-in `enginebox-fleet-mcp` server exposes six storage tools:

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

See [LayerFS backend operation](LAYERFS-BACKEND.md) for source import, immutable caches and node-local configuration, and the [backend validation results](LAYERFS-BACKEND-VALIDATION.md) for the earlier live storage qualification.

## Verification boundary

Controller and MCP tests exercise strict registration/request validation, exact import approval, absence of shell/path escape hatches, failed node results, durable jobs and draining during a storage operation. The node executor is substituted in controller route tests; those tests do not claim a deployed remote controller or production host migration.

Core Incus transport tests additionally refuse hibernation, VM snapshots, restores and copies when a LayerFS attachment marker is present. Bootstrap preparation and template publication also reject marked VMs or snapshots, including markers left by a partial attachment. Old snapshots and reusable templates containing such a marker are refused. An external checkout is not captured by a VM's disk/RAM snapshot, and its file checkpoint must remain a separate operation.
