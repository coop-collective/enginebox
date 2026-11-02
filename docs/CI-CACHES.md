# Verify dependency caches

`verify.yml` restores pnpm's download store after installing the pinned pnpm,
using setup-node's lockfile-keyed cache. The second setup-node invocation is
intentional: the first selects Node 24, then pnpm is available for store discovery.
`pnpm install --frozen-lockfile` always runs; installed `node_modules` is not cached.

After the frozen install selects the pinned Rust toolchain,
`Swatinem/rust-cache` v2.9.3 uses `cmd-format: "pnpm exec {0}"` to inspect the
actual workspace compiler instead of downloading/inspecting a host-default
toolchain. It restores Cargo registry/git downloads and
compatible target dependency artifacts. Its keys include compiler identity,
Cargo inputs and the explicit OS/architecture namespace. Default cleanup removes
workspace-crate artifacts and incremental state; this does not promise zero
workspace compilation. `cache-bin: false` excludes installed executable tools.
Only successful main pushes publish Rust seeds (`save-if`); PRs restore them.
Main and PR use the same key namespace so development artifacts can be reused
even when main additionally builds release artifacts.

GitHub's [cache scope rules](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching)
allow PRs to restore default-branch caches. pnpm may also save a PR-merge-ref
cache; main cannot restore that PR cache. Seeds first become available after a
successful main run with this workflow. A PR run before merge can therefore be
cold. Lock/compiler/input changes can miss or restore a compatible fallback;
inspect the action's resolved key and hit logs rather than assuming a hit.

The rechecked baseline [run 41384962825](https://github.com/coop-collective/enginebox/actions/runs/41384962825)
had no configured caches. Windows install/check/test/build took 13/84/388/711 s.
The hypothesis is less download and dependency compile work on repeated runs;
cache transfer, workspace compilation and all checks still cost time. Measure
restore/save time and compile time over at least 20 main/PR runs before claiming
net savings. Local warm targets are not a measurement of GitHub cache hit rate.

No validation result is cached or skipped. This changes neither full LayerFS
candidate checks nor runner placement. Private inventories/raw connection logs
stay outside Git. Upstream behavior: [rust-cache v2.9.3](https://github.com/Swatinem/rust-cache/tree/v2.9.3),
[setup-node v7.1.0](https://github.com/actions/setup-node/tree/v7.1.0).
