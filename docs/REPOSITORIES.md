# Project repositories

Engine Box can host Git repositories per [project](QUEUE.md#pools), so agents running in Engine Box workspaces clone and push without anyone's personal GitHub account. Repositories live in [Cloudflare Artifacts](https://developers.cloudflare.com/artifacts/) (beta), reached through the **Artifacts gateway**: a small Worker (`packages/artifacts-gateway`) that holds Artifacts bindings and answers only requests the controller signs. Engine Box stores no Cloudflare API token. A GitHub App backend is planned next behind the same interface.

This is separate from the optional [GitHub sync](GITHUB-SYNC.md) and from LayerFS project storage: a hosted repository is an ordinary Git remote that agents use directly.

## Model

- **One namespace per project.** Each Engine Box project maps to one Artifacts namespace, `<namespace_prefix>-<project>` (default prefix `enginebox`). Project IDs are the controller's (`^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$`), so a prefix is at most 14 characters and the namespace fits Artifacts' 63-character limit.
- **One binding per namespace.** An Artifacts Workers binding is bound to exactly one namespace (`namespace` is required in the binding config), so the gateway carries one binding per provisioned project. Adding a project means adding it to the gateway's configuration and redeploying; until then, creating a repository there answers 409 ("the repository gateway does not serve this project's namespace").
- **Repository names** are 1–63 lowercase letters, digits, `.`, `_` or `-`, starting with a letter or digit, not ending in `.git` or `.lock`, without `..`.
- **Records.** The controller keeps one private record per repository (`repositories/records/<project>/<name>.json`, mode 0600): project, name, backend (`artifacts`), description, remote, default branch, creation time. Records never contain a token. `last_push_at` is read live and is null when it is unavailable.
- **Tokens are short-lived and minted on demand.** Clients receive repository tokens only by asking for one (`POST …/tokens`, at most 24 hours), and runs that name a repository receive a write token inside their private native home.

## Setup

You need a Cloudflare account on the **Workers Paid** plan (Artifacts is not available on Workers Free; Artifacts operations and storage are billed, see [pricing](https://developers.cloudflare.com/artifacts/platform/pricing/)), Wrangler signed in to that account (`pnpm --filter @enginebox/artifacts-gateway exec wrangler login`), and a hostname on a zone in that account for the gateway's custom domain.

1. **Generate the deploy configuration** (written privately to `.state/artifacts-gateway/wrangler.json`; nothing account-specific is committed):

   ```sh
   ENGINEBOX_ARTIFACTS_ACCOUNT_ID=<32-hex account ID> \
   ENGINEBOX_ARTIFACTS_HOSTNAME=artifacts.example.com \
   ENGINEBOX_ARTIFACTS_PROJECTS=default,alpha \
   pnpm --filter @enginebox/artifacts-gateway configure
   ```

   Optional: `ENGINEBOX_ARTIFACTS_NAMESPACE_PREFIX` (default `enginebox`), `ENGINEBOX_ARTIFACTS_WORKER_NAME` (default `enginebox-artifacts-gateway`). The file has `workers_dev: false`, `preview_urls: false`, the custom domain route, one `artifacts` binding per project, and the `ReplayGuard` Durable Object that remembers request nonces.

2. **Deploy and set the shared secret on the Worker:**

   ```sh
   pnpm --filter @enginebox/artifacts-gateway dry-run
   pnpm --filter @enginebox/artifacts-gateway run deploy
   openssl rand -hex 32 > /tmp/gateway-secret   # keep it private; delete after step 3
   pnpm --filter @enginebox/artifacts-gateway run secret < /tmp/gateway-secret
   ```

3. **Configure the controller** with the owner token (the same secret):

   ```sh
   curl -X PUT "$ENGINEBOX_API/v1/repository-backends/artifacts-worker" \
     -H "Authorization: Bearer $ENGINEBOX_OWNER_TOKEN" -H "Content-Type: application/json" \
     -d "{\"url\":\"https://artifacts.example.com\",\"namespace_prefix\":\"enginebox\",\"secret\":\"$(cat /tmp/gateway-secret)\"}"
   rm /tmp/gateway-secret
   ```

The secret is write-only on the controller: stored in owner-only private state (`repositories/backends/artifacts-worker.json`, mode 0600, never in Git), never returned and never logged. `GET` answers the redacted view; `DELETE` removes the configuration (records stay, and operations that need the backend answer 409 until it is configured again). While repositories exist, a new `PUT` may replace the URL and secret but not the prefix. To rotate the secret, set the new value on the Worker, then `PUT` it on the controller (requests fail with 502 in between).

To add a project later, rerun step 1 with the longer `ENGINEBOX_ARTIFACTS_PROJECTS` list and `deploy` again.

## Gateway protocol

The gateway answers only `POST` with a JSON body (at most 16 KiB) on its custom domain. Every request carries `X-EngineBox-Timestamp` (unix seconds), `X-EngineBox-Nonce` (32 lowercase hex) and `X-EngineBox-Signature: v1=<hex HMAC-SHA256(secret, canonical)>`, where `canonical` is `v1\nPOST\n<path>\n<timestamp>\n<nonce>\n<hex SHA-256 of the body>`. The timestamp must be within ±300 seconds, each nonce is accepted once (the `ReplayGuard` Durable Object remembers it for 600 seconds; only correctly signed requests reach it), and the comparison is constant time. Any failure answers `401 {"error":"unauthorized"}` without detail. Other requests: `GET`/`OPTIONS` 405, any `Origin` header 403, workers.dev hosts and unknown paths 404. Responses carry `Cache-Control: no-store` and no CORS headers. The Worker logs operation, namespace and status only.

| Path | Body | Result |
|---|---|---|
| `/repos.create` | `{namespace, name, description?}` | `{created:true, repository:{id,name,description,default_branch,remote,last_push_at}}`; `409 already_exists`. The creation token is revoked and never returned. |
| `/repos.get` | `{namespace, name}` | `{repository}`; `404 not_found` |
| `/repos.list` | `{namespace, cursor?}` | `{repositories:[…], cursor}` (pages of 200; no `remote` in listings) |
| `/tokens.create` | `{namespace, repo, scope:"read"\|"write", ttl}` | `{token:{id, plaintext, scope, expires_at}}`; ttl 60–90 000 |
| `/tokens.revoke` | `{namespace, repo, id}` | `{revoked}` |

`namespace` must start with `<NAMESPACE_PREFIX>-` (else `403 namespace_not_allowed`) and have a binding (else `404 namespace_not_provisioned`). Binding errors map to codes only: `NOT_FOUND` 404, `ALREADY_EXISTS` and `*_IN_PROGRESS` 409, `INVALID_*` 400, anything else `502 upstream`.

## API

All bodies are JSON. Errors are `{"error":"…"}`.

| Request | Who | Result |
|---|---|---|
| `PUT /v1/repository-backends/artifacts-worker` `{url, secret, namespace_prefix?}` | Owner | `200 {"backend":{"kind":"artifacts_worker","configured":true,"url","namespace_prefix","secret":"[redacted]","updated_at"}}`. `url` is the gateway's `https://` origin (no path; workers.dev refused). |
| `GET /v1/repository-backends/artifacts-worker` | Owner | The same view, or `{"backend":{"kind":"artifacts_worker","configured":false}}` |
| `DELETE /v1/repository-backends/artifacts-worker` | Owner | `200 {"deleted":true,"backend":{…,"configured":false}}` |
| `GET /v1/repository-backends` | Owner | The selected backend (exactly one may be configured), or `{"backend":{"kind":null,"configured":false}}`; repository operations answer 409 while none is. |
| `GET /v1/repositories` | Owner | Every repository record (no backend call) |
| `POST /v1/projects/{project}/repositories` `{name, description?}` | Owner, or `write` grant | `201` created, `200` when it already exists (idempotent on name): `{"repository":{project,name,backend:"artifacts",description,remote,default_branch,created_at,last_push_at}}`. Never a token. |
| `GET /v1/projects/{project}/repositories` | Owner, or any grant | `{"project","repositories":[…]}` |
| `GET /v1/projects/{project}/repositories/{name}` | Owner, or any grant | `{"repository":{…}}`; 404 when unknown |
| `POST /v1/projects/{project}/repositories/{name}/tokens` `{access:"read"\|"write", ttl_seconds?}` | Owner, or a grant (`read` grants mint `read` only) | `201 {"token","expires_at","remote","default_branch","access"}`. `ttl_seconds` 60–86400, default 3600. Nothing about the token is stored. |

Statuses: 400 invalid field, 403 not granted, 404 unknown repository, 409 no backend configured, project not served by the gateway, or limits; 502 the gateway refused (for example a wrong secret) or is unreachable (no backend detail is returned).

Use a token with git:

```sh
git -c http.extraHeader="Authorization: Bearer $TOKEN" clone "$REMOTE"
```

The full token string (including its `?expires=<unix seconds>` suffix) is the bearer value.

## Grants

- **Service tokens** take `repositories: [{"project":"alpha","access":"write"}]` at mint (one entry per project). `write`: create, list, read, mint read or write tokens, and name the project's repositories on runs. `read`: list, read and mint read tokens. A token whose `project` restriction is set may hold repository grants only for that project. A repository-only token needs no `operations`. Everything else answers 403.
- **OAuth.** Consent shows "Manage repositories in project X" per project (projects that have a pool or a repository; only the requested one when the app sent `project=`), with no access, read or write. The grant appears in `enginebox_grants` as `{"kind":"repositories","project":"alpha","access":"write"}` and every access token minted for the connection carries it. The public proxy forwards the four repository routes. See [OAuth](OAUTH.md).

## Runs with a repository

A run request may add `"repository":"<project>/<name>"` (see [FLEET.md](FLEET.md#agent-runs-v2)):

- A service token needs a `write` grant on that project; a pool run (owner or token) needs the repository's project to equal the pool's project. Otherwise 403. Unknown repository: 404. No backend: 409.
- The run records only the name (`"repository":"alpha/game"` on reads, listings and the callback payload).
- **At admission**, not at acceptance, the controller mints a write token whose lifetime is the run's `timeout_seconds` plus 600 seconds (at most 87 000 seconds). A queued run holds no token.
- The guest writes a git config into the run's private directory, `~/.local/state/enginebox/agent-runs/repositories/<run_id>/gitconfig` (directory 0700, file 0600):

  ```ini
  [http "https://<account>.artifacts.cloudflare.net/git/<namespace>/<repo>.git"]
  	extraHeader = "Authorization: Bearer <token>"
  ```

  Git sends the header only to URLs under that remote. The CLI process gets `GIT_CONFIG_COUNT=1`, `GIT_CONFIG_KEY_0=include.path` and `GIT_CONFIG_VALUE_0=<that file>` (the user's own git config still applies), plus `ENGINEBOX_REPO_REMOTE` and `ENGINEBOX_REPO_DEFAULT_BRANCH`, so `git clone "$ENGINEBOX_REPO_REMOTE"` and `git push` just work. A repository run refuses caller `env` names starting with `GIT_CONFIG` and `GIT_ASKPASS`.
- The token never enters run records, sidecars, queue files, logs, launch events or callbacks; launch failure messages and returned output scrub it. The directory is removed when the run ends, and the token is revoked through the gateway (`/tokens.revoke`); if revocation fails it still expires on its own.
- Repository runs use the guest command `run-repository`. A workspace whose guest helper does not list it fails the run before anything starts, with `failure: {"stage":"bootstrap","reason":"guest_outdated"}`. A token that cannot be minted fails it with `{"stage":"resolve","reason":"repository_unavailable"}`.
- Codex runs in its `workspace-write` sandbox, which normally has no network access; pushing from Codex needs a profile or workspace that allows it.

## Backends

| Backend | Status |
|---|---|
| `artifacts_worker` (Cloudflare Artifacts through the gateway Worker's bindings) | Implemented |
| `github-app` | Next |

A backend implements `repositories::Backend` in `packages/enginebox-controller/src/repositories.rs`: `create` (idempotent, reports whether it created), `get`, `list` (with last push times), `mint(project, name, access, ttl)` and `revoke(project, name, token_id)`. HTTP goes through a `Transport` trait so tests replace the remote service. Exactly one backend is configured at a time. A GitHub App backend would map a project to an installation and organization, create repositories through the App, and mint installation tokens restricted to one repository with `contents: read|write`; its configuration would be another owner-only `PUT /v1/repository-backends/github-app`.

An earlier draft of this feature called the Artifacts REST API with a Cloudflare account API token; it was replaced by the gateway so no account-wide credential is stored on the controller.

## Artifacts interfaces used

Verified against the Artifacts docs ([Workers binding](https://developers.cloudflare.com/artifacts/api/workers-binding/), [Workers get started](https://developers.cloudflare.com/artifacts/get-started/workers/), [Git protocol](https://developers.cloudflare.com/artifacts/api/git-protocol/), [limits](https://developers.cloudflare.com/artifacts/platform/limits/), [errors](https://developers.cloudflare.com/artifacts/api/errors/)) and the `Artifacts` types shipped with workerd 1.20261019.1 and Wrangler 4.152.0 (binding config `{"binding","namespace","remote"?}`, `namespace` required).

| Binding call | Use |
|---|---|
| `env.ARTIFACTS_n.create(name, {description})` | Create; `ALREADY_EXISTS` → the controller reads the existing repository instead. |
| `(await get(name)).info()` | Remote, default branch, `lastPushAt`. |
| `list({limit: 200, cursor})` | Names and last push times (listings carry no remote). |
| `(await get(name)).createToken(scope, ttl)` | `{id, plaintext, scope, expiresAt}`. |
| `(await get(name)).revokeToken(idOrPlaintext)` | Revoke a run's token at run end, and the creation token at once. |

Repository handles are disposed after each call. Git clients use the full `art_v1_…?expires=<unix seconds>` token as the bearer value.

## Not yet

- Deleting or renaming repositories through Engine Box (records only grow; delete in Artifacts and the record stays until removed by hand).
- Per-repository grants (grants are per project).
- Branch protection or push policies; anyone holding a write token can push any branch.
- The GitHub App backend.
