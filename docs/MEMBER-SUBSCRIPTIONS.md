# Member subscriptions

An app such as Unity Jam lets each person connect their **own** Claude or ChatGPT
subscription and run **their own** prompts with it. Engine Box holds the
subscription; the app never sees a token. The rules:

- A run started for member X uses only X's own personal credential. There is no
  fallback to another member's, a shared one, a team account, an API key, the
  controller's accounts or the app owner's.
- Only X can connect, use or remove X's subscription, through an app X approved
  with the `agent-credentials` OAuth scope.

The feature is off unless the controller and dashboard run with
`ENGINEBOX_MEMBER_CREDENTIALS=1` (on the supervisor allowlist from this release;
an older supervisor needs it in `optional-services.env`). Off, every route below
answers 404 and the consent screen refuses the scope (`invalid_scope`).

## Identity: the `agent-credentials` scope

The app sends the person through [OAuth](OAUTH.md) with
`scope=agent-credentials` and, usually, `project=<id>`. Any member of the org may
approve it, because it acts only as them; an admin's approval is not needed and
does not let the app act as anyone else. The access token carries:

| Field | Value |
|---|---|
| `agent_credentials` | `[{project}]`, the ticked projects |
| `member` | the consenting user (`usr_…`); never `owner` or a service actor |
| `operations` | `agent_credentials.manage`, `agent_credentials.run` (no agent-run operation) |
| `project` | the requested project, when the app named one |

The member is taken from the token, never from a header; the public proxy
forwards no actor or org header. The owner token is refused on these routes
(403): owners manage credentials on `/v1/credentials`. When the member leaves the
org, or the Engine Box turns the feature off, the next refresh answers
`invalid_grant`.

`enginebox_grants` lists `{kind: "agent_credentials", project, member}`.

## API (acting as the token's member)

Credentials belong to the member in the token's org; `{project}` names the grant.
Every body is JSON; `{}` where none is needed. IDs are 32 lowercase hex characters.

| Route | Body | Answer |
|---|---|---|
| `GET /v1/projects/{p}/me/credentials` | | `200 {credentials: [Credential]}`: the member's own `claude` and `codex` personal credentials only |
| `POST /v1/projects/{p}/me/credentials` | `{provider: "claude"\|"codex", label?}` | `201 {credential}` when created, `200` with the existing one (one per provider per member per org) |
| `POST /v1/projects/{p}/me/credentials/{id}/login` | `{}` | `202 {session}`: the provider's native sign-in into that credential's private home |
| `GET /v1/projects/{p}/me/credentials/{id}/sessions/{s}` | | `200 {session}` |
| `POST /v1/projects/{p}/me/credentials/{id}/sessions/{s}/input` | `{text}` | `200 {session}`: Claude's pasted authorization code |
| `POST /v1/projects/{p}/me/credentials/{id}/sessions/{s}/cancel` | `{}` | `200 {session}` |
| `POST /v1/projects/{p}/me/credentials/{id}/delete` | `{}` | `200 {deleted: true}`: the record, its home (the provider's tokens) and any sign-in in progress |

`Credential` is `{id, provider, label, status: "pending"|"connected"|"failed",
created_at, updated_at, connected_at}`; no owner, org, sharing or secret field.

`session` is `{id, credential_id, provider, status:
"starting"|"waiting"|"succeeded"|"failed"|"cancelled"|"expired", input_required,
message, expires_at, authorization_url?, user_code?}`. `authorization_url` is the
allowlisted provider page (`auth::authorization_url`); `user_code` is Codex's
device code. Sign-in sessions expire after 10 minutes.

An app reaches only credentials it connected: those with the member marker, for
`claude` or `codex`. Connecting never adopts a personal credential the member
made on the dashboard; it creates a separate one. Another member's credential,
the member's own dashboard credential, a team account, a key or an unknown ID
are all the same `404 {"error":"Unknown credential."}`, so a member cannot probe
what exists.

**Limits.**
- Credentials: 16 per member per org, 512 per org, 4096 in all (`409`).
- Sign-ins in progress: 2 per member, 16 per org, 64 in all (`429`).

**Code input errors** (`…/sessions/{s}/input`):
- `400`: a body that is not `{text}`, or a malformed code;
- `404`: an unknown sign-in;
- `409`: the sign-in is not waiting for a code;
- `410`: the sign-in has ended ("start a new sign-in").

**Dashboard.** The dashboard's credential listing carries `is_member_connected`.
For those credentials, Share is hidden in the panel and refused by the API.

**Refresh when off.** If member subscriptions are turned off, a grant's
engine-session part keeps working at refresh; a subscription-only grant ends.
`share_with_org_pools`, `key` and `owner_user_id` are refused (400), and a
credential connected through an app can never be shared later, even from the
dashboard (its marker lives in `agent-credentials/member/<id>.json`).

Every change is audited as `credential.member_connect|login|login_input|login_cancel|delete`
with the member as actor and only the provider and status as detail: never a
code, token or link. A connect that returns the member's existing credential is
recorded with outcome `existing`. The member marker is removed last on delete; if
that fails, the error is logged and the orphan is swept when the store opens.

## Runs (acting as the token's member)

| Route | Body | Answer |
|---|---|---|
| `POST /v1/projects/{p}/me/agent-runs` | `{provider, prompt, model?, timeout_seconds?, resume_id?, workdir?, env?, idempotency_key?, callback?, profile?, run_kind?, pool?, credential?}` | `202 {run}` (`run.member`, `run.credential_id`), `200` for a replayed key |
| `GET /v1/projects/{p}/me/agent-runs` | | `200 {runs}`: the member's own runs in the project, newest first |
| `GET /v1/projects/{p}/me/agent-runs/{id}` | | `200 {run}` |
| `POST /v1/projects/{p}/me/agent-runs/{id}/cancel` | `{}` | `202 {run}` |

The run view, `callback` and `idempotency_key` are those of
[pool runs](FLEET.md#agent-runs-v2). Idempotency keys belong to the member, so they
hold across the app's hourly token renewals. Any other field (`repository`,
`repo_profile`, …) is refused (400).

**Selection: own only, no fallback.**
- `credential` is `{"own": true}` (the default) or `{"id": "<one of your own>"}`.
- The controller takes the member's own connected personal credential for
  `provider` in the token's org (`Credentials::member_select`).
- If there is none, the answer is `409 {reason: "credential_required"}`, even
  when a team account, an API key, another member's shared credential or the
  controller's own accounts could serve the provider.
- Naming another person's credential gets the same 409.

**Placement.**
- The run goes to `pool`, or the project's first **member-runs pool**.
- The owner designates member-runs pools with
  `POST /v1/pools/{pool}/member-runs {enabled}`; the designation is kept in
  `agent-member-pools.json`, so pool records keep their format.
- With no member-runs pool in the project, the answer is
  `409 {reason: "no_member_pool"}`.
- Pool isolation:
  - Ordinary runs are refused on a member-runs pool.
  - Member runs are refused anywhere else.
  - A member-runs pool shares no workspace with another pool.
  - Its workspaces never receive the controller's own provider accounts:
    attach and reconcile skip them.

**Switching a pool.** `POST /v1/pools/{pool}/member-runs` is refused while
the pool has queued or running runs (drain it first). Enabling forgets the
pool's attachment receipts, so reconciliation never delivers the controller's
accounts there again.

**Preparation.** A member run never attaches the controller's accounts. The
workspace is cleared with `enginebox-guest clear-credentials` before its first
member run after the controller starts or the pool is designated, and again
whenever an attachment receipt reappears. Clearing removes:
- delegated provider files;
- delegation markers and setup tokens;
- Claude settings overrides (`ANTHROPIC_*`, `CLAUDE_CODE_*`, `AWS_*`,
  `GOOGLE_*`, `VERTEX_*`, `apiKeyHelper`);
- `primaryApiKey` in `~/.claude.json`.

The workspace's attachment receipt is dropped. A guest helper that predates
member runs fails with `member_runs_unsupported`.

**Delivery: per run, short-lived, removed at the end.**
- **Admission check.** At admission the controller reads the run's own
  credential again (`Credentials::member_material`). The record must still
  exist, be connected, belong to the run's member and be for the provider.
- **Lifetime.** The access must stay valid for the run's whole `timeout_seconds`
  plus a minute, checked when it is read and again right before dispatch (after
  any host-lock wait, wake or preparation). A native client keeps the token it started with, so it cannot
  be re-keyed mid-run; a run whose timeout outlasts the access is refused with
  `credential_unavailable`.
- **Access material only, never a refresh token:**
  - Codex: an `auth.json` with `refresh_token: ""`.
  - Claude: the access token. Until the full-login change, a credential
    connected with `claude setup-token` delivers that token.
- **Transport.** The material goes to the guest on stdin inside a `run-member`
  envelope, never in argv. A detached run never spools it: the guest helper
  writes the rest of the request to its spool and passes the material to the
  worker through a pipe.
- **Guest checks.** The guest refuses the run if the CLI could authenticate any
  other way:
  - a delegated file;
  - `~/.claude.json` `primaryApiKey`;
  - an override in user, local, managed (`/etc/claude-code/managed-settings.json`)
    or project settings (`.claude/settings[.local].json` from the working
    directory up to the workspace root);
  - a Codex system or managed configuration (`/etc/codex/config.toml`,
    `/etc/codex/managed_config.toml`) or any other configuration naming another provider, an API key, a bearer token
    or a base URL. This covers the run's `CODEX_HOME/config.toml` and every
    project `.codex/config.toml` from the working directory up to the workspace
    root. The keys are `model_providers`, `model_provider`,
    `preferred_auth_method`, `forced_login_method`, `experimental_bearer_token`,
    `env_key`, `http_headers`, `env_http_headers`, `api_key` and `*base_url`;
    they are refused at the top level and in `profiles.*`, while MCP server
    tables (`mcp_servers.*`) keep their own headers, keys and URLs. A
    configuration that cannot be parsed is also refused.
- **Where the material lives in the run:**
  - Codex: only in the run's private `CODEX_HOME`.
  - Claude: only in the run's environment, with `CLAUDE_CONFIG_DIR` set to the
    member's own configuration directory.
- **Template setup.** Member runs keep the workspace template's client setup;
  copies are bounded and links are skipped.
  - Codex: `CODEX_HOME` gets `config.toml`, `AGENTS.md`, `prompts`, `skills`,
    `rules` and `plugins`.
  - Claude: the member's configuration directory gets `CLAUDE.md`,
    `settings.json` (with authentication overrides stripped), `skills`,
    `agents`, `commands`, `plugins`, `output-styles`, and from `~/.claude.json`
    the user MCP servers and onboarding flags.
  - Never copied: credentials, sessions, history or project data.
  - Copies keep execute bits (owner-only) and are refreshed only when the
    template changes (a digest of paths, sizes and times). Codex links each run
    to the member's own cached copy. `.claude.json` is read within 16 MiB.
- **Session history.** Each member's session history is kept apart:
  `~/.local/state/enginebox/members/<key>/` holds `codex-sessions` and the
  `claude` directory, where `<key>` is a digest of the member's ID.
- **Resume.** A session is resumed only by the member whose run started it.
  Ordinary runs never resume a member's session.
- **Redaction and cleanup.**
  - The values are scrubbed from output by the guest, and again by the
    controller before recording a result or a launch failure.
  - The run directory is removed on every exit path.
- **Idempotency.** A member run's idempotency digest includes the member and
  credential, so a key never replays a run on another credential. Run views
  name the run's own `member` and `credential_id`.
- **Designation re-check.** The pool's designation is read under the scheduler
  lock at acceptance and checked again at admission. A run on the wrong kind of
  pool fails with `pool_unavailable`.
- **Callers** still cannot set `ANTHROPIC_*`, `CLAUDE_*`, `OPENAI_*` or `CODEX_*`.
- **Stored state.** Queue entries, run records and details keep only the member
  and credential IDs. An older controller cannot read a member run's queue entry
  and sets it aside, so it never runs one on its own accounts.
- **Named workspaces.** Named-workspace runs (`/v1/hosts/…/agent-runs`) cannot
  target a member-runs pool's workspace.

## Not yet in this release

Per-member workspace isolation, Claude full login with central refresh,
encryption at rest, and usage reporting arrive in the following changes of this
series.
