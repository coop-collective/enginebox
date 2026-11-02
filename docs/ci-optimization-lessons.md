# CI optimization lessons for this repository

Status: **analysis only**, 2 November 2026. This document maps Owen
Gretzinger’s Boardy CI playbook onto Engine Box’s *own* GitHub Actions. It
does not rewrite workflows. Unimplemented recommendations are labeled as
such.

Source: [How I made our CI 20x faster](https://x.com/owengretzinger/status/2116291924219060319)
(Owen Gretzinger, Boardy). Boardy went from ~40 minutes to ~2 minutes, mostly
by changing runners, lint/typecheck tools, test isolation, cache seeding and
pre-push hooks.

This is a different codebase. Engine Box is a Rust + pnpm monorepo (~85k lines
of Rust across 10 crates, plus TypeScript/Node packages). The slow path here
is **repeated `rustc` work, especially `cargo build --release` with thin LTO
on Windows**, not ESLint or Vitest isolate. Product CI (Engine Box as a runner
for *other* repos) is documented separately in [CI.md](CI.md),
[GITHUB-ACTIONS.md](GITHUB-ACTIONS.md) and
[GITHUB-ACTIONS-ACCELERATION.md](GITHUB-ACTIONS-ACCELERATION.md).

## Inventory: what actually runs

### Workflows

| Workflow | Trigger | Runner | What it does |
|---|---|---|---|
| [`verify.yml`](../.github/workflows/verify.yml) | every PR and `main` push | 3-OS matrix: `ubuntu-24.04`, `macos-15`, `windows-2025`. Linux on `main` may use `vars.ENGINEBOX_LINUX_RUNNER`, else GitHub-hosted | **The PR gate.** One sequential job per OS: checkout → Node 24 → global pnpm 12.15.0 → frozen install → `cargo fmt` → `pnpm check` → `pnpm test` → Linux-only preparation + `pnpm test:ci-templates` → `pnpm lint:rust` → `pnpm build` → Linux-only `fleet:smoke`, Playwright, `pnpm test:e2e` |
| [`linux-node.yml`](../.github/workflows/linux-node.yml) | PR path filter on install/controller/core | GitHub `ubuntu-24.04` (Docker runner var on `main` only) | Installer tests, `enginebox-install` tests/clippy, release `enginebox-node` |
| [`native-gpu-node.yml`](../.github/workflows/native-gpu-node.yml) | PR path filter on install | Windows / Linux Docker / 3-OS package matrix | GPU node image, bootstrap, `enginebox-node` |
| [`native-ci-worker.yml`](../.github/workflows/native-ci-worker.yml) | PR path filter on `enginebox-ci` | GitHub `ubuntu-24.04` only | fmt, `enginebox-ci` tests/clippy, release worker binary |
| [`windows-wsl.yml`](../.github/workflows/windows-wsl.yml) | PR path filter on install | `windows-2025` + `ubuntu-24.04` image build | WSL installer/runtime qualification |
| [`release.yml`](../.github/workflows/release.yml) | version tags / dispatch | 5 native targets | Full `pnpm check && test && lint:rust && build`, then packaged archives |
| [`enginebox-runner-canary.yml`](../.github/workflows/enginebox-runner-canary.yml) / [`enginebox-ci-dispatch.yml`](../.github/workflows/enginebox-ci-dispatch.yml) | `workflow_dispatch` | Engine Box group or GitHub | Product canary / dispatch qualification; not the PR gate |
| [`publish-wsl-preview.yml`](../.github/workflows/publish-wsl-preview.yml) | dispatch | GitHub | Publish a previously qualified WSL image |

Path-filtered installer workflows already skip unrelated PRs. **`verify.yml`
has no path filters** and always runs the full three-OS job.

### Job shape (the thing agents wait on)

`verify.yml` is one job per OS. Inside that job, lint, typecheck, tests, clippy,
release build and (on Linux) e2e are **strictly sequential**. OS jobs run in
parallel (`fail-fast: false`). Concurrency cancels superseded pushes on the
same ref.

Root scripts from [`package.json`](../package.json):

```text
pnpm check      = cargo check --workspace --all-targets --locked
                  + dashboard tsc + site node --check + fleet tsc
                  + several node --check packages
pnpm test       = cargo test --workspace --no-fail-fast --locked
                  + dashboard/site/deploy/fleet/gateway/preparation tests
pnpm lint:rust  = cargo clippy --workspace --all-targets --locked -- -D warnings
pnpm build      = cargo build --workspace --release --locked   # thin LTO + strip
                  + dashboard vite + site + fleet wrangler dry-run
```

Release profile ([`Cargo.toml`](../Cargo.toml)): `lto = "thin"`, `strip = true`.
That is a second, more expensive compile after debug `cargo test`.

### Caching, checkout, sharding, hooks

| Mechanism | Current state |
|---|---|
| `actions/cache` / `Swatinem/rust-cache` / sccache | **None** in any workflow |
| `actions/setup-node` `cache:` | **Unset** (no pnpm store cache) |
| Checkout | `actions/checkout@v7` default; no `filter: blob:none`. `publish-wsl-preview.yml` uses `fetch-depth: 0` |
| Test sharding | **None.** `cargo test` uses the default libtest thread pool on the 4-vCPU GitHub VM |
| `cargo-nextest` | Not used |
| nx / turbo affected | Not used. Product policy in [INTEGRATION-CI.md](INTEGRATION-CI.md) is the opposite for *candidate* checks: selected results cannot satisfy a required full check |
| Pre-commit / pre-push / lefthook / husky | **None** |
| AGENTS.md | Lists `pnpm install --frozen-lockfile`, `pnpm check`, `pnpm test`, `pnpm lint:rust`, `pnpm build`, `pnpm test:e2e` |
| ESLint / Oxlint / Biome | **Not in the repo.** Dashboard has no lint script. Site and gateways use `node --check` |
| TypeScript | Dashboard already on **TypeScript 7.0.2** (`tsc --noEmit`). Fleet controller/node still on **5.9.3** |
| Vitest | Only `@enginebox/fleet-controller`. Default isolate; Cloudflare workerd plugin. No `isolate: false` |
| Other JS tests | `tsx --test` (dashboard) and `node --test` (site, gateways, environment-service) |
| Engine Box runners | **PRs stay on GitHub-hosted.** Linux jobs on `main` may use the `enginebox-ci` group when the repo variable is set. Windows/macOS have no Engine Box CI hosts. Prepared LayerFS dependency reuse is an opt-in product profile, not how this repository’s PR CI runs |

## Measured baseline (2 November 2026)

Queue wait on GitHub-hosted `verify.yml` is currently **~3 seconds** from
workflow `createdAt` to first job `startedAt` (not Owen’s 11-minute queue).
Wall-clock PR time is the slowest OS job.

Evidence: successful PR run
[41384962825](https://github.com/coop-collective/enginebox/actions/runs/41384962825)
(`feat/engine-node-dialogs`, 21m24s) and successful `main` run
[41383357053](https://github.com/coop-collective/enginebox/actions/runs/41383357053)
(22m36s). Step durations below are from the PR run.

### `native (windows-2025)` — critical path, 21m20s

| Step | Duration |
|---|---|
| Setup + checkout + Node + global pnpm | ~31s |
| `pnpm install --frozen-lockfile` | 13s |
| `cargo fmt` | 2s |
| `pnpm check` | 1m24s |
| `pnpm test` | **6m28s** |
| `pnpm test:ci-templates` | 18s |
| `pnpm lint:rust` | 29s |
| `pnpm build` (release + LTO) | **11m51s** |

### `native (ubuntu-24.04)` — 16m26s, extra Linux work after the same sequence

| Step | Duration |
|---|---|
| Setup + checkout + Node + global pnpm | ~6s |
| `pnpm install` | 10s |
| `cargo fmt` | 1s |
| `pnpm check` | 47s |
| `pnpm test` | 3m18s |
| Linux preparation handoff | 20s |
| `pnpm test:ci-templates` | 4s |
| `pnpm lint:rust` | 21s |
| `pnpm build` | **6m37s** |
| `pnpm fleet:smoke` | 1m07s |
| Playwright Chromium install | 21s |
| `pnpm test:e2e` | **3m13s** |

### `native (macos-15)` — 13m16s

Same sequence as Windows without Linux extras. `pnpm test` 4m34s, `pnpm build`
6m54s.

Four recent successful `verify.yml` runs (PR and `main`) all started jobs
within 3–4 seconds. Windows finished in **21–22 minutes** every time. Speeding
Linux e2e does **not** shorten PR wait until Windows drops below Linux.

A PR that also touches `packages/enginebox-install/**` waits on
`windows-wsl.yml` in parallel (about 23 minutes on the same dialogs PR). That
is a second critical path for installer changes only.

Compile surface: ~533 `#[test]` functions, 71 JS/TS test files. Time is not
in a 20k-test suite; it is in **four Cargo invocations** (`check`, `test`,
`clippy`, `build --release`) plus Playwright on Linux.

## Lesson map

JS-specific Boardy tactics are translated to the closest Engine Box equivalent.

| # | Boardy lesson | Engine Box verdict | Why |
|---|---|---|---|
| 1 | Get off GitHub’s default runners | **Partially applicable; already done for `main` Linux only. Not the current PR bottleneck.** | Product already provisions Engine Box JIT runners and prepared LayerFS profiles. This repo’s PRs are intentionally GitHub-hosted (`enginebox-ci` allow-lists `refs/heads/main`). Queue wait is ~3s, not 11 minutes. Windows/macOS have no Engine Box hosts, and Windows is the PR critical path. Faster Linux self-hosted VMs would not cut PR wall clock today. Unqualified: paid 8-vCPU GitHub/Blacksmith Windows runners, Engine Box Windows/macOS CI hosts. Related in-flight work: [PR #113](https://github.com/coop-collective/enginebox/pull/113) (warm connected runner reserves) is **product startup latency**, not this workflow’s compile tax. |
| 2 | Replace ESLint with Oxlint | **Not applicable as a CI-time win.** | There is no ESLint/Oxlint/Biome job. `pnpm check`’s JS side is `tsc` plus `node --check`. Optional: add Oxlint later for dashboard TSX quality; that would be a new check, not a replacement of a 2-minute linter. Closest Rust equivalent already in use: `rustfmt` (~1–2s) and Clippy. |
| 3 | Switch typecheckers every time a faster one ships | **Partially already done; low remaining CI impact.** | Dashboard is already on TypeScript 7.0.2. Fleet controller/node still use TypeScript 5.9.3 `tsc --noEmit`. `bun check` does not fit a pnpm + Cargo workspace. The 47s–84s `pnpm check` step is dominated by `cargo check --all-targets`, not `tsc`. Closest Rust equivalent: stay on the pinned `rust-toolchain.toml` (1.96.0); do not churn compilers for speed. Unimplemented: migrate fleet packages to TS 7 and measure; only do it if `tsc` is split out of the Cargo job. |
| 4 | Shard tests onto bigger machines | **Applicable later, not first.** | `pnpm test` is 3–6.5 minutes, not an 8-minute single target. Sharding without a shared `target/` cache multiplies cold `rustc`. Owen’s own finding: bigger machines beat more shards because CPU went to loading/compiling. Closest equivalent: 8-vCPU Windows/Linux for the verify job, or `cargo nextest` partitions **after** rust-cache exists. Unimplemented; do not shard first. |
| 5 | Stop reloading the app for every test file (`isolate: false`) | **Mostly not applicable.** | Vitest is only the Cloudflare fleet-controller suite (workerd). Isolation is load-bearing there. Dashboard uses `tsx --test`; most coverage is `cargo test`, which already shares a process per binary. Closest Rust equivalent: `cargo nextest` (reuse test binaries, better scheduling) and avoiding per-crate `cargo test -p` loops that relink. Unimplemented: nextest; **do not** set Vitest `isolate: false` on workerd tests without a dedicated leak-reset design. |
| 6 | Fix setup: cache from `main`, clone, match cores, don’t delete tests | **Applicable, high value. Not done.** | No Actions cache of Cargo `target/` or the pnpm store. GitHub scopes PR caches to the PR branch, so a `main`-seeded save-on-default-branch cache is required (Owen’s exact failure mode). `pnpm install` is already cheap (10–31s); **compile** is the setup. Checkout is already 2–6s, so blobless clone is a small gain and is a poor fit while LayerFS tests require `git-lfs`. Cargo disables incremental when `CI=true`; fingerprint reuse across `check` → `test` → `clippy` still helps within one job. `pnpm build --release` cannot reuse the debug artifacts from `pnpm test`. Unimplemented: `Swatinem/rust-cache` + `setup-node` `cache: pnpm`, saved from `main`. |
| 7 | Run lint, typecheck, build and tests in parallel; peel extras off the slowest job | **Applicable, high value. OS matrix is parallel; steps inside each job are not.** | The PR waits on `max(windows, linux, macos)` of a single fat job. Windows is slowest because of sequential debug tests then release LTO. Linux then appends smoke + Playwright. Splitting jobs without cache can *increase* wall clock (each job cold-compiles). With cache, peel: (a) stop doing release LTO on PRs, (b) Linux e2e/smoke as their own job, (c) fmt+clippy vs tests. Unimplemented. |
| 8 | Only run what a PR touches (`nx affected`); test the skip logic | **Applicable for this GitHub repo; must not weaken LayerFS candidate gates.** | Installer workflows already path-filter. `verify.yml` does not: a dashboard-only or docs-only PR still does three-OS Rust release LTO. Product [INTEGRATION-CI.md](INTEGRATION-CI.md) forbids selected tests as evidence for an accepted candidate — that rule stays. For *this repository’s Actions*, path filters / `dorny/paths-filter` with a required “skip logic must fail if Rust changed” job are appropriate. Unimplemented. Full suite remains required on `main` and before release tags. |
| 9 | Run checks before every push (hook, not AGENTS.md) | **Applicable. Not done.** | No hook. AGENTS.md already lists the full suite; Owen’s point is that agents skip prose. A hook must stay in the “few seconds” range or it will be disabled: `cargo fmt --all --check` plus dashboard `tsc` on changed files, not full Clippy + release build. Unimplemented. Use lefthook or a checked-in `.githooks` path; keep it optional for contributors who only use the dashboard. |

## Ranked next steps (this repo)

Impact is **PR wall-clock** (currently ~21 minutes, Windows). Effort is
workflow/script change size, not calendar time. Hypotheses are labeled; only
the baseline column is measured.

| Rank | Change | Status | Expected PR wall clock | Effort | Notes |
|---|---|---|---|---|---|
| 1 | **Do not run workspace `cargo build --release` on pull requests** | Unimplemented | **~21 min → ~9–11 min** | Small | Measured 11m51s on Windows, 6m37s on Linux. Keep debug `cargo test` + Clippy on PRs. Keep `pnpm --filter enginebox-dashboard build` / site / fleet so JS still builds. Retain full `pnpm build` on `main` and in `release.yml`. Risk: release-profile-only errors; mitigated by `main` + tag jobs. This is the single largest, safest win. |
| 2 | **Seed Cargo + pnpm caches from `main`** | Unimplemented | Further cut of remaining 3–6 min `pnpm test` compile, especially Windows | Small–medium | `Swatinem/rust-cache` (or sccache) plus `setup-node` `cache: pnpm`. Save on `main`; restore on PRs with `shared-key`. Hit rate is a hypothesis until 20–30 runs are audited (Owen’s method). pnpm install is already 10–31s, so this is for `target/`, not `node_modules`. |
| 3 | **Peel Linux extras into parallel jobs** (after 1–2) | Unimplemented | Linux 16 min → ~7 min unit + ~5 min e2e in parallel; **PR clock stays on Windows until 1 lands** | Medium | `fleet:smoke` + Playwright + `test:e2e` are 4m41s after Rust release. Split only once cache exists, or e2e will cold-install. Keep a required aggregate check so a skipped e2e job cannot look green. |
| 4 | **Path-filter `verify.yml` with a tested skip gate** | Unimplemented | Docs/TS-only PRs: minutes instead of 21 | Medium | Example: docs-only → markdown/link job; dashboard-only → Node jobs + Linux e2e, skip Windows/macOS Rust; Rust → full matrix. **Required:** a job that fails if skip logic hides a `Cargo.lock` / `packages/enginebox-*` change (Owen lesson 8). Nightly or `main` still runs everything. Do not reuse this as LayerFS selected-check evidence. |
| 5 | **Drop redundant `cargo check` from the fat job** | Unimplemented | ~45–80s inside `pnpm check`, small vs rank 1 | Small | Clippy is `cargo check` plus lints. After `cargo test`, Clippy was 21–29s (fingerprints from the earlier check). A CI-specific `pnpm check:js` plus `pnpm lint:rust` avoids a dedicated check-profile compile. Keep `pnpm check` locally. |
| 6 | **Pre-push hook for fmt + changed-file typecheck** | Unimplemented | Saves agent round-trips, not the 21 min wait | Small | `cargo fmt --all --check` is 1–2s in CI. Add dashboard `tsc` when `packages/dashboard/**` changes. Do not put Clippy or `pnpm build` in the hook. |
| 7 | **Larger Windows runner / `cargo-nextest`** | Unimplemented | Unknown; measure after 1–2 | Medium | Owen: buy cores, not shards. GitHub-hosted is 4 vCPU. Try one 8-vCPU Windows job and compare the same SHA. nextest helps scheduling once binaries exist; it does not remove LTO. |
| 8 | **Move PR Linux onto Engine Box runners** | Unimplemented / product-gated | Does not beat rank 1 while Windows is critical | Large | Needs runner-group workflow allow-list changes, Docker-in-VM for `fleet:smoke`/`test:preparation:linux`, and the still-unqualified prepared-dependency canary. Complements [PR #113](https://github.com/coop-collective/enginebox/pull/113) rather than replacing workflow-shape work. Cost must be modeled (Owen’s bill went $230 → $4,300/mo). |

### Suggested order

1. Rank 1 on `verify.yml` pull_request only (keep `main` + `release.yml` as they are).
2. Rank 2 cache, then audit 20 Linux/Windows runs for hit rate and `waiting / setup / running` like Owen.
3. Rank 3–4 once cold-compile is no longer paid per split job.
4. Rank 6 hook in the same timeframe as 1; it is independent.
5. Do not adopt Blacksmith/Oxlint/bun/`isolate: false` as a cargo-cult port.

## What this document does not change

No workflow YAML was edited. In particular this PR does **not**:

- switch `runs-on`
- add `filter: blob:none` (LayerFS tests need git-lfs; checkout is already 2–6s)
- enable Vitest `isolate: false`
- introduce nx/turbo skipper into the LayerFS candidate gate
- treat Engine Box prepared runners as a proven speedup for this repository
  (comparative Actions timing is still an open P5/P6 item in
  [IMPLEMENTATION-PLAN.md](IMPLEMENTATION-PLAN.md))

## Related docs

- [CI.md](CI.md) — optional `enginebox-ci` module and this repo’s runner variables
- [GITHUB-ACTIONS.md](GITHUB-ACTIONS.md) — App, JIT runners, LayerFS work roots
- [GITHUB-ACTIONS-ACCELERATION.md](GITHUB-ACTIONS-ACCELERATION.md) — prepared dependency profile (opt-in, live canary still open)
- [INTEGRATION-CI.md](INTEGRATION-CI.md) — protected full checks vs selected tests
- [PREPARED-ACTIONS-SETUP.md](PREPARED-ACTIONS-SETUP.md) — operator setup for accelerated pools
