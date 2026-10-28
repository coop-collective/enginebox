# GitHub App setup with Kitbash

One GitHub App supports repository sync and Actions runners. Reuse the existing
runner App and its three credentials; a second repository App is optional.
Kitbash supports Cloudflare Secrets Store and the Infisical adapter below. Both
keep credential values outside chat and normal CI jobs. GitHub issues App
credentials; Kitbash generates webhook secrets and returns metadata through MCP.

## 1. Configure one App and installation

In organization **Settings → Developer settings → GitHub Apps**, edit your existing
App or create one. Use separate registrations for staging and production.

- Homepage: your dashboard origin, matching `ENGINEBOX_DASHBOARD_ORIGIN`.
- Callback: `https://YOUR-DASHBOARD/api/github/callback`, with no query string.
- Enable expiring user access tokens. Engine Box uses PKCE and refresh tokens;
  no device-flow configuration is needed. Start authorization from the dashboard.
- Webhook: `https://YOUR-DASHBOARD/api/github/webhook/runners`, JSON, active,
  with the existing matching webhook secret. Subscribe to **Workflow job**.
- Repository permissions: **Metadata read**, **Contents read/write** for sync,
  **Actions read** for runners. **Workflows write** is optional for exporting
  edits to `.github/workflows`; also set `app.workflows_write: true` when enabling it.
- Organization permissions: **Self-hosted runners read/write** for organization runners.
- Install on the organization and include the intended repositories. When adding
  permissions to an existing App, accept the new permissions on that installation.
  Keep the installation, Client ID, private key and client secret.

A GitHub App can request repository and organization permissions together. Added
permissions take effect after the installation owner approves them; reconnecting
OAuth alone cannot approve them. [Permission selection](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/choosing-permissions-for-a-github-app),
[permission updates](https://docs.github.com/en/apps/maintaining-github-apps/modifying-a-github-app-registration#changing-the-permissions-of-a-github-app).

Contents write is for repository sync, not required by the self-hosted runner protocol.
Record the **Client ID** and slug as metadata. Generate a client secret and RSA private
key only if you do not already have them. Keep provider values in private files or
the trusted vault, never chat or Git. OAuth authorization and installation remain
separate GitHub steps, but both features use the same App and account authorization.

## 2. Deliver existing credentials through Cloudflare

Use Kitbash's [MCP file uploads and controller delivery](https://github.com/coop-collective/kitbash/blob/main/docs/BROKER-SETUP.md#deliver-uploaded-credentials-to-a-native-controller)
from the trusted `~/Workspace/kitbash` checkout. Register these three names for the
chosen target with `kitbash_register_secrets`:

| Secret name | Source | Controller filename |
|---|---|---|
| `ENGINEBOX_GITHUB_RUNNERS_CLIENT_SECRET` | Existing GitHub App | `runners-client-secret` |
| `ENGINEBOX_GITHUB_RUNNERS_PRIVATE_KEY` | Existing GitHub App PEM | `runners-private-key` |
| `ENGINEBOX_GITHUB_RUNNERS_WEBHOOK_SECRET` | Existing matching secret, or broker-generated for a new App | `runners-webhook-secret` |

The `RUNNERS` prefix is retained for compatibility; these credentials now serve
both features. Do not duplicate or rename stored secrets. If already delivered,
reuse the private file references from the receiver receipt and skip uploading.

For a new delivery, generate only a missing webhook secret, then enroll the trusted
controller using `pnpm run setup:controller` in Kitbash. Discover the destination
with `kitbash_secret_deliveries`, then use `kitbash_upload_secrets` and its Python
helper to send private credential files. The existing App and webhook value are
preserved. No human upload page is needed.

The receiver decrypts on the controller, sets the matching GitHub webhook secret,
and stages owner-only files plus a `runners.json` App fragment with absolute file
references. It does not attach credentials to application Workers or Actions VMs.

A delivered receipt is **staging evidence only**. Put the fragment's App fields under
**`app`** in the complete private `<controller-state>/github.json`, alongside `schema`,
`callback_url` and `pools`; see [github.example.json](../examples/github.example.json).
The fragment's filename does not require a separate `runners` registration in the
configuration. Use `pools: []` until the dedicated CI worker and runner group are ready.
The helper does not activate Enginebox configuration or restart a developer workspace.

## 3. Optional Infisical adapter

Use [github-secrets.kitbash.json](../examples/github-secrets.kitbash.json) for the
operator helper's `.kitbash/secrets.json`. Fill the real project UUID/domain and
map each target to your existing environment slug and `/github` folder. The
example's `production` environment is illustrative; use `prod` if that is the
actual Infisical environment. The helper uses `path`; broker policies use `secretPath`.

Register the `enginebox` alias, target and scoped principal grant with the broker.
Provider entries use `generate: false`, `rotation: provider`; webhook generation
uses `generate: true`, `bytes: 32`, `rotation: migration-required`. Existing values
must be preserved. `setup` reports missing provider values as pending; GitHub must
issue them. The example does not grant authorization to itself.

```text
/kit:secrets setup enginebox staging
/kit:secrets status enginebox staging
```

From a trusted operator checkout with the target's Infisical login, stage the three
existing runner names into a **new** private directory:

```sh
# KITBASH_ROOT is the trusted Kitbash installation.
node "$KITBASH_ROOT/bootstraps/shared/files/scripts/secrets.mjs" run --env staging -- \
  node packages/enginebox-deploy/src/github-secrets.mjs \
  --directory /etc/enginebox/secrets/github --single-app
```

Use `--env production` only for the production target. The adapter validates all
three values before creating mode-0700/0600 files, refuses to overwrite an existing
directory, cleans newly staged files on write failure and prints names/status only.
On Windows use a private NTFS directory with an owner-only ACL; Unix mode bits do
not enforce Windows ACLs. A trusted vault agent can supply the same files instead.
Never expose this directory to project processes, reusable images or normal CI jobs.

For existing separate-App deployments, the adapter without a mode flag still stages
all six `ENGINEBOX_GITHUB_{REPOS,RUNNERS}_*` names. Register those six in your helper
policy and use [github-separate-apps.example.json](../examples/github-separate-apps.example.json).
`--repos-only` still stages only repository credentials. Neither is needed for the
single-App configuration.

## 4. Activate and verify

Copy the single-App example to `<controller-state>/github.json`, with an owner-only
parent and mode 0600. Set the callback, one Client ID/slug and private file references.
Inline values remain supported for existing private configurations, but file references
keep secrets out of configuration. Secret files must be regular, non-symlink files
and owner-only on Unix. They are read when the controller activates.

If switching an already connected two-App or repository-only configuration to `app`,
first drain and resolve CI jobs and disconnect the old accounts. Then update the
configuration and connect once after activation. Enginebox refuses to reuse the old
accounts across modes. Repository selections must be added again; LayerFS files remain.
Use the normal standby, drain and activation release procedure; staging files alone
is not an active integration.

1. Open dashboard **GitHub**, connect once and select a disposable private repository.
2. **Choose repositories** shows Contents approval. **Review runner permissions** checks
   Actions and organization runner permissions on the same App; no second login is needed.
3. Open **Import or sync in Storage**, review/import credential-free source, then export
   an accepted test revision and verify its GitHub commit. Revocation must stop transfers.
4. Configure the actual installation/group IDs and dedicated CI worker according to
   [runner setup](GITHUB-ACTIONS.md#setup-and-supported-interfaces). Route a non-deploying
   canary to its `enginebox-<pool>` label. Verify the real conclusion, private diagnostics,
   VM deletion and runner cleanup. Exercise cancellation and drain before migrating traffic.

Shared mode has one OAuth refresh record. Disconnecting either connection API removes
both capabilities and repository selections only after jobs are resolved. Short-lived
installation tokens remain scoped to each operation; App keys never enter Actions VMs.

Rotate provider keys through GitHub and a new private credential directory, then
repoint configuration during a drained handoff. Revoke the old key after verification.
Webhook-secret replacement needs coordinated GitHub/controller activation and inspection
of failed deliveries. Runtime OAuth refresh tokens and JIT configurations are not setup
vault entries. Offline tests do not prove live OAuth, an Actions job or performance savings.
