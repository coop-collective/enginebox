# Prepared Actions dependency comparison

Standalone research harness; product code does not depend on it. Use only fresh,
owned Linux containers/volumes with no credentials or developer workspace data.
The root helper needs `CAP_SYS_ADMIN` solely for private bind mounts inside this
disposable container. Do not expose the Docker socket to it.

Mount the source `packages/` and `research/` read-only at `/repo/packages` and
`/repo/research`; mount a fresh Linux named volume at `/project`. Provide Node
24.22.0, pnpm 12.15.0, Python 3, mount/umount, and set
`ENGINEBOX_DISPOSABLE_PROOF=1`, `BENCHMARK_FIXTURE=frontend`. The measurement used
Linux arm64 on Docker Desktop, 2 CPUs, 2 GiB RAM, and a 256 PID bound.

First run with network access to obtain the pinned public fixture dependencies:

```sh
node /repo/research/actions-preparation/benchmark.mjs seed
```

Then run a fresh container using the same disposable volume with networking
disabled, `--cap-add=SYS_ADMIN --security-opt=apparmor=unconfined`, and the same
limits/toolchain:

```sh
node /repo/research/actions-preparation/benchmark.mjs protected
```

It performs 21 pairs, alternating execution order. Each conventional case starts
without node_modules and installs offline from a **warm pnpm cache with default
import mode**. Each prepared case includes the production configuration/runtime/
source fingerprint, read-only generation verification, root handoff and receipt.
Both execute the same dependency assertions and Vitest fixture; validation is
outside the dependency-ready timer. The fast case proves zero package downloads.
A root write attempt to the protected tree must fail.

The harness writes private `/project/result-protected.json`, including all samples.
It exits nonzero unless median readiness is at least **1.2× faster** and sampled
p95 is lower than conventional warm install. `BENCHMARK_MIN_SPEEDUP` can raise
that threshold. This is a research/recipe gate; it does not self-approve a fleet
catalog entry. Owner approval and real pool qualification remain separate.

Fixture reset is outside the timer; production gets a separate private LayerFS
branch for every job. This harness **does not measure LayerFS fork/mount, checkout,
VM provision, generation transfer or whole-job cost**. It exercises the exact
recipe and root helper on a Linux named volume. SDK branch/reopen qualification
is separately recorded in [validation](../../docs/reports/ACTIONS-PREPARATION-VALIDATION.md).
No production p95, cross-node startup or overall GitHub cost claim follows from
these samples. Tiny workloads can be slower because validation has fixed costs.

The first implementation rescanned every dependency file. On a 36 MB fixture,
its median was 1535 ms versus 170 ms for conventional warm pnpm. Protecting the
approved tree and avoiding redundant configuration subprocesses removed that
regression. The final frontend fixture contains 91,114,103 dependency bytes:
React/React DOM 19.3.1, TypeScript 5.9.3, lodash 4.17.21, @types/node 24.10.0,
Vite 8.3.4, Vitest 5.0.5, plugin-react 6.1.2, Tailwind 4.3.3 and Playwright 1.64.1.
Exact resolved dependencies are pinned in [the standalone fixture lockfile](fixture/pnpm-lock.yaml).
The [21-pair numeric results](results-linux-arm64.json) passed the 1.2× gate. They include
no Store snapshots, package caches, account state or real customer source.

Final recorded median: 127.5 ms conventional warm pnpm vs
83.3 ms protected preparation (34.7% less time;
1.53× speedup). Sampled p95: 175.1 ms vs
102.0 ms. Both produced correct dependency/test results and the
prepared case downloaded zero packages. These are dependency-stage diagnostics.
