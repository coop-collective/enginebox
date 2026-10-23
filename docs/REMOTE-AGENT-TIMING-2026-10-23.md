# Remote agent timing — October 23, 2026

Engine Box still wakes a prepared fresh workspace in under one second internally.
The full controller allocation takes longer. This campaign does **not** establish
that Engine Box starts agents faster than Cursor, Claude Cloud or Codex Cloud:
provider access and different observable timing boundaries prevent that conclusion.

## Measured Engine Box results

Live Rust controller 0.3.1-live.0, `native-dev` template revision 4, one disposable
4-vCPU / 4-GiB VM. Timers ran on the controller host and excluded the SSH transport
used to initiate the experiment. HTTP results were polled every 100 ms.

| Operation | Samples | Median | Range | Measurement boundary |
| --- | ---: | ---: | ---: | --- |
| Assign fresh warm workspace: internal readiness | 1 | 0.900 s | — | Reserve, thaw and guest readiness |
| Same allocation: full controller job | 1 | 2.629 s | — | Controller work, including checks and assignment |
| Resume a paused VM | 3 | 1.694 s | 1.620–1.699 s | Full controller job through guest readiness |
| Boot a stopped VM | 3 | 7.982 s | 7.943–8.001 s | Full controller job through guest readiness |
| Codex: fresh conversation + tiny read task | 3 | 7.415 s | 7.119–11.939 s | HTTP launch through terminal result |
| Claude: fresh conversation + tiny read task | 3 | 4.909 s | 3.480–5.128 s | HTTP launch through terminal result |
| Codex: follow-up in exact conversation | 1 | 6.904 s | — | HTTP launch through terminal result |
| Claude: follow-up in exact conversation | 1 | 3.377 s | — | HTTP launch through terminal result |

Observed HTTP completion adds polling overhead: paused-VM resume was
1.648–1.750 s, and stopped-VM boot was 8.013–8.104 s. These are different from
the controller job duration. Native-run HTTP admission was 0.301–0.421 s.

Every fresh conversation read the fixture with a native tool and returned the
expected marker. The follow-ups reused the exact native session IDs. A follow-up
may answer from existing conversation context, so its time is not a fresh-file-I/O
benchmark. Boot IDs stayed unchanged across pause/resume and changed on all three
cold boots. The owned fixture was stopped and deleted under the host lock.

This was a small warm-path smoke test, not a load test or p95 measurement. The
tiny fixture was prepared outside the timer. These results say nothing about
large repository checkout, dependency installation, memory-snapshot restore from
disk, LayerFS or concurrent-agent throughput. The other development VMs remained
untouched.

Native versions: Codex CLI **0.163.0**, Claude Code **2.1.302**, Cursor Agent
**2026.10.18-ea085cb**. Claude explicitly used `haiku`, reported as
`claude-haiku-5-5`; Codex used its configured native default. These are not a
controlled cross-model inference comparison.

## Provider-hosted measurements and blockers

| Service | What was actually observed | What remains unmeasured |
| --- | --- | --- |
| Cursor Cloud | Configured user API key rejected with HTTP 401; native CLI subscription login is a separate connection | VM readiness, first tool call, completion |
| Claude Cloud | Three `claude --cloud` submissions acknowledged in **9.970, 11.843, 23.417 s**; median **11.843 s** | VM readiness, first action and completion |
| Codex Cloud | Installed 0.163.0 CLI task list empty; its environment picker had no selectable environment | All live cloud task timing |

Claude's initial interactive trust/setup attempt was excluded from the three
measurements. Native CLI version was **2.1.304**. Acknowledgement means the CLI
printed a created cloud session; it does not mean the VM was ready. Attaching
with `claude --cloud SESSION_ID` was refused because that capability is not
enabled for the account. No readiness/completion timing is inferred from the
successful submission. Four read-only cloud sessions were submitted including
the initial setup attempt; their remote completion was not observed.

The current Codex documentation describes published reusable environments, while
the installed CLI exposes the older `cloud` task interface. An empty CLI picker
does not establish the state of the newer account UI. A suitable published
environment and an observable task channel are needed for a real comparison.
[Codex Cloud environments](https://learn.chatgpt.com/docs/environments/cloud-environments).

Cursor already prepares bootable environment Builds and keeps copies pre-warmed.
We should compare against an active prepared Build, and separately measure an
unprepared first launch. Reuse of prepared dependencies alone is not unique to
Engine Box. [Cursor Builds](https://cursor.com/docs/cloud-agent/builds).

The Cursor API offers per-run SSE events for status, tool calls and terminal
results; the research probe is ready for a valid Cloud API key. Claude's native
`--cloud` command creates hosted sessions, while Remote Control steers a session
on the user's own machine. Those are different execution locations and should not
be mixed in the result table. [Cursor Cloud API](https://cursor.com/docs/cloud-agent/api/endpoints),
[Claude cloud sessions](https://code.claude.com/docs/en/claude-code-on-the-web).

## Defects and telemetry implications found

Cursor on Engine Box could not admit a fresh run. Its controller connection was
connected with delegation marked ready, but the installed guest version was
`2026.10.18-ea085cb`; credential delegation requires the qualified version
`2026.10.16-e3cb207` in `delegation::CURSOR_VERSION`. The access-format version
guard therefore prevents a fresh attachment. The public API returned a generic
HTTP 400. The version mismatch is the concrete configuration defect to resolve;
this is not evidence of Cursor inference slowness or a subscription-quota failure.
No existing account or development workspace was modified to bypass that guard.

The campaign also demonstrated why token counters need a scope label:

- Codex's first conversation returned 28,572 input / 46 output tokens; its exact
  resumed session returned 60,824 / 92. On this version the native terminal
  report is a **session total**, not a safe per-task delta.
- Claude's first `usage.output_tokens` was 179 and its follow-up was 111, while
  follow-up `modelUsage.outputTokens` was 290. `usage` describes the current
  invocation; `modelUsage` and the cost total include previous session work.

Do not sum cumulative rows, infer missing values as zero, or present list-price
cost estimates as subscription balances. The separate ACP/fleet task change uses
this evidence to distinguish invocation counts from session totals.

## Cursor follow-up qualification

Cursor Agent **2026.10.22-27dcf5f** subsequently passed three fresh conversations
and one exact-session resume in a separate disposable 4-vCPU / 4-GiB workspace.
The official installer and native sandbox preflight passed first. The controller's
existing connected account supplied access-only material; its refresh credential
was retained on the controller. The expired browser login request was not treated
as a completed new login.

| Operation | Samples | Median | Range |
| --- | ---: | ---: | --- |
| Cursor fresh conversation + tiny read task | 3 | 23.073 s | 20.690–54.122 s |
| Cursor follow-up in exact conversation | 1 | 18.478 s | — |

Fresh times were **54.122, 20.690 and 23.073 seconds**. Each fresh run emitted a
completed native read-tool event and returned `enginebox ENGINEBOX_STARTUP_OK`.
All four invocations exited successfully without timeout, and the follow-up
returned the exact original native session ID. The native default was **Auto
Balance**, whose response reported routing to **Grok 4.7**. These settings differ
from the Codex and Claude samples above.

This timer covers the controller host invoking `incus exec`, the Rust guest
helper, native CLI initialization, the task and the complete result. It excludes
workspace allocation, CLI installation and the fleet HTTP admission path. The
native result reported approximately 12.7–13.6 seconds for the fresh turns; that
field is not a measurement of overall process startup. No first-token, first-tool
or VM-only latency is inferred from these completion times. The first sample also
includes costs that may be amortized by subsequent CLI launches; this experiment
did not isolate them.

The qualification deliberately used the candidate's exact version constraint on
credential delivery. It does not mean the deployed controller or existing
revision-4 templates have been upgraded. The compatible controller/guest release
and a newly prepared revision-5 template must be rolled out together before
claiming the normal fleet API is repaired. Signed-in developer workspaces are not
silently reinstalled.

## Next comparable run

1. Roll out the qualified Cursor version and new template, then repeat through
   the normal controller HTTP path. Native guest inference and resume now pass.
2. Configure a valid Cursor Cloud user API key locally and select a prepared
   Build for the same benchmark repository.
3. Select a suitable Codex Cloud environment and an accessible cloud task channel.
4. Enable an observable Claude cloud session channel for the account.
5. Match model/settings, repository SHA, prepared-state policy and client location;
   record first tool action and terminal result for at least ten fresh runs per
   service. Measure parallel capacity separately.

Reproduction scripts are under [research/agent-startup](../research/agent-startup/README.md).
Private receipts retain raw run records and native output; session IDs, host
inventory, credentials and transcripts are excluded from this report and Git.
