# Pinned updates and rollback

Controller and dashboard updates use a stable native `enginebox-supervisor` process. It owns the public loopback ports; versioned controller and Node processes use private dynamic ports. Persistent provider homes, credentials, run state and settings live outside release directories.

The currently implemented guarantee is a health-checked, drained controller/dashboard cutover while the supervisor stays running. Existing native processes inside Incus workspaces are not restarted. Changes to the supervisor itself, the operating system, Node runtime, Incus or kernel are separate maintenance operations and are not described as downtime-free. A hardware or supervisor failure is recovery, not a seamless update.

## First transition from the Python controller

The v0.2 Python controller has neither this ownership lock nor the drain/standby protocol. It cannot participate in the Rust rolling handoff. The first migration requires a planned control-plane interruption: stop new submissions, let legacy jobs and authentication sessions finish, stop the Python API, preserve its private controller/provider state, then activate and verify the Rust controller. Keep the old installation and an owner-only state backup until validation finishes. Existing Incus workspaces and native processes inside them should remain running; stopping the old API must not be used as a reason to stop VMs.

This migration has not been performed merely by testing two Rust controller versions. Do not run Python and Rust writers against the same credential/session state. Only subsequent compatible Rust controller/dashboard releases qualify for the rolling-update protocol below. A separately tested legacy adapter would be needed to remove the initial control-plane interruption.

The Unix supervisor's optional `state_dir` can point directly at the legacy controller state root after its writers stop. Select the parent of the old Python `jobs/` directory. This preserves native home paths, account connections and installed-client links while keeping the new immutable installation separate. All subsequent candidates, rollbacks and supervisor restarts use the same configured root. Keep a private backup, but do not restore stale provider refresh credentials over a newer live state as a routine rollback. Path preservation is not proof of provider or schema compatibility; qualify those before cutover.

## Stage an exact version

Use the terminal bootstrap with the desired exact version, or stage reviewed local artifacts directly:

```sh
enginebox-install --prefix /absolute/prefix stage \
  --version 0.4.0 \
  --manifest /downloads/enginebox-0.4.0-TARGET.json \
  --manifest-sha256 TRUSTED_MANIFEST_SHA256 \
  --archive /downloads/enginebox-0.4.0-TARGET.tar.gz
```

`0.4.0` is an example target, not a claim that such a release is published. The manifest's version and target must exactly match. Archives are size-bounded, checksum-verified and extracted with traversal, links, special files, case-alias duplicates and reserved Windows filenames rejected. A staged version cannot be overwritten. Another update process holds an exclusive filesystem lock; concurrent changes are rejected.

Staging does not change `current.json` or stop services. On the first installation, `activate --version VERSION` selects the initial release. Offline activation is refused while the supervisor holds its lifetime lock.

## Apply and roll back

```sh
enginebox-install --prefix /absolute/prefix update \
  --version 0.4.0 --token-file /absolute/prefix/etc/api-token

enginebox-install --prefix /absolute/prefix rollback \
  --token-file /absolute/prefix/etc/api-token
```

The default management address is `http://127.0.0.1:8765`; use `--url` for another local port. Tokens are read from a private file and never passed as CLI argument values. `rollback` targets the previous active release; `update --version VERSION` selects any retained, compatible version explicitly. `status` prints the persisted current/previous versions.

The update sequence is:

1. Validate the installed candidate's platform, pinned version and state-schema compatibility.
2. Start its controller in standby and its optional dashboard on unused loopback ports. Probe exact controller version/schema and dashboard health. Standby does not recover jobs, refresh credentials or acquire mutation ownership.
3. Pause new API mutations at the supervisor gate; existing forwarding mutations finish before draining begins. Agent-list, usage and account observations also wait because native provider commands can refresh credentials. Pure reads continue to the old controller. At most 256 ownership-dependent requests can wait; requests over this limit or beyond the bounded drain wait receive a retryable error. Authentication is checked before admitting these requests. Dashboard POSTs are retained while their actual API requests wait, avoiding a nested proxy deadlock.
4. Ask the old controller to drain. It stops admitting new work and waits for active jobs, provider authentication sessions, background work and in-flight mutations. Only then does it release the exclusive state-owner lock. A timeout restores the old controller and leaves its running work intact.
5. Activate the candidate under the same state lock and confirm its active readiness. If activation fails, drain any uncertain candidate ownership before restoring the old controller.
6. Atomically persist the current/previous version pointer, switch the in-memory upstream, and release queued requests to the new controller. Outstanding old streamed responses retain their old process until they finish.

Retained release directories supply old hash-named `/_next/static/` JS/CSS/media assets and build-ID manifests when the new dashboard returns 404. This allows an already-open browser to fetch its previous release's chunks. Fallback does not serve HTML, state, configuration, server code or arbitrary JSON. Do not delete old release directories until their rollback and browser-asset retention window has passed; automatic pruning is intentionally absent.

The stable supervisor creates authenticated per-child loopback lifetime sockets. If it exits unexpectedly, the controller detects EOF and drains before releasing state; the replacement supervisor retries a busy ownership lock for the configured deadline. This is designed to avoid orphan controller writers. A full supervisor crash still interrupts its listener connections and can exceed the recovery deadline when work remains active.

## State compatibility is a rollback gate

Manifests declare the state schema a release writes and the range it can read. An online update or rollback is accepted only when it can read the active schema **and writes the same schema**. Version numbers alone do not make state reversible. No implicit migration is performed, and no snapshot of live credential/session data is mistaken for a safe rollback.

An incompatible schema change needs a separate, explicitly designed migration, tested restore path and maintenance window. Keeping an older executable is insufficient if a newer version has irreversibly changed its state.

## LayerFS attachment compatibility

Attached Incus VMs require the attachment-aware generation of both fleet/controller and node tools. A rollback to older fleet binaries is unsupported until every checkout is cleanly detached. The LayerFS workspace receipt upgrade to schema 2 makes older storage CLIs refuse those receipts; it does not protect the separate Incus lifecycle API. Older fleet tools ignore the new VM attachment markers and could snapshot, hibernate, restore or copy a VM without coordinating its external files.

Incus checkout attachment therefore remains a node-administrator operation in this release, with no automatic controller/API attachment. Before that promotion, release installation and rollback must enforce a minimum compatible controller/node generation against active attachment inventory. That is an explicit release gate requiring a migration/compatibility design and tests; the shared controller state schema has not been changed implicitly to claim this enforcement.

## Verification

The installer crate exercises archive/hash/version/target rejection, ownership markers and update locks, rollback schema gates, a failed candidate health check, failed activation, drain timeouts, request continuity with queued mutations and credential-refresh observations, authenticated admission and management routes, blocked direct ownership routes, retained browser assets, and a deterministic release-builder/native-installer round trip. A Windows-only regression additionally grants explicit Everyone access to a disposable directory and verifies that private-directory creation replaces that DACL completely.

On October 19, 2026, the opt-in process qualification also passed on macOS arm64 using actual controller binaries `0.3.0` and an isolated `0.3.1-test.1` build, the actual supervisor and the real production Vinext dashboard. Its final run observed 263 successful GETs and 60 successful POSTs across update and rollback, with zero request errors. It checked exclusive state ownership; retained completed jobs, templates and private provider scratchnotes; an existing authenticated browser cookie; a prior release's hashed asset; rejection of a mismatched candidate version; recovery after an actual candidate's incompatible state prevented activation; and recovery after killing/restarting the supervisor. The latter is crash recovery, separately excluded from the request-continuity claim. All state and processes were disposable, and fake SSH/provider programs prevented contact with fleet hosts or inference providers. Service-manager execution, Windows machine installation and the legacy Python migration remain separate qualification gates.

The same qualification launched the actual `enginebox-fleet-mcp` executable and exchanged newline-delimited stdio JSON-RPC frames using MCP protocol `2025-11-25`: initialize, initialized notification, tool discovery, catalog read, template registration and another catalog read. The same MCP process continued making successful authenticated catalog calls after the update, rollback and supervisor restart. This checks the real stdio-to-HTTP transport without using inference quota.

A follow-up run qualified the optional external state root with the real supervisor, the same `0.3.0`/`0.3.1-test.1` controller fixtures, the production dashboard and native stdio MCP. It observed 128 successful GETs and 47 successful POSTs with zero request errors across update/rollback, preserved the external ownership lock, jobs, templates, private scratchnotes and browser session, and recovered after supervisor restart. The installation's default state root never acquired a controller owner lock. This remains a disposable fixture qualification, not a claim that the legacy production cutover has occurred.

```sh
cargo test -p enginebox-install
cargo clippy -p enginebox-install --all-targets -- -D warnings
sh -n packages/enginebox-install/assets/install.sh

# Explicit real-process and stdio MCP qualification; builds the second version in .state,
# without modifying the repository workspace version. Requires built dashboard
# artifacts to include the browser-cookie/old-asset checks.
cargo build -p enginebox-controller --bin enginebox-controller
cargo build -p enginebox-cli --bin enginebox-fleet-mcp
cargo test -p enginebox-install --test real_process_updates -- --ignored --nocapture
```

Reference packaging research: [cargo-dist installer formats](https://axodotdev.github.io/cargo-dist/book/installers/index.html) cover shell, PowerShell and native package-manager distribution. Engine Box's state ownership, draining and rollback protocol is application-specific; downloading a new binary alone does not provide it. [Windows Task Scheduler](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/schtasks-create) documents the user task mechanism used here.
