# Registered compute nodes

The native controller's owner-only `GET /v1/compute` returns a single versioned
node inventory for configured Incus hosts and enrolled workers. Both are
registered machines. Their existing authorities remain the source of identity:

- `host:<host-id>` adopts a validated entry in the persistent fleet configuration.
  Existing SSH/local access, host reserves, template receipts and VM ownership
  remain in place. No extra worker credential or contributor record is created.
- `worker:<node-id>` refers to a completed, non-revoked machine enrollment.
  Its scoped credential, heartbeat, local limits and lease reconciliation remain
  authoritative. Pending and revoked registrations stay in separate history.

These namespaced IDs are stable across restarts and cannot collide when a host
and worker have the same source ID. API operations continue to use their original
host/node IDs. Removing a configured host or revoking a worker removes that
registration from the active Compute inventory; neither silently deletes its
workspaces. The endpoint never exposes SSH targets or credentials.

`GET /v1/fleet` and `GET /v1/contributors` also include their respective
`compute_nodes` projections. The dashboard consumes those same controller-owned
objects while loading each source independently, so a failed SSH inventory does
not hide an enrolled workstation or add a second SSH probe each refresh.

Every node reports registration method, runtime, connection status, workload
readiness, contribution allowance, available capacity where known, template
names and supported management actions. Online means a successful host inventory
or a current worker heartbeat. An idle worker's `worker_running: false` means no
owned child job is running; it does not mean the node is disconnected. Blocked
lease reconciliation is shown separately from connectivity. A connected Incus
host with exhausted admission capacity remains online and reports
`capacity_limited`; free physical disk is not an available workspace allowance.
Available template names include configured legacy templates; this inventory
does not claim that a configured template has a current installation receipt or
that its engine has passed a launch test.

Allowances use CPU cores and bytes consistently. Incus allowances subtract host
reserves and apply configured contribution limits; available capacity also
honors existing commitments and slot limits. Worker allowances come from the
accepted local enrollment budget. Worker free capacity and running-workspace
counts are unknown until its runtime reports them; they are not inferred from
an idle job slot. Workload capabilities describe the execution adapter, not
provider authentication, GPU readiness or a configured GitHub runner pool.

No reinstallation is needed for existing hosts or workers. This inventory
adoption is not a migration from SSH/Incus to the outbound Cloudflare fleet.
That migration still requires the identity, state-ownership and fencing
qualification in [the implementation plan](IMPLEMENTATION-PLAN.md). Enrolling
the same dedicated server as a second contributor would double-count capacity
and does not migrate its Incus workspaces.
