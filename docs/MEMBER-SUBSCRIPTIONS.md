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

Another member's credential, a team account, a key or an unknown ID is the same
`404 {"error":"Unknown credential."}`, so a member cannot probe what exists.
`share_with_org_pools`, `key` and `owner_user_id` are refused (400), and a
credential connected through an app can never be shared later, even from the
dashboard (its marker lives in `agent-credentials/member/<id>.json`).

Every change is audited as `credential.member_connect|login|login_input|login_cancel|delete`
with the member as actor and only the provider and status as detail: never a
code, token or link.

## Not yet in this release

Run selection, per-run delivery, member-isolated placement, Claude full login,
encryption at rest and usage reporting arrive in the following changes of this
series; until then, connecting a subscription does not make any run use it.
