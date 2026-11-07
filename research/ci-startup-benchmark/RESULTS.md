# Preconnected runner and pnpm cache results

Measured November 2, 2026, with five alternating cold/warm pairs. All ten jobs
passed the same real Worker and Docker container integration checks.

The approved pnpm seed reduced median dependency installation from **2.565 s to
1.276 s**: **50.3% less time, or 2.01× faster**. Every cold installation fetched
128 packages; every warm installation fetched zero. **Median queued-to-complete
job time was 51 s in both modes. This experiment does not establish an overall
queued-job speedup.**

The first successful job was picked up in **2 s** and reached its first explicit
workflow step in **7.626 s**, after an online idle runner had been verified.
These are ready-reserve measurements, not a machine-first-boot guarantee.

## Fixed workload and execution contract

| Input | Value |
| --- | --- |
| Fixture | `coop-collective/cloudflare-template` |
| Source | `24a4fe9ded7ca31ffc2af996802246146b105d46` |
| Workflow | `.github/workflows/compatibility.yml`, `perf/ci-startup-benchmark` |
| Enginebox runtime | `0.3.1-ciwarm.1`, candidate source `69a249bdbab2fc7820d562a4482feacf47e5b274` |
| Final clean image identity | `613e557c2921ec0042b5d1b6d182a8cfa91535717c4207896dcc9f5d8cc73841` |
| Resources | Linux x64 VM, 4 CPUs, 4 GiB RAM, 30 GiB disk |
| Tools | Node `v24.14.0`, pnpm `12.15.0`, official runner `2.338.2` |
| Lifecycle policy | Only esbuild and workerd allowed by the committed `allowBuilds` policy |
| Cache kind | Clean image-local pnpm package store and public registry metadata |

Every measurement has the same source commit/tree, lockfile, package/workspace
inputs, image, toolchain, resources and seed-manifest digest. Each job used a
different ephemeral runner and private VM disk. Consumed VMs were removed; no
completed job VM, writable branch, credentials or workflow output became a cache
seed. The seed came from a separate credential-free builder with scripts disabled
and was validated offline from a fresh HOME and checkout path. Actual jobs ran
their approved lifecycle scripts normally.

Cold jobs removed only their own private copies of the package store and registry
metadata. Warm jobs retained them. Both began without node_modules and ran:

```sh
pnpm install --frozen-lockfile \
  --store-dir /opt/enginebox-benchmark/seed/store \
  --package-import-method copy --reporter ndjson
pnpm run benchmark:integration
```

Both modes set `PNPM_CONFIG_CACHE_DIR` to the private seeded metadata directory.
This is conventional pnpm-cache reuse on private VM disks, not a LayerFS
installed-tree benchmark. The fixed approved seed can benefit the first job too;
the cold/warm order is an experimental control, not automatic learning from a
previous workflow's disk.

Each job passed seven Worker/Vitest checks and six built-Worker journeys. These
include real container cold start, reuse and tenant isolation, plus queue delivery
through a container to D1/KV. No browser testing is claimed. The benchmark runs
the committed coverage and built-Worker integration command; it is not a timing
of every additional check in the repository's broader compatibility workflow.

The same fixture commit also passed the broader [GitHub-hosted compatibility
check](https://github.com/coop-collective/cloudflare-template/actions/runs/41385845085/job/125086375120).
That validates compatibility; these ten EngineBox measurements do not establish
a matched speed comparison against GitHub-hosted hardware.

## Timing summary

Seconds; p50 is the median of five samples per mode. Sampled p95 uses nearest
rank and is therefore the largest of five values, not a production p95 estimate.

| Metric | Cold p50 | Cold sampled p95 | Warm p50 | Warm sampled p95 |
| --- | ---: | ---: | ---: | ---: |
| Queue to runner pickup | 2.000 | 3.000 | 3.000 | 3.000 |
| Queue to first explicit workflow step | 7.626 | 8.284 | 7.600 | 9.252 |
| Dependency installation | 2.565 | 2.755 | 1.276 | 1.353 |
| Worker/container integration | 30.568 | 30.834 | 31.094 | 31.772 |
| Runner execution, started to completed | 49.000 | 51.000 | 48.000 | 50.000 |
| Whole job, queued to completed | 51.000 | 53.000 | 51.000 | 53.000 |

The dependency improvement is consistent across these samples. Integration took
slightly longer in the warm group, and the queued-job medians and sampled p95s
tie. Do not describe the dependency speedup as a 2× faster complete CI job.

## Every valid sample

Times are seconds, rounded to three decimals where appropriate. Links identify
the exact numeric GitHub jobs; their logs and artifacts require repository access.

| Sample | Pickup | First step | Dependencies | Integration | Execution | Whole job | Downloaded packages |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| [Cold 1](https://github.com/coop-collective/cloudflare-template/actions/runs/41388447451/job/125094373046) | 2 | 7.626 | 2.755 | 30.028 | 49 | 51 | 128 |
| [Warm 1](https://github.com/coop-collective/cloudflare-template/actions/runs/41388550816/job/125094690088) | 2 | 6.657 | 1.287 | 31.094 | 48 | 50 | 0 |
| [Cold 2](https://github.com/coop-collective/cloudflare-template/actions/runs/41388655777/job/125095010516) | 3 | 7.726 | 2.525 | 30.568 | 49 | 52 | 128 |
| [Warm 2](https://github.com/coop-collective/cloudflare-template/actions/runs/41388759465/job/125095332385) | 3 | 6.738 | 1.353 | 30.930 | 47 | 50 | 0 |
| [Cold 3](https://github.com/coop-collective/cloudflare-template/actions/runs/41388870310/job/125095662677) | 3 | 6.982 | 2.369 | 30.834 | 48 | 51 | 128 |
| [Warm 3](https://github.com/coop-collective/cloudflare-template/actions/runs/41388981830/job/125095993052) | 3 | 8.352 | 1.236 | 31.403 | 49 | 52 | 0 |
| [Cold 4](https://github.com/coop-collective/cloudflare-template/actions/runs/41389086768/job/125096309702) | 2 | 8.284 | 2.568 | 30.666 | 51 | 53 | 128 |
| [Warm 4](https://github.com/coop-collective/cloudflare-template/actions/runs/41389194686/job/125096633170) | 3 | 7.600 | 1.276 | 31.011 | 48 | 51 | 0 |
| [Cold 5](https://github.com/coop-collective/cloudflare-template/actions/runs/41389301286/job/125096947866) | 2 | 6.223 | 2.565 | 29.974 | 48 | 50 | 128 |
| [Warm 5](https://github.com/coop-collective/cloudflare-template/actions/runs/41389404953/job/125097255309) | 3 | 9.252 | 1.244 | 31.772 | 50 | 53 | 0 |

## Measurement and availability limits

The queue anchor is the numeric job's GitHub `created_at`, not workflow creation
or local dispatch time. Pickup uses `started_at`; whole-job time ends at
`completed_at`. These API timestamps have second precision. The first-step marker
uses the guest clock, and dependency/integration timers use monotonic elapsed
time. Across all ten jobs, the guest marker fell 223–982 ms after the corresponding
API step-start second, consistent with its timestamp precision.

Cold cache deletion is outside the dependency timer but inside total job time.
The combined tool/Docker/seed-verification step, which contains that deletion,
had p50 and sampled p95 of 1 s in both modes. Its individual API durations were
0–1 s; that resolution does not isolate deletion cost. Whole-job differences
must not be attributed exclusively to package caching.

Every dispatch waited for an actual online, non-busy runner. Prewarming therefore
requires booting and connecting capacity before demand. This was a sequential
workload with a minimum of one idle runner and a maximum of two runners, not a
burst-load SLO. A spare was observed online while active work ran. An earlier
normal reload encountered idle retirement and the existing 60-second reserve
retry backoff before new capacity appeared; that preparation delay preceded the
valid dispatches. No precise portion is attributed to shared periodic work.

The historical [Enginebox job from run 41308139811](https://github.com/coop-collective/cloudflare-template/actions/runs/41308139811/job/124852780972)
had **46 s** of pickup delay: its recorded timestamps are
12:47:42 to 12:48:28 UTC on November 1. The first successful current
sample's pickup was 2 s. The historical source was
`4f117e6fec929175e5b5ae86ff4fbad4c1445339`, with a different workflow/tool setup;
this is a diagnostic reference, not a controlled startup-speedup estimate.

The [earlier failed canary](https://github.com/coop-collective/cloudflare-template/actions/runs/41387821938/job/125092475390)
used the retired image and failed at Docker socket access before dependencies or
integration ran. Its evidence remains retained separately and is excluded from
all statistics above. The corrected image was checked in the actual runner
execution context before the valid series.

Downloaded-package counts are pnpm events, not measured network bytes. No
transferred-byte, reserved-idle-compute, storage-growth or cost advantage is
claimed. Ten successful runs do not establish a production latency distribution,
concurrent capacity limit, or availability guarantee. Raw account/host responses,
logs, manifests and artifacts remain private; this report contains only reviewed
identities, timing summaries and job links. Results are diagnostic evidence, not
automatic CI approval or authority to publish a reusable cache.
