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
- Operator fleet MCP exposed exactly four tools and fetched actual authenticated inventory through the API.
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

## Remaining qualification

Fresh-host package/bridge installation, an independent operator's second machine, host reboot, off-host backup restore, upgrades/rollback, arbitrary shared mounts, production multitenancy and native subscription inference remain separate gates. Browser control was exercised directly over MCP; no paid model prompt or account login was used. Interactive noVNC viewer transport remains separate from the working headless browser path.
