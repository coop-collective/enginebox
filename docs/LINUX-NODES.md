# Register a provisioned Linux server

Windows WSL workers and managed Incus servers use the native controller's same
one-time invitation, scoped node credential, session/epoch, heartbeat and
revocation authority. Their execution adapters remain different: Windows runs
private LayerFS template workspaces, while an Incus server keeps its existing
controller-approved SSH/local transport and VM ownership.

The Linux installer adopts a provisioned server. It does not repartition disks,
install Incus, change networking, replace VM state or install provider tools on
the host. A new bare server first needs the existing inspected host plan/apply
flow, an approved fleet entry and a root-owned `enginebox-host` helper. It must
be Linux x86-64 with systemd, curl, Python 3 and ordinary core utilities.

## Enable managed registration

The operator enables `managed_registration = true` on the approved host entry
in the controller's private fleet configuration, then loads it through a drained
controller update or configuration restart. This policy fails closed: that host
cannot admit new work until its bound node is enrolled, connected and ready.
Existing VMs and workspaces are preserved. Inspection and administrative cleanup
remain available.

This field is deliberately omitted for legacy unmanaged hosts. Older controller
versions reject the field instead of silently ignoring the registration policy,
so downgrading an adopted server requires an explicit migration decision.

In **Compute → Add machine**, choose **Linux** and the configured server. The
controller binds its one-time code to that host's configuration, Incus project
and pool, and machine identity observed through its approved transport. The
installer cannot pick another host or turn a Windows invitation into an Incus
registration. Pending invitations are renewed under the same identity.

Copy the generated Bash command onto that server. It verifies a pinned release
installer before invoking it with root privileges. The installer verifies its
native executable, inspects the local machine and Incus helper, enrolls through
the public HTTPS controller, and installs a boot-enabled systemd service. The
code is passed to the native registration command through stdin. Re-running the
same installation keeps its credential, including a pending enrollment whose
acknowledgment was lost. A different controller or node identity is rejected.

The existing host reserves and contribution limits are authoritative during
adoption; the installer reports them rather than changing them. Compute displays
one physical machine and one capacity budget, even though the registration has
its own private credential ID. An absent, expired, revoked or mismatched binding
prevents new admission. Losing its connection does not stop existing VMs.

## Service and private state

Default locations:

- `/opt/enginebox/managed-node/bin/enginebox-node`: verified native executable.
- `/opt/enginebox/managed-node/private/host.json`: private identity and credential.
- `/opt/enginebox/managed-node/private/status.json`: redacted heartbeat evidence.
- `enginebox-managed-node.service`: starts at boot and restarts after failure.

The installer refuses unrelated directories, symlinks, unsafe ownership and an
unrelated service under the same name. It never removes the private identity to
recover a failed registration. If a fresh ready heartbeat cannot be confirmed,
it reports that failure while retaining the installed service and registration.

Use `systemctl status enginebox-managed-node.service` and
`journalctl -u enginebox-managed-node.service` for diagnostics. Do not paste the
private registration file into logs or support messages. Reinstallation is not a
substitute for resolving a revoked credential or a changed host binding.

## Publishing and qualification

The **Build registered Linux node installer** workflow tests a disposable Linux
installation, native enrollment behavior and the pinned binary before publishing
an immutable preview on manual dispatch. Its assets include
`install-linux-node.sh`, `enginebox-node`, `SHA256SUMS` and `provenance.json`.
Configure the dashboard's private `ENGINEBOX_LINUX_NODE_INSTALLER_URL` and
`ENGINEBOX_LINUX_NODE_INSTALLER_SHA256` with the verified asset. These settings
load when the stable supervisor starts; use the documented drain procedure
before a configuration restart.

The disposable installer test proves identity preservation, ownership checks,
checksum rejection, service configuration and readiness handling with controlled
fixture programs. It does not prove that a real machine rebooted, that Incus is
installed, or that GPU rendering or a game engine works. Those require their
own machine and workload checks. The Linux role shares enrollment management
with Windows; it does not migrate legacy workspaces to the Cloudflare fleet.
