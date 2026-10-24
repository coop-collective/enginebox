# Research and experiments

This directory holds standalone experiments and comparison benchmarks used to
inform Engine Box development. They run manually with explicit inputs and are
excluded from the product's pnpm/Cargo workspace and release artifacts.

| Directory | Purpose |
| --- | --- |
| [layerfs-validation](layerfs-validation/README.md) | Pinned upstream LayerFS experiments, dependency/cache controls and a disposable Incus attachment campaign. |
| [storage-benchmarks](storage-benchmarks/README.md) | Storage distribution and recovery qualification; current product checks are in the fleet packages. |
| [agent-startup](agent-startup/README.md) | Manual remote-agent timing probes with separate admission, VM readiness and completion measurements. |

Product services, CLIs, installers and libraries remain in `packages/`. Tests that
verify their supported behavior also remain beside them: unit and regression
suites, native agent/browser qualification, update/rollback tests, storage recovery
tests, and the Rust lifecycle boot measurements. Those tests call the production
interfaces and guard release behavior; using a disposable VM does not make them
standalone research.

Follow each experiment's prerequisites and ownership checks. Run state, source
copies, inventories, credentials and reports belong in private ignored storage
such as `.state/`, never in this directory. Documented results remain in `docs/`.
Moving a harness here does not qualify its assumptions for production, change its
explicit live-run requirements or make it part of normal CI.

The LayerFS harness still uses `/opt/layerfs-validation/` inside its disposable
Linux fixture. That is an owned guest runtime path, independent of this repository
layout.
