# Reference-host validation

October 19, 2026. These are development acceptance results on newly created VMs and isolated test directories in an owner-authorized workspace, not a throughput benchmark or a claim of compatibility with every supported host configuration. Private host names, addresses, inventory and raw evidence are excluded from the repository.

## Rust migration qualification

The 0.3 development branch passes the Rust workspace tests, strict Clippy, TypeScript checks, 20 dashboard unit tests and 19 dashboard browser tests on macOS. Native release binaries also build on Linux x86_64. A Windows GNU cross-target check covers every package and test target, but does not establish Windows execution, service installation or native provider sign-in.

The actual native supervisor/controller/dashboard test stages a separately compiled next controller version, continuously calls authenticated GET/POST endpoints through upgrade and rollback, and verifies exclusive ownership, retained jobs/templates/private Claude scratchnotes, valid browser cookies and previous immutable browser assets. Failed version health and failed activation retain or restore the prior controller. Killing/restarting the supervisor releases old children through their lifelines. An actual stdio fleet MCP process also survives those transitions. See [UPDATES.md](UPDATES.md) for the reproducible opt-in test and its receipt.

These results do not reuse the historical provider inference claims below. The first Python-to-Rust cutover requires a short control-plane interruption; compatible Rust-to-Rust updates use the tested handoff. Current live Linux guest qualification is recorded separately as it completes. LayerFS remains an [evaluated candidate](LAYERFS.md), not a qualified dependency.

## Historical v0.2 qualification

The remaining sections record the Python v0.2 implementation. They describe prior evidence, not automatic acceptance of rewritten Rust paths.

## Tested platform

| Component | Observed version/configuration |
|---|---|
| Operator | macOS ARM64, Python 3.14 |
| Compute host | Ubuntu 26.04 x86_64, KVM, 48 logical CPUs, approximately 125 GiB RAM |
| Virtualization | Incus 6.0.5, QEMU 10.2.1 |
| Storage | Explicit 480 GiB bounded Btrfs development loop pool on existing storage |
| Guests | Ubuntu 24.04, 4 vCPU, 4 GiB RAM, 30 GiB private disk |
| Native clients | Codex CLI 0.163.0; Claude Code 2.1.302; Cursor Agent 2026.10.16-e3cb207 |
| Guest browser | Official Playwright MCP 0.0.83, installed Chromium, sandbox enabled |
| Dashboard | Vinext 1.2.1, built on Linux Node 22.22.3 |

## Observed behavior

- Bootstrap created a separate owned project/profile/pool and prepared a new base using an existing network. The original workspace, network/firewall setup and existing GitHub runner remained in place.
- Provisioning failures resumed with the same configuration after fixing the guest installer. A completed second apply returned `changed: false` with no operations.
- Two standard workspaces were created through the authenticated REST API from the clean prepared snapshot. Both reported native binaries, Chromium, working IPv4 routing and DNS.
- Clones had distinct machine IDs and boot IDs, and independent writable files. On the inspected running clone, PID 1's cached machine ID matched the filesystem, identity marker and VM UUID.
- A process retained its RAM-only random token, PID and boot identity through pause/resume and through hibernate/wake.
- Official browser MCP performed navigation, an inline accessibility snapshot, typing, clicking and a valid PNG screenshot. Local storage persisted across a fresh MCP process. The test used temporary profiles and no provider accounts.
- Operator fleet MCP fetched authenticated inventory and exposed the original lifecycle tools. Template/pool work subsequently expanded the interface to eight tools; authenticated catalog and inventory calls were verified through a real MCP session.
- The packaged API and Vinext dashboard were installed as loopback-only systemd services. The deployed dashboard resumed and paused a real fleet VM; persisted jobs and readiness checks succeeded, with no browser errors or API-token exposure. Desktop and mobile views were checked.
- A real operator MCP session created, polled and stopped an additional disposable VM through the server-hosted API. The stopped test VM was then explicitly removed.

## Timings

The first measurements include SSH from the operator to the host. They are individual samples with mostly idle guests, not percentile estimates or service guarantees.

| Operation | Observed time |
|---|---:|
| Prepared disk copy, sample 1 / 2 | 0.90 / 0.86 seconds |
| Copy through usable guest readiness, sample 1 / 2 | 17.42 / 17.63 seconds |
| Guest kernel + userspace boot (`systemd-analyze`) | 2.53 seconds |
| API pause job | 3.45 seconds |
| API resume from pause, including readiness | 4.29 seconds |
| API hibernate job | 10.01 seconds |
| API wake from hibernation, including readiness | 8.50 seconds |

Guest boot time excludes firmware, copying, controller calls and networking between the operator and host. Pause retains RAM; hibernation writes it to storage. Neither operation is described as universally instant.

Moving the controller onto the compute host reduced measured control overhead. A subsequent small-workspace clone took **0.12 seconds to copy and 12.38 seconds through readiness**. The deployed dashboard's resume job completed in **0.40 seconds** and pause in **0.25 seconds**; click-to-confirmation, including SSH tunnel and UI polling, was approximately **1.9 seconds** for each. These are separate samples and VM sizes, not an isolated performance comparison.

The tested fleet configuration reserves 12 logical CPUs and 40 GiB RAM for the host, CI and other services. With the original 4-vCPU workspace and two new 4-vCPU workspaces allocated, admission allowed six additional standard workspaces. A frozen workspace remained counted. This is a conservative allocation budget, not evidence that eight simultaneous model sessions achieve a particular throughput or fit subscription limits.

## Portable templates and warm pools

The authenticated API registered the portable `native-dev` JSON manifest and installed its official Codex/Claude/Cursor plus browser recipe into a new owned base on the existing host. No host bootstrap or existing user workspace was replaced. The installer recovered the same partial base after fixing Incus 6.0.5's rejection of unsetting an absent clean-base marker. Preparation and finalization then completed in **140.49 seconds**; the catalog reported the installed revision in sync.

Two configured warm reserves were prepared independently and paused. Two concurrent allocation requests claimed distinct VMs. Their reservation-through-readiness timings were **0.386 and 0.399 seconds**; full server job execution was **0.626 and 0.625 seconds**. The test's two-second polling observed both complete in about **2.02 seconds**. These are individual samples, not latency guarantees. Each allocation preserved its physical VM name and recorded the requested friendly workspace name. Both assigned VMs remained excluded from the fresh pool, and automatic refill restored two new ready reserves.

A custom allocation from the newly installed template was verified at **3 vCPU, 3 GiB RAM, 32 GiB root disk and 5 GiB memory-state disk**. Its prepared disk copy took **0.129 seconds**, guest readiness **14.358 seconds**, and full server job **14.751 seconds**. A CPU request below the template minimum was rejected by the API before a VM/job was created. The actual guest manifest reported the native-client and browser versions listed above.

A subsequent control-plane upgrade preserved both ready reserves and their eligibility. The live dashboard imported the published manifest by HTTPS in **0.384 seconds** and retained its Installed/Up to date status. Its Minimum and Recommended dropdowns resolved to 2/2/30 and 4/4/40 CPU/RAM/disk. Pausing and resuming the custom disposable workspace took approximately **1.96 seconds** each from click to confirmation; other rows retained their states and no unrelated Working indicator appeared. Desktop/mobile checks reported no browser errors or horizontal overflow. The three disposable allocation-test workspaces were explicitly stopped and removed, retaining the installed template and two fresh reserves. Offline regressions cover concurrent claims, lost claim responses, interrupted preparation, pool isolation, strict resource floors, receipt/host binding changes, revisions and allocation admission. The Python suite ran **146 tests** with **one opt-in live-browser test skipped**; **8 installer tests**, **16 dashboard unit tests**, **12 browser fixture tests**, TypeScript and the production build passed. The tests do not establish native subscription authentication or inference.

## Native account setup

The Subscriptions panel uses the installed Codex, Claude and Cursor login commands in an assigned private workspace. A new disposable 2-vCPU/2-GiB VM was used for native prompt and cancellation checks. Each provider produced its expected official browser link; Codex also supplied a device code and Claude requested its one-time authorization code. Native status reported all three disconnected before and after these checks. No authorization was completed and no inference request was sent.

Live REST checks rejected session access/cancellation under a different provider or workspace and rejected pausing the fixture while sign-in was active. Cancellation removed the temporary link/code and left no native login or supervisor process. All tests kept the fixture running until explicit cleanup. Claude's PTY hyperlink formatting exposed an output-parser issue; incremental terminal-control handling fixed it, and the actual native prompt then passed.

The deployed dashboard then started all three native flows through its authenticated proxy, displayed the real provider prompts and cancelled them successfully. Desktop and 390-pixel mobile views were checked without horizontal overflow. The disposable account-test workspace was removed afterward; the two user workspaces retained their states, two fresh reserves remained ready, and the existing GitHub runner stayed active.

Offline validation ran **172 Python tests** with **one opt-in live-browser test skipped**, **8 installer tests**, **19 dashboard unit tests** and **16 browser fixture tests**, plus TypeScript and production builds. Auth regressions include code input without echo/storage, native-status verification after exit, process-group cleanup on cancel/transport EOF/expiry, cancellation during a slow status check, duplicate-login locking, bounded sessions, exact provider URLs and split terminal-control sequences. Fixture browser tests cover workspace selection, session recovery, polling cleanup, reconnect/cancel and mobile layout. These prompt/cancellation checks did not establish inference; the later authorized run below tested it separately.

## Native inference and conversation resume

The owner authorized a small test with existing native account connections in one
private workspace. Each provider received its own unique test directory. No API
key override was supplied, no billing setting was changed, and the test did not
pause the workspace or log out existing accounts. Raw JSON, session identifiers
and random canaries remain private.

| Native client | Code and assertions | Exact-session resume | Browser MCP |
| --- | --- | --- | --- |
| Codex 0.163.0 | Passed after the sandbox diagnostic below | Passed; recalled conversation-only canary | Passed |
| Claude Code 2.1.302, Haiku | Passed | Passed; recalled conversation-only canary | Passed |
| Cursor Agent 2026.10.16-e3cb207 | Provider quota blocked | Not attempted after quota rejection | Not attempted |

The first prompt requested a tiny `add` function and native shell assertions. A
new CLI process resumed the exact saved conversation, recalled a random canary
that was absent from the resume prompt and project files, added `multiply`, and
ran four assertions. Independent inspection then executed the generated code and
confirmed all assertions passed for Codex and Claude.

The command patterns were:

```sh
codex exec --json --skip-git-repo-check \
  -c model_reasoning_effort='"low"' -c approval_policy='"never"' \
  -c sandbox_mode='"workspace-write"' -
codex exec resume SESSION_ID --json --skip-git-repo-check \
  -c model_reasoning_effort='"low"' -c approval_policy='"never"' \
  -c sandbox_mode='"danger-full-access"' -
claude -p --output-format stream-json --verbose --model haiku \
  --permission-mode acceptEdits --allowedTools Read,Write,Edit,Bash --max-turns 10
claude -p --resume SESSION_ID --output-format stream-json --verbose --model haiku \
  --permission-mode acceptEdits --allowedTools Read,Write,Edit,Bash --max-turns 12
agent -p --output-format stream-json --force --trust PROMPT
```

Codex's first `workspace-write` turn authenticated and responded, but its native
tool sandbox failed with `bwrap: Operation not permitted`. The resumed diagnostic
used `danger-full-access` inside the existing VM and passed file and command
execution. That verifies the harness with VM isolation; it does not qualify the
default inner sandbox or recommend disabling it generally.

The third turn resumed those same Codex/Claude sessions and used only the installed
browser MCP to open a temporary loopback fixture, type a name, click a button,
inspect an inline snapshot, and save a screenshot. Both produced a valid PNG and
observed the expected greeting. No external website or account page was opened.
Claude used the test's separate browser-home override. Codex did not visibly
honor that environment override, so per-test browser-profile isolation was not
established for its native MCP launch; the configured client profile remained
subject to the browser service's lock.

Cursor returned a team usage-limit error with a provider-reported reset date of
October 23, 2026. Authentication had succeeded, but usable quota had not. The run
was not retried, and on-demand billing was not enabled. Its native inference,
resume and browser integration remain unqualified.

Each invocation had a 180-second limit and process-group cleanup. No invocation
timed out. A final read-only check found no process still using the test directory
as its working directory. These results cover original native account credentials;
the separate central-adoption test below covers delegated access.

## Central adoption and delegated native access

After the owner-authorized adoption into the controller, automatic synchronization
delivered access to one disposable assigned workspace. Model-free checks verified
the same pinned native versions, mode-0600 credential files, delivery markers, no
provider API-key override and no genuine refresh credentials in that workspace.

| Provider | Delivered native material | Native status |
| --- | --- | --- |
| Codex | External `chatgptAuthTokens` mode, empty refresh-token field | Connected using ChatGPT |
| Claude | `settings.env.CLAUDE_CODE_OAUTH_TOKEN`, no native OAuth cache | `loggedIn: true`, `authMethod: oauth_token` |
| Cursor | Access JWT in both cache slots, matching the pinned native `--auth-token` persistence behavior | `authenticated`, `isAuthenticated: true` |

Cursor's two fields contain the same access credential, not its browser refresh
credential. This is a tested compatibility format, not a promise that native
status proves usable quota or that future client versions retain the format.

After the [Ubuntu bubblewrap profile repair](CODEX-SANDBOX.md), Codex's model-free
sandbox check passed. Two small inference turns then passed with
`sandbox_mode="workspace-write"` throughout; no sandbox bypass was used. Claude
also passed two small Haiku turns with the delegated token. Each client created a
function, executed native assertions, resumed the exact session in a fresh process,
recalled a conversation-only canary, changed the file and executed the follow-up
assertions. Independent code execution confirmed the results and the canary's
absence from the generated code. Cursor received only a status check, with no new
inference attempt.

The first/resumed process times were **7.824 / 6.731 seconds** for Codex and
**6.402 / 6.486 seconds** for Claude. These are individual acceptance samples, not
performance benchmarks. No process timed out; the final inspection found no
process still using the test directory as its working directory. Automatic
expiry/renewal and live credential reload remain separate qualification steps.

The authenticated API then hibernated this disposable workspace and resumed it,
without another inference request. Jobs completed in **15.091 / 12.570 seconds**.
Machine ID and boot ID were unchanged, as were both native conversation transcript
checksums, generated code checksums and delegation markers. All three resume-hook
attachments returned `unchanged`; post-wake native status and access-only checks
passed for every client. The workspace was left running for the controller's
separate credential-renewal checks.

## Controller connections and automatic template synchronization

The deployed controller adopted all three existing sign-ins without a new browser login. Both existing private workspaces received access-only credentials automatically. Original native Codex and Claude refresh operations completed successfully, changed the access credentials, and the controller delivered the renewed generations to both workspaces. All three connections remained ready across the final controller restart. This checks explicit native renewal; it does not establish hot token replacement inside an already-running CLI or every elapsed-time expiry case.

Isolated controller homes exercised all three native browser/device prompts and clean cancellation without using the real accounts. The Claude setup-token flow exposed its manual-code input after a regression fix for its cursor-positioned terminal prompt. No test authentication caches or native processes remained.

Registering native development revision 2 automatically queued and completed node installation in **162.068 seconds**, including the documented Codex sandbox fix. A subsequent fresh warm assignment completed in **2.117 seconds** measured by the server job, including delivery of all three connections. The clean template and unassigned reserves received no credentials. Multi-node synchronization, new-node discovery, binding/revision changes, capacity failures and retries were verified offline; this live installation has one node. Inventory changes currently require a controller restart.

The final deployed dashboard passed read-only checks at 1440-pixel desktop and 390-pixel mobile widths: three controller connections ready, no workspace connection selector, template revision 2 ready on 1/1 nodes, no installation-target selector, and no browser/page/HTTP errors. The checks made no control mutations and found no API token or password in the rendered document or browser storage.

Final offline validation: **205 Python tests** with **one opt-in browser test skipped**, **8 installer tests**, **19 dashboard unit tests**, **18 fixture browser tests**, TypeScript and production builds. The sandbox disposable and fresh-allocation disposable were removed. The two original private workspaces remain running, with two ready reserves on the new template. Superseded clean reserves were stopped and quarantined with disks retained; original bases and the separate CI runner were preserved.

## 0.2.0 preview: native MCP, usage and resource limits

The new operator MCP launch path was exercised against the deployed authenticated
controller in a fresh disposable Ubuntu 24.04 VM with 2 vCPU, 3 GiB RAM and a
35 GiB root disk. The test used the real stdio MCP client to allocate the
workspace, launch the installed native clients and poll their private runs.
Codex `0.163.0` and Claude Code `2.1.302` each wrote a proof file, then resumed
the exact returned native session ID in a new process. Both recalled a
conversation-only random marker and read their earlier file. An independent
guest read confirmed both files contained the expected content. Codex retained
its workspace-write sandbox; Claude used its native unattended permission mode.
No alternate inference gateway or API-key billing path was introduced.

Cursor Agent `2026.10.16-e3cb207` initially failed before inference because its
own sandbox helper lacked Ubuntu's user-namespace permission. A scoped helper
profile fixed native startup without disabling the global restriction. Native
model-free tests allowed workspace writes and denied an outside-home write and
outbound socket; see [Cursor sandbox qualification](CURSOR-SANDBOX.md). The exact
operator MCP launch was then retried with the sandbox still enabled. It reached
the provider, which rejected inference because the test team's usage limit had
been reached and reported a reset on October 23, 2026. The native process exited
with status 1 without hitting the Engine Box timeout or output bound. Cursor
inference, tool use and conversation resume remain blocked by that quota; a
returned session ID alone is not a successful inference/resume test.

The same reviewed sandbox repair then passed its model-free preflight in both
existing assigned workspaces. Their boot IDs remained unchanged. The repair did
not reinstall native clients or reset account/session state.

The controller installed native development template revision 3 automatically.
A new cold clone from that revision contained the exact reviewed helper and
passed both native Codex and Cursor sandbox commands after boot; AppArmor remained
in enforce mode and the global namespace restriction stayed enabled. Admission
initially rejected the clone because retained old cache replicas consumed its
disk budget. Two obsolete, quarantined, never-assigned replicas were verified
against the pre-upgrade inventory and removed; the retry succeeded. Both
disposable test VMs were removed after testing. The two original assigned
workspaces and separate original workspace remain running, the new pool has two
ready frozen reserves, and the single GitHub runner remains active. Retained
base snapshots were not removed.

The deployed Agents panel passed read-only browser checks at 1440-pixel desktop
and 390-pixel mobile widths. All three controller connections were ready. The
Codex native app-server collector returned live allowance data and the UI showed
remaining percentage and quota reset time. The checks found no browser/HTTP
errors, horizontal overflow, credentials in the document/browser storage or
provider-control mutations. Claude/Cursor allowance and all billing-cycle dates
remain explicitly unavailable; provider links are available instead.

Offline contribution-limit tests cover configured CPU/RAM ceilings, unrelated
workloads, active/frozen/template allocations, stopped-workspace admission, warm
refill and a warm claim at the limit. No existing host's contribution budget was
changed by the release. This qualifies admission behavior of the Linux backend,
not a Windows/macOS resource service.

The test-gated 0.2.0 runtime wheel/source distribution was built on macOS arm64
with Python 3.14.6. The wheel was installed into clean environments on that Mac
and on Linux x86_64 with Python 3.14.4, outside the source checkout. Locked
dependency installation, declared-module imports, CLI dispatch and read-only
host inventory passed. The Mac also reported local Docker Engine 29.3.0 and
kept Docker VM capacity separate from the physical machine's totals. Artifact
hashes and platform receipts accompany the preview release. These packages are
runtime/operator tools; they are not a universal host or desktop-worker installer.

Final offline validation for this preview: **262 runtime tests** on both macOS
and Linux, with **one opt-in browser test skipped** on each; **8 installer tests**;
**20 dashboard unit tests**; **19 fixture browser tests**; TypeScript and production
builds. Clean wheel imports covered all 20 declared modules. The Cursor policy
fix received an independent focused review before the final package build.

## Remaining qualification

Fresh-host package/bridge installation, an independent operator's second machine, host reboot, off-host backup restore, upgrades/rollback, arbitrary shared mounts and production multitenancy remain separate gates. Native inference and central delegation are qualified only as recorded above; automatic expiry/renewal, long-running credential reload and Cursor quota availability require separate validation. Interactive noVNC viewer transport remains separate from the working headless browser path.
