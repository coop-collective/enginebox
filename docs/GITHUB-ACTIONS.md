# GitHub connections, repository sync and Actions service

Implementation added October 24, 2026. This is an optional, single-owner GitHub.com integration. It has offline protocol, state, Git and dashboard tests. **Real OAuth approval, organization JIT registration and a disposable Linux workflow have not been qualified in this change.** No existing runner, organization setting or developer workspace was changed.

Validation on October 24: `pnpm install --frozen-lockfile`, `pnpm check`, `pnpm lint:rust`, `pnpm build` and `pnpm test:e2e` passed. The complete unit/integration suite passed with `RUST_TEST_THREADS=1 pnpm test`; parallel runs had timing failures in existing native-auth tests under build load. Browser coverage passed all 26 dashboard and six site cases using isolated test ports. The final Git safety change also passed the GitHub crate tests, Rust lint and release build. Live tests remain opt-in and were not run.

## Product contract

Engine Box supplies disposable execution environments and prepared storage; GitHub keeps workflows, scheduling, secrets, checks and job logs. An account connects through GitHub App OAuth, chooses an installation and selects private or public repositories. An independent runner connection grants organization administration. The core runtime remains usable with no GitHub configuration.

There was no earlier GitHub account connection or two-way remote sync in this codebase. LayerFS had reviewed local Git imports and filesystem checkpoints. The new two-way sync is explicit **pull and push of one selected Git branch in a private controller checkout**, not automatic merging or a background mirror of every VM. Pull clones or fast-forwards; push publishes existing commits without force. Dirty worktrees and divergence require owner resolution. The selected branch, installation and repository ID remain bound together. Unselecting or disconnecting retains local files.

The service uses GitHub Apps instead of a classic OAuth App's broad `repo`/`admin:org` scopes. GitHub App installations support selected repositories and permission separation. User OAuth authenticates the owner; it does not itself install an App or override organization approval. [GitHub App and OAuth differences](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/differences-between-github-apps-and-oauth-apps)

## Permission upgrade

Configure two App registrations so repository users do not have to accept runner management:

| App | Permissions | Purpose |
|---|---|---|
| Repository App | Metadata read, Contents read/write | Selected private repos and ordinary Git pull/push |
| Repository App, optional | Workflows write | Only if the owner wants pushes that edit `.github/workflows`; otherwise GitHub can reject those pushes |
| Runner App | Metadata read, Actions read, organization Self-hosted runners read/write | `workflow_job` events, job inspection and organization JIT registration |

Both use the same dashboard callback URL. The runner App subscribes to `workflow_job`. Its installation must include each CI repository, and an organization administrator must approve its permissions. The dashboard exposes separate connection, installation and permission-review actions. An App's requested permissions are registration settings, not arbitrary scopes the browser can upgrade in an OAuth URL. [Permission selection and Git access](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/choosing-permissions-for-a-github-app)

OAuth uses a random state, S256 PKCE, a ten-minute signed browser flow cookie and a single-use controller record. User access and refresh tokens stay in private controller state. Expiring tokens are required; refresh is serialized. Git receives an access token only in its child environment; it is never embedded in a remote URL or Git config. User/install grant access is checked again before transfer. [GitHub App user authorization](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-user-access-token-for-a-github-app), [token refresh](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/refreshing-user-access-tokens)

## Native Actions execution

```mermaid
sequenceDiagram
    participant G as GitHub Actions
    participant C as Optional GitHub service
    participant W as Disposable Incus worker
    participant L as Node-local LayerFS
    G->>C: Signed workflow_job queued
    C->>C: Validate installation, selected repo, labels, limits
    C->>G: Recheck job and request JIT config
    C->>W: Private stdin: one-use JIT config
    W->>W: Inspect, admit and reserve clean CI VM
    opt Approved LayerFS project
      W->>L: Fork private checkout from approved generation
      W->>W: Attach checkout to stopped VM
    end
    W->>G: Official runner connects and accepts one matching job
    G->>C: in_progress / completed
    W->>W: Save private diagnostics, stop and verify identity
    W->>L: Detach; retain private job checkpoint; close sandbox
    W->>W: Delete only exact disposable VM
```

One webhook endpoint per App can serve its installations. A webhook provides demand notification, not the Actions execution protocol. GitHub still assigns a job to the official runner according to group and labels; it can assign another compatible queued job, so the provisioning request is not a promise of job affinity. Workflows must opt into the Engine Box label. A webhook cannot redirect `ubuntu-latest` or silently replace an unrelated runner. [GitHub routing and ephemeral runners](https://docs.github.com/en/actions/reference/runners/self-hosted-runners)

The controller verifies HMAC-SHA256 over the exact raw request bytes, limits body size, journals job identities before dispatch and ignores duplicate/out-of-order queue events. Each pool requires an `enginebox-<pool-id>` label and all requested labels must match. It rechecks the live job before JIT issuance. JIT calls are not blindly retried after an uncertain response. JIT configuration travels over worker stdin, then exists only in that disposable VM; App keys and refresh tokens never reach it. Organization JIT uses `organization_self_hosted_runners: write`. [JIT API](https://docs.github.com/en/rest/actions/self-hosted-runners), [workflow_job event](https://docs.github.com/en/webhooks/webhook-events-and-payloads#workflow_job)

Worker execution uses a separate `enginebox-ci-*` Incus project and state root. It reserves a fresh credential-free template clone under the ordinary capacity/host locks. Reservation markers keep agent account reconciliation away even if an operator later registers that project in the main fleet. CPU/RAM admission is checked again immediately before start. The official runner must be preinstalled in the template; no credentials or registration are captured there. Docker-based actions require Docker **inside the disposable VM**, never a host Docker socket.

The worker verifies its pinned runner version, starts `run.sh --jitconfig`, bounds execution time, exports private diagnostics and removes only the VM with its recorded Incus UUID and allocation ID. After the worker proves deletion, the controller removes only its own matching, non-busy leftover JIT registration if GitHub has not already removed it. Uncertain provisioning, execution, log export, registration cleanup or detach retains its receipts/resources for inspection. The controller records uncertainty, holds that pool's capacity and does not replay the job. A workflow cancellation is delivered by GitHub to its native runner; the worker timeout is a fallback bound.

Drain stops new provisioning while admitted workers finish. Controller handoff also waits for workers, so a long job can delay an upgrade. Standby does not recover GitHub state. Running records found after an active-owner restart become uncertain. The first implementation has no takeover of an orphaned worker, uncertain-job repair button or periodic GitHub queue backfill. Failed webhook deliveries must be inspected/redelivered in GitHub; the integration does not promise recovery of events it never received.

## Setup and supported interfaces

1. Register the repository App and, when enabling CI, the separate runner App. Enable expiring user tokens. Set callback to `https://YOUR-DASHBOARD/api/github/callback`. Set the runner webhook to `https://YOUR-DASHBOARD/api/github/webhook/runners`; its random secret must match controller configuration. The repository webhook endpoint also validates signatures but currently only acknowledges events; it does not mutate checkouts on a `push` event.
2. Copy [github.example.json](../examples/github.example.json) to `<controller-state>/github.json`, fill real values there, and keep that file private to the controller user (0700 parent/0600 file on Unix). This example contains placeholders, not usable App credentials. Setting `runners` to null and `pools` to `[]` enables repository-only operation. Restart through the normal standby/drain/activation release procedure.
3. Open **GitHub** in the dashboard. Connect the repository account, open installation management, select only desired GitHub repositories, refresh installations, choose the repo and branch, and add it. Pull creates `<controller-state>/github/checkouts/<repository-id>`. Edit/commit there using the owner's filesystem access; Push publishes that branch. Automatic workspace credential delegation, repo-to-VM checkout assignment and filesystem merge are not implemented.
4. For CI, provision a separate local Linux CI inventory using the ordinary bootstrap/template procedure. Use a dedicated project named `enginebox-ci-<name>`, no warm pools, a clean prepared snapshot and the guest's configured non-root account. Provision the official runner under `/opt/actions-runner` in that base before snapshotting it. Verify the downloaded archive against the selected release's checksum and record the resolved version. Configure the separate [worker file](../examples/ci-worker.example.json) with the same version. The example version 2.337.0 is an upstream reference, **not a live-tested Engine Box runner version**. [Official runner releases](https://github.com/actions/runner/releases)
5. Provision a runner group in the organization with access restricted to the intended repositories/workflows; use its actual numeric group ID and the runner App installation ID in the pool. Connect runner administration in the dashboard and approve its installation. The controller verifies the connected user's installation visibility and actual runner permission before provisioning. [Runner group API and permissions](https://docs.github.com/en/rest/actions/self-hosted-runner-groups)
6. Run one disposable, non-deploying workflow first. Use the group name and label in workflow YAML, for example:

```yaml
# Example workflow fragment; change the group to the one explicitly provisioned.
jobs:
  test:
    runs-on:
      group: enginebox-ci
      labels: enginebox-linux
    timeout-minutes: 20
    permissions:
      contents: read
    steps:
      - run: 'printf "Native Engine Box runner: %s\n" "$RUNNER_NAME"'
```

The worker command is administrator-owned configuration, not request input. On the same node, invoke the built `enginebox-ci-worker --config /private/path/ci-worker.json`. A remote controller can use an administrator-provisioned SSH wrapper with fixed host/config/command and strict host-key checking; stdin must be forwarded and only the worker JSON returned on stdout. Worker configuration and fleet resources never come from webhook payloads. No worker command is executed by the mere presence of a GitHub repository connection.

Implemented controller API (owner bearer authentication except signed webhooks):

| Method/path | Operation |
|---|---|
| `GET /v1/github` | Safe connection, repo, pool and CI status |
| `POST /v1/github/{repos,runners}/connect` | Begin OAuth; dashboard uses its dedicated cookie-setting route |
| `POST /v1/github/callback` | Consume state/code; server-to-server only |
| `GET /v1/github/{repos,runners}/installations` | Paginated installations and App install link |
| `GET /v1/github/installations/<id>/repositories` | Paginated repo picker from owner access |
| `POST /v1/github/select` | `{installation_id, repository_id, branch}` |
| `POST /v1/github/sync` | `{repository_id, direction: "pull" or "push"}`; returns a durable operation job |
| `POST /v1/github/remove` | `{repository_id}`; preserves checkout |
| `POST /v1/github/{repos,runners}/disconnect` | `{}`; remove local connection after CI drains; remote App uninstall/revocation remains in GitHub |
| `POST /v1/github/drain` | `{draining: true or false}` |
| `POST /v1/github/webhook/{repos,runners}` | Signed raw webhook body, independent of dashboard session |

The dashboard exposes the public callback and webhook forwarding paths and preserves the raw signature body. Do not expose the controller's owner API directly. Redact OAuth callback query strings in reverse-proxy access logs. Tokens, App PEMs, Git checkouts, diagnostics and receipts stay out of Git. The existing private filesystem boundary is not an encrypted vault.

`ebox module ci ...`, automatic App registration, runner-group creation, workflow rewriting and fleet placement are **not implemented commands/capabilities**. The built worker binary is included in newly packaged releases, but configuring a pool does not install it onto a remote node.

## LayerFS storage and cache design

Optional worker configuration sets `layerfs` to the node's qualified LayerFS configuration and maps a GitHub full name to an approved project/generation:

```json
{
  "layerfs": "/etc/enginebox/layerfs.json",
  "projects": {
    "example-org/private-project": {"project": "private-project", "from": "prepared-deps"}
  }
}
```

This fragment extends the worker example. `from` can be null to fork the project's original imported base. For a prepared generation, use the existing reviewed Git import, a fresh preparation fork, the compatible dependency recipe, and a completed file checkpoint. Approve that credential-free generation before referencing it. Never select an earlier CI job's private checkpoint as a base.

Each job gets a distinct writable branch mounted at `/project` with the existing exclusive Incus attachment protocol. Toolchains remain in the clean VM image. The native runner's default `_work` stays on its disposable VM disk; workflows must explicitly use `/project` to benefit from its prepared files. The adapter does not silently alter `actions/checkout` or claim that every action cache is redirected.

For example, a reviewed workflow can synchronize the exact `actions/checkout` tree into `/project`, preserve an installed dependency tree only when its lockfile/toolchain key matches the approved generation, and run build/test from `/project`. Do not disable checkout cleaning globally to retain unknown prior-job files. `actions/cache` still uses its own GitHub-supported protocol; Engine Box is not an Actions cache server.

Cache keys must include owner/installation, repository, trust class (trusted branch versus untrusted contribution), OS/architecture, toolchain/ABI, package manager/version, lockfile digest and recipe revision. Publish immutable generations with one trusted writer. An untrusted job may consume only an approved compatible generation and must never publish a base used by a trusted workflow. Repository selection alone does not make arbitrary public PR code safe to run alongside private services. The current service is for one trusted owner; public hostile multi-tenancy remains a release gate.

After a job, detach only after the VM stops, then create a **private job file checkpoint** and close its LayerFS sandbox. This may contain workflow-generated secrets or source and is never automatically promoted to a cache. The first worker retains these private branches and local diagnostic archives; operators must budget storage and manage retention. It does not implement safe branch garbage collection. Disk-reserve admission bounds further work when storage fills.

LayerFS currently provides node-local shared storage and private CoW checkouts; [its qualified backend](LAYERFS-BACKEND.md) and [Incus attachment](LAYERFS-INCUS.md) are not a distributed object-store service. Cross-node immutable generation replication, per-node installation receipts for CI data, demand-based prefetch, placement and remote cache eviction remain planned. Template desired-state synchronization retains its existing independent contract. No claim of distributed speed/cost superiority is made by this implementation.

## Existing runner migration and acceptance

Runner administration grants the authority needed to register Engine Box runners. It does not make deletion or replacement of every existing runner a safe default. Inventory existing groups, labels, workflows and busy jobs first; add Engine Box as a separate group, run a representative canary and move selected workflow routing. Drain the old runner service on its actual host and remove only reviewed offline registrations after no jobs depend on it. Keep the old group available for rollback until the canary passes. This change does not implement bulk runner deletion or stop an existing service.

Qualification must exercise the actual App's approval/revocation, private clone/push, branch protection, duplicate/reordered/redelivered events, a successful and failing workflow, cancellation, timeout, capacity rejection, lost JIT response, worker/controller crash, drain/upgrade and diagnostic export. Create unique disposable resources; never use a developer workspace as the fixture. Record REST version **2026-03-10**, actual runner version, guest/Incus/QEMU versions, LayerFS revision, resources and image/cache digests. Offline tests do not establish live GitHub registration or a successful workflow.

Compare the same commit, workflow and CPU/RAM with (a) fresh conventional runner, (b) clean Engine Box VM, (c) cold LayerFS and (d) compatible warm LayerFS. Report queue, provisioning, dependency, build/test and teardown time, p50/p95, failures, transfer bytes, physical storage, cache hit rate and contention alongside an unrelated workspace. Include invalidated lockfiles, no-cache recovery, large/native dependency workloads and cancelled jobs. Model full cost: compute duration plus idle reserves, storage/replication, network, operations and any applicable GitHub charges. Until those measurements exist, lower cost and faster dependency readiness are hypotheses.
