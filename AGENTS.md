# Working on Engine Box

- Keep provider harnesses native. The owner explicitly requested controller-managed provider connections and credential delegation to their authorized private workspaces. Keep refresh credentials in private controller storage and use provider-supported or explicitly version-qualified native authentication formats. Do not proxy inference, replace agent loops or pool accounts across unrelated users.
- Keep the core runtime independent of GitHub and of the optional CI module.
- Treat `docs/DESIGN.md` and `docs/IMPLEMENTATION-PLAN.md` as the current scope. Explicitly label proposed commands and unimplemented capabilities.
- Never put host inventories, account state, provider tokens, GitHub registration tokens, SSH keys, real customer projects or VM snapshots in Git.
- Bootstrap must inspect before changing a host. Never repartition disks, overwrite unrelated profiles or change global NFS/firewall services implicitly.
- Preserve the distinction between a credential-free reusable base and a private checkpoint that includes credentials and RAM.
- Registered template manifests are controller-owned desired state. Synchronize them to every registered node, including nodes added later; retain per-node capacity checks, installation receipts and visible failures.
- Product services, libraries, CLIs, installers and their regression/integration tests belong under `packages/`. Standalone research, comparison benchmarks and disposable experiments belong under `research/` and stay out of product dependencies and release artifacts. Keep root configuration, documentation and example configuration separate.
- Use the pinned root pnpm workspace for Rust and TypeScript: `pnpm install --frozen-lockfile`, `pnpm check`, `pnpm test`, `pnpm lint:rust`, `pnpm build`. Run `pnpm test:e2e` for dashboard behavior. Rust dependencies remain in `Cargo.lock`; JavaScript dependencies remain in `pnpm-lock.yaml`.
- Controller releases must stage in standby, drain the current owner before activation, retain compatible prior releases and reject incompatible state rollback. Never recover active job records from a standby process.
- Live integration tests must create disposable environments. Do not pause, restore or stop a developer's workspace as a test fixture.
- Use exact provider documentation and record tested versions. Installation success is not proof of subscription eligibility or successful inference.
- Keep changes small and identify what was verified versus what remains planned.
