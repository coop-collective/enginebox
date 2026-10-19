# Rust VM boot benchmarks

On 2026-10-19, a disposable 2-vCPU / 2-GiB workspace reached full Rust readiness in **0.512 seconds median from frozen RAM**, **6.034 seconds from a clean stop**, and **5.846 seconds from disk-backed hibernation**. These are host-local lifecycle measurements on one server, with native Codex, Claude Code and Cursor installed. They are not inference latency, a saturation test, or a managed-service latency guarantee.

| Starting state | Samples | Resume action median (s) | Full readiness median (s) | Full readiness p95 (s) | Readiness + all three CLI versions median (s) | Readiness + CLI versions p95 (s) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Stopped boot | 10 | 0.306 | 6.034 | 7.096 | 7.084 | 8.184 |
| Frozen resume | 20 | 0.127 | 0.512 | 0.541 | 1.096 | 1.130 |
| Hibernation restore | 5 | 4.381 | 5.846 | 8.053 | 6.632 | 8.722 |

Median is the middle observation, averaging the two middle values for even counts. p95 uses nearest rank, `ceil(0.95 × n)`; at n=5 and n=10 it is the maximum observed value. The hibernation sample is especially small. No samples were discarded.

## Measurement boundaries

The release integration test is [boot_benchmarks.rs](../packages/enginebox-core/tests/boot_benchmarks.rs). It calls the production Rust `EngineBox::lifecycle` and `fleet::wait_ready` functions on the Incus host, without an SSH round trip per operation.

Each sample first acquires the existing root-owned host lease, checks all native provider leases are idle, and records host load and a guest process marker. It then cleanly stops, freezes, or hibernates **its own fixture**. That preparation time is recorded separately. The start timer begins immediately before `lifecycle("resume")`, which includes Incus status queries, the start request and the returned state. Full readiness adds the actual Rust helper-version validation and guest-readiness loop. Lock acquisition, prior stop/pause/hibernate, API queueing, dashboard polling and credential delegation are outside this start timer.

Full readiness checks the configured UID/GID/home/workspace, machine identity including PID1, identity service, completed cloud-init guard, installation manifest, native commands, browser executable, network route and DNS. It checks command availability against the preparation manifest; it does **not** launch a model request or authenticate an account.

The last two columns additionally launch `codex --version`, `claude --version` and `agent --version` sequentially as the configured guest user. These prove the installed native programs execute; they do not measure interactive prompt/session readiness. Across all samples the versions were Codex **0.163.0**, Claude Code **2.1.302**, and Cursor **2026.10.16-e3cb207**.

Every stopped boot changed the Linux boot ID. All 20 frozen resumes and all five stateful restores preserved the boot ID and the PID/start time of a running marker process. These are process-continuity checks; no authenticated conversation or application-specific in-memory workload was exercised.

## What makes the warm path fast

For frozen resumes, median lifecycle action time was **0.127 s** and the subsequent full-readiness call took **0.390 s**. Independent probes after readiness measured **0.251 s** for the three serial helper-version checks and **0.139 s** for the direct guest-readiness probe. These diagnostic probes repeat the checks after native CLI execution; their medians are explanatory measurements, not a second additive latency guarantee.

Frozen RAM remains allocated and avoids a new guest boot. Pausing this fixture took **0.126 s median**. Hibernation released the running VM's RAM but took **7.506 s median** to write its state; restoring that state took **4.381 s median** for the lifecycle action alone, before readiness. In this small idle fixture, hibernation preserved the process but did not provide a major startup-latency advantage over a clean boot.

The clean-stop readiness loop polls at one-second intervals. Eight starts reached readiness near six seconds and two near seven seconds. Those observations include boot/Incus-agent availability and polling granularity; they do not identify firmware/kernel time in isolation.

The prebuilt clean snapshot's CoW copy took **0.092828 s** once. That number excludes initial boot, helper installation, native guest preparation and readiness. This test is not repeated fresh-workspace creation or a fresh Ubuntu/dependency download benchmark. Host page caches were not dropped, because doing so would affect existing workspaces. “Stopped boot” therefore means a fresh guest boot with ordinary host caches, not cold storage caches.

## Comparison with earlier observations

[The earlier validation](VALIDATION.md#portable-templates-and-warm-pools) recorded warm reservation-through-readiness at **0.386 / 0.399 s**, complete server jobs at **0.626 / 0.625 s**, and UI polling near two seconds. Those were two historical warm-assignment samples with different VM sizing, readiness checks and controller boundaries. This run measures repeated lifecycle resume, including current helper validation, and does not execute reservation/rename/credential delegation. It neither proves a language speedup nor establishes an equivalent-path regression.

Earlier Rust disposable qualification measured about **2.93 s** for resume/readiness while commands traversed SSH. That test used the same lifecycle function followed by a direct guest-readiness helper call. This run measures the production `wait_ready` path host-locally, including three additional helper-version checks, and reports serial guest-exec costs separately. The difference is consistent with transport and measurement-boundary overhead; it is not a controlled before/after optimization experiment.

True warm assignment was deliberately excluded from the live production project: marking a fixture assigned could cause the controller to delegate real account credentials. All benchmark fixtures stayed in `pool.state=preparing`, with no connections or inference. A future end-to-end allocation benchmark should use an isolated controller/project and explicitly controlled account hooks.

## Machine, contention and admission

The host was Ubuntu **26.04**, kernel **7.0.0-30-generic**, on one Intel Xeon **6527P** with **24 cores / 48 logical CPUs**, and **134,673,010,688 bytes** of RAM (about 125.42 GiB). Incus was **6.0.5**, QEMU **10.2.1**. The existing storage pool was a **480-GiB Btrfs loop pool** backed by NVMe storage. The guest was Ubuntu **24.04**, cloned from an owned credential-free native-development snapshot, then prepared with the Rust recipe and rebooted outside the timed samples.

Each fixture had **2 vCPU, 2 GiB RAM, 30 GiB root disk and 4 GiB state disk**. Admission retained **12 logical CPUs**, **40 GiB RAM** and **32 GiB storage** in reserve. Before allocation, the pool had **382 GiB** of logical root/state commitments and **66 GiB** of remaining admitted storage room. One 34-GiB fixture fit; two would require 68 GiB. A concurrent two-VM boot batch was therefore **not admitted or attempted**.

Other existing workspaces and frozen reserves remained allocated. A separate LayerFS test VM remained running and idle with **4 vCPU / 8 GiB**; its build had completed and no build or dependency downloads ran during the timing window. The parent also avoided host builds. Host one-minute load ranged **1.14–1.55** across sample observations, with available RAM **94.55–96.74 GiB**. This is a lightly loaded shared host, not an isolated machine or a parallel-agent throughput benchmark. Sample lease waits were below **0.000029 s** and excluded from start timings.

The test rejects source snapshots with raw VM configuration, external disks or passthrough devices; the qualified source contained only its root disk and the existing managed-bridge NIC. It checked the source snapshot's configuration, devices, profiles, name and stateful flag were unchanged. Cleanup verified the exact fixture UUID/owner before deleting the fixture, and its marked staging directory was removed. The final private receipt records `cleaned: true`. No existing user VM or controller service was changed.

## Reproduction and provenance

Runtime source commit: `c8aaf37dab78111af0a7444313304b7f9e46bc2b` (merged Rust implementation), Engine Box **0.3.0**, Rust **1.96.0**. The source was synchronized into an isolated checkout on the host and rebuilt with `--release --locked`; no deployment occurred.

Benchmark source SHA-256: `739e4385f921dd20402fa03e94408c2f9405ec755aa0a975b6c2dbac118e9314`.

Benchmark executable SHA-256: `064948028c2f7dcb6bf8a837f57f8015587e83c3c917c013da453acd8368e404`.

| Installed helper | SHA-256 |
| --- | --- |
| enginebox-guest | `82ddc7a3a14607dbd604600c94c78f3a5dd7f0bdbf765fbdbbef7bee66f9735e` |
| enginebox-host | `24365389747b795488045c72cfc9ba7056495a77619cf488a58842b9bc4f35a6` |
| enginebox-mcp | `5d7602a2a1fbfd4132082aae76b354f5c9260a614e1b317817e2667d60f997e0` |

The private receipt was opened at **2026-10-19T12:21:27.676364856Z** after fixture setup, and completed at **2026-10-19T12:25:57.153058782Z**. It includes the coordination gate wait; the full test took **304.09 s** including setup and cleanup. Raw private JSON remains outside Git in `.state/rust-boot-benchmarks-20261019.json`; sanitized timings follow below.

Build on the Linux host through the pinned workspace:

```sh
pnpm exec cargo test --release -p enginebox-core --test boot_benchmarks --no-run --locked
pnpm exec cargo build --release -p enginebox-core --bin enginebox-host --locked
```

Run the resulting integration-test executable as root with the explicit environment contract documented at the top of the test. Provide a private host configuration with `target="local"`, an owned credential-free `BASE/SNAPSHOT`, Linux release helper binaries, the exact runtime source SHA, and an absolute report path in a root-owned mode-0700 directory. Use a unique optional timing-gate file to coordinate other workloads. Run `--exact host_local_boot_benchmark --ignored --nocapture` under an outer timeout; the qualification used 1,200 seconds. The test performs fresh capacity admission and refuses remote transport or debug builds.

The Linux release benchmark passed all 35 samples. Strict Linux Clippy for this integration test also passed. The test is opt-in and does not provision a VM in ordinary CI.

## Raw samples

All values below are seconds. `stop_or_pause_seconds` is excluded from the other columns. `resume_action_seconds` is included in `to_ready_seconds`; `to_all_cli_versions_seconds` includes readiness plus three sequential native version commands.

```csv
phase,sample,stop_or_pause_seconds,resume_action_seconds,to_ready_seconds,to_all_cli_versions_seconds
stop,1,0.679294,0.320834,6.033074,7.024342
stop,2,0.677311,0.305957,6.007948,7.060408
stop,3,0.689661,0.299971,6.013780,7.056535
stop,4,0.684250,0.290866,6.023304,7.107306
stop,5,0.675793,0.305428,6.016439,7.040778
stop,6,0.686450,0.309611,6.034905,7.091519
stop,7,0.679425,0.310500,6.054323,7.076144
stop,8,0.673213,0.305097,7.095705,8.142123
stop,9,0.665763,0.309750,6.071748,7.104580
stop,10,0.679249,0.305168,7.096492,8.183503
pause,1,0.131918,0.120646,0.519899,1.091654
pause,2,0.146853,0.126990,0.489817,1.081486
pause,3,0.131596,0.116442,0.506256,1.158720
pause,4,0.099609,0.115975,0.508830,1.101821
pause,5,0.131167,0.126402,0.493533,1.099214
pause,6,0.127535,0.131673,0.521148,1.072813
pause,7,0.105018,0.125940,0.508471,1.098704
pause,8,0.126335,0.131583,0.530047,1.115596
pause,9,0.120803,0.116618,0.493239,1.076871
pause,10,0.126500,0.121262,0.524264,1.103945
pause,11,0.126170,0.126258,0.488193,1.057313
pause,12,0.120982,0.127194,0.514283,1.085309
pause,13,0.126518,0.131409,0.523705,1.115304
pause,14,0.126262,0.131639,0.529417,1.115301
pause,15,0.111058,0.131613,0.541187,1.112167
pause,16,0.111685,0.141872,0.569532,1.129992
pause,17,0.115734,0.121018,0.496831,1.058651
pause,18,0.115302,0.131220,0.528788,1.094003
pause,19,0.137135,0.120979,0.504851,1.085455
pause,20,0.121536,0.130858,0.496522,1.081897
hibernate,1,7.506219,4.470183,8.052673,8.722126
hibernate,2,7.501709,4.356683,5.829885,6.631537
hibernate,3,7.533673,4.381342,5.846150,6.519347
hibernate,4,6.549089,4.397234,6.956346,7.629881
hibernate,5,7.529049,4.363070,5.805283,6.459927
```
