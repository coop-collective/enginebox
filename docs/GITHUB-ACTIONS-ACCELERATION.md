# LayerFS preparation for GitHub Actions

Status: **implemented opt-in foundation**, October 25, 2026. Required runner
preparation, the post-checkout pnpm action, owner-approved catalog, verified fleet
transfer and explicit native Store activation are implemented. See
[operator setup](PREPARED-ACTIONS-SETUP.md) and [tested evidence](reports/ACTIONS-PREPARATION-VALIDATION.md).
Live GitHub/Incus qualification, automatic warm compute placement, automatic
workflow PR creation and comparative performance/cost qualification remain open.

## Decision

Make preparation a requirement of an **accelerated runner profile**. Every job
accepted by that profile must have a private LayerFS workspace, the profile's
compatible caches, and a preparation receipt before workflow steps start. A
missing mount or invalid authorization fails preparation. A missing cache can
produce an explicitly reported cold run; it must never be reported as a hit.

Offer two levels with distinct guarantees:

| Level | Workflow adoption | What Enginebox prepares |
| --- | --- | --- |
| Compatible execution | Change runner group/label | LayerFS-backed working directory, pinned tools and supported package caches outside the checkout; ordinary checkout/install commands still run. |
| Prepared dependencies | Also replace the install/cache steps with an Enginebox preparation action after checkout and tool selection | Validate actual source and installation inputs, attach a compatible installed tree when qualified, otherwise perform the recipe's normal locked install. |

The second level is the intended product advantage: fork usable dependency state
instead of repeatedly downloading, unpacking and installing it. The first level
provides a migration path and can reduce downloads; merely placing files on
LayerFS does not establish faster builds. Neither level skips tests or accepts
previous build results as evidence for new source.

OAuth/App installation authorizes access, `workflow_job` webhooks signal demand,
and the official runner remains responsible for executing Actions. Preparation
happens in Enginebox's worker and runner hooks. There is no global webhook that
injects cache behavior into GitHub-hosted jobs.

## Supported integration points

GitHub's [job hooks](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/run-scripts)
provide `ACTIONS_RUNNER_HOOK_JOB_STARTED` and `ACTIONS_RUNNER_HOOK_JOB_COMPLETED`.
The start hook is synchronous, can use environment files, and fails the job on
nonzero exit. Install bounded, administrator-owned hooks outside the runner
application directory. The completion hook can report measurements; the host
worker must still own teardown after runner exit.

Source inspection of [runner v2.337.0 JobExtension](https://github.com/actions/runner/blob/v2.337.0/src/Runner.Worker/JobExtension.cs)
shows directory preparation, context setup and action download happen before
the start hook. That hook precedes container startup and workflow action steps.
Consequently, provision the work filesystem **before starting the listener**;
do not replace the whole work root in the hook and hide downloaded actions or
runner bookkeeping. A start hook is not a supported general hook after every
checkout step, and does not receive all future evaluated checkout inputs.

[JobHookProvider](https://github.com/actions/runner/blob/v2.337.0/src/Runner.Worker/JobHookProvider.cs)
processes the hook's environment files. Use those for recipe-specific package
configuration, not to spoof `GITHUB_WORKSPACE` or GitHub's expression contexts.
The hook runs as the runner user: it is not an isolation boundary and must not
hold controller credentials or authorize arbitrary mounts.

These are reviewed upstream interfaces, not a live-tested Enginebox runner
version. Pin and qualify the exact runner release and action commit SHAs before
enabling a profile.

## Workspace and dependency layout

Keep the runner installation, registration credentials and diagnostics on the
disposable VM's private disk. Before its listener starts, set native JIT `work_folder` to `/project/work` on
the attached private LayerFS branch. Administrator bootstrap validates the mount
and creates fresh directories; the official runner creates its normal work
layout. The implemented layout is:

```text
private LayerFS job branch
  work/                     -> native runner _work
    <repository>/<repository>/  ordinary GITHUB_WORKSPACE and checkout
    _actions/               runner-managed action downloads
    _temp/                  private job data; never a reusable cache
  caches/
    packages/               recipe-specific writable cache fork
    metadata/               private package metadata/cache state
  prepared/                 approved dependency-generation inputs

private VM disk
  runner/                   official binary, JIT state, logs
  home/                     private tool configuration and credentials
```

Never attach a previous job's work root. Runner temporary files, action
credentials and arbitrary workflow writes can contain secrets. Reusable inputs
are separate approved generations, with each job receiving isolated writes.
Cache mounts/paths stay outside every checkout destination. Do not put a mount
inside a directory that `git clean` may recursively delete.

For the initial Linux host-job profile, install pinned tools in the clean base
and configure package cache paths through each tool's supported settings. Start
with pnpm rather than claiming support for every package manager. An arbitrary
workflow can override environment settings, delete dependencies or compile
uncacheable inputs; mandatory preparation cannot force a cache hit for those
commands. Record the effective configuration and cache miss reason.

Container jobs need separate qualification. A host path in a package environment
variable is not automatically the same path inside a job container or Docker
action. The runner maps work and tool directories into containers
([ContainerInfo](https://github.com/actions/runner/blob/v2.337.0/src/Runner.Worker/Container/ContainerInfo.cs));
cache mounts and environment paths must agree there too. GitHub also offers
Linux [container customization hooks](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/customize-containers),
currently a public preview. Use a distinct container profile after proving its
mount/path contract. Do not force arbitrary existing jobs into a container or
advertise unsupported container workloads as accelerated. Docker stays inside
the disposable VM, never on an exposed host socket.

## Preserve checkout correctness

Leave the workflow's official `actions/checkout` in place. Reviewed
[checkout v7 cleaning code](https://github.com/actions/checkout/blob/v7/src/git-directory-helper.ts)
removes untracked/ignored files during normal cleaning, and can delete the
directory's contents when the repository is absent or mismatched. Preloading
`node_modules` into that directory before checkout therefore loses the intended
benefit. Turning off cleaning globally creates stale-source risks.

The post-checkout preparation action must inspect the actual selected worktree
and toolchain, including a configured checkout `path`. Keep ref overrides,
pull-request merge commits, sparse checkout, submodules, LFS, multiple checkouts
and checkout's security/credential behavior under the official action's control.
`GITHUB_SHA` and the queued webhook are insufficient to identify what a later
checkout step actually selected. Unsupported arrangements use ordinary install
or fail an explicitly strict profile with a clear reason.

Git remains confined to this dedicated Actions compatibility environment and
the existing reconciliation environment. Normal LayerFS project workspaces
remain Git-free. Accepted LayerFS main is not automatically the Git commit a
workflow requested. A later source optimization would require a verified mapping
of repository ID, Git commit/tree and LayerFS source generation, plus equivalent
checkout semantics. It is not part of the first acceleration slice.

## Reuse installed dependencies after checkout

The preparation action replaces the dependency-install operation; it
does not wrap arbitrary shell commands or intercept `git`, `npm` or the Actions
protocol. It runs after checkout and tool selection, before build/test:

1. Read the actual checkout's dependency inputs and effective tools. Produce a
   bounded manifest and look for the exact generation already admitted by the
   host resolver. A different key uses ordinary install or fails require-hit;
   v1 does not request additional generations from inside the job. No owner token
   or generic filesystem API enters the job.
2. On an exact compatible hit, create the private installed-tree view at its
   qualified path. The root helper validates the immutable manifest and one-use request while the
   approved tree stays read-only; it renames the tree through a root-only view of
   the same private fork. No per-file content scan is needed at handoff because
   workflows cannot modify the generation first. Standalone/local recipe calls
   retain full content verification. Validate the receipt and perform required
   consistency work. Preserve the checkout's tracked source.
3. On a miss, run the recipe's ordinary locked install using approved download
   caches where possible. A missing or incompatible generation must not produce
   a partial tree that is reported as ready.
4. Run all requested builds/tests. Record dependency operation time, downloaded package count,
   generation identity and miss reason, separately from workflow success. Exact
   downloaded bytes and broader job timing remain qualification instrumentation.

An installed-tree key includes tenant/installation and repository IDs, allowed
trust class, base-image digest, OS/architecture/libc, runtime/compiler ABI,
package-manager version, dependency manifests and lockfiles, workspace/package
configuration, patches, install flags, lifecycle-script policy and recipe
revision. Include relevant source when install scripts or local workspace
dependencies consume it. A lockfile match alone is insufficient. Keep stable
guest paths; native modules and package metadata may embed them.

Do not silently skip package lifecycle scripts: define which effects the recipe
captures, which must run for every job, and when reuse is disallowed. Start with
plain Node/pnpm dependencies; native modules and source-dependent installation
need independent fixtures. The prepared action may still run a cheap package
consistency step. Measure that work instead of describing every hit as zero work.

Leaving a subsequent `npm ci` in the job defeats installed-tree reuse because
[npm explicitly removes existing node_modules](https://docs.npmjs.com/cli/v11/commands/npm-ci/).
The adoption flow should generate a reviewable workflow change that replaces
the install operation and removes only redundant cache transfers. Keep unrelated
artifact and application caches. Do not pretend an unchanged `actions/cache`
step uses LayerFS: it retains GitHub's cache protocol. Likewise, setup actions
with their own cache restore may overwrite local state and need review.

There is already a useful narrow
[LayerFS dependency experiment](reports/LAYERFS-VALIDATION.md#dependency-cache-control):
one TypeScript package was reused with zero downloads using an immutable pnpm
store, a seeded private metadata cache, offline/frozen-store settings and copy
import. Ordinary read-only-store attempts failed, including native ext4
controls. Changing mount paths also caused metadata writes. This is evidence
for a specific recipe, not a monorepo, native-module or Actions qualification.
The report records pnpm 12.14.0; today's workspace pin is 12.15.0 and must be
tested explicitly. [pnpm's frozen-store contract](https://pnpm.io/settings/store#frozenstore)
requires that no other process modifies the published store during immutable
reads. A job-private writable fork is a different mode; never enable immutable
reads while another writer can change that database.

## Bind preparation to authorized jobs

GitHub routes by [runner group and labels](https://docs.github.com/en/actions/reference/runners/self-hosted-runners),
not by the webhook that caused provisioning. The current worker receives that
webhook's repository and uses it to choose an optional LayerFS base. A broad
pool can receive another compatible repository's job. Preloading private data
from the demand event is therefore not an acceptable service boundary.

For the first profile, use a **single-repository runner group and an explicit
cache-read policy shared by every workflow allowed in that group**. Check the
actual group's selected repository IDs and applicable workflow restrictions
through the [GitHub group API](https://docs.github.com/en/rest/actions/self-hosted-runner-groups)
before admission. Labels alone do not enforce repository isolation. If the
organization cannot enforce the required access boundary, do not preload its
private repository/dependency generations. Only non-sensitive tools/public
packages can be present before a stronger assignment-bound mechanism exists.

Repository restriction alone does not distinguish trusted and untrusted code.
Preloaded contents must be safe for every admitted job to read. If one workflow
can execute fork code, do not preload a trusted-only or private cache into that
shared access domain. Privileged/release workflows need a separate reviewed
profile. Mutating runner-group access during an admitted attempt is an
administrative change requiring drain/reconciliation; a one-time API check is
not a durable policy lock.

The start hook verifies the assigned job against its expected repository/policy,
with the service corroborating run attempt and runner identity through GitHub.
Record actual execution identity separately from the demand job ID. `GITHUB_JOB`
is a workflow job key, not the numeric REST job ID; do not match by job name
alone. A mismatch fails before workflow steps and never authorizes new data.
The hook does not repair a broad group's already-exposed private mount.

Later, generalized pools can start with an empty private LayerFS work root and
request repository generations only after verified assignment. That requires a
bounded, attempt-scoped host broker and qualified attachment/materialization
protocol. The current Incus adapter attaches only to stopped VMs; do not assume
it supports safe hot attachment, or give a guest the Incus/controller socket.

Ordinary jobs cannot publish trusted reusable generations. Build them in a
separate clean preparation environment from an approved recipe/revision, with
restricted package-fetch credentials when required. Publish only allowlisted,
credential-free outputs with provenance. Private registries do not make their
package content public. Logs, homes, registry credentials, runner state and job
checkpoints never become reusable bases. Untrusted job results remain private
or are discarded under bounded retention policy.

## Distribution and ownership

Target the existing Cloudflare fleet authority for preparation catalog entries,
leases, placement and receipt acceptance. Nodes own native execution and local
LayerFS branches. R2 retains immutable generation chunks; optional storage nodes
and node-local caches provide the existing verified read path. Prefer a node
that already holds the required generation when capacity permits. A cold miss
must account for transport, verification and mount time before useful work.

The current [fleet transfer](LOCAL-FLEET.md) hydrates a complete verified SQLite
snapshot from bounded chunks. It does not lazily mount arbitrary files directly
from R2. Explicit native activation of a transported preparation Store is implemented
and tested for direct SDK reopen/branch isolation. Its subsequent Incus mount and
Actions execution remain qualification gates. Do not advertise instant cross-node startup based
on local forks. Smaller recipe-specific dependency generations avoid coupling
each job to an entire project Store; that packaging also needs implementation.

Keep the GitHub adapter optional. Generic preparation manifests, resource leases
and immutable-cache contracts belong to Enginebox; GitHub identities, runner
hooks and workflow adoption belong to the Actions module. The deployed Rust
controller and the Cloudflare stack must not both own an attempt. A local native
proof can precede migration, but is explicitly local qualification.

## Implementation sequence and release gates

Implementation and qualification are tracked separately:

| Slice | Implemented | Remaining qualification/capability |
| --- | --- | --- |
| 1. Required preparation and routing | Single-repo/workflow group checks, native LayerFS work root, mandatory bounded hook, host actual-job verification | Real GitHub assignment/cancellation and Incus mount/teardown under this profile |
| 2. pnpm recipe | Explicit script policy, exact configuration key, checked installed tree, cold locked fallback, private receipts, owner catalog | Native modules and broader monorepo/checkout compatibility |
| 3. Cross-node preparation | Existing verified R2/storage-origin transfer, bounded snapshot cache, discovery-record binding, explicit immutable-main activation | Native mount/build after cold transfer, concurrent fleet placement and eviction of retired live Stores |
| 4. Adoption and release | Dashboard profile selection and commit-pinned workflow preview/download, operator guide, offline and disposable Linux tests | Automatic workflow PR creation, real private-repo canary, speed/cost measurements |

The first live canary should run this repository's locked pnpm install and a
representative build/test on disposable Linux capacity. Include a known failing
test so cached preparation cannot mask failures. Test dependency changes,
source-dependent install scripts, path changes, submodules/LFS and multiple
checkouts with separate fixtures. Container jobs, hostile public contributions
and native Windows/macOS are separate release profiles, not inherited claims.

Compare an ordinary ephemeral runner with its normal cache enabled, the current
Enginebox runner, and cold/warm prepared Enginebox on identical source and
resources. Also compare a conventional runner with equivalent local warm caches
to isolate LayerFS's contribution. Report queue, provisioning, hydration,
checkout, dependency, build/test and teardown time; p50/p95, downloaded bytes,
cache correctness, disk growth and full compute/idle/storage/network cost.
Include concurrent jobs and a fresh node. A local fork timing or one-package
offline install is not evidence that the service beats GitHub Actions overall.

## Measured implementation adjustment

The initial per-job dependency hash scan lost badly to conventional warm pnpm.
The implemented accelerated worker therefore protects approved inputs read-only
and uses a bounded guest-root handoff, preserving generation validation while
avoiding repeated dependency reads/copies. Workflows run with no-new-privileges
and cannot access privileged service groups/sockets under this host-job profile.
Runtime/config/source checks and actual tests still run. See the
[reproducible comparison and benefit gate](../research/actions-preparation/README.md).
A dependency-stage win is not evidence of a full-job or cold-node cost win.
