# LayerFS checkout attachment to Incus

The adapter exports a single private checkout to an existing, owned Linux VM at `/project`. The LayerFS daemon, Store openers and mapped export stay in the host's Linux kernel. The VM receives ordinary file operations through a directory disk; it never receives the backing SQLite Store, Docker socket, SDK keys, or another workspace's files. Native provider homes remain on the VM's private disk.

The path is:

```text
Host-local project Store
  → workspace's LayerFS mount in its Docker sandbox
  → bindfs ownership projection in that same sandbox
  → host export of that same live mount
  → Incus virtiofs directory disk
  → VM /project
```

The pinned SDK projects every inode as UID 501/GID 20. Engine Box's normal guest identity is UID/GID 1001. The adapter uses **bindfs 1.14.7** with `--map=501/1001:@20/@1001` to translate ownership without copying or recursively changing the checkout. The selected immutable Docker image must already contain that version; the qualification Dockerfile includes it. This extra FUSE layer adds work to file operations; attachment qualification is not a large-build performance claim. [bindfs ownership mapping](https://bindfs.org/docs/bindfs.1.html)

Export uses Linux `open_tree` and `move_mount` to transfer the existing mapped mount out of the Docker mount namespace. A short-lived child enters the source namespace, clones the mount using an already opened descriptor, restores the host namespace and attaches it to the owned export directory. The caller never changes namespaces. The host must support these syscalls; legacy `mount --bind /proc/PID/root/...` was refused in qualification and is not a fallback.

The device explicitly uses `io.bus=virtiofs` and `io.cache=none`. The host must already provide `virtiofsd`. The adapter currently requires an explicitly stopped, stateless VM; dynamic attachment is not yet qualified. Incus 6.0.5 additionally rejects unmanaged directory disks with `migration.stateful=true`. The adapter records the exact prior local setting, then sets `migration.stateful=false` on only the selected VM. Clean detach restores the prior local value, or removes the override so the VM inherits its profile again. Profiles remain untouched. A changed override is refused rather than overwritten. **RAM hibernation is disabled while attached**; coordinated file/RAM checkpointing remains unqualified. [Incus disk devices](https://linuxcontainers.org/incus/docs/main/reference/devices_disk/), [Incus 6.0.5 disk validation](https://github.com/lxc/incus/blob/v6.0.5/internal/server/device/disk.go)

A project-specific VM can receive its checkout before it starts, then remain pre-attached for later pause/resume. Dynamically attaching a new checkout to an already frozen generic VM remains unqualified; the adapter currently requires a clean stop and refuses unclaimed warm-pool reserves.

The initial 9p candidate was rejected during live qualification: Incus 6.0.5 ran QEMU as UID/GID 65534 without effective capabilities, while its 9p directory driver used `security_model=passthrough`. The mapped checkout correctly presented UID/GID 1001, and direct writes through bindfs succeeded as 1001, but guest creation failed as both 1001 and root. Readability and `test -w` alone did not prove write support. The adapter uses the existing virtiofs service instead of widening checkout permissions or elevating QEMU. [Incus 6.0.5 QEMU directory configuration](https://github.com/lxc/incus/blob/v6.0.5/internal/server/instance/drivers/driver_qemu_templates.go)

## Operator sequence

Provision the qualified LayerFS configuration and image first. The target must be an Engine Box VM with the expected `user.enginebox.owner` value, not a reusable clean base or an unclaimed warm-pool VM. Create the private branch using the ordinary LayerFS `fork` command, then:

```sh
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json \
  incus-attach checkout-a --project enginebox --instance ebox-private \
  --owner enginebox --uid 1001 --gid 1001

# Start the VM through its normal fleet lifecycle.
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json \
  incus-verify checkout-a
```

`incus-verify` checks the original VM UUID, ownership markers, exact directory device, original Docker process/start time, host boot ID, mapped mount identity, guest filesystem type and the selected guest user's access. It does not start or repair a guest. A successful attach is not readiness until this guest check succeeds.

While attached, storage commands, dependency installation, file checkpoints, remount, close, recovery and fork-from operations are refused for that checkout. Changes must come from its VM. Engine Box also blocks hibernation, VM snapshots, restore and copying a marked VM or snapshot. Normal pause/resume and clean stop preserve the export; the external filesystem must remain available. Do not manipulate these devices with raw Incus commands during agent work.

To checkpoint file changes, stop the VM through its normal lifecycle, then:

```sh
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json \
  incus-detach checkout-a
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json \
  checkpoint checkout-a --message 'Changes after clean VM shutdown'
```

The same checkout can then be attached again. Completed attachment receipts are retained in private history. Removing the VM's external device precedes normal host unmount and bindfs unmount; there is no lazy unmount or forced discard. CPU/RAM used by bindfs remains within its existing Docker sandbox's limits. VM capacity remains governed by fleet admission.

## Failure boundary

The caller holds the storage workspace lease, and the adapter additionally acquires the shared Engine Box host lease before changing Incus. A schema-2 private attachment receipt, including the prior local stateful setting, is written before mutation. The VM UUID, container ID/PID/start time, host boot ID, export path and mount identity are pinned. Partial failure leaves an uncertain attachment and the VM markers intact for inspection. A changed or restarted owner is refused rather than rebound silently.

Automatic repair of an interrupted attachment, restart of the mapped export after host reboot, coordinated file/RAM snapshots, cross-node movement and hostile-tenant qualification remain outstanding. A VM snapshot alone does not include this checkout. The existing LayerFS preview durability and filesystem limits still apply; native inference sessions, game-engine workloads and sustained I/O have separate qualification requirements.

The reproducible fixture harness is [incus-attachment.mjs](../packages/layerfs-validation/incus-attachment.mjs). It requires an explicit live-test flag, a fresh storage root, an exact disposable VM marker and a fresh private report path. It exercises guest UID/GID 1001 file operations, branch isolation, lifecycle refusals, pause/resume, clean detach/checkpoint, reattachment and cold VM restart. A failed fixture is retained for evidence export and ownership-checked cleanup.
