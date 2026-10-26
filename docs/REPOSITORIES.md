# Project repositories

Engine Box can host Git repositories per [project](QUEUE.md#pools), so agents running in Engine Box workspaces clone and push without anyone's personal GitHub account. Repositories live in a **backend**. The first backend is [Cloudflare Artifacts](https://developers.cloudflare.com/artifacts/) (beta); a GitHub App backend is planned next behind the same interface.

This is separate from the optional [GitHub sync](GITHUB-SYNC.md) and from LayerFS project storage: a hosted repository is an ordinary Git remote that agents use directly.

## Model

- **One namespace per project.** Each Engine Box project maps to one Artifacts namespace, `<namespace_prefix>-<project>` (default prefix `enginebox`). Artifacts creates a namespace the first time a repository is created in it. Project IDs are the controller's (`^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$`), so a prefix is at most 14 characters and the namespace fits Artifacts' 63-character limit.
- **Repository names** are 1–63 lowercase letters, digits, `.`, `_` or `-`, starting with a letter or digit, not ending in `.git` or `.lock`, without `..`.
- **Records.** The controller keeps one private record per repository (`repositories/records/<project>/<name>.json`, mode 0600): project, name, backend, description, remote, default branch, creation time. Records never contain a token. `last_push_at` is read live from the backend and is null when it is unavailable.
- **Tokens are short-lived and minted on demand.** The backend's API token stays on the controller. Clients receive repository tokens only by asking for one (`POST …/tokens`, at most 24 hours), and runs that name a repository receive a write token inside their private native home.

## Setup

1. A Cloudflare account on the **Workers Paid** plan (Artifacts is not available on Workers Free; billing for Artifacts operations and storage applies, see [pricing](https://developers.cloudflare.com/artifacts/platform/pricing/)).
2. A [Cloudflare API token](https://developers.cloudflare.com/fundamentals/api/get-started/create-token/) with **Account → Artifacts → Read** and **Account → Artifacts → Edit**, limited to that account. Nothing else is needed.
3. Configure the backend with the owner token:

```sh
curl -X PUT "$ENGINEBOX_API/v1/repository-backends/artifacts" \
  -H "Authorization: Bearer $ENGINEBOX_OWNER_TOKEN" -H "Content-Type: application/json" \
  -d '{"account_id":"<32-hex account ID>","namespace_prefix":"enginebox","api_token":"<token>"}'
```

The API token is write-only: it is stored in owner-only private state (`repositories/backends/artifacts.json`, mode 0600, never in Git), is never returned, and is never logged. `GET` answers the redacted view; `DELETE` removes the configuration (records stay, and operations that need the backend answer 409 until it is configured again). While repositories exist, a new `PUT` may replace the API token but not the account or prefix.

## API

All bodies are JSON. Errors are `{"error":"…"}`.

| Request | Who | Result |
|---|---|---|
| `PUT /v1/repository-backends/artifacts` `{account_id, namespace_prefix?, api_token}` | Owner | `200 {"backend":{"kind":"artifacts","configured":true,"account_id","namespace_prefix","api_token":"[redacted]","updated_at"}}` |
| `GET /v1/repository-backends/artifacts` | Owner | The same view, or `{"backend":{"kind":"artifacts","configured":false}}` |
| `DELETE /v1/repository-backends/artifacts` | Owner | `200 {"deleted":true,"backend":{"kind":"artifacts","configured":false}}` |
| `GET /v1/repositories` | Owner | Every repository record (no backend call) |
| `POST /v1/projects/{project}/repositories` `{name, description?}` | Owner, or `write` grant | `201` created, `200` when it already exists (idempotent on name): `{"repository":{project,name,backend,description,remote,default_branch,created_at,last_push_at}}`. Never a token. |
| `GET /v1/projects/{project}/repositories` | Owner, or any grant | `{"project","repositories":[…]}` |
| `GET /v1/projects/{project}/repositories/{name}` | Owner, or any grant | `{"repository":{…}}`; 404 when unknown |
| `POST /v1/projects/{project}/repositories/{name}/tokens` `{access:"read"\|"write", ttl_seconds?}` | Owner, or a grant (`read` grants mint `read` only) | `201 {"token","expires_at","remote","default_branch","access"}`. `ttl_seconds` 60–86400, default 3600. Nothing about the token is stored. |

Statuses: 400 invalid field, 403 not granted, 404 unknown repository, 409 no backend configured (or limits), 502 the backend refused or is unreachable (no backend detail is returned).

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
- The token never enters run records, sidecars, queue files, logs, launch events or callbacks; launch failure messages and returned output scrub it. The directory is removed when the run ends, and the token is revoked through the Artifacts API (`DELETE …/tokens/{id}`); if revocation fails it still expires on its own.
- Repository runs use the guest command `run-repository`. A workspace whose guest helper does not list it fails the run before anything starts, with `failure: {"stage":"bootstrap","reason":"guest_outdated"}`. A token that cannot be minted fails it with `{"stage":"resolve","reason":"repository_unavailable"}`.
- Codex runs in its `workspace-write` sandbox, which normally has no network access; pushing from Codex needs a profile or workspace that allows it.

## Backends

| Backend | Status |
|---|---|
| `artifacts` (Cloudflare Artifacts REST API) | Implemented |
| `github-app` | Next |

A backend implements `repositories::Backend` in `packages/enginebox-controller/src/repositories.rs`: `create` (idempotent, reports whether it created), `get`, `list` (with last push times), `mint(project, name, access, ttl)` and `revoke(project, token_id)`. HTTP goes through a `Transport` trait so tests replace the remote API. A GitHub App backend would map a project to an installation and organization, create repositories through the App, and mint installation tokens restricted to one repository with `contents: read|write`; its configuration would be another owner-only `PUT /v1/repository-backends/github-app`.

## Artifacts API used

Verified against the Artifacts docs ([REST API](https://developers.cloudflare.com/artifacts/api/rest-api/), [Git protocol](https://developers.cloudflare.com/artifacts/api/git-protocol/), [authentication](https://developers.cloudflare.com/artifacts/guides/authentication/), [limits](https://developers.cloudflare.com/artifacts/platform/limits/), [errors](https://developers.cloudflare.com/artifacts/api/errors/)). Base `https://api.cloudflare.com/client/v4/accounts/{account}/artifacts/namespaces/{namespace}`, `Authorization: Bearer <API token>`, v4 envelope.

| Call | Use |
|---|---|
| `POST /repos {name, description?}` | Create. `409`/code `10201` (`ALREADY_EXISTS`) → read it instead. The response's token is discarded and every active token of the new repository is revoked. |
| `GET /repos/{name}` | Read (remote, default branch, `last_push_at`); `404`/`10200` when missing. |
| `GET /repos?limit=200&sort=name&direction=asc&cursor=` | List with last push times. |
| `POST /tokens {repo, scope:"read"\|"write", ttl}` | Mint; returns `{id, plaintext, scope, expires_at}`. TTL 60–31 536 000 s (Engine Box allows at most 86 400 for clients). |
| `DELETE /tokens/{id}` | Revoke a run's token at run end. |
| `GET /repos/{name}/tokens?state=active` | Find the creation token to revoke. |

Not used yet: namespace management, fork, import, repository deletion.

## Not yet

- Deleting or renaming repositories through Engine Box (records only grow; delete in Artifacts and the record stays until removed by hand).
- Per-repository grants (grants are per project).
- Branch protection or push policies; anyone holding a write token can push any branch.
- The GitHub App backend.
