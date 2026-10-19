# Rust workspace migration

The 0.3 development line moves the runtime, host helper, native-agent supervision, API/MCP, installers and release tooling to Rust. The dashboard remains TypeScript/Vinext. Every program, library, test and installer entrypoint lives under `packages/`.

| Package | Responsibility |
| --- | --- |
| `enginebox-core` | Incus transport, host capabilities, capacity/admission, bootstrap, templates, fresh warm pools, snapshots and guest preparation |
| `enginebox-agents` | Native provider connection lifecycle, access-only credential delivery, private native run records, exact resume, usage, computer/browser helpers and provider CLI installation |
| `enginebox-controller` | Authenticated HTTP API, durable bounded jobs, background reconciliation and exclusive mutation ownership |
| `enginebox-cli` | `ebox` administration and operator fleet MCP |
| `enginebox-install` | Immutable release staging, platform install plans, stable API/dashboard supervisor, update, rollback and artifact construction |
| `dashboard` | Owner dashboard, session authentication and server-side API proxy |

The root pins pnpm 12.14.0 and Rust 1.96.0. With `cargo.enabled: true`, `pnpm install` downloads both ecosystems and provisions the Rust toolchain; Cargo performs compilation. This is one workspace/dependency entrypoint, not a replacement of npm and crates.io with a single public registry. Both `pnpm-lock.yaml` and `Cargo.lock` remain required. [pnpm's Cargo documentation](https://github.com/pnpm/pnpm/blob/main/pnpm/docs/cargo.md) labels this integration experimental.

## Compatibility boundary

Preserve native provider homes and private workspace state. Existing credentials live under the controller state root's `agents/homes/`; refresh credentials stay on the controller. Native run history lives in `agent-runs/`. Keep API tokens, owner password, dashboard session secret and fleet configuration outside immutable releases. A controller rollback must reload durable records after acquiring ownership, rather than reuse stale in-memory state.

The new `--state-dir` and `ENGINEBOX_STATE_DIR` select the **controller state root**. The old Python API's `--state-dir` selected its `jobs/` subdirectory. During migration, pass the parent of the old jobs directory. Do not create a second empty account directory and ask the owner to sign in again.

Linux nodes and guests now execute native `enginebox-host` and `enginebox-guest` helpers. Merely replacing the controller binary does not upgrade a node or an old template. Stage the correct Linux architecture artifacts, qualify the node helper, and create/qualify a new clean template revision before routing new workspace creation to it. Existing private VM homes and memory checkpoints must not be replaced or recycled.

## Updates

The stable supervisor retains the public API/dashboard listeners while a candidate controller starts in standby. A standby controller does not recover active job records. The supervisor gates new mutations, drains admitted jobs and authorization sessions, transfers the exclusive state lock, then switches routing. Read observations and retained immutable dashboard chunks support clients already using the previous release. Failed readiness, activation or incompatible state leave the previous compatible version available. See [update semantics and limits](UPDATES.md).

This mechanism covers service releases. Kernel, Incus, GPU driver, host reboot, supervisor replacement and native provider upgrades have their own lifecycle requirements. A filesystem checkpoint is not a VM memory checkpoint; [LayerFS](LAYERFS.md) is a first-class storage backend requiring explicit provisioning on qualified nodes.

## Qualification record

Do not treat v0.2 Python validation as proof of this port. The historical live tests remain in [VALIDATION.md](VALIDATION.md). The migration must separately pass Rust unit/integration tests, dashboard unit/browser checks, clean artifact installation, actual controller handoff/failure/rollback, Linux native-helper compatibility and provider-session checks before replacing the running service. Windows builds validate an operator/controller binary, not a Windows VM, Docker contributor or GPU execution backend.

The initial Python v0.2 to Rust migration cannot use this ownership handoff: v0.2 has neither the owner lock nor the drain endpoint. Wait for admitted API/auth work to finish, stop the legacy control-plane services, preserve/back up the private state, then start the Rust supervisor. This briefly interrupts the control plane; it does not stop independent Incus workspaces. Subsequent compatible Rust releases use the tested rolling handoff.

## Initial migration sequence

1. Inspect the deployed service commands, private state root, fleet bindings, node helpers and template receipts. Record running user workspaces separately from reusable bases and fresh reserves. A stopped or quarantined VM is not proof that its private data can be deleted.
2. Qualify the candidate on disposable state and VMs. Include native code execution, exact conversation resume and browser tools; report provider quota blocks separately. Verify capacity for a newly prepared, credential-free template and its replacement reserves before directing new allocations to it.
3. Stage the checksum-pinned node and controller archives into separate owned prefixes. Initial offline activation only selects a release; it does not launch a service. Prepare the supervisor configuration with the existing absolute state root, unchanged fleet bindings and unchanged API/dashboard secrets. Inspect the generated service plan before applying it.
4. Stop new submissions and let existing jobs and provider authorization finish. Stop both legacy control-plane services, then make the final private backup with all legacy writers stopped. A preparatory copy made while services were running is not the final cutover backup.
5. Install the qualified node helper and synchronize guest tools only on idle, authorized private workspaces. Migrate recognized legacy browser entries through the explicit guest operation; preserve custom entries, native homes and private backups. Do not run full clean-base preparation on signed-in workspaces.
6. Start the Rust supervisor as the state-owning service user. Check exact version and active readiness, authenticated dashboard login/proxy, existing accounts, templates and completed native conversations, plus fleet MCP access. Validate the refreshed template before allowing new allocations. Keep the legacy installation available until these checks pass.
7. If initial activation fails, stop every candidate writer before restarting legacy services. Preserve the current provider state rather than restoring stale refresh credentials. After new Rust-only state or LayerFS attachments have been introduced, returning to Python is not a supported routine rollback; use the compatible Rust release protocol instead.

Staging, a passing service fixture and production activation are distinct milestones. Record which occurred. Kernel changes, user workspace restarts and deletion of old reserves are not implicit migration steps.
