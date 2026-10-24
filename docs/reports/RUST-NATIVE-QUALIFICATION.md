# Rust native agent qualification

The ignored integration test in
[`live_native_runs.rs`](../../packages/enginebox-agents/tests/live_native_runs.rs)
exercises the real Rust `AgentService`, guest credential delegation and original
provider CLIs. It creates a separate, capacity-admitted Incus project and VM;
existing workspaces and the running controller are not test fixtures.

## What the test proves

Each available provider creates a small Python module and runs assertions. An
independent guest process checks the file and verifies that a conversation-only
canary was not written into the project. The test drains and recreates
`AgentService`, reloads its durable record, then resumes the exact native session.
The second turn recalls the canary without receiving it again, adds another
function and runs assertions. The test independently checks both functions and
the unchanged native conversation ID.

Optional browser qualification resumes that same conversation for a third turn:
the native agent uses the configured `enginebox-browser` MCP to navigate to a
guest-loopback fixture, fill a form, click its button and save a screenshot. The
receipt requires native MCP tool events, the displayed greeting, the same session
ID and an independently checked PNG file. No external website is involved.

Original controller homes are read only under their existing provider leases.
Only access material is copied into a fresh private candidate controller state;
refresh credentials stay in the original homes. The test refuses unknown expiry
or less than 30 minutes of remaining access. It does not change provider billing.
The candidate controller and guest retain private native histories for resume.
Every disposable VM/project is removed with ownership checks; the operator must
remove candidate access copies after exporting the sanitized receipt.

Native exit zero means the CLI completed, not that the requested code or browser
task succeeded. Independent file, conversation and screenshot proofs are required.
A provider quota rejection is recorded as blocked and is not retried. An accepted
quota-blocked test result does not qualify that provider's inference or resume.
Claude's `total_cost_usd`, when present, is saved as a native-reported cumulative
session estimate; resumed totals must not be summed or shown as account balance.

## Results on 2026-10-20

The real calls used original subscription access in disposable 2-vCPU / 2-GiB
Ubuntu workspaces, with 30-GiB root disks and Rust 0.3.0 candidate helpers. Each
clone passed a credential-free source check, current Rust guest preparation and a
reboot before credential delivery. These results qualify ordinary Incus guest
filesystems; they do not qualify inference on a LayerFS attachment.

| Native CLI | First coding turn | Exact resumed turn | Browser turn | Outcome |
| --- | ---: | ---: | ---: | --- |
| Codex 0.163.0 | 12.422 s | 8.372 s | 19.818 s | Code, conversation memory, native MCP and PNG passed |
| Claude Code 2.1.302 (`haiku`) | 7.869 s | 6.048 s | 12.828 s | Code, conversation memory, native MCP and PNG passed |
| Cursor 2026.10.16-e3cb207 | 49.082 s | Not attempted | Not attempted | Provider usage limit blocked inference |

Codex produced 12 browser MCP events and a 12,187-byte PNG; Claude produced six
browser MCP events and an 11,693-byte PNG. Both observed the expected greeting and
retained the exact conversation ID. The browser calls used the Rust guest MCP
launcher after exact legacy-entry migration. Claude's final native-reported
cumulative session cost estimate was $0.00554646. No billing settings changed.
These are individual end-to-end run observations, including service admission and
model latency, not inference benchmarks or service latency guarantees.

Cursor reached the native provider service, which reported a team usage limit
with a reset date of **2026-10-23**. The failed call was not retried. Installation,
authentication transport and session-ID allocation do not establish successful
inference; Cursor's tool use, exact resume and browser checks remain blocked.

The first Claude browser probe successfully used MCP and saved a relative
screenshot in the working directory. Its strict output-directory assertion
correctly failed. The final probe supplied an exact absolute screenshot filename
and independently verified that file. A separate follow-up was rejected by the
host lock before any run record existed; no admitted prompt was replayed.

After native work drained, the same disposable guest completed three samples of
each lifecycle path:

| Starting state | Full readiness samples (s) | Median (s) |
| --- | --- | ---: |
| Frozen RAM | 0.495, 0.505, 0.520 | 0.505 |
| Clean stop | 6.288, 6.313, 6.245 | 6.288 |

The timer covers the host-local Rust resume call plus helper, identity, network
and DNS readiness. It excludes lock wait, preparation, the preceding stop/pause,
HTTP, credential delivery and inference. Boot identity changed on all cold starts
and remained unchanged for all freeze/resume samples. Host load averages stayed
low (one-minute load 0.76 before and 0.90 after). These six smoke samples are
consistent with the earlier [0.512-second Rust warm median](RUST-BOOT-BENCHMARKS.md),
not a controlled Python comparison, saturation test or hibernation measurement.

The final fixture exited successfully, released guest leases and removed its
owned VM/project. All qualification access copies were removed after sanitized
receipt export. Original controller credentials and existing user VMs were not
modified. Local agents tests passed (22 native-contract cases and six service
cases), as did strict agents Clippy; the real fixture remains explicitly ignored
in ordinary test runs.

## Findings from the migration

The legacy prepared native-dev revision 3 snapshot lacked the distro bubblewrap
helper/profile required by the Ubuntu 24.04 Codex sandbox. An ordinary file write
worked, but the model-free sandbox failed with `Failed RTM_NEWADDR: Operation not
permitted`, and the kernel recorded `unprivileged_userns` capability denials.
Syncing helper executables alone cannot repair that old template. Running current
Rust guest preparation in the disposable clone and rebooting it repaired the
native sandbox. Existing warm reserves require explicit replacement with a newly
qualified template generation.

Claude's original headless command used `acceptEdits` with no permission prompts.
That combination still denied Python/test shell commands. The native command now
preapproves `Read`, `Write`, `Edit`, `Bash` and the configured Enginebox browser MCP
namespace, while retaining native deny rules and the normal permission mode. It
does not use bypass-permissions or bare mode. The latter would skip native memory
and subscription authentication. See the official
[headless CLI guidance](https://code.claude.com/docs/en/headless) and
[permission rules](https://code.claude.com/docs/en/permissions).

Codex's noninteractive `never` approval policy rejected browser navigation even
after the native MCP server connected. For a private home config containing the
exact enabled Engine Box browser command and arguments, the launch supplies
`mcp_servers.enginebox-browser.default_tools_approval_mode="approve"` only when
that server default is absent. It retains `workspace-write` and leaves per-tool
approval overrides, disabled tools and other MCP servers unchanged. Missing,
custom, disabled or malformed entries receive no override. If an ancestor of the
working directory contains a project `.codex/config.toml`, the launch defers to
native configuration instead of overriding project policy; those projects need
an explicit native browser approval default for unattended navigation.
Codex 0.163.0 accepted this leaf override in a model-free
`mcp get --json` probe and retained a configured `disabled_tools` list. That output
omits approval-policy fields, so it cannot independently attest per-tool policy.
See the official [MCP configuration reference](https://learn.chatgpt.com/docs/extend/mcp?surface=cli).

Legacy browser MCP entries may still point at the Python launcher. Binary sync
does not migrate those configuration entries; they must be migrated through the
explicit guest operation before existing workspaces can use the Rust launcher.
Custom provider configurations must be preserved.

## Running the qualification

Run only on an explicitly authorized Linux node, as the account owning the source
controller homes. Supply a credential-free prepared snapshot and a private fleet
configuration whose selected host uses local transport. Stage the three candidate
guest helpers as root-owned, non-writable release files. The test verifies their
digests after installation in its own VM.

```sh
ENGINEBOX_NATIVE_RUN_QUALIFICATION=1 \
ENGINEBOX_TEST_FLEET=/private/fleet.toml \
ENGINEBOX_TEST_HOST=qualified-node \
ENGINEBOX_TEST_SOURCE=prepared-base/snapshot \
ENGINEBOX_TEST_RELEASE_BIN=/root-owned/candidate/bin \
ENGINEBOX_NATIVE_SOURCE_HOMES=/private/controller/agents/homes \
ENGINEBOX_NATIVE_TEST_STATE=/private/fresh-qualification-state \
ENGINEBOX_NATIVE_PREPARE_GUEST=1 \
ENGINEBOX_NATIVE_BROWSER_QUALIFICATION=1 \
ENGINEBOX_NATIVE_BOOT_SAMPLES=1 \
pnpm exec cargo test --release -p enginebox-agents \
  --test live_native_runs -- --ignored --nocapture
```

`ENGINEBOX_NATIVE_TEST_STATE` must not already exist. The preparation option runs
the actual Rust guest setup, then reboots only the disposable VM before auth is
delivered. Browser qualification enables browser preparation as well. Each native
turn is bounded to 180 seconds; there are no automatic inference retries.
The fixture can wait up to 60 seconds for the specific host-lock admission guard,
only while no worker is pending and the durable run-file set is unchanged.
Provider failures, transport failures and admitted runs are never replayed.
`ENGINEBOX_NATIVE_TEST_PROVIDERS=codex,claude` can select distinct providers for a
focused follow-up, avoiding another request to an account already quota-blocked.
The boot-samples option runs three clean-stop/start and three freeze/resume
readiness samples after native work has drained. Timing includes the Rust
lifecycle call and helper/identity/network/DNS readiness, and excludes lock wait,
the preceding stop/pause, HTTP, credential delivery and inference. It checks boot
identity changes for cold starts and preservation for freeze/resume. These are
small smoke samples, not a controlled comparison with Python.

When the production host has no candidate node helper on sudo's secure path, the
qualification can run inside an administrator-created private mount namespace.
Bind a task-owned directory containing the candidate `enginebox-host` over
`/usr/local/bin` only in that namespace, then drop back to the source-home owner
before executing the test. This leaves the production host path and services
unchanged. Never run two credential-refresh writers against an original home.

This test does not exercise HTTP/MCP controller admission, production cutover,
LayerFS-mounted inference, desktop control or provider requests across VM
hibernate. Those remain separate acceptance checks.
