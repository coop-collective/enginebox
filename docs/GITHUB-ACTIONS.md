# GitHub connections, repository sync and Actions service

Implementation added October 24, 2026; single-App configuration added October 28, 2026. This is an optional, single-owner GitHub.com integration. It has offline protocol, state, Git and dashboard tests. **Real OAuth approval, organization JIT registration and a disposable Linux workflow have not been qualified in this change.** No existing runner, organization setting or developer workspace was changed.

[Required LayerFS preparation](GITHUB-ACTIONS-ACCELERATION.md) is now implemented
as an opt-in accelerated profile with repository-safe admission, a mandatory
LayerFS work root and a post-checkout pnpm dependency action. See
[prepared runner setup](PREPARED-ACTIONS-SETUP.md) for supported configuration and
[qualification evidence](reports/ACTIONS-PREPARATION-VALIDATION.md) for its limits.


## Product contract

Engine Box supplies disposable execution environments and prepared storage; GitHub keeps workflows, scheduling, secrets, checks and job logs. An account connects through GitHub App OAuth, chooses an installation and selects private or public repositories. One App installation and one OAuth account connection support both repository access and organization runner administration. Separate Apps remain an optional isolation choice. The core runtime remains usable with no GitHub configuration.

The existing [LayerFS GitHub adapter](GITHUB-SYNC.md) owns import, review, main acceptance, conflict handling and conditional export. The new account connection selects the repositories and branch that adapter may access. Normal project workspaces contain no native Git checkout. The connected App revalidates the connected owner's access before each transfer and mints an installation token restricted to that single repository and the required Contents permission. The token crosses private node stdin, is checked against the node's repository/branch binding, and lives in a temporary owner-only credential file until the command returns. Refresh tokens and App private keys stay at the controller. No parallel controller checkout or force-push path is added.

The service uses GitHub Apps instead of a classic OAuth App's broad `repo`/`admin:org` scopes. GitHub App installations support selected repositories and permission separation. User OAuth authenticates the owner; it does not itself install an App or override organization approval. [GitHub App and OAuth differences](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/differences-between-github-apps-and-oauth-apps)

## Permission upgrade

Use one GitHub App with the permissions needed by the enabled features:

| Feature | Permissions | Purpose |
|---|---|---|
| Repository sync | Metadata read, Contents read/write | Selected private repos and ordinary Git pull/push |
| Workflow edits, optional | Workflows write | Exports that edit `.github/workflows`; set `app.workflows_write: true` |
| Actions runners | Metadata read, Actions read, organization Self-hosted runners read/write | `workflow_job` events, job inspection and organization JIT registration |

An existing runner App can add Contents read/write; an existing repository App can add Actions read and organization Self-hosted runners read/write. Keep its Client ID, private key, client secret, webhook secret and installation. The installation owner must accept added permissions before they take effect. OAuth cannot request arbitrary additional App permissions through a scope parameter. [Permission selection](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/choosing-permissions-for-a-github-app), [updating an existing installation](https://docs.github.com/en/apps/maintaining-github-apps/modifying-a-github-app-registration#changing-the-permissions-of-a-github-app).

Configure the registration once under `app` in `github.json`. The dashboard exposes one account connection, repository selection and a runner permission review. Both capabilities share one serialized refresh-token record; repository transfers still receive only a single-repository Contents token, runner management receives only Actions/runner permissions, and prepared-job verification receives only single-repository Actions read. No App credentials enter workload VMs. Contents write is needed for sync, not for running an Actions job itself.

Existing `repos` plus optional `runners` configuration remains supported, with independent OAuth accounts and webhook secrets. Use [the separate-App example](../examples/github-separate-apps.example.json) only when that separation is desired. Do not combine it with `app` or duplicate the same registration in both slots. To switch an already connected configuration, drain and resolve jobs, disconnect existing accounts, change configuration during a supervised handoff, then connect once and reselect repositories. This prevents reusing an unrelated App's tokens or another user's selections. LayerFS project files remain intact.

OAuth uses a random state, S256 PKCE, a ten-minute signed browser flow cookie and a single-use controller record. User access and refresh tokens stay in private controller state. Expiring tokens are required; refresh is serialized. Git receives the short-lived installation token only in its child environment; it is never embedded in a remote URL or Git config. Node delegation accepts both legacy and current stateless token formats. [Installation token scope and lifetime](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-an-installation-access-token-for-a-github-app) User/install grant access is checked again before transfer. [GitHub App user authorization](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-user-access-token-for-a-github-app), [token refresh](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/refreshing-user-access-tokens)

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

One webhook endpoint per App can serve its installations. A webhook provides demand notification, not the Actions execution protocol. GitHub still assigns a job to the official runner according to group and labels; it can assign another compatible queued job, so the provisioning request is not a promise of job affinity. After proved worker and registration cleanup, the service rechecks the original job; if it is still queued, it can request fresh capacity. A newer in-progress/completed webhook prevents requeueing. Uncertain attempts never take this path. Workflows must opt into the Engine Box label. A webhook cannot redirect `ubuntu-latest` or silently replace an unrelated runner. [GitHub routing and ephemeral runners](https://docs.github.com/en/actions/reference/runners/self-hosted-runners)

The controller verifies HMAC-SHA256 over the exact raw request bytes, limits body size, journals job identities before dispatch and ignores duplicate/out-of-order queue events. Each pool requires an `enginebox-<pool-id>` label and all requested labels must match. It rechecks the live job before JIT issuance. JIT calls are not blindly retried after an uncertain response. JIT configuration travels over worker stdin, then exists only in that disposable VM; App keys and refresh tokens never reach it. Organization JIT uses `organization_self_hosted_runners: write`. [JIT API](https://docs.github.com/en/rest/actions/self-hosted-runners), [workflow_job event](https://docs.github.com/en/webhooks/webhook-events-and-payloads#workflow_job)

Worker execution uses a separate `enginebox-ci-*` Incus project and state root. It reserves a fresh credential-free template clone under the ordinary capacity/host locks. Allocation, start and cleanup wait up to 30 seconds to acquire a busy host lock; only the lock handshake is retried. Reservation markers keep agent account reconciliation away even if an operator later registers that project in the main fleet. CPU/RAM admission is checked again immediately before start. The official runner must be preinstalled in the template; no credentials or registration are captured there. Docker-based actions require Docker **inside the disposable VM**, never a host Docker socket.

The worker verifies its pinned runner version, starts `run.sh --jitconfig`, bounds execution time, exports private diagnostics and removes only the VM with its recorded Incus UUID and allocation ID. After the worker proves deletion, the controller removes only its own matching, non-busy leftover JIT registration if GitHub has not already removed it. Uncertain provisioning, execution, log export, registration cleanup or detach retains its receipts/resources for inspection. The controller records uncertainty, holds that pool's capacity and does not replay the job. A workflow cancellation is delivered by GitHub to its native runner; the worker timeout is a fallback bound.

Before allocating, the worker checks host capacity under the host lock. A known
shortage returns `waiting_capacity` with explicit proof that no allocation was
attempted. The controller removes the unused JIT registration, rechecks that the
GitHub job is still queued, and defers the whole pool for 60 seconds before a new
capacity attempt. This also applies to prepared runners before any checkout or
assignment exists. A failure after allocation starts, an incomplete inventory, or
unconfirmed registration cleanup still requires operator reconciliation. Existing
uncertain attempts are never automatically reclassified or replayed.

Drain stops new provisioning. Workers run detached (their own process group, the report written to a private file), so a controller handoff neither waits for nor interrupts them, and the next owner adopts a live worker by its recorded pid and program ([Work that outlives a controller](UPDATES.md#work-that-outlives-a-controller)). Standby does not recover GitHub state. A running job whose worker is gone becomes uncertain. Background reconciliation finishes an uncertain job once GitHub shows it completed and its runner idle or gone: the runner registration is removed and `enginebox-ci-worker --reclaim <name>` deletes only the VM matching that request's receipt. Nothing is replayed. There is no periodic GitHub queue backfill. Failed webhook deliveries must be inspected/redelivered in GitHub; the integration does not promise recovery of events it never received.

## Setup and supported interfaces

1. Register one GitHub App, or update the existing registration with the permissions above. Enable expiring user tokens. Set callback to `https://YOUR-DASHBOARD/api/github/callback`. Set the App webhook to `https://YOUR-DASHBOARD/api/github/webhook/runners`; its random secret must match controller configuration. The single App uses this endpoint for all its events. In shared mode `/api/github/webhook/repos` is also a signed alias, so an existing repository webhook URL can stay in place. Other event types are acknowledged; `push` events do not mutate checkouts.
2. Follow [the Kitbash secret setup](GITHUB-SECRETS.md). Copy [github.example.json](../examples/github.example.json) to `<controller-state>/github.json`, fill non-secret App metadata and reference private secret files, and keep the configuration private to the controller user (0700 parent/0600 file on Unix). The example has no credential values. Use `app` with `pools: []` for one App without runner provisioning. A legacy repository-only configuration can retain `repos`, `runners: null` and `pools: []`. Restart through the normal standby/drain/activation release procedure.
3. Open **Workspaces → GitHub connection** in the dashboard. Connect the GitHub account once, approve only desired repositories in its installation, refresh installations, choose the repository and branch, and add it. **Prepare source and sync** opens the workspace source/storage flow with that selection. Fetch/review the source, approve its credential-free import, then use LayerFS workspaces and accepted-main export. Install the updated `ebox` on registered storage nodes: OAuth delegation requires `--token-stdin` and repository/branch metadata in `github-settings`. When the account module is configured it takes precedence over manually configured node tokens; a revoked or deselected repository cannot silently fall back to a broader static credential.

4. For CI, provision a separate local Linux CI inventory using the ordinary bootstrap/template procedure. Use a dedicated project named `enginebox-ci-<name>`, no warm pools, a clean prepared snapshot and the guest's configured non-root account. Provision the official runner under `/opt/actions-runner` in that base before snapshotting it. Verify the downloaded archive against the selected release's checksum and record the resolved version. Configure the separate [worker file](../examples/ci-worker.example.json) with the same version. The example version 2.337.0 is an upstream reference, **not a live-tested Engine Box runner version**. [Official runner releases](https://github.com/actions/runner/releases)
5. Provision a runner group in the organization with access restricted to the intended repositories/workflows; use its actual numeric group ID and the App installation ID in the pool. Use **Review runner permissions** and approve the installation permissions. Shared mode uses the same connected account; only separate-App mode needs another OAuth connection. The controller verifies the connected user's installation visibility and actual runner permission before provisioning. [Runner group API and permissions](https://docs.github.com/en/rest/actions/self-hosted-runner-groups)
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

### Linux CI guest privileges

Prepare the dedicated CI base with passwordless `sudo` for its configured runner
user before taking the clean snapshot. This matches the administrative access of
[GitHub-hosted Linux VMs](https://docs.github.com/en/actions/reference/runners/github-hosted-runners#administrative-privileges),
so existing steps such as `sudo apt-get install` work. Keep the runner process
non-root; elevation applies inside that disposable VM. This does not change the
host's sudo policy or the ordinary developer-workspace template.

For the default `agent` user, run the following **as root inside the dedicated CI
base**, with `sudo` already installed. Adapt the username if the worker uses a
different guest account:

```sh
set -eu
test "$(id -u agent)" -ne 0
candidate=$(mktemp)
trap 'rm -f "$candidate"' EXIT
printf '%s\n' 'agent ALL=(ALL:ALL) NOPASSWD: ALL' > "$candidate"
visudo -cf "$candidate"
install -o root -g root -m 0440 "$candidate" /etc/sudoers.d/90-enginebox-ci-agent
visudo -c
test "$(su - agent -c 'sudo -n id -u')" = 0
```

Retain the previous snapshot for rollback. Run the ordinary clean-base checks,
reset guest identity, stop the base, and create a new snapshot before changing
the CI inventory's template reference. The native runner canary checks both that
the job starts non-root and that `sudo -n` can elevate without a prompt. Qualify
package installation in a real workflow as well; this privilege check alone
does not establish package availability or network access.

The worker command is administrator-owned configuration, not request input. On the same node, invoke the built `enginebox-ci-worker --config /private/path/ci-worker.json`. A remote controller can use an administrator-provisioned SSH wrapper with fixed host/config/command and strict host-key checking; stdin must be forwarded and only the worker JSON returned on stdout. Worker configuration and fleet resources never come from webhook payloads. No worker command is executed by the mere presence of a GitHub repository connection.

Implemented controller API (owner bearer authentication except signed webhooks):

| Method/path | Operation |
|---|---|
| `GET /v1/github` | Safe connection, repo, pool and CI status; `shared_app` identifies the unified mode and its account has kind `github` |
| `POST /v1/github/{repos,runners}/connect` | Begin OAuth; both routes use the same account in shared mode; dashboard uses its dedicated cookie-setting route |
| `POST /v1/github/callback` | Consume state/code; server-to-server only |
| `GET /v1/github/{repos,runners}/installations` | Paginated installations and App install link |
| `GET /v1/github/installations/<id>/repositories` | Paginated repo picker from owner access |
| `POST /v1/github/select` | `{installation_id, repository_id, branch}` |
| `POST /v1/github/remove` | `{repository_id}`; preserves checkout |
| `POST /v1/github/{repos,runners}/disconnect` | `{}`; remove local connection after CI drains; either route disconnects both capabilities in shared mode; remote uninstall/revocation remains in GitHub |
| `POST /v1/github/drain` | `{draining: true or false}` |
| `POST /v1/github/webhook/{repos,runners}` | Signed raw webhook body, independent of dashboard session |

The dashboard exposes the public callback and webhook forwarding paths and preserves the raw signature body. Do not expose the controller's owner API directly. Redact OAuth callback query strings in reverse-proxy access logs. Tokens, App PEMs, Git checkouts, diagnostics and receipts stay out of Git. The existing private filesystem boundary is not an encrypted vault.

`ebox module ci ...`, automatic App registration, runner-group creation, workflow rewriting and automatic Actions pool placement are **not implemented commands/capabilities**. The existing native CI automation, workflow dispatch and template interfaces described in [CI](CI.md) remain available. The new built worker binary is included in newly packaged releases, but configuring a pool does not install it onto a remote node.

## LayerFS storage and cache design

Prepared profiles require both controller `preparation` and worker `acceleration`
configuration. The former optional-mount-only configuration fails validation.
Use the [prepared worker and catalog setup](PREPARED-ACTIONS-SETUP.md), which
includes migration instructions and example configuration.

Each admitted job receives an isolated LayerFS branch at `/project`; native JIT
configuration places the official runner's work directory at `/project/work`.
Root-owned bootstrap and a mandatory start hook check the mount and actual job
assignment before workflow steps. Pnpm caches live outside checkout under
`/project/caches`. Official `actions/checkout` still selects and cleans source.
The separate preparation action restores exact compatible installed dependencies
after checkout or performs ordinary locked install. Arbitrary workflow commands
and GitHub's `actions/cache` protocol are not intercepted.

The Cloudflare fleet owns immutable preparation catalog approvals/revocations;
its existing verified chunk transport hydrates Store snapshots. A node-side
resolver rechecks approval and invokes explicit native activation with the
matching SDK discovery record. Activation copies into a new owned Store and
protects its main revision; subsequent job branches remain private. This is a
complete SQLite transfer, not lazy mounting of R2. Native Store reopen and branch
isolation are tested; Incus attachment of a transferred preparation and a real
Actions run are still release gates. Warm compute placement remains configured
rather than automatic.

After a job, detach only after the VM stops, create a **private job file
checkpoint** and close the sandbox. This may contain workflow secrets/source and
never becomes an approved generation automatically. Native branches and diagnostic
archives retain the existing bounded-admission/manual-retention contract. The
hydrated snapshot cache can evict immutable copies; it does not evict live Stores.
No cost or speed superiority claim is made before comparative measurements.

## Existing runner migration and acceptance

Runner administration grants the authority needed to register Engine Box runners. It does not make deletion or replacement of every existing runner a safe default. Inventory existing groups, labels, workflows and busy jobs first; add Engine Box as a separate group, run a representative canary and move selected workflow routing. Drain the old runner service on its actual host and remove only reviewed offline registrations after no jobs depend on it. Keep the old group available for rollback until the canary passes. This change does not implement bulk runner deletion or stop an existing service.

Qualification must exercise the actual App's approval/revocation, private clone/push, branch protection, duplicate/reordered/redelivered events, a successful and failing workflow, cancellation, timeout, capacity rejection, lost JIT response, worker/controller crash, drain/upgrade and diagnostic export. Create unique disposable resources; never use a developer workspace as the fixture. Record REST version **2026-03-10**, actual runner version, guest/Incus/QEMU versions, LayerFS revision, resources and image/cache digests. Offline tests do not establish live GitHub registration or a successful workflow.

Compare the same commit, workflow and CPU/RAM with (a) fresh conventional runner, (b) clean Engine Box VM, (c) cold LayerFS and (d) compatible warm LayerFS. Report queue, provisioning, dependency, build/test and teardown time, p50/p95, failures, transfer bytes, physical storage, cache hit rate and contention alongside an unrelated workspace. Include invalidated lockfiles, no-cache recovery, large/native dependency workloads and cancelled jobs. Model full cost: compute duration plus idle reserves, storage/replication, network, operations and any applicable GitHub charges. Until those measurements exist, lower cost and faster dependency readiness are hypotheses.
