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

The device explicitly uses `io.bus=virtiofs` and `io.cache=unsafe`. The host must already provide `virtiofsd`. The adapter currently requires an explicitly stopped, stateless VM; dynamic attachment is not yet qualified. Incus 6.0.5 additionally rejects unmanaged directory disks with `migration.stateful=true`. The adapter records the exact prior local setting, then sets `migration.stateful=false` on only the selected VM. Clean detach restores the prior local value, or removes the override so the VM inherits its profile again. Profiles remain untouched. A changed override is refused rather than overwritten. **RAM hibernation is disabled while attached**; coordinated file/RAM checkpointing remains unqualified. [Incus disk devices](https://linuxcontainers.org/incus/docs/main/reference/devices_disk/), [Incus 6.0.5 disk validation](https://github.com/lxc/incus/blob/v6.0.5/internal/server/device/disk.go)

Incus calls the selected cache mode `unsafe`; in the qualified version it launches virtiofsd with `--cache=always`, without enabling a writeback flag. Guest page caching permits executable and shared-mmap workloads. The attachment's exclusive custody is therefore essential: all Engine Box writes outside that VM are blocked, detach requires a cleanly stopped guest, and checkpointing follows detach. Never share the same writable checkout with another VM. The runtime cannot guarantee coherence against raw host or administrator mutations outside these controls. [Incus virtiofsd cache mapping](https://github.com/lxc/incus/blob/v6.0.5/internal/server/device/device_utils_disk.go), [virtiofsd cache modes](https://gitlab.com/virtio-fs/virtiofsd/-/blob/v1.13.2/src/passthrough/mod.rs)

A project-specific VM can receive its checkout before it starts, then remain pre-attached for later pause/resume. Dynamically attaching a new checkout to an already frozen generic VM remains unqualified; the adapter currently requires a clean stop and refuses unclaimed warm-pool reserves.

The separate [allocation preparation contract](LAYERFS-ALLOCATION.md) reserves a stopped VM before any external checkout is attached. Preparation is not a completed automatic attachment transaction; finalization and credential delegation need their own verified handoff.

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

`incus-verify` checks the original VM UUID, ownership markers, exact directory device, original Docker process/start time, host boot ID, mapped mount identity, guest filesystem type and the selected guest user's access. It also creates a unique private probe directory in the checkout, writes and directly executes a tiny script as that user, then deletes its exact probe file and directory. Creation, execution or cleanup failure refuses readiness. It does not start or repair a guest. A successful attach is not readiness until this guest check succeeds. Normal success and failure attempt exact probe cleanup; a timeout, SIGKILL or stuck filesystem can leave the probe for inspection. No recursive cleanup or automatic replay is attempted.

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

The caller holds the storage workspace lease, and the adapter additionally acquires the shared Engine Box host lease before changing Incus. A schema-3 private attachment receipt, including operation intent and the prior local stateful setting, is written before mutation. Schema 2 remains readable. The VM UUID, container ID/PID/start time, host boot ID, export path and mount identity are pinned. Partial failure leaves an uncertain attachment and the VM markers intact for inspection. A changed or restarted owner is refused rather than rebound silently.

### Reconcile completed attachment effects

An operator can inspect an interrupted attach/detach without replaying it:

```sh
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json \
  incus-reconcile checkout-a

# Only when the inspection proves the complete original effects:
sudo ebox --host local storage layerfs --config /etc/enginebox/layerfs.json \
  incus-reconcile checkout-a --apply
```

The default command returns observed identities, `reason`, `next_action`, `target_state` and `can_apply`; it does not change the attachment receipt. `--apply` also never changes Incus, starts/stops a VM, re-exports a mount, runs a checkpoint or removes a file. It can only finalize private metadata when two consecutive observations prove that the original operation already completed:

- **Attach completed:** the exact owned VM is stopped and stateless, both custody markers and the exact directory device remain, `migration.stateful=false`, and the original live container's mapped mount matches the host export and saved device/inode identity.
- **Detach completed:** the exact VM is stopped and stateless, its device and both markers are absent, the exact prior local stateful setting is restored, both export/mapped paths are absent, and the original container is still running with its original identity. The private writable checkout is retained; checkpoint it explicitly before closing.

Successful metadata finalization retains the old receipt and observations in a private reconciliation intent record, then links that record from the updated attachment receipt. An orphan intent record alone does not mean finalization completed. Guest readiness still requires normal boot and `incus-verify`. A running VM, partial cleanup, inherited/foreign device, stacked mount, changed owner, missing resource or changing observation remains blocked and retains custody. The reconciler never treats an empty directory as a mounted checkout.

After a host reboot the command reports `host-restarted` and preserves all attachment state. It does not access a historical PID's mount namespace, remount the old overlay or guess that checkpoint restoration is acceptable. A changed/stopped/unavailable Docker container similarly returns `container-unverified`. Inspection still works without SDK artifacts or a running Docker socket, provided the original private configuration/bridge binding and receipts can be read. These outcomes provide a recovery diagnosis, not automatic host-restart recovery or power-loss durability.

Old schema-2 `attaching`/`detaching` records identify their operation and can be reconciled if the full proof is present. An old `uncertain` record has lost that intent and remains blocked, even when some resources appear complete. Schema-3 records retain attach/detach intent after an error. Older storage binaries reject the new schema instead of ignoring that information; older-controller rollback still has the separate [attachment compatibility gate](UPDATES.md).

The new fixture tests cover lost final receipts, preservation of private files, partial attach/detach, exact restoration of inherited/local stateful settings, reused PIDs after restart, foreign devices/owners, mount-table parsing and changes between observations. These are offline recovery-contract tests; the October 19 live qualification below predates reconciliation. Rebooting a shared host or stopping an existing workspace is not part of this test path.

On October 20, focused storage/CLI tests passed on macOS (34 tests) and Linux (35 tests, including actual kernel mount-table and `stat` identity inspection); strict Clippy passed on both. A separate actual Linux CLI fixture supplied missing SDK/socket paths and a synthetic previous boot ID. Both inspection and `--apply` returned `host-restarted`, retained the byte-identical attachment receipt and private-file sentinel, and created no reconciliation record. The temporary fixture was removed. No VM/container mutation or real host reboot was performed, and the opt-in live storage test remained skipped.

Automatic completion of partial external operations, restart of the mapped export after host reboot, coordinated file/RAM snapshots, cross-node movement and hostile-tenant qualification remain outstanding. A VM snapshot alone does not include this checkout. The existing LayerFS preview durability and filesystem limits still apply; native inference sessions, game-engine workloads and sustained I/O have separate qualification requirements.

With guest kernel `6.8.0-147-generic` and the initial `io.cache=none` candidate, virtiofs file creation, directories, rename, chmod and links worked, but direct script execution returned `EFAULT` (`Bad address`). Explicit interpreter execution worked. This matches an independently reported Ubuntu 6.8/virtiofs execution issue, but does not identify one exact kernel patch as the cause. Guest-only HWE kernel `7.0.0-38-generic` made direct script and ELF execution pass with cache disabled, but writable shared mmap still failed with `ENODEV`. These failed candidates are retained as qualification evidence. Guest kernel version and direct executable/mmap tests are part of qualification; readable files alone are insufficient. [Incus execution report](https://discuss.linuxcontainers.org/t/cant-execute-files-on-mounted-host-directory/24630)

The reproducible fixture harness is [incus-attachment.mjs](../research/layerfs-validation/incus-attachment.mjs). It requires an explicit live-test flag, a fresh storage root, an exact disposable VM marker and a fresh private report path. It exercises guest UID/GID 1001 file operations, branch isolation, lifecycle refusals, pause/resume, clean detach/checkpoint, reattachment and cold VM restart. A failed fixture is retained for evidence export and ownership-checked cleanup.
## Live qualification — 2026-10-19

The final disposable run passed all **53 recorded commands in 33.905 seconds**. It used Linux host kernel `7.0.0-30-generic`, Incus `6.0.5`, virtiofsd `1.13.2`, bindfs `1.14.7`, the pinned LayerFS revision `968a5d0be0ea3e9456b06f9148b65523138e9490`, and an Ubuntu 24.04 guest upgraded only inside the fixture to HWE kernel `7.0.0-38-generic` (`linux-image-generic-hwe-24.04=7.0.0-38.38~24.04.4`). The actual virtiofsd process arguments confirmed `--cache=always` and no writeback flag.

The passing cases included guest UID/GID 1001 writes, directory creation, rename, chmod, symlinks and hardlinks; direct script and ELF execution; writable shared mmap with flush; another branch's isolation; attached-operation refusals; pause/resume preserving the guest process ID and start time; clean stop/detach; checkpoint/remount and readback of normal and mmap writes; cold reattachment and readback; and restoration of both an inherited stateful setting and an explicit local `false` setting. Both test workspaces closed cleanly.

| Operation | Observed command time |
| --- | ---: |
| Attach | 0.774 / 0.715 s |
| Guest verification including write/execute/delete probe | 0.261 / 0.276 s |
| Pause | 0.039 s |
| Resume the already running VM | 0.045 s |
| Clean stop | 0.609 / 0.618 s |
| Detach | 0.527 / 0.476 s |
| File checkpoint after detach | 0.443 / 0.447 s |
| LayerFS remount after checkpoint | 1.152 s |

These are individual observations from a cached synthetic fixture, not percentiles, large-project measurements, complete agent startup times or game-engine build benchmarks. The start-command acknowledgment is not cold-boot readiness; no standalone cold-boot latency is claimed here.

The fixture used a 2-CPU/4-GiB VM and a private Docker daemon capped at 4 CPUs/6 GiB; LayerFS sandboxes had 1 CPU/1 GiB each within that daemon. Admission preserved 12 CPUs, 40 GiB host memory and 32 GiB storage-pool reserve. The host's existing Docker `29.1.3` exposed API `1.52`, so the SDK's required API `1.54` could not run on it. Qualification instead used an explicitly selected, task-owned Docker `29.5.3` Unix socket with private data/network state. This privileged, host-PID-namespace validation daemon kept the LayerFS processes in the host kernel without changing the default daemon. It is a qualification fixture, not evidence of hostile-tenant isolation or a production installer recommendation. A normal deployment needs a compatible Docker API and the documented same-kernel topology; the runtime checks the selected socket/API before mutation.

The initial 9p and cache-disabled candidates were rejected for the failures described above. A later run passed the storage checks but sent its final ACPI shutdown before `systemd-logind` registered the power-button input. After the original Incus stop operation finished, normal shutdown succeeded. The final harness waits for the guest system to finish booting (running or degraded) and for logind to be active before exercising clean shutdown. Storage readiness and whole-OS shutdown readiness are distinct.

Private reports, receipts, source hashes and daemon arguments were exported before teardown. Ownership-checked cleanup then deleted the fixture VM, normally unmounted all three retained mapped host exports, normally stopped the private Docker daemon and verified all 16 inner container processes exited, removed the three owned outer containers and three owned outer volumes, and removed the private daemon data/socket directories plus synthetic SDK runtime state. Final checks found no experiment VM, containers, volumes or host mounts. The default Docker daemon stayed at `29.1.3` with the same process ID. Only inert private reports/configurations, source/build artifacts, pinned test binaries and image cache were retained. No existing user VM, provider home, host profile or default Docker service was changed.
