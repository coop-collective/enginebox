# PR verification build profile

`verify.yml` runs `pnpm build:pr` on `pull_request`: build every Rust workspace
member in the development profile, then run the same dashboard, site and fleet
builds as `pnpm build`. This reuses compatible artifacts from the preceding full
check/test/clippy sequence instead of compiling the workspace again with release
optimization and thin LTO. It does not qualify release performance or packaging.

Main pushes and manual dispatch still run `pnpm build`, including the unchanged
release profile. `release.yml` also retains its full release build. All three OS
jobs, tests, lint, Linux preparation/smoke and browser checks remain required;
there are no path filters or selected-test substitutions. LayerFS candidate
validation continues to follow [INTEGRATION-CI.md](INTEGRATION-CI.md).

The Actions API baseline, rechecked on 2026-11-02, is
[run 41384962825](https://github.com/coop-collective/enginebox/actions/runs/41384962825):
Windows check 84 s, test 388 s, release build 711 s; total Windows job 1,280 s.
The first job started 3 s after run creation. Removing the second optimized
compilation targets that 711 s build step, but still pays development binary
linking and frontend builds. This is a timing hypothesis until the changed
workflow completes; do not report the entire baseline build time as savings.

[Cargo profiles](https://doc.rust-lang.org/cargo/reference/profiles.html) define
these development/release differences. Local hardware/connection inventories and
raw benchmark logs stay outside Git per AGENTS.md; measured comparisons belong
in the accompanying PR's validation record.
