# Workspaces

The dashboard's Workspaces view groups code by durable project ID. Hosted Git
repositories, optional GitHub connections, LayerFS sources, native agent pools,
and development services appear under that identity. An environment is a
runtime allocation; deleting or replacing one does not create a new workspace.
The previous `?panel=repositories` and `?panel=github` URLs remain supported.

## Source and execution flows

- **New workspace → New hosted repository** uses the existing repository backend
  and project-scoped API. The gateway must be configured and its project namespace
  provisioned. Creating the repository does not materialize source in LayerFS.
- **Existing GitHub repository** links a repository already approved through the
  connected GitHub App. Use GitHub connection to choose repositories or approve
  permissions; this does not require a second installation. The source review
  link carries both project identity and repository selection into Storage.
- **Review and prepare source** uses the existing reviewed import and LayerFS
  controls. Normal LayerFS workspaces remain Git-free. Hosted repositories are
  a compatibility source API; automatic hosted-repository-to-LayerFS bootstrap
  is not implemented by this view.
- **Run an agent** submits to an existing project pool through the native run
  API. A pool's configured profile and provider credential delegation remain in
  force. Optional repository access grants native Git compatibility access; it
  does not imply checkout, dependency preparation, acceptance, or promotion.
  Unconfirmed submissions are never automatically replayed.
- Development services expose their existing URLs. Runtime state and pool
  leases are distinct: a working/reserved lease does not prove that its host
  is online. Unobserved host states are shown as unknown.

`GET /v1/workspaces` is a redacted owner-only projection, not a new source of
project authority. It joins repository records, project pools, storage catalog,
retained native runs, launch events and measured controller jobs. It does not
return task text, process output, credentials, filesystem paths or environment
variables. Existing delegated APIs remain available: repository write grants can create
hosted sources, and native run grants remain scoped to their project pool.
GitHub source mapping remains owner/admin-only: account-level repository
selection does not authorize an agent to attach that source to its project.
Neither read nor write repository grants permit linking arbitrary selected
GitHub sources. The aggregate grants no new agent authority.

## Measured coverage

The view distinguishes absent measurements from measured zero. It is an
observation view, not a performance benchmark or cost-savings claim.

| Signal | Measurement and scope |
| --- | --- |
| Native activity | Up to 256 retained run summaries; total run duration is never labelled as test or dependency-install time. Pool working counts include retained leases requiring reconciliation. |
| Environment launch | Bounded native launch-event journal, with measured stage duration and queue wait when reported. |
| Source preparation | Environment-service `metrics.preparation`: source copy time, validated regular source-file logical bytes, optional configured preparation-command time, total elapsed time, revision, observation timestamp and success/failure. Git copies and incomplete source copies have unknown source bytes. |
| Prepared revision reuse | Separate `metrics.preparation_reuse` receipt records a successful marker reuse and its elapsed time without overwriting the actual copy/prepare receipt. This is not a dependency-cache hit rate. |
| Development startup | `metrics.startup` measures starting services and reaching readiness, including typed failed receipts. |
| Source refresh | `metrics.source_update` reports application time, logical source bytes and written/removed file counts. |
| Storage operations | Measured controller operation duration excludes queue wait. New jobs record validated project identity at admission. Checkout-only operations use the latest successful host inventory; unobserved or legacy unattributed jobs stay unknown. Up to 30 operations per project are drawn from the retained 200 ordinary jobs. Import `bytes` means logical source bytes. |
| Explicit command timings | Native CLI `storage layerfs exec <checkout> --measure test\|build\|dependencies\|dev\|other -- <command> ...` records the latest receipt per category. Receipt contains declared category, success/failure/uncertainty, elapsed milliseconds and observation timestamp, never argv or output. Interrupted receipts remain uncertain with null elapsed time. Durations include executor helper overhead; native `deps` records the dependencies category automatically. Storage list exposes these on each checkout. |
| Checkouts | Latest successful `list` receipt per node, with its observation timestamp. A subsequent empty list clears older checkout observations. Refresh Storage to collect a new receipt. |
| Integration | Latest successful project `github-settings` receipt per host supplies accepted main revision and export state. A checkpoint or successful agent run is never called merged. Review/promotion authority remains with the existing acceptance flow. |

Environment receipts contain no commands, command arguments, output, environment
values or credentials. Durations use a monotonic clock; wall-clock timestamps
identify observations. The latest receipts are bounded and survive runtime
state persistence. Preparation commands are deliberately labelled **configured
preparation command**: they may install dependencies, but the system does not
infer that classification from shell text. Native CLI executor calls can opt in with `--measure` and an explicit category;
the latest bounded receipt per category appears after a Storage refresh. This
records declared intent, not a heuristic classification of shell text. Arbitrary
native-harness commands that bypass this path are not individually instrumented.
The dashboard continues to refuse arbitrary exec requests. Command timing
receipts and assistant prose are not CI acceptance evidence.

Cloudflare fleet project storage and cache indexes have a separate authority and
are not queried by this native-controller dashboard projection. Shared native
pnpm caches must not be attributed as exclusive per-project disk usage. Source
bytes are not physical cache bytes, and observed reuse does not establish speed
or cost improvements without a comparable baseline.

## Ideator integration verified in source

Ideator can create an Enginebox repository through its owner/admin operation,
record the project repository reference, and pass it to native runs with scoped
credentials. Existing repo bindings are not silently replaced. The current
Enginebox dispatch gate in Ideator accepts planning/lead/loop work, requires no
feature ID, and excludes builder and verification tasks. This dashboard change
does not enable builder dispatch or modify Ideator's legacy Git execution path.
A repository being present is therefore not proof that Ideator has provisioned
a LayerFS development environment or that a feature has been accepted.

Native Storage list receipts also report a bounded usage sample, cached for up to
five minutes. `storage_usage.projects` measures each native Store volume, mapped
through known GitHub generations to its logical project; `shared_caches` measures
root-level pnpm store/metadata volumes and is explicitly **shared node cache**.
The collector reports logical regular-file bytes, allocated file blocks and
file counts, deduplicating hardlinks within each volume. Allocated blocks may
share CoW extents and are not unique physical storage. A partial sample is a
lower bound; unavailable values are null. The dashboard displays collection
status and observation timestamp and never sums shared caches as exclusive
per-workspace storage.
