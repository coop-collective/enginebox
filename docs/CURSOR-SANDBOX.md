# Cursor Agent sandbox on Ubuntu 24.04

Cursor Agent creates a user namespace in its bundled `cursorsandbox` helper
before selecting Landlock or a bubblewrap fallback. Ubuntu's AppArmor profile
for `/usr/bin/bwrap` alone does not authorize that first step. With the normal
namespace restriction enabled, the helper can enter `unprivileged_userns` and
fail with `sys_admin`/`uid_map` permission errors.

The native CLI exposes a model-free `agent sandbox run` command and sandbox
controls. Cursor describes namespace-based Linux isolation, Landlock/bubblewrap
backend diagnostics, and the boundary between workspace and other paths in its
[CLI parameters](https://cursor.com/docs/cli/reference/parameters) and
[sandbox documentation](https://cursor.com/docs/agent/security/run-modes).
Cursor support also identifies the missing AppArmor `userns` rule for its CLI
helper in [this first-party diagnosis](https://forum.cursor.com/t/agent-cli-linux-sandbox-preflight-fails-unshare-eperm-unless-run-under-strace-apparmor-restrict-unprivileged-userns-1/160039/5).

## Scoped repair

The Rust `enginebox-guest cursor-sandbox` helper creates an Engine Box-owned AppArmor profile attached
only to the configured guest home's Cursor helper path:

```text
/home/agent/.local/share/cursor-agent/versions/*/cursorsandbox
```

The version component is the only wildcard in the attachment. The profile grants
the namespace, mount, capability, networking and signal permissions the helper
needs to construct Cursor's native sandbox. Same-profile signals allow cleanup
of helper children. Landlock/namespace controls enforce the native filesystem and
network policy after setup; the AppArmor profile itself is not a replacement for
those controls. The outer VM remains the isolation boundary for an untrusted
workspace user, who can replace files in their own home.

The repair does not change global namespace sysctls, disable AppArmor, switch the
native sandbox off, install a setuid helper, or wrap commands in `strace`. It does
not install Cursor's enterprise `.deb`: the inspected `0.6.2` package also ships
a sysctl configuration and repository setup, and leaves the CLI's `userns` rule
commented out. Engine Box uses its own explicit helper policy instead.

Guest setup installs the required Ubuntu AppArmor/bubblewrap tools when Codex or
Cursor is enabled. After the Cursor client installation, it applies the scoped
policy and requires the native model-free preflight to pass before creating a
ready base. A preserved local policy that breaks the preflight cannot silently
produce a ready template.

## Repair an existing authorized private guest

Run the following **inside that guest as root**, with its actual configured user,
home and workspace. The helper is included in the native release and staged into
`/usr/local/bin` during guest preparation:

```sh
enginebox-guest cursor-sandbox \
  --home /home/agent --user agent --workspace /workspace --apply
```

Without `--apply`, the command validates configuration and native client identity
and reports the planned profile. A bounded `--version` check rejects unrelated
programs also named `agent` before any policy changes or sandbox subcommand.
Applying also executes `agent sandbox run /bin/true` as the
ordinary guest user using a clean environment. No model request or provider login
is needed. Existing clients, account state, services and VM execution state are
not reinstalled or restarted.

The helper refuses malformed attachment paths, a different user's home, symlink
or nonregular profile files, writable-by-others profiles, an unexpected existing
attachment, local disable/complain overrides, or an already loaded vendor CLI
profile requiring reconciliation. An existing matching Engine Box profile is
preserved, including operator edits, and must load in enforce mode and pass the
native preflight. It is qualified on Ubuntu 24.04/AppArmor 4, not arbitrary Linux
distributions or desktop operating systems.

## Verification

During v0.2 qualification on 2026-10-19, Cursor Agent `2026.10.16-e3cb207` in a disposable Ubuntu 24.04
workspace failed its native preflight with `uid_map` permission errors before
this profile was loaded. The same ordinary-user command passed afterward.

A script executed through `agent sandbox run` then verified:

- Creating a file inside `/workspace` succeeded.
- Creating a file in the ordinary user's home outside the workspace was denied.
- An outbound socket connection was denied with the native default-deny policy.
- Native diagnostics reported `CURSOR_SANDBOX=native`, backend `fully_enforced`,
  and `enginebox_cursor_sandbox (enforce)`.
- `kernel.apparmor_restrict_unprivileged_userns` stayed `1`; ordinary
  `unshare -Ur /bin/true` still failed.

The historical Python helper applied twice and passed preflight both times. Offline
regressions cover constrained attachment construction, dry runs, idempotency,
preservation of matching operator edits, local override/symlink/foreign-profile
rejection and inclusion in bootstrap assets. This verifies the tested sandbox
startup and boundaries, not all possible escape boundaries or provider inference.
See [VALIDATION.md](reports/VALIDATION.md) for separate native MCP/inference qualification.
