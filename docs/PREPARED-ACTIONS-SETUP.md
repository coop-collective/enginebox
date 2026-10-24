# Prepared GitHub Actions setup

Implemented October 25, 2026; **opt-in Linux host-job profile, awaiting a live
GitHub/Incus canary**. See [design](GITHUB-ACTIONS-ACCELERATION.md) and
[qualification evidence](reports/ACTIONS-PREPARATION-VALIDATION.md). Normal
workspaces and the core runtime remain independent of GitHub.

## 1. Bind an accelerated pool

Complete [GitHub App and Kitbash secrets setup](GITHUB-SECRETS.md). No new OAuth
scope injects dependencies into workflows. The runner App supplies organization
runner administration and repository Actions read; webhooks supply demand. A
separate repository-scoped Actions-read token stays in the host verifier and
never enters the VM. The official runner still runs the workflow.

Use one private repository and a group restricted to its numeric repository ID
and explicit workflow references. All admitted code must be allowed to read this
repository's dependency generation; initially use protected-branch/manual jobs,
not hostile pull requests. GitHub plan support for workflow-restricted groups is
a prerequisite. The controller refuses a broad group or differing restrictions.
Drain before changing its membership, allowed workflows, image or recipe.

Add this field to the corresponding pool in private `github.json`:

```json
"preparation": {
  "repository_id": 789,
  "workflows": ["example-org/private-project/.github/workflows/ci.yml@refs/heads/main"]
}
```

Use [ci-worker-prepared.example.json](../examples/ci-worker-prepared.example.json)
for that pool's worker. Match the repository and **runner App installation** IDs.
`acceleration.image` must be the installed **registered template manifest digest**,
not an arbitrary cache label. The worker checks it before cloning. Register and
synchronize the credential-free template through the existing template service;
static inventory-only templates cannot satisfy this profile. Bake and qualify
Node, exact pnpm, Python 3, GNU timeout, util-linux setpriv and the official runner into that template.
The runner executes with no-new-privileges; privileged service groups/sockets are refused. This host-job profile does not support sudo installs or Docker jobs.
Keep the guest user, home, tool versions and paths stable across preparation and
jobs. The example's zero digests and IDs are placeholders.

The controller generates JIT configuration with `work_folder: /project/work`.
After attaching and verifying the private LayerFS fork, root bootstrap installs a
bounded hook and policy under `/run/enginebox-ci`. The hook checks the mount,
workspace, repository, workflow and actual run attempt; host-side GitHub job
inspection verifies the assigned runner and numeric job. The demand webhook's
job may differ. A missing attachment or verification failure blocks job steps.
The approved dependency directory is bound read-only before listener startup. A root-only alternate view and one-use guest helper perform the exact directory handoff after checkout. The helper checks the assigned repository, immutable manifest, input key and non-symlink destination, then renames within the private fork without scanning or copying every dependency. The root helper has no credentials, network protocol, host socket or publication authority.
Cold resolution has a 20-minute worker bound in addition to the workflow timeout;
unknown outcomes retain capacity and private receipts for inspection.

A standard pool has neither `preparation` nor worker `acceleration`/`layerfs`.
The old optional-mount-only worker configuration now fails validation: migrate it
to this explicit profile or remove its LayerFS fields. Start with one runner per
profile/node until concurrent native attachment and resolver operation is
qualified. The resolver serializes its own state; it does not share state roots
with a running fleet-node process or another resolver.

## 2. Produce a clean dependency generation

Use a **dedicated preparation project and disposable environment**, never a Store
that has run CI jobs. The SQLite snapshot contains retained history, not just the
visible main tree. Never put registry tokens, Git credentials, homes, logs or
private job checkpoints anywhere in that Store, even temporarily. Approved
private package content remains private to this repository's read domain.

Use the existing reviewed source import, `fork`, stopped-VM attachment and file
checkpoint/promotion procedures from [LayerFS](LAYERFS-BACKEND.md) and
[Incus attachment](LAYERFS-INCUS.md). Prepare inside a fresh VM from the same
registered template. Stage the approved source at the **exact eventual checkout
path**, normally `/project/work/REPO/REPO` (append the workflow's checkout `path`
when configured). Put builder tools and temporary registry credentials on the
private VM disk, outside `/project`. Do not register a runner in this environment.

Copy [preparation-policy.example.json](../examples/preparation-policy.example.json)
outside the Store, replacing its image digest and scope. From the trusted Enginebox
installation in that disposable environment:

```sh
node packages/enginebox-preparation/src/cli.mjs build \
  --source /project/work/REPO/REPO --root /project \
  --policy /private/preparation-policy.json
```

This runs pnpm 12.15.0 with frozen lockfile, copy import and **explicitly disabled
lifecycle scripts**. It creates `prepared/<key>/node_modules` and a bounded
manifest, with writable package caches outside the checkout. It hashes the
actual Node ABI/platform/libc, version-qualified pnpm configuration inputs, source path, package
inputs, patches and policy. User/global configuration files and supported environment settings are hashed without storing
their values. The reader is qualified for pnpm 12.15.0, and nested workspace selections are refused. Changed registry authentication can conservatively cause a miss.
Local dependencies and source-dependent configurations include source content;
workspace configuration and package hooks cannot publish installed trees in v1; the action uses ordinary locked installation for them. External dependency symlinks are rejected. Never use deny
for a project that needs install scripts without adapting and validating it.

Record the returned key privately. Remove the staged `work/` tree and any other
non-output files from this **disposable preparation branch**, leaving only
`caches/` and `prepared/` at the root. Stop/detach the preparation VM through its
owned attachment protocol, checkpoint/close the branch and explicitly promote
it to that preparation project's main with the observed `--expected-main`.
No production source project is used as this dependency-only Store.

On the node, create its bounded, credential-free SQLite backup:

```sh
umask 077
/opt/enginebox/bin/ebox --host local storage layerfs \
  --config /etc/enginebox/layerfs.json \
  preparation-snapshot prepared-private-project prep-001 \
  --key ACTUAL_64_HEX_KEY --credential-free > /private/preparation-snapshot.json
```

The native command refuses open workspaces, previous `ci-` workspace history and
unexpected root entries. The owner must still review the **whole Store's history**
and clean-builder provenance; `--credential-free` is an assertion, not a secret
scanner. Keep the snapshot and discovery-record paths private.

## 3. Publish, approve and resolve

Register a separate fleet project for the dependency Store and enroll a node
credential scoped to it using [local/fleet setup](LOCAL-FLEET.md). Keep owner
approval credentials outside builders, resolver configuration and job VMs.

Copy [preparation-publication.example.json](../examples/preparation-publication.example.json)
to a private mode-0600 file, set the project-scoped node token and matching scope,
then run from the installed workspace with its locked dependencies:

```sh
node packages/fleet-node/src/publish-preparation.ts /private/publication.json
```

This uses the existing immutable generation/chunk upload path. It binds the SDK
revision, native project name and discovery-record digest into the generation,
and writes a private approval document. It **does not approve** the generation.
After review, use a trusted owner session (`fleet-owner.curl` holds the owner
Authorization header, mode 0600):

```sh
curl --config /private/fleet-owner.curl --request POST \
  --header 'Content-Type: application/json' \
  --data-binary @/private/preparation-approval.json \
  https://fleet.example.com/v1/projects/prepared-private-project/preparations
```

The catalog is owner-written, immutable per key and limited to published,
credential-free recipe generations. Nodes can read approved mappings; they
cannot approve or revoke. `DELETE` the same URL with `/<key>` appended to revoke.
Revocation is durable and prevents new resolutions, including one already
transferring. Drain active jobs separately; revocation does not erase data already
admitted to an authorized VM. Rebuilding a revoked key requires a new policy or
recipe identity, not overwriting its tombstone.

Copy [preparation-resolver.example.json](../examples/preparation-resolver.example.json)
to the worker's private resolver path. Set the exact approved key, native project
name, matching scope and project-scoped node credential. Its state root must be
private and exclusive. The worker invokes this resolver before forking:

```sh
node packages/fleet-node/src/preparation.ts /etc/enginebox/preparation-resolver.json
```

The resolver checks approval/scope and first calls native `preparation-ready`. An already activated base must match the approved snapshot/key/record/revision and unchanged main; it then skips snapshot hydration and full snapshot hashing. Missing state permits cold activation; uncertain or different state fails. Approval is checked again after inspection.

On a cold node it hydrates verified SQLite chunks through the
existing local/storage-origin/R2 path, rechecks revocation and invokes native
`preparation-restore`. Native activation verifies snapshot and discovery bytes,
SDK revision, SQLite integrity and prepared-tree shape, then copies into a **new
owned writable Store**. It never adopts the cache file as a live Store or replaces
an existing project. Reuse requires the same snapshot/key/record and unchanged
main. Imported preparation main cannot be promoted; jobs fork it privately.
Each generation needs a new local project name. Failed activation retains its
journal and resolver lock for inspection; do not blindly delete the lock/replay.

The hydrated snapshot cache has bounded eviction; live native Store volumes and
private job branches are outside that eviction namespace. Operators still budget
and retain those separately. Node selection is configured by the pool's worker;
automatic warm-compute placement is not implemented. The Cloudflare catalog owns
approval/storage, while the existing native controller owns the Actions attempt.

## 4. Adopt and qualify a workflow

In **GitHub → Prepared runner profile**, select the pool, enter the actual group
name and full commit-pinned checkout/Enginebox action references, then preview or
download the workflow fragment. Review it in a workflow PR using the existing
source/LayerFS acceptance and GitHub export process. The dashboard does not
rewrite existing YAML or open/merge a PR automatically. Preserve repository-specific
checkout inputs, source ref, permissions and every required build/test.

The important order is:

```yaml
- uses: actions/checkout@FULL_REVIEWED_COMMIT_SHA
  # Keep your existing checkout inputs.
- uses: OWNER/enginebox/packages/enginebox-actions@FULL_REVIEWED_COMMIT_SHA
  with:
    install-scripts: deny
    path: '.'
    require-hit: 'false'
    offline: 'false'
- run: pnpm test
```

Run the action after checkout and tool selection. Replace the dependency install
step and review redundant pnpm/setup-action cache restores. A later `npm ci`
removes the restored tree; arbitrary install commands are not intercepted.
Unrelated artifact caches remain independent. The action leaves official checkout
semantics intact and checks actual files, not the webhook SHA. An exact hit requests the protected tree from the one-use root helper within the private fork; a miss performs a locked pnpm install.
The standalone recipe verifies file hashes; the runner fast path relies on verified generation bytes protected read-only until handoff, avoiding a repeated full-tree scan. `require-hit: 'true'` makes the warm canary fail on a
miss, and `offline: 'true'` forbids dependency network fetches on fallback.

`install-scripts: run` requires a matching worker policy and always uses normal
locked installation. Catalog publication of installed trees currently requires
`deny`; use a separately reviewed local cache base for script-enabled compatible
execution. No workflow result is promoted automatically. Receipts in the private
branch and the step summary distinguish installed-tree hits from installs and
report elapsed preparation time/downloaded package count. They are diagnostics,
not trusted CI evidence or a measured downloaded-byte total.

Run successful and known-failing private jobs, cold/warm and fresh-node cases,
checkout-path and lockfile/config changes, cancellation and permission revocation
before migrating existing runners. Keep the prior runner group for rollback.
Container jobs, Windows/macOS, native modules, arbitrary monorepos, hostile PRs,
and speed/cost superiority remain separate qualification gates.

## 5. Require measured benefit before rollout

Run [the comparison harness](../research/actions-preparation/README.md) on disposable
capacity with a representative dependency fixture. It includes actual input checks,
read-only protection, handoff and receipt time. Its conventional baseline has a
warm pnpm cache and default package import mode; it is not an empty-cache straw man.
The command fails unless prepared median is at least 1.2× faster and its sampled
p95 is lower. This is a recipe qualification gate, not automatic pool approval.

The measured Linux fixture shows a dependency-stage benefit; cold transfer, VM
provision, LayerFS mount, checkout and full job cost remain separate gates. Do not
enable a production acceleration claim until the same comparison plus a real
GitHub/Incus canary demonstrates a benefit for that pool's workload. Tiny dependency
trees can lose to ordinary warm pnpm because fixed validation costs dominate.
