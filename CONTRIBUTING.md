# Contributing

Engine Box is in early development. Start with the [design](docs/DESIGN.md) and [implementation milestones](docs/IMPLEMENTATION-PLAN.md). The current implementation includes server bootstrap, Incus lifecycle controls, a fleet API/MCP and a Vinext dashboard. Platform qualification and optional CI integration remain open.

```sh
pnpm install --frozen-lockfile
pnpm check
pnpm test
pnpm lint:rust
pnpm build
```

Product programs and their unit, integration and release-qualification tests live under `packages/`. Standalone research, comparison benchmarks and disposable experiments live under [`research/`](research/README.md), outside the product workspace and release artifacts. Keep tests that verify a shipped service with that service; a disposable fixture alone does not make a regression test research. The root contains workspace configuration. The root pins pnpm 12.14.0 and Rust 1.96.0. pnpm's experimental Cargo integration provisions the toolchain and downloads both Rust and JavaScript dependencies. Cargo still compiles Rust and uses its standard `Cargo.lock`; pnpm retains the JavaScript lockfile. Keep both committed. Plain Cargo also works when the optional generated pnpm vendor directory is absent.

For dashboard changes, run `pnpm --filter enginebox-dashboard exec playwright install chromium` and `pnpm test:e2e` after the build. The browser suite uses test fixture services. Build deployment artifacts on the target platform. `cargo fmt --all --check` checks Rust formatting.

Offline tests must not contact a server. Integration tests must use a disposable VM and explicitly identify the host, project and artifacts they create. Never use a signed-in workspace as an integration-test target.

For a change, explain the behavior, how it was verified and any remaining platform limits. Preserve official provider clients and the optional-module boundary. Add regression coverage for configuration, command handling, lifecycle transitions and host mutations where relevant.

Contributions are accepted under the repository license, the Business Source License 1.1 (see [licensing](docs/LICENSING.md)). Contributors must have the rights to submit their work. The project needs the right to offer commercial licenses for Engine Box, and a contributor license agreement is planned; until it is in place, do not assume your contribution is accepted on any other terms. Grants under earlier licenses remain valid for the versions they covered. Provider binaries, third-party proprietary code and customer workspace recipes do not belong in this repository.
