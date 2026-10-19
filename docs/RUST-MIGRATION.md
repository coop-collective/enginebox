# Rust workspace migration

The 0.3 development line moves the runtime, host helper, native-agent supervision, API/MCP, installers and release tooling to Rust. The dashboard remains TypeScript/Vinext. Every program, library, test and installer entrypoint lives under `packages/`.

| Package | Responsibility |
| --- | --- |
| `enginebox-core` | Incus transport, host capabilities, capacity/admission, bootstrap, templates, fresh warm pools, snapshots and guest preparation |
| `enginebox-agents` | Native provider connection lifecycle, access-only credential delivery, private native run records, exact resume, usage, computer/browser helpers and provider CLI installation |
| `enginebox-controller` | Authenticated HTTP API, durable bounded jobs, background reconciliation and exclusive mutation ownership |
| `enginebox-cli` | `ebox` administration and operator fleet MCP |
| `enginebox-install` | Immutable release staging, platform install plans, stable API/dashboard supervisor, update, rollback and artifact construction |
| `enginebox-storage-layerfs` | First-class checkout storage, approved imports, immutable dependency reuse, file checkpoints and bounded recovery |
| `dashboard` | Owner dashboard, session authentication and server-side API proxy |

The root pins pnpm 12.14.0 and Rust 1.96.0. With `cargo.enabled: true`, `pnpm install` downloads both ecosystems and provisions the Rust toolchain; Cargo performs compilation. This is one workspace/dependency entrypoint, not a replacement of npm and crates.io with a single public registry. Both `pnpm-lock.yaml` and `Cargo.lock` remain required. [pnpm's Cargo documentation](https://github.com/pnpm/pnpm/blob/main/pnpm/docs/cargo.md) labels this integration experimental.

## Compatibility boundary

Preserve native provider homes and private workspace state. Existing credentials live under the controller state root's `agents/homes/`; refresh credentials stay on the controller. Native run history lives in `agent-runs/`. Keep API tokens, owner password, dashboard session secret and fleet configuration outside immutable releases. A controller rollback must reload durable records after acquiring ownership, rather than reuse stale in-memory state.

The new `--state-dir` and `ENGINEBOX_STATE_DIR` select the **controller state root**. The old Python API's `--state-dir` selected its `jobs/` subdirectory. During migration, pass the parent of the old jobs directory. Do not create a second empty account directory and ask the owner to sign in again.

Linux nodes and guests now execute native `enginebox-host` and `enginebox-guest` helpers. Merely replacing the controller binary does not upgrade a node or an old template. Stage the correct Linux architecture artifacts, qualify the node helper, and create/qualify a new clean template revision before routing new workspace creation to it. Existing private VM homes and memory checkpoints must not be replaced or recycled.

## Updates

The stable supervisor retains the public API/dashboard listeners while a candidate controller starts in standby. A standby controller does not recover active job records. The supervisor gates new mutations, drains admitted jobs and authorization sessions, transfers the exclusive state lock, then switches routing. Read observations and retained immutable dashboard chunks support clients already using the previous release. Failed readiness, activation or incompatible state leave the previous compatible version available. See [update semantics and limits](UPDATES.md).

This mechanism covers service releases. Kernel, Incus, GPU driver, host reboot, supervisor replacement and native provider upgrades have their own lifecycle requirements. [LayerFS](LAYERFS.md) is a first-class checkout backend exposed through the controller, MCP and dashboard, with explicitly provisioned artifacts on qualified Linux nodes. File checkpoints remain separate from VM memory checkpoints; its Incus attachment and older-controller rollback boundaries are documented independently.

## Qualification record

The Rust port merged on October 19, 2026. Native Linux, macOS and Windows build/test/Clippy checks passed, alongside dashboard checks. Real macOS/Linux archive staging, supervised Rust-to-Rust controller/dashboard handoff and rollback, Linux native-helper preparation and browser control have separate recorded qualifications. Native provider login prompts and cancellation were exercised without completing sign-in or consuming inference. See [the qualification record](VALIDATION.md#rust-migration-qualification).

Do not treat v0.2 Python inference/session validation as proof of rewritten paths, or a merged port as a deployment. The initial legacy cutover, successful authenticated Rust provider sessions, fresh physical-host bootstrap, Windows service installation and independently reproduced installation remain acceptance gates. Windows builds validate operator/controller tooling, not a Windows VM, Docker contributor or GPU execution backend. The proposed [persistent core/agent environment service](DEV-ENVIRONMENTS.md) is additional work, not a capability supplied automatically by the language migration.

The initial Python v0.2 to Rust migration cannot use this ownership handoff: v0.2 has neither the owner lock nor the drain endpoint. Wait for admitted API/auth work to finish, stop the legacy control-plane services, preserve/back up the private state, then start the Rust supervisor. This briefly interrupts the control plane; it does not stop independent Incus workspaces. Subsequent compatible Rust releases use the tested rolling handoff.
