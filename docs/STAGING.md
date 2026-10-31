# Staging environment and the test→prod pipeline: enablement runbook

The deploy workflow (`.github/workflows/deploy.yml`) ships every push to `main`:

1. **build** — one versioned release (`VERSION_BASE-live.RUN_NUMBER`), with the deploy tests. A newer commit cancels an
   older build.
2. **staging** — that exact artifact is deployed to the staging controller, then the **smoke gate** runs: the version is
   active and ready, `/v1/pools`, `/v1/agent-runs` and `/v1/templates` answer, and (once a provider account is
   connected, see below) a trivial pool run succeeds and reports its start path.
3. **production** — the same artifact is promoted unless `ENGINEBOX_PROD_HOLD` is `true`. The production deploy also
   rolls out the host node helper (stage, activate, republish), so controller and guest helper never drift.
4. **watch** — for `watch.seconds` (600) the workflow checks readiness and the start failures of runs created after the
   cutover. Losing readiness for 3 checks in a row, or at least 3 start failures that are also at least half of the new
   runs, rolls production back to the previous version automatically and fails the run.

Nothing waits on a timer: a passing push reaches production in build + deploy time. Pending staging and production
jobs are replaced by newer runs (one deployment per environment at a time); a production cutover or its watch is
never cancelled mid-flight. Docs-, examples- and workflow-only pushes do not deploy.

## Isolation

| | Production | Staging |
|---|---|---|
| Install prefix | `/opt/enginebox-rust/controller` | `/opt/enginebox-staging/controller` |
| State root | `/opt/enginebox-control/state` | `/opt/enginebox-staging/state` |
| API / dashboard | `127.0.0.1:8765` / `:3000` | `127.0.0.1:8865` / `:3100` |
| Supervisor unit | `enginebox-supervisor.service` | `enginebox-staging-supervisor.service` |
| Incus project | `enginebox-fleet` | `enginebox-staging` (limits: 6 instances, 8 vCPU, 12 GiB) |
| Fleet budget | `max_instances` per its fleet.toml | `max_instances = 3`, `contribution_cpu = 8`, `contribution_memory_gib = 12` |
| API token, dashboard secrets | its own | freshly generated, never copied |
| Provider sign-ins | the team's accounts | none by default (see below) |
| Host node helper (`/opt/enginebox-node`) | owned and rolled out | never touched |

Both controllers share the host's Incus daemon, storage pool, bridge and `enginebox-host` operation lock. Staging's
contribution caps bound its CPU/RAM; it can still briefly contend for the host lock during its own lifecycle operations.
Because staging does not roll out the node helper, staging runs a candidate controller against the node helper
production currently has — which is exactly the order a production cutover goes through.

## Enable it

1. **Bootstrap staging on the host** (as `ubuntu`, from a checkout of this repository, with a release artifact
   directory such as a deploy build or the `controller-preview` artifact):

   ```sh
   packages/enginebox-deploy/staging/bootstrap.sh --version 0.3.1-workers.41226420500 --artifacts DIR        # plan
   packages/enginebox-deploy/staging/bootstrap.sh --version 0.3.1-workers.41226420500 --artifacts DIR --apply
   ```

   It is idempotent and refuses to run if staging paths overlap production's or the staging port is taken by
   something else. If you bootstrap a different version, set `controller.installer` in
   `packages/enginebox-deploy/environments/staging.json` to that release's `bin/enginebox-install`.
2. **GitHub Environments** `staging` and `production`, both restricted to the `main` branch, each with the four
   deployment secrets (`ENGINEBOX_DEPLOY_HOST`, `ENGINEBOX_DEPLOY_USER`, `ENGINEBOX_DEPLOY_SSH_KEY`,
   `ENGINEBOX_DEPLOY_KNOWN_HOSTS`; see `packages/enginebox-deploy/README.md`). Same host, so the values may be equal;
   a separate key per environment is better.
3. **Repository variables**: `ENGINEBOX_DEPLOY_ENABLED=true`. Optional: `ENGINEBOX_PROD_HOLD=true` (keep builds in
   staging), `ENGINEBOX_DEPLOY_RUNNER`, `ENGINEBOX_DEPLOY_VERSION_BASE`, `ENGINEBOX_STAGING_CONFIG`,
   `ENGINEBOX_DEPLOY_CONFIG`.
4. Push to `main` (or run the workflow with `target=staging` first) and watch the run.

## Provider sign-ins in staging

Staging starts with no provider accounts, so the smoke gate checks the controller and API only and reports
`run: skipped`. To gate on a real agent run, sign one **dedicated test account** (not a production team account) into
the staging dashboard (`http://127.0.0.1:3100` over an SSH tunnel), create a small pool (for example `staging`, the
`native-dev` template, `max_workspaces: 2`), and set in `staging.json`:

```json
"smoke": { "pool": "staging", "provider": "codex", "prompt": "Reply with exactly: OK", "expect": "OK", "timeout_seconds": 300 }
```

The run then also exercises placement, wake-from-frozen/stopped and the start-path metrics of the workspace lifecycle.

## Hold, manual runs and rollback

- **Hold**: set `ENGINEBOX_PROD_HOLD=true`. Builds keep landing in staging; clear it (or run `target=production`) to
  promote.
- **One environment**: run the workflow with `target=staging` or `target=production`. `update` with
  `target=production` skips staging and is the emergency path.
- **Rollback**: run with `operation=rollback`, `target=production` (or `staging`) and an exact retained `version`. The
  node helper is re-activated at the same version.
- **Status**: `operation=status` with a target.
