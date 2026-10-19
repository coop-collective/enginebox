# Reference-host validation

October 19, 2026. These are development acceptance results on newly created VMs, not a throughput benchmark or a claim of compatibility with every supported host configuration. Private host names, addresses, inventory and raw evidence are excluded from the repository.

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

## Remaining qualification

Fresh-host package/bridge installation, an independent operator's second machine, host reboot, off-host backup restore, upgrades/rollback, arbitrary shared mounts, production multitenancy and native subscription inference remain separate gates. Browser control was exercised directly over MCP; no paid model prompt or account login was used. Interactive noVNC viewer transport remains separate from the working headless browser path.
