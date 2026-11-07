# CI startup and pnpm comparison

See [the measured ten-job results](RESULTS.md) for the qualified template run,
including startup, dependency and complete-job timings and their limits.

Standalone research tools; no product dependency or cache publication authority.
Use a disposable repository and one exact commit, lockfile, Node/pnpm version,
runner image, resource allocation and lifecycle policy across every sample.
Record at least five runs per mode when time permits and alternate cold/warm
order. A single pair is diagnostic, not a production latency distribution.

The first workflow step writes an ISO timestamp into `BENCHMARK_FIRST_STEP_AT`
through `GITHUB_ENV`. After checkout and tool selection, set:

```sh
export BENCHMARK_CACHE_MODE=cold # warm or prepared for an exact hit
export BENCHMARK_CACHE_KIND=pnpm-store # installed-tree for protected preparation
export BENCHMARK_IMAGE=actual-template-manifest-digest
export BENCHMARK_RESOURCES=2CPU/4GiB
export BENCHMARK_INSTALL_POLICY=frozen-lockfile,ignore-scripts
node /path/to/measure.mjs identity "$RUNNER_TEMP/measurement.json"
node /path/to/measure.mjs cache "$RUNNER_TEMP/measurement.json" EXACT_CACHE_KEY false
node /path/to/measure.mjs run "$RUNNER_TEMP/measurement.json" dependencies -- pnpm install --frozen-lockfile --ignore-scripts --reporter=ndjson
node /path/to/measure.mjs run "$RUNNER_TEMP/measurement.json" integration -- pnpm run test:integration
```

`identity` refuses a dirty checkout. Put the helper and all outputs outside it.
`run` preserves command failure and records monotonic elapsed milliseconds,
command, wall-clock boundaries and exit status. It writes no command output or
environment into the artifact. Use public, non-secret command arguments only.
Pnpm NDJSON progress reports downloaded **package count**, never network bytes;
without progress events that field is `null`, not an invented zero.
The prepared action's existing receipt can supply its exact generation identity
and zero-download count; mark a prepared hit only after successful handoff.

Upload `measurement.json` with `if: always()`. A failed run remains useful failure
evidence; the comparison refuses to count it as a successful speedup. Fetch each
numeric job with `gh api repos/OWNER/REPO/actions/jobs/JOB_ID > job.json` and write
a private comparison config:

```json
[
  {"job":"cold/job.json","measurement":"cold/measurement.json"},
  {"job":"warm/job.json","measurement":"warm/measurement.json"}
]
```

```sh
node research/ci-startup-benchmark/analyze.mjs /private/comparison.json > /private/result.json
node --test research/ci-startup-benchmark/analyze.test.mjs
```

The analyzer requires the same measured source commit/tree, lock, tools, image,
resources, install policy and integration command. The measured checkout commit
must equal the GitHub job commit. It rejects reused job IDs, incomplete/failed
jobs, cache misses mislabeled warm and missing stages. It reports per-mode counts,
sample p50/p95 and every raw sample for queue-to-first-step, queue-to-runner,
dependency operation, integration runtime, runner execution and queued-to-complete
job time. API job timestamps have one-second precision; stage timers use the
guest monotonic clock. Cross-clock startup measures assume synchronized clocks.
The first-step marker measures arrival at a workflow step; it does not include
completion of checkout or prove first useful test execution.

Use actual job `created_at` as queue anchor. If unavailable, provide `queuedAt`
and `queueSource` from the queued-job webhook, with explicit provenance. Do not
substitute workflow-run creation time for job queue time. Artifacts are workflow
diagnostics and cannot authorize CI acceptance or reusable cache publication.

Cold means an empty job-private pnpm store and no node_modules. Warm means a
fresh job-private fork of an approved clean store or exact-key cache restore,
still without node_modules. Prepared means an approved protected installed-tree
generation restored after checkout. Never seed future jobs by snapshotting a
completed workflow VM/branch: checkout credentials, runner registration and
workflow writes are private. Use the dedicated clean preparation flow for
LayerFS publication. Ordinary GitHub cache behavior is a distinct control and
must not be reported as LayerFS acceleration.

Qualify Docker access after boot with the exact runner identity. Incus execution
with `--user 1001 --group 1001` does not initialize the account's supplementary
Docker group. A temporary socket ACL also disappears when Docker recreates its
socket. In a dedicated Docker-capable disposable runner image, configure
`/etc/systemd/system/docker.socket.d/enginebox-agent.conf` with:

```ini
[Socket]
SocketGroup=1001
SocketMode=0660
```

Use the actual runner's primary GID. Apply this only inside its reviewed clean
image, then reboot a fresh clone and verify `docker info`, seed readability,
private cache writes and the full integration suite using the exact UID/GID
without supplementary groups. Keep the builder credential-free and discard the
qualification clone. This configuration grants the runner Docker control within
its disposable VM; the protected installed-tree profile remains separate.

Whole-job measurements include cache restore and upload overhead; a faster
dependency command alone does not establish a faster job. These tools do not
measure reserved idle compute, storage growth, transferred bytes or full cost.
Keep raw API/account artifacts private; publish only reviewed timing summaries.

## Clean image-local store for Docker jobs

The protected installed-tree Actions profile intentionally disallows Docker
access. For a full Docker integration fixture, use the ordinary runner profile
with an image-local public pnpm store and an independently restricted repository
runner group. This is conventional pnpm-store reuse on private VM disks, not a
LayerFS installed-tree qualification. A preconnected idle runner receives its
own clean VM before demand; consumed VMs are deleted, never turned into seeds.

Run this in a dedicated credential-free Linux builder with exact Node and pnpm
already installed, from a reviewed clean commit:

```sh
ENGINEBOX_DISPOSABLE_PROOF=1 node research/ci-startup-benchmark/seed.mjs \
  --source /private/fixture --output /private/new-seed \
  --repository-id 123 --trust private-repository \
  --node v24.22.0 --pnpm 12.15.0 --base-image ACTUAL_64_HEX_BASE_DIGEST
```

The bounded seed builder copies package.json, the lockfile, and optionally the
exact reviewed `allowBuilds` map for esbuild/workerd (other workspace settings
are refused). It supplies an empty private HOME/config, removes inherited authentication/proxy environment,
permits public version dependencies only, refuses credential-bearing inputs and
non-default lockfile origins, disables scripts and pnpm hooks, and verifies a
second frozen installation offline, then repeats from a fresh HOME and checkout
path using only exported caches. Output contains only `store/`, `metadata/` and a manifest
binding repository, trust, source commit/tree, manifests, exact toolchain/base
image and both caches' content digests. Public registry metadata is required by
pnpm 12.15 even when package contents are present; it must be seeded explicitly.
The manifest includes `workspaceSha256` and `portableOfflineVerified: true`.
Builder scratch, node_modules and config
are deleted. A failed build has no valid manifest and must never be snapshotted.
This is provenance and input isolation, not a universal secret scanner; use only
an owner-reviewed clean builder and public dependency fixture.

Bake only these outputs into the credential-free template, never the builder's
home, source checkout or runner state. Record the resulting **final** image
manifest digest as `BENCHMARK_IMAGE` for both modes. Keep the seed manifest in
both cold and warm VMs and set `BENCHMARK_SEED_MANIFEST` to its path; `identity`
checks repository, lock/package hashes, platform and tools against it and binds
its digest into the comparison identity. Do not run a whole-store hash inside
the timed install: the trusted clean image already fixes seed provenance.

Set `PNPM_CONFIG_CACHE_DIR=/opt/enginebox-benchmark/seed/metadata` identically in
both jobs. For the cold control remove only that job's private `store/` and
`metadata/` copies before installation. For the warm control keep both. Start both without node_modules and use
identical install flags and package lifecycle policy. The seed always denies
lifecycle scripts; the actual workflow may separately allow reviewed packages
such as esbuild to run their required installation. A store contains download
inputs, not completed lifecycle effects. Record the workflow's explicit policy
in `BENCHMARK_INSTALL_POLICY` and run all real container integration checks.
This measures a controlled cold-to-seeded benefit; it does not learn reusable
state by snapshotting a job that has run arbitrary workflow code.

## Live run orchestration

`run.mjs` submits real workflows only when explicitly executed. It defaults to
five alternating cold/warm pairs for `coop-collective/cloudflare-template`,
`compatibility.yml`, ref `perf/ci-startup-benchmark`, runner group 4, and commit
`24a4fe9ded7ca31ffc2af996802246146b105d46`. The exact dependency command and
`pnpm run benchmark:integration` are part of its measured-workload contract.
Node v24.14.0 and pnpm 12.15.0 must match the qualified runner image.

Create a private configuration file, explicitly changing `expectedSha` if the
reviewed workflow commit changes:

```json
{
  "output": "/private/new-ci-startup-comparison",
  "expectedSha": "24a4fe9ded7ca31ffc2af996802246146b105d46"
}
```

```sh
node research/ci-startup-benchmark/run.mjs /private/comparison-config.json
```

The output directory must not exist. It is created mode 0700. Every dispatch
requires a fresh remote-ref check and an online, non-busy `ebox-ci-` runner with
the configured label in the configured GitHub group. Readiness polls every ten
seconds for at most five minutes. A synced intent journal precedes each
`gh workflow run`. Only that command's returned numeric ID or repository run URL
authorizes subsequent polling. A lost response or missing ID stops the batch;
the harness never searches by timestamps, retries submission, or silently resumes
an existing output directory.

Each returned run is followed for at most sixty minutes. Raw job/run responses,
available logs, the exact run's named artifact, and measurement JSON remain in
its private sample directory. Numeric job `created_at` supplies queue timing.
Any failed sample, source/toolchain/cache mismatch, changed stage command or
unexpected runner group stops further submissions. `comparison.json` is updated
after each valid sample; `result.json` is written only after all ten match.
Timeouts retain the known run ID without cancelling or replaying it. Live tests
and their artifacts remain distinct from CI approval or cache publication.

`node --test research/ci-startup-benchmark/run.test.mjs` uses a fake gh protocol
and submits no workflows. It covers all ten samples, durable dispatch intent,
ambiguous responses, changed refs, unavailable runners, run timeouts, failed
samples, source/command mismatches, and refusal to replay an existing directory.
