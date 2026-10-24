# Portable workspace templates

This document describes native agent VM templates. Repository-defined app dev
templates use the separate [project config](PROJECT-CONFIG.md) schema; CI jobs
select [CI toolchain templates](CI.md). Each adapter validates its own template
kind and lifecycle.

A template describes a supported workspace recipe, its resource requirements and its revision. Register it once on the controller. Its reconciler installs that desired revision on every registered node and reports each node's capacity or installation failures. Existing workspaces keep their current environment.

The registry and installer live in `packages/enginebox-core/src/templates.rs`. They are independent of the dashboard and can be called by the API or another operator application. The installer reuses the existing bootstrap library for guest preparation; it does not reinstall the host or require editing its original bootstrap configuration.

## Manifest v1

Start with [`native-dev.json`](../examples/templates/native-dev.json) or [`native-desktop.json`](../examples/templates/native-desktop.json). A complete manifest looks like:

```json
{
  "schema_version": 1,
  "id": "native-dev",
  "revision": 4,
  "label": "Native development",
  "description": "Official native clients and a browser MCP.",
  "recipe": "native-clients-v1",
  "source_image": "images:ubuntu/24.04/cloud",
  "minimum": {"cpu": 2, "memory_gib": 2, "disk_gib": 30},
  "recommended": {"cpu": 4, "memory_gib": 4, "disk_gib": 40},
  "native_clients": ["codex", "claude", "cursor"],
  "config": {
    "browser": true,
    "desktop": false,
    "guest_computer_mcp": false
  }
}
```

Required fields are `schema_version`, `id`, `revision`, `label`, `minimum` and `recommended`. Other fields default to the values above, with an empty description. IDs contain 1–32 lowercase letters, digits or hyphens and start with a letter. Revisions are positive integers. Changing a registered manifest requires a larger revision; registering identical normalized content is idempotent. Native-client list ordering does not affect the digest.

Only the `native-clients-v1` recipe and Ubuntu 24.04 cloud image are supported. `native_clients` can select any subset of Codex, Claude Code and Cursor Agent, including an empty list. The three `config` flags are booleans, and computer MCP requires desktop support. Unknown fields, duplicate JSON keys, other images, arbitrary scripts, package URLs and provider credentials are rejected. The installer uses the client versions and installation rules bundled with the installed Engine Box release.

The manifest digest identifies the normalized specification. It is not a digest of every downloaded OS/package byte: the source image alias and some upstream installers can change. To deliberately rebuild after an Engine Box recipe update, publish a new template revision. Existing prepared bases are not silently reprovisioned.

## Resource requests

Both `minimum` and `recommended` require all three integer fields. Recommended values must meet every minimum. Custom requests must also supply all three fields and meet every minimum. Accepted bounds are 1–512 CPU threads, 1–512 GiB RAM and 12–16,384 GiB root disk, subject to the host's actual capacity and configured reserves.

These are declared template requirements, not an automatic workload benchmark. The example requirements suit starting a native-agent workspace; a large build or desktop workload can require more.

Installation provisions the base with **recommended CPU/RAM and minimum root-disk size**. This keeps later requests at the declared minimum possible without shrinking a filesystem. Clones can expand the root disk to their requested size. Saved-memory storage is separate from the root disk; base preparation reserves recommended RAM plus 2 GiB for it. Fleet admission must include that additional commitment.

Use `size: "minimum"`, `size: "recommended"`, or an explicit `resources` object on the fleet allocation API.

## Registry and deployments

Register from the dashboard or `POST /v1/templates` with `{ "manifest": { ... } }`. Use `GET /v1/templates` to read installation receipts and `POST /v1/templates/native-dev/sync` with `{}` to retry all nodes.

`Fleet::install_template` is a blocking provisioning operation and belongs in a background job. It accepts an existing fleet `Host` object. Its prepared source and snapshot can then be exposed as a fleet template. Hosts must already provide a Linux x86_64 KVM/Incus environment supported by bootstrap, an Engine Box owned project, an existing storage pool and a managed bridge. Creating those host resources is explicitly refused by this adapter.

Set `network` on the host to select its bridge. If omitted, the installer inspects the NICs of the host's configured template bases, including their profiles, and accepts one unambiguous network. It understands managed NIC `network` and bridged NIC `parent`. A new host without an existing template base needs an explicit network. Guest user, UID/GID, home and workspace paths come from the host's guest mapping, not from an imported manifest.

Installation creates a separate deterministic `ebox-template-…` base and profile, with names derived from the manifest and host binding. Bootstrap checks ownership, configuration, KVM, available RAM and free disk before preparing them. After bootstrap succeeds, the installer reacquires the shared host lock, verifies that this exact base and its disk-only clean snapshot were prepared, and stops the base to release active RAM. It publishes a receipt only after successful finalization. It never signs into a provider, copies user credentials, changes unrelated profiles or rolls back by deleting resources. A failed or interrupted installation retains its owned resources for an idempotent retry.

Do not wrap installation in an outer host operation lock: bootstrap and finalization acquire `/run/lock/enginebox-host.lock` themselves. The adapter serializes the host's commitment policy as `host.admission`. Bootstrap checks active CPU/RAM allocations across every Incus project, full root/state disk commitments in the selected pool, actual available memory/disk, configured reserves and the managed instance limit. It repeats those checks while holding the shared host lock before creating or preparing the base. Frozen and unknown VM states keep their CPU/RAM commitments. Retrying the same base does not charge a second disk or instance slot. A completed base reinstall is a no-op for admission. The host lock coordinates Engine Box operations; independent Incus administrator commands do not participate in that lock.

Registry files live under `<state_dir>/templates/`, with private directories and mode-0600 JSON records. Writes use an atomic rename after flushing. Registration uses a process-shared file lock; concurrent installation of the same host/template is rejected. The registry retains previous manifests and the latest deployment receipt per host/template. Jobs and their transient progress belong to the calling service.

`get` and `list` return normalized manifest fields plus `manifest_digest` and `deployments`. Each catalog deployment includes `in_sync`, based on the currently registered manifest digest. Catalog entries omit SSH connection targets and the raw host binding. `deployments(host_id)` returns full private receipts for backend use. Receipts include:

- Template ID, label, revision, manifest digest, minimum and recommended resources.
- Actual Incus `source` and `snapshot`, bootstrap fingerprint and installation time.
- Host ID, target, project, pool, network and guest mapping.
- `host_binding_digest`, bound to target/project/pool/guest mapping and the configured network.

A backend must compare the receipt's `host_binding_digest` with `host_binding_digest(host)` before offering it. Reusing a host ID for a different server or guest mapping must not reuse its old receipt. A new manifest revision makes old receipts out of sync until that revision is installed on the host; it does not modify existing VMs or delete old bases. Host deletion or external snapshot changes require live verification as well: a receipt is evidence of an earlier successful deployment, not proof that Incus still has the resource.

## Remote imports

Submit `{ "url": "https://raw.githubusercontent.com/your-org/workspaces/main/native-dev.json" }` to `POST /v1/templates`.

Imports accept UTF-8 JSON up to 1 MiB from HTTPS port 443. The importer rejects URL credentials, fragments, redirects, compressed responses and non-public DNS addresses. It pins the validated public IP for the TLS connection while verifying the original hostname, sends no authorization/cookies, and ignores proxy environment variables. Network reads use a 15-second deadline and socket timeouts; DNS resolution uses the operating system resolver. URL query values and response contents are not included in download errors or registry records.

The low-level Rust registry only stores the specification. Through the fleet API/dashboard, registration immediately establishes controller desired state and triggers synchronization to every registered node. The reconciler retries failures with backoff and discovers newly configured hosts after controller restart. The catalog exposes desired revision and per-node pending/syncing/ready/failed state; `POST /v1/templates/{id}/sync` requests an immediate fleet-wide retry. Node selection is unnecessary. HTTPS authenticates the serving origin; manifests do not currently carry publisher signatures. A reviewed manifest can also be registered from raw JSON without any download.

## Current boundaries

There is no arbitrary Dockerfile, shell hook, archive import, third-party recipe plugin, credentials embedded in a template, guest identity override or automatic base garbage collection. The first recipe supports native clients plus browser/desktop capabilities on Ubuntu 24.04. Adding a new recipe requires reviewed Engine Box code and a new supported recipe identifier. Registration and adapter tests run without a server; actual recipe installation still needs a disposable-host integration test for the Engine Box release being deployed.

Catalog records can be exported from the dashboard as portable v1 JSON. Exports include the resource and recipe contract but omit deployment receipts, host bindings and snapshot references, so they can be imported into another installation.
