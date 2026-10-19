# Stopped workspace preparation

The controller can prepare an owned, stopped VM for a registered LayerFS project.
This is a usable **allocation prerequisite**, not automatic checkout attachment.
It does not fork a checkout, attach `/project`, boot the guest, or delegate agent
credentials. The returned `ready` is false. Ordinary create/allocate behavior
and controller state schema 1 remain unchanged.

From an authenticated operator terminal, queue a binary compatibility check:

```sh
ebox api /v1/storage/hosts/worker/compatibility --post <<'JSON'
{}
JSON
```

Then queue a stopped preparation using administrator-registered IDs:

```sh
ebox api /v1/storage/hosts/worker/prepare --post <<'JSON'
{"project":"game","name":"ebox-game-agent-a","template":"native","size":"minimum"}
JSON
```

Both return a durable job. Poll the returned job ID with `fleet_job` or
`GET /v1/jobs/<id>`. MCP exposes `storage_inspect` with
`operation: "compatibility"`, `storage_prepare`, and `storage_allocation`.
The receipt can also be read through a queued request:

```sh
ebox api /v1/storage/hosts/worker/allocation --post <<'JSON'
{"name":"ebox-game-agent-a"}
JSON
```

The storage registry still supplies the executable and repository paths;
requests cannot supply host paths, commands, owners, or arbitrary VM sources.
Compatibility checks `ebox node-protocol` from that registered executable and
`enginebox-host protocol` resolved exactly as the actual inventory helper is.
The exact behavioral protocol, Linux platform, external-checkout lifecycle
guards, and receipt format must match. A package version alone is insufficient.
These read-only reports describe compiled support; they do not prove Docker,
FUSE, kernel, Incus, guest or native-harness readiness. Existing file operations
retain their existing independent prerequisite checks.

Preparation holds the controller host mutex and shared node lease, validates the
template and resource minima, checks physical/contribution reserves, and copies
a credential-free template statelessly. It always creates a new stopped VM;
generic frozen warm-pool assignment is not used. Templates carrying an existing
pool/allocation/workspace claim are rejected before copying.

A private controller receipt is fsynced before copy. The copy request itself
sets `boot.autostart=false`, `user.enginebox.pool.state=preparing`, a distinct
`layerfs-preparation` pool size, an allocation UUID, a registered project alias,
and the project ownership marker. A successful receipt additionally records the
new Incus UUID and template/snapshot provenance. Disk and instance-slot
admission include the stopped VM. CPU/RAM are not held indefinitely for a
stopped preparation: capacity must be rechecked before eventual start, including
the separate LayerFS sandbox commitment.

The existing `preparing` guard prevents both current and prior schema-1 fleet
controllers from treating this VM as an ordinary private workspace. Warm refill
does not claim it. Ordinary lifecycle and agent-access routes reject it. This
protects the **stopped preparation only**; it is not a downgrade fence for a
running attached guest. Explicit administrator/raw Incus access remains outside
the controller's guarantees.

Interrupted or failed preparations retain their receipt and any partial VM.
There is no retry, replay, force-delete, or automatic release. A lost response
may mean the copy completed. Existing durable jobs fail on controller restart
without replaying, while the separate allocation receipt survives job pruning.
Reading a receipt is not a live health check. Do not remove the preparation
marker to make an incomplete VM startable.

## Safe administrator cleanup

Until explicit cancellation/finalization is implemented, an administrator can
release disk and instance capacity after inspecting a retained preparation:

1. Drain the owning controller and ensure no job/native session still references
   the preparation. Keep the private receipt as evidence. Do not act solely on
   the job's success/failure label.
2. On the exact registered node, inspect the exact project/name using
   `incus --project PROJECT config show INSTANCE --expanded` and
   `incus --project PROJECT query '/1.0/instances/INSTANCE?project=PROJECT'`.
   Compare the allocation UUID to the receipt, the local `volatile.uuid` to its
   saved Incus UUID when available, the owner to PROJECT, and the root disk pool
   to the admitted pool. The local pool state must still be `preparing` and its
   size must be `layerfs-preparation`. If an interrupted copy has no saved Incus
   UUID, the original copy's allocation marker plus the retained receipt and
   stopped instance must be independently verified; never select a VM by name
   alone.
3. Prove the VM is **Stopped**, has no stateful/RAM checkpoint, and remains
   credential-free. Verify no `user.enginebox.layerfs` or
   `user.enginebox.layerfs.workspace` key exists in local or expanded config,
   no `ebox-layerfs` disk or other external checkout device exists, and no
   storage attachment receipt/export claims this VM. A partial/uncertain
   attachment must first go through the storage reconciliation/inspection
   workflow in [LAYERFS-INCUS.md](LAYERFS-INCUS.md); do not remove its markers or
   filesystem paths to bypass custody.
4. Only after those proofs, use normal
   `incus --project PROJECT delete INSTANCE` for that exact prepared VM. Do not
   use `--force`, remove unrelated snapshots/volumes, or clear storage exports.
   If the VM is running, changed, foreign, or any proof is missing, stop cleanup
   and inspect the discrepancy instead.
5. Verify the exact VM is absent and retain its controller allocation receipt.
   It intentionally prevents reusing that allocation name; use a new name for
   the next preparation. A missing VM does not by itself authorize deleting
   shared source storage or LayerFS sandboxes. Resume the controller afterward.

## Remaining automatic attachment transaction

Before enabling fork → prepare → attach → start, implement an explicit
attachment-aware controller state/release fence. Existing schema-1 rollback
must not gain access to running attached guests. Do not silently migrate every
ordinary fleet or simply add an ignored JSON feature flag.

Finalization must consume the exact allocation receipt and VM UUID under a
durable transaction, bind a separately owned LayerFS checkout, acquire/release
the host lease without nesting the adapter's own lease, deliberately transition
the preparation reservation, attach while stopped, re-admit combined resource
budgets, start, and verify guest writes/execution/mmap before exposing readiness
or credentials. Reconcile each interrupted transition without replaying work;
do not release a failed VM back to the generic warm pool. Native session working
directories must use the attached project path. Project-specific pre-attached
warm VMs are a subsequent qualification; dynamic attachment to the generic
frozen pool remains unqualified.

The process-boundary tests cover successful stopped preparation, a lost copy
response, incompatible helpers, retained interrupted receipts, inherited claim
and changed-copy custody refusal, ordinary lifecycle/credential guards after controller reopen, orphan
name refusal, and no warm-pool reuse or automatic cleanup. This step has not
been exercised against a live node; the separately documented manual Incus
adapter qualification does not prove this controller workflow.
