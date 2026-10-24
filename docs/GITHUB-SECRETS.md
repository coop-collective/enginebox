# GitHub App setup with Kitbash and Infisical

This follows the local Kitbash checkout at `~/Workspace/feature-registry`: [secret setup](https://github.com/coop-collective/kitbash/blob/main/docs/SECRETS-BROKER.md), [operator workflow](https://github.com/coop-collective/kitbash/blob/main/bootstraps/shared/files/docs/secrets.md), and its `kitbash-secrets` skill. Infisical owns application secrets; the isolated broker returns names/status only. Real provider credentials must come from GitHub. Nothing in this change registers an App, writes a vault, changes organization runners or deploys credentials.

## 1. Register two GitHub Apps

Use organization **Settings → Developer settings → GitHub Apps → New GitHub App**, or personal developer settings for a repository-only trial. Choose distinct App names/slugs for repository access and runner administration. Use separate registrations and credentials for staging and production.

For both Apps:

- Homepage: your dashboard origin, matching `ENGINEBOX_DASHBOARD_ORIGIN`.
- Callback: `https://YOUR-DASHBOARD/api/github/callback`, with no query string.
- Enable expiring user access tokens. Engine Box requests `offline_access` and uses PKCE; no device-flow configuration is needed.
- Record **Client ID** and App slug as configuration metadata. Generate a **client secret** and download an **RSA private key** through GitHub. Store these directly in Infisical from your trusted operator session; do not paste them into chat or GitHub Actions logs.
- Enable the webhook, use JSON, and configure the corresponding URL below. Copy its broker-generated secret from Infisical into GitHub using your trusted operator session.

| Registration | Repository permissions | Organization permissions | Webhook URL/events |
|---|---|---|---|
| Repository App | Metadata read; Contents read/write; optional Workflows write | None | `/api/github/webhook/repos`; authorization events are delivered by GitHub, source push events are not needed |
| Runner App | Metadata read; Actions read | Self-hosted runners read/write | `/api/github/webhook/runners`; subscribe to **Workflow job** |

When enabling Workflows write, also set `repos.workflows_write: true` in controller configuration. Otherwise delegated exports request Contents only. Install the repository App on **only selected repositories**. Install the runner App in the organization and include the CI repositories. Organization owners must approve runner permissions. OAuth authorization and App installation are distinct steps.

GitHub App user permissions are the intersection of the App grant and the user's access. [User authorization](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-user-access-token-for-a-github-app), [permission selection](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/choosing-permissions-for-a-github-app).

## 2. Register the secrets policy

Use [github-secrets.kitbash.json](../examples/github-secrets.kitbash.json) as the operator helper's `.kitbash/secrets.json`, replacing the project UUID/domain with your existing Infisical project. It contains names and identifiers only. Targets are explicit: `local → dev`, `staging → staging`, `production → production`, each using the dedicated `/github` folder.

| Infisical name | Source | Controller filename |
|---|---|---|
| `ENGINEBOX_GITHUB_REPOS_CLIENT_SECRET` | GitHub repository App | `repos-client-secret` |
| `ENGINEBOX_GITHUB_REPOS_PRIVATE_KEY` | Downloaded repository App PEM | `repos-private-key` |
| `ENGINEBOX_GITHUB_REPOS_WEBHOOK_SECRET` | Broker-generated 32 random bytes | `repos-webhook-secret` |
| `ENGINEBOX_GITHUB_RUNNERS_CLIENT_SECRET` | GitHub runner App | `runners-client-secret` |
| `ENGINEBOX_GITHUB_RUNNERS_PRIVATE_KEY` | Downloaded runner App PEM | `runners-private-key` |
| `ENGINEBOX_GITHUB_RUNNERS_WEBHOOK_SECRET` | Broker-generated 32 random bytes | `runners-webhook-secret` |

Ask the broker operator to register the `enginebox` project alias with these six policies, the intended environment and `/github` source folder. Its policy uses `secretPath`; the helper example uses `path`. The example does not authorize itself. Provider-issued entries have `generate: false`; webhook entries have `generate: true`, `bytes: 32`. For repository-only operation, omit runner entries from the policy/helper configuration and set `runners: null`, `pools: []` in `github.json`.

After that alias and the agent's scoped grant are configured, use Kitbash:

```text
/kit:secrets setup enginebox staging
/kit:secrets status enginebox staging
```

Or invoke `kitbash_secrets` with `{"operation":"setup","project":"enginebox","target":"staging"}`. `setup` generates missing webhook secrets, preserves existing values, and reports missing provider names as **pending**. Enter the four GitHub-issued values through the trusted Infisical UI/provider connection, then check status again. Keep PEM newlines intact. No broker was connected or provisioned by this implementation.

For this native-controller integration, register secret storage/generation only. Kitbash's Cloudflare Worker sync does **not** install files on a Linux controller. The existing code-deployment workflow does not need these App secrets and must not receive them at build time. If you later automate trusted credential staging with GitHub Actions, configure an exact repository/environment/workflow OIDC identity with read access to `/github`, a protected environment, and reviewed credential-consuming code. Keep deployment keys in a separate `/ci` folder. Do not grant ordinary Actions workload VMs access to `/github`.

## 3. Install credentials on the trusted controller host

Run this as the controller service identity from a **trusted operator checkout**, outside the coding agent's filesystem/process boundary. The parent directory must already be private and writable. Set up the operator's `.kitbash/secrets.json` as above and log in to Infisical using the configured domain. The existing Kitbash helper injects only registered application values into this adapter; the adapter writes files and prints names/status, never values:

```sh
# KITBASH_ROOT points to the trusted Kitbash installation (locally: ~/Workspace/feature-registry).
# Run from the trusted Engine Box checkout containing .kitbash/secrets.json.
node "$KITBASH_ROOT/bootstraps/shared/files/scripts/secrets.mjs" run --env staging -- \
  node packages/enginebox-deploy/src/github-secrets.mjs \
  --directory /etc/enginebox/secrets/github
```

Use `--env production` only for the production identity/project target. Add `--repos-only` after the destination for repository-only setup. The destination must be new: this command refuses to overwrite an existing directory, validates all required names before writing, creates mode-0700/0600 files on Unix, and removes newly staged files on a write failure. For Windows controllers use a private NTFS directory with an owner-only ACL established by the operator; Unix mode bits do not enforce Windows ACLs. An existing vault/secret agent may instead materialize these same files under the controller's private runtime identity.

Copy [github.example.json](../examples/github.example.json) to `<controller-state>/github.json`, mode 0600 with a private parent. Set the callback, both Client IDs/slugs, and secret file references. Inline values are supported for existing private configurations, but the example uses `{"file":"/absolute/private/path"}` so application configuration contains no credentials. Unix secret files must be regular, non-symlink files with owner-only access. They are read on controller activation, not on every request. Avoid exposing this directory to project processes or reusable images.

For runner pools, set the actual organization installation ID, runner group ID and private worker configuration path. Follow [runner setup](GITHUB-ACTIONS.md#setup-and-supported-interfaces). Stage the release in standby, drain the current owner, then activate through the normal supervisor procedure. Updating files alone does not activate a new configuration. Do not restart a developer workspace as a qualification fixture.

## 4. Verify and rotate deliberately

1. Open the dashboard **GitHub** panel; connect repository access and select one disposable private test repository/branch.
2. Open **Import or sync in Storage**. Fetch/review and import its credential-free source into LayerFS. Publish an accepted test change and verify its actual GitHub commit. Removing the grant must prevent the next transfer.
3. Connect runner administration, approve the installation, and verify the expected group/pool. Opt one non-deploying test workflow into its `enginebox-<pool>` label. Verify GitHub's real conclusion, private diagnostics, VM deletion and runner cleanup.
4. Test cancellation and drain before migrating selected workflow routing from an existing runner group. Keep old capacity until the canary succeeds.

Provider keys rotate through GitHub, then Infisical and a **new** private credential directory. Repoint the configuration during a drained release handoff, test, and revoke the previous provider key only after the new one works. The current webhook verifier accepts one secret per App: changing it needs a coordinated GitHub/controller cutover and inspection/redelivery of failed deliveries. Random replacement is not a seamless rotation protocol.

User OAuth refresh tokens and issued JIT configurations are runtime state, not entries to copy into the setup vault or CI secrets. They remain private to the controller/one-use worker respectively. The setup tests use fixtures; they do not establish real OAuth approval, vault access, successful hosted Actions execution or performance savings.
