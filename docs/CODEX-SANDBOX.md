# Codex sandbox on Ubuntu 24.04

Cursor Agent needs a separate application-scoped helper profile. See
[Cursor sandbox setup](CURSOR-SANDBOX.md); the distro bwrap profile below does
not authorize Cursor's earlier namespace setup step.

Guest preparation installs Ubuntu's `bubblewrap`, `apparmor-profiles` and
`apparmor-utils` packages when Codex is enabled. On guests with AppArmor active,
it installs and loads the distribution's `bwrap-userns-restrict` profile before
checking the native Codex sandbox. This follows OpenAI's documented
[Linux sandbox prerequisites](https://learn.chatgpt.com/docs/sandboxing#prerequisites).

Ubuntu 24.04 restricts unprivileged user namespaces by default. Without the
distribution helper and profile, Codex's bundled bubblewrap can fail with
`Operation not permitted` even when authentication and inference work. The
AppArmor audit log can show capability denials under `unprivileged_userns`.
Ubuntu describes the restriction and application-specific profiles in its
[24.04 release notes](https://documentation.ubuntu.com/release-notes/24.04/).

The distribution profile grants `/usr/bin/bwrap` the permissions needed to
construct its sandbox, then transitions sandboxed commands into a child profile
that denies capabilities. See the
[upstream AppArmor profile](https://gitlab.com/apparmor/apparmor/-/blob/master/profiles/apparmor/profiles/extras/bwrap-userns-restrict).
Engine Box does not disable the guest or host namespace restriction, grant
Codex a general AppArmor exemption, or switch its native sandbox to
`danger-full-access`.

Preparation preserves an existing regular profile at
`/etc/apparmor.d/bwrap-userns-restrict`. It rejects symlinks, other file types,
and local `disable` or `force-complain` overrides instead of replacing an
operator's policy. A conflicting local profile must be reconciled by its
operator. Successful provisioning requires the model-free sandbox check to
pass, so a preserved but incompatible policy cannot silently produce a ready
template.

## Model-free check

Run this as the configured ordinary guest user, from its workspace:

```sh
cd /workspace
codex -c 'sandbox_mode="workspace-write"' sandbox -- /bin/true
```

This uses Codex's own sandbox command and requires no provider login or model
request. The syntax above was verified with Codex CLI `0.163.0`; this version
selects the Linux sandbox automatically, so do not add the older `linux`
subcommand. Guest preparation runs the check with the configured user, home and
workspace rather than assuming these example paths.

For a stronger disposable-workspace check, run a local script through the same
command which verifies that writing within the workspace succeeds, writing to
an otherwise writable directory outside it fails, and creating or connecting an
outbound network socket fails with
`-c sandbox_workspace_write.network_access=false`. Socket creation itself can
be denied; do not assume a socket exists before checking the failure.

## Verification

On 2026-10-19, a disposable Ubuntu 24.04.5 VM with Linux `6.8.0-147-generic`,
Codex `0.163.0`, bubblewrap `0.9.0-1ubuntu0.3` and AppArmor
`4.0.1really4.0.1-0ubuntu0.24.04.9` passed all three filesystem/network checks
after the repair. The same installation block passed twice, preserved a local
profile edit, and rejected a deliberate local disable override. Generic
unprivileged `unshare -Ur` remained denied and
`kernel.apparmor_restrict_unprivileged_userns` remained `1`.

The narrow repair also passed the native model-free check in two existing
workspaces without restarting them or changing their authentication. These
checks verify sandbox startup and the tested boundaries; they do not establish
provider entitlement, successful post-repair inference, or every possible
sandbox escape boundary. No reboot test was performed during this repair.

## Package-manager caches

The workspace-write sandbox leaves the guest user's home read-only, so corepack,
pnpm, npm, bun and cargo would fail to write their caches before a project's
tests run (`ERR_PNPM_STORE_DIR_OPEN_OPERATION_LOCK`, `EROFS` under
`~/.cache/node/corepack`). Before each Codex run Engine Box creates
`~/.cache`, `~/.npm`, `~/.local/share/pnpm`, `~/.bun/install/cache`,
`~/.cargo/registry` and `~/.cargo/git` and passes them as
`sandbox_workspace_write.writable_roots` (in a profile's generated config, or as
a `-c` override). The rest of the home stays read-only; symlinked cache paths
are never offered. The caches persist on the workspace between runs.
