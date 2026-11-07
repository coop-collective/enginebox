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

**Delivery: per run, short-lived, removed at the end.**
- At admission the controller reads the run's own credential again
  (`Credentials::member_material`). The record must still exist, be connected,
  belong to the run's member and be for the provider.
- It sends access material only, never a refresh token:
  - Codex: an `auth.json` with `refresh_token: ""`.
  - Claude: the access token. Until the full-login change, a credential
    connected with `claude setup-token` delivers that token.
- The material goes to the guest on stdin inside a `run-member` envelope (never
  argv). A detached run's spooled input is deleted as soon as its worker reads it.
- The guest:
  1. refuses the run if the workspace holds any other provider credential
     (`~/.codex/auth.json`, `~/.claude/.credentials.json`, a setup token, a
     delegation marker, or `~/.claude/settings.json` with an `ANTHROPIC_*` or
     `CLAUDE_CODE_*` env override or `apiKeyHelper`), so the native CLI can
     authenticate only as the member;
  2. writes Codex's file only to the run's private `CODEX_HOME`
     (`~/.local/state/enginebox/agent-runs/profiles/<run>/codex-home`), or sets
     `CLAUDE_CODE_OAUTH_TOKEN` only in the run's process environment;
  3. scrubs the values from returned output;
  4. removes the run directory when the run ends.
- Callers still cannot set `ANTHROPIC_*`, `CLAUDE_*`, `OPENAI_*` or `CODEX_*`.
- Queue entries, run records and details keep only the member and credential
  IDs. An older controller cannot read a member run's queue entry and sets it
  aside, so it never runs one on its own accounts.
- A run whose credential is gone, disconnected or expired fails at admission
  with launch reason `credential_unavailable`. A workspace whose guest helper
  predates member runs fails with `member_runs_unsupported`.

## Not yet in this release

Per-member workspace isolation, Claude full login with central refresh,
encryption at rest, and usage reporting arrive in the following changes of this
series.
