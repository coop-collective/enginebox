# Enginebox integration and CI

Enginebox owns the reusable task/agent workflow: private workspaces, submissions, batching, merge/import execution, validation, evidence and accepted channels. Gamejam is an application consuming Enginebox APIs. It presents chat, ideas, tasks, reviews and playtests, and submits the owner's choices; it must not implement a second merge queue, agent scheduler, storage controller or CI authority. Native coding clients retain their own planning loops and authentication.

This boundary applies to ordinary software projects as well as games. Enginebox core remains usable without a Git forge or a CI provider. Required checks are an optional project policy, enforced by Enginebox once configured. Gamejam projects should configure that policy through Enginebox; a UI approval never substitutes for missing validation.

## The Beanstalk principles

The source review covered Gitstalk checkout `63dae2ccf2cf76d09384f525042c21f163f383fc`, especially its [validator](https://github.com/coop-collective/gitstalk/blob/63dae2ccf2cf76d09384f525042c21f163f383fc/packages/gateway/src/engine/v2/v2-validator.ts), [landing rules](https://github.com/coop-collective/gitstalk/blob/63dae2ccf2cf76d09384f525042c21f163f383fc/packages/gateway/src/engine/v2/v2-landing.ts), [CI capacity/cancellation](https://github.com/coop-collective/gitstalk/blob/63dae2ccf2cf76d09384f525042c21f163f383fc/packages/gateway/src/engine/ci.ts) and [optional skipper contract](https://github.com/coop-collective/gitstalk/blob/63dae2ccf2cf76d09384f525042c21f163f383fc/docs/09l-optional-test-skipper.md). These are inspected source contracts, not a claim about every hosted repository's enabled settings.

| Principle | Enginebox contract |
|---|---|
| Small independent beans | Each task works on a private LayerFS branch with an exact base and immutable result identity. Agents do not write the accepted project directly. |
| Sprout before stalk/main | Integration produces a frozen candidate. The accepted/live channel advances only after its required full checks and policy decisions. |
| Parallel preparation, short publication | Many tasks prepare concurrently. Compatible results share a candidate and validation run; only the accepted-head compare-and-swap is serialized. |
| Bounded unvalidated work | Task, integration, CI and editor leases use explicit capacity. Cancellation does not release native capacity until the prior executor is reconciled. |
| Failure stays attributable | A failed batch retains its receipts and leaves accepted state unchanged. Automatic splitting/bisection remains planned; an operator can submit a new smaller batch. |
| Protected evidence | Check identity includes the candidate, generation, recipe, executor and scope. Selected-test success cannot satisfy a required full check. A changed candidate or recipe requires new evidence. |
| Safe skipping | Selection is an opt-in pre-land optimization. Unknown inputs/dependencies fall back to full checks. Full candidate validation remains required. |

One hundred tasks must not imply one hundred serialized editor startups and full suites. The intended flow is concurrent headless work, bounded integration batches, and a reusable engine pool validating frozen candidates. A batch may contain many task generations; one exact candidate build can provide their shared preview. Isolated per-task previews remain an explicit capacity-consuming operation.

## Implemented controller gate

The Cloudflare Project Durable Object now stores protected check requirements and immutable candidate identities. A candidate names its base, integrated generation, and **1–128 change IDs with their result-generation hashes**. These are integration records; registering a list does not perform a merge. All referenced generations must already be published. Enginebox's integration executor is responsible for materializing their combined content.

The owner configures checks with `POST /v1/projects/:project/checks`:

```json
{
  "checks": [
    {
      "name": "store-integrity",
      "recipe": "<sha256-of-the-versioned-verifier-recipe>",
      "kind": "verify"
    }
  ]
}
```

`verify` proves snapshot bytes and SQLite integrity only. A compile/test recipe uses `kind: "layerfs-exec"` and includes its protected `argv` and `timeoutSeconds`. Enginebox rejects a job with another executor, command, timeout or recipe. The recipe identifier is supplied by the trusted owner/integration service; it should bind engine/toolchain, plugins, target, environment and command version. A native execution base must separately satisfy the pinned LayerFS qualification contract.

Stage through `POST /v1/projects/:project/candidates` with:

```json
{
  "id": "batch-42",
  "project": "demo",
  "base": null,
  "generation": "<integrated-generation-sha256>",
  "changes": [
    { "id": "task-one", "generation": "<task-result-generation-sha256>" },
    { "id": "task-two", "generation": "<task-result-generation-sha256>" }
  ]
}
```

The candidate freezes the current protected requirements. A verification job binds its `generation` and adds `check: {candidate, name, recipe, scope}`. Scope is `full` or `selected`; only a completed **full** job for the matching recipe qualifies. Starting another full attempt invalidates that check's previous qualifying job until the new attempt completes. Successful results must report the same check identity and scope. Changing project requirements invalidates the old candidate's evidence.

Promotion uses `POST /v1/projects/:project/accept` with `{generation, expected, candidate}`. It rejects missing/red/incomplete full checks, mismatched candidate identity, stale base and changed recipes. Its durable R2 decision records the complete candidate membership and qualifying job IDs. Pending acceptance freezes that evidence until the decision settles, including after a restart; another full attempt cannot replace it midway through writing the receipt. The normal parent/head compare-and-swap still applies. Repeating an already completed promotion returns its original receipt.

With an empty required-check policy, Enginebox remains a storage/workspace service and retains explicit owner-controlled acceptance. Neither mode enables an agent to accept its own changes. Gamejam should use the configured gate, presenting its result rather than inferring success from uploads, cache hits or a completed agent conversation.

The runtime tests include a **100-change candidate** covered by one full check, rejection of selected results, recipe/executor drift, stale sessions and concurrent dispatch. This qualifies the controller records and gate, not 100 native agents or a 100-way LayerFS merge. `pnpm fleet:smoke` exercises the configured gate with the actual built-in storage verifier in separate containers.

The [logical integration executor](LOGICAL-INTEGRATION.md) now materializes an operator-selected batch from immutable references in one node-local LayerFS Store. Successful integration publishes a generation, moves member tasks to Testing and queues protected `verify` and `engine-build` checks. [Engine build recipes](ENGINE-BUILDS.md) bind exported source, toolchain and immutable playable artifacts. The controller records owner review receipts and enforces optional [playtest evidence](PLAYTEST-EVIDENCE.md) before promoting the batch. Gamejam's approval action calls this controller; it does not coordinate acceptance itself. A real two-task Godot flow has exercised integration, Linux/Web builds, browser recording, CPU media validation and reviewed acceptance; see the [qualification record](LAYERFS-INTEGRATION-VALIDATION.md).

## Test reuse and engine caches

Gitstalk's current optional skipper supports a restricted Node ESM test runner, caps analysis at 16 MiB and 10,000 files/directories, and starts from a full green baseline with unchanged before/after input snapshots. Selected passes never seed a new full baseline. Configuration, lockfile, fixture or inventory changes, unsupported dependencies and analyzer errors fall back to the full suite. The gateway rejects a selected response to required full validation.

Do not apply that analyzer to an Unreal/Unity project or treat unchanged source paths as proof that assets are unaffected. An engine-specific analyzer would need code and asset dependencies, importer/build configuration, engine/plugin versions, target platform, compiler/shader settings and the relevant tests. Dynamic or unknown dependencies require broader checks. Binary conflicts need explicit resolution; `.uasset`, scene and model changes do not become mergeable because their storage uses chunks.

Cache reuse and validation reuse are separate. Shader/import/build caches can reduce work while a required full check still runs. Reuse needs compatible toolchain/input keys and verified retained bytes. A selected test result or cached cooked artifact alone cannot certify a new integrated generation. The present implementation deliberately has no automatic test-skipping or cross-generation green-result reuse.

## Remaining Enginebox work

Automatic compatible-batch selection, dependency handling, bounded sprout windows, red-batch isolation/bisection, engine-pool scheduling and engine-aware test analysis remain Enginebox work. The current operator-driven executor is qualified for same-Store logical integration and Godot Web/Linux builds. Unity/Unreal recipes require native qualification, and large real-project editor/cache startup remains unmeasured by this flow. Cross-node result import must reconstruct a branch from its base using supported LayerFS operations; independently written SQLite databases and WAL files are never merged. Same-node `branch.publish` is not a complete distributed acceptance protocol.

Gamejam should integrate through these shared Enginebox capabilities rather than filling those gaps with app-specific infrastructure. Its remaining work is application UX, game/community data and presentation of Enginebox task, review, session and release state.
