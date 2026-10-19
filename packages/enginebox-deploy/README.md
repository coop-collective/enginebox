# Deploy your Engine Box controller

This package builds a pinned Linux x86-64 controller/dashboard release and updates
an already running Rust supervisor over SSH. It preserves the installation's
existing private state root, API token, dashboard secrets, fleet configuration,
and exposed node helper. Initial host provisioning, the first Python-to-Rust
cutover, node binaries, supervisor binaries, operating-system updates and VM
retirement remain explicit maintenance operations.

The GitHub workflow is dormant by default. Enabled deployments run on pushes to
`main`; manual dispatch supports an exact update version, an exact installed
rollback version, or a health check. A failed template installation marks the
deployment unsuccessful even if the controller update already succeeded.

## Private mirror or another fork

Use a separate private repository when your deployment configuration should be
private. GitHub supports [duplicating a repository without creating a fork](https://docs.github.com/en/repositories/creating-and-managing-repositories/duplicating-a-repository).
Keep an `upstream` remote pointing to Engine Box and `origin` pointing to your
deployment repository. For example, `enginebox-live` can contain the same source
history plus its own deployment configuration and template recipes. Keep the
project's license and notices intact.

After the initial copy, bring updates in through a reviewed branch:

```sh
git fetch upstream
git switch -c upstream-update
git merge upstream/main
# Resolve and review changes, then open a PR against your deployment main.
```

Use normal merges after setup; repeated mirror pushes can overwrite private
configuration branches. Merging the reviewed PR to `main` triggers the enabled
deployment workflow. Do not commit fleet host inventories, API tokens, provider
homes, SSH keys, `.env` files, or VM snapshots, even to the private repository.

## Configure the environment

Copy `environments/example.json` to `environments/production.json`. These paths
must match the existing server installation. `remote_node` is the absolute Node
22.12+ executable on that server. The pinned `controller.installer` remains
available in its retained release; it is not a mutable `current` symlink. The
remote script verifies that the server's supervisor configuration binds the same
installation prefix, private state root, token file and loopback listener.

Create a GitHub Environment named `production`, restrict deployment branches to
`main`, and configure these **Environment secrets**:

| Secret | Value |
| --- | --- |
| `ENGINEBOX_DEPLOY_HOST` | Server hostname or IP address |
| `ENGINEBOX_DEPLOY_USER` | Existing unprivileged controller service owner |
| `ENGINEBOX_DEPLOY_SSH_KEY` | Dedicated OpenSSH private key |
| `ENGINEBOX_DEPLOY_KNOWN_HOSTS` | Independently verified exact host-key line(s) |

Host-key lines must use the exact hostname, or `[hostname]:port` for a nonstandard
port. Wildcards, hashed hostnames and certificate-authority records are not
accepted by this small deployment client. Verify the server fingerprint through
an existing trusted channel; do not replace pinning with `StrictHostKeyChecking=no`
or a fresh, unverified `ssh-keyscan` during CI.

Set these **repository variables**:

| Variable | Default / purpose |
| --- | --- |
| `ENGINEBOX_DEPLOY_ENABLED` | Must be exactly `true` to enable deployment |
| `ENGINEBOX_DEPLOY_ENVIRONMENT` | `production` |
| `ENGINEBOX_DEPLOY_CONFIG` | `packages/enginebox-deploy/environments/production.json` |
| `ENGINEBOX_DEPLOY_VERSION_BASE` | `0.3.1`; automatic pins become `0.3.1-live.RUN_NUMBER` |
| `ENGINEBOX_DEPLOY_PORT` | `22` |
| `ENGINEBOX_DEPLOY_RUNNER` | JSON string `"ubuntu-24.04"`; an isolated trusted runner can use a JSON label array |
| `ENGINEBOX_DEPLOY_ALLOW_PUBLIC` | Omitted; public forks require explicit `true` in addition to the enable variable |

For an approved isolated Linux x86-64 runner, the runner variable can be
`["self-hosted","Linux","X64","enginebox-live"]`. The runner needs Git, Node,
tar, OpenSSH, a C/C++ linker/toolchain, and the ability to install the pinned pnpm
and Rust toolchain. Build jobs receive no deployment secrets. The separate deploy
job receives its Environment secrets only for the deployment step. GitHub plan
and billing restrictions can prevent hosted jobs or private Environment secrets;
see [GitHub's environment availability](https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments).

Authorize the dedicated key on the server with an entry shaped like:

```text
restrict ssh-ed25519 PUBLIC_KEY enginebox-deployment
```

OpenSSH's [`restrict` option](https://man.openbsd.org/sshd.8#restrict) disables
forwarding, agent forwarding, PTYs, X11 forwarding and user rc execution. It
still allows commands as the service owner. This package uses SSH commands for
private temporary staging, SFTP transfer and a Node operation protected by
`flock`; it does not install a forced-command gateway. Protect write access to
deployment `main`, its workflow and Environment. The key must not be reused as a
personal login key. Existing administrator-approved node/sudo access is preserved;
this package grants no additional sudo policy.

## Templates and private settings

Set `templates.files` to explicit paths such as `templates/live-native.json`.
Only files in that package directory are read. IDs must start with
`templates.namespace` followed by `-`, or appear in `templates.allow_ids`. To
manage an existing `native-dev` recipe, explicitly add `native-dev` to that list
and commit its current manifest before making changes. No templates are deleted.

Recipe changes require a greater integer `revision`. Re-registering the same
unchanged revision is supported. Engine Box owns synchronization to every
registered node, including later additions; CI polls the controller's current
revision/digest and every node's synchronization status. A failed node, changed
revision, missing fleet or deadline produces a visible deployment failure.
Resource admission stays on each node. This workflow never deletes a workspace
or expands capacity to make a template install succeed.

Provider credentials, the fleet inventory and dashboard environment remain in
their existing private server files. Custom application secrets belong in those
administrator-managed private settings, not template recipes or release archives.
The deployment schema rejects arbitrary command fields and credential fields.

## Build, deploy, check and roll back

The build script exports the checked-out Git commit into a temporary source
directory. It sets that copy's workspace version and matching workspace records
in `Cargo.lock`, then runs the locked install, workspace checks, tests, Clippy and
release build. The tracked source version is unchanged. Dashboard dependencies
are materialized in the archive, and the manifest's version matches the built
controller's `--version`. Pinned archive and manifest checksums are checked before
and after SSH transfer; no release is overwritten.

```sh
node packages/enginebox-deploy/src/build.mjs \
  --version 0.3.1-live.1 --output .state/deploy-artifact

# Requires the four SSH environment secrets above in this process environment.
node packages/enginebox-deploy/src/deploy.mjs \
  --config packages/enginebox-deploy/environments/production.json \
  --operation update --version 0.3.1-live.1 --artifacts .state/deploy-artifact

node packages/enginebox-deploy/src/deploy.mjs \
  --config packages/enginebox-deploy/environments/production.json --operation status

node packages/enginebox-deploy/src/deploy.mjs \
  --config packages/enginebox-deploy/environments/production.json \
  --operation rollback --version 0.3.0
```

Update stages the exact archive, checks its matching complete release's attachment
and reservation protocol, then calls `enginebox-install update`. It never calls
offline `activate` against the running installation. Rollback uses the same
supervisor health/schema/drain path with an explicit retained target, and does
not downgrade or re-register templates. Old releases without the required
attachment-aware protocol are rejected. Node helper updates remain a separate
compatibility maintenance operation.

The stable supervisor remains in place; updates do not replace its service unit,
restart VMs, copy provider homes or restore older refresh credentials. Successful
output contains only versions, readiness and template counts. A template failure
does not automatically roll back an already active controller. If SSH disconnects,
the remote operation may still be running: check status and controller logs before
retrying. The owned `/tmp/enginebox-deploy.*` artifact directory is retained on an
uncertain outcome; a successful run removes its own directory. Local key files
are private and removed on normal completion, SIGINT and SIGTERM. On GitHub they
live in a run-specific, owner-marked directory under `RUNNER_TEMP`; an `always()`
step verifies that marker and removes that exact directory after cancellation.
Host failure or SIGKILL can still interrupt cleanup; the runner must clear its
temporary storage before reuse. Health checks and rollback do not read template
files, so a missing recipe cannot block those maintenance operations.

Each successful build is retained as a workflow artifact. Rerun a failed deploy
job with its original artifact. Rebuilding different bytes under the same version
is rejected; dispatch a fresh version when rebuilding is necessary. Rollback
requires an already installed release and does not download or guess a target.

## Validation

```sh
pnpm --filter @enginebox/deploy test
```

The tests cover strict configuration, template ownership, exact artifact hashes,
shell argument preservation, SSH host-key policy, version rewriting, template
failure detection and redacted command errors. A Linux-only disposable fixture
exercises remote update/status/rollback against a local HTTP server and fake
binaries while verifying that private state and token files remain unchanged.
That fixture does not claim a production deployment; qualify the real workflow
against your existing supervisor before relying on automatic updates.
