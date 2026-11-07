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

## Isolation between members

Each workspace (Incus VM) of a member-runs pool serves **one member for its whole
life**:
- The first member run placed on a workspace binds it to that member
  (`agent-member-bindings.json`). From then on only that member's runs are placed
  there; other members' runs wait or get a new workspace from `autoscale`.
- A member run prefers the member's own workspace (their state is there) over
  an unbound one, whatever the VM states.
- The binding is made when the run is durably admitted; a failed admission
  rolls it back.
- **Release.** The binding ends only when the workspace is wiped:
  - Idle reclamation of a created member deletes the VM and unbinds it, so its
    successor starts fresh from the pool's credential-free template.
  - When member runs wait and every free workspace is bound to another member
    in a pool already at `max_workspaces`, the scheduler deletes the
    longest-idle bound created workspace, and autoscale creates a fresh one.
    So N workspaces never serve only N members forever.
  - The owner releases a workspace with `POST /v1/pools/{pool}/member-runs/release
    {host_id, name, recreated?}`. A created workspace is deleted, and autoscale
    makes a fresh one. A listed workspace is released only with
    `recreated: true`, the owner's statement that it was wiped or recreated.
    Releasing is refused while the workspace runs.
  - Turning member runs off for a pool, or deleting it, is refused while any of
    its workspaces is bound. Release them first.
- **Upgrade from before bindings.** At start, a member-runs workspace that
  served exactly one member becomes theirs. One that served several is
  `tainted`: nobody's until the owner releases it. Resuming a session there is
  refused with that reason.
- Therefore no two members ever share a UID, HOME, `/tmp`, work tree, session
  history or leftover process, at the same time or one after the other.
- A resume (`resume_id`) of another member's session is refused, because it
  would land on that member's workspace.
- A named-workspace run (`/v1/hosts/{h}/instances/{w}/agent-runs`) cannot target
  a member-runs or member-bound workspace.
- **Fleet hosts only.** A member-runs pool uses fleet hosts only. Refused are:
  - a registered contributor node, matched by node ID or by its managed host
    binding's `host_id` (a contributed machine whose owner has root);
  - a host assigned to an org other than the platform org (someone's own
    machines, such as a room owner's PC).

  The check runs under the scheduler lock when a pool is designated or
  replaced, and again for each member run at admission, so a host enrolled
  later is caught.
- A member run that waits because every workspace is someone else's says so:
  "all are in use by other members".

Capacity follows from this: a pool needs one workspace per active member. Use
`autoscale` with a sensible `max_workspaces`. Owner-listed workspaces stay bound
until released.

**Infrastructure the operator must provide** (not enforced by this code):
- **Egress allowlist.** The design asks that member workspaces reach only the
  provider APIs, package registries and Git remotes. Engine Box does not manage
  Incus network ACLs yet; apply an ACL to the member-runs pool's network.
- **A credential-free template.** Use a base template for member-runs pools, not
  a private checkpoint that contains provider credentials. The guest refuses
  member runs in a workspace that holds any, so a wrong template fails closed
  rather than falling back.

## Claude full login and central refresh

- **Sign-in.** A member's Claude subscription signs in with
  `claude auth login --claudeai`, the interactive login, not `setup-token`. It
  shows Anthropic's page and takes the pasted code like setup-token does. Page
  and callback are allowed only as pairs:
  - `claude.com/cai/oauth/authorize` with `platform.claude.com/oauth/code/callback`;
  - `claude.ai/oauth/authorize` with `console.anthropic.com/oauth/code/callback`,
    allowed only for the full login.
- **Staging.** A member's sign-in runs in a staging home of its own
  (`agent-credentials/signin/<id>`). Only when it completes is the result moved
  into the credential's home, under its lease. Runs keep reading the current
  login during a sign-in, and a sign-in never makes a run wait.
- **What is stored.** `.claude/.credentials.json` in the credential's own home,
  holding an access token that expires within hours and its refresh token.
- **Verification.** A sign-in is `unverified` (never stored) when:
  - the client wrote no new credential;
  - or it left no refreshable login behind.

  A completed full login removes any earlier setup token; if that fails, the
  sign-in fails.
- **Setup-token fallback.** `ENGINEBOX_MEMBER_CLAUDE_SETUP_TOKEN=1` (an operator
  fallback, on the supervisor allowlist) restores setup-token sign-in and lets
  runs receive that one-year token. Without it, a setup-token-only credential
  shows `status: "reconnect"` and runs fail with "connect it again".
- **Refresh.**
  - **Too long to cover.** A run whose timeout is longer than a fresh login
    lasts (Claude: 8 hours) is refused before any renewal.
  - **When it renews.** At each run's admission, if less than the run's timeout
    plus five minutes of access remain (at least 30 minutes), the controller
    renews the access centrally with the provider's own client (`auth::refresh`,
    shared with controller accounts):
    - Codex: `app-server account/read {refreshToken: true}`.
    - Claude: `auth login --claudeai` with the refresh token in its environment.
  - **Lease.** This happens under the credential's own lease, so one member's
    concurrent runs serialise their reads and at most one refresh runs per
    credential. The refresh token never leaves the home.
  - **Final check.** Coverage is re-checked with the current time after
    renewal.
- **Refused versus transient.**
  - Only a refused grant marks the credential `reconnect`: `invalid_grant`, a
    revoked, expired or reused refresh token, or a missing login (shared
    helpers `auth::record_refresh_rejected` and `auth::refresh_rejected`).
  - Network errors, rate limits, timeouts, a missing client or a busy lease fail
    only that run, with "try again shortly".
  - A new sign-in clears the marker.

**Qualification still needed.** The PTY flow of `claude auth login --claudeai`
(its page, prompt and credential file) is covered by a stand-in client in
`credentials::tests`. It still has to be qualified against the real Claude Code
release with a test account before the feature flag is turned on in production.

## Encryption at rest

Member subscriptions require a key-encryption key (KEK). Without one, connect
and sign-in answer 503.

**Key file.** `ENGINEBOX_CREDENTIALS_KEY_FILE` names a file readable by its
owner only (mode 0600), outside the state directory and outside the
controller's install tree (`<prefix>/releases/…`), both of which are backed up
and copied. It holds exactly 64 hex characters (one trailing newline allowed)
or 32 random bytes. A 32-character text key is refused. The variable is on the
supervisor allowlist.

**What is sealed.**
- A member credential's provider files are kept in `<home>/sealed.json` and
  never as plaintext at rest: `.claude/.credentials.json`, `.codex/auth.json`,
  and a fallback setup token.
- Each file is AES-256-GCM encrypted under a fresh per-credential data key,
  with the credential ID and file name as associated data. The data key is
  wrapped by the KEK, so a blob swapped between credentials fails to decrypt.
- Sealing replaces the set with exactly the files present, so a file the
  provider's client removed (an old setup token) is gone from it too.

**When plaintext exists.**
- **Sign-in:** the provider's own client writes plaintext in the staging home.
  - On completion it is moved into the credential's home and sealed at once.
  - Without a key, or if sealing fails, the plaintext is wiped and the sign-in
    fails (fail closed). The admin `/v1/credentials/{id}/login` path is refused
    for a member's credential without a key.
  - Whatever the outcome (failed, unverified, cancelled, expired), the staging
    home is removed. The refused-renewal marker is cleared only after the new
    login is sealed.
- **Run admission:**
  - The sealed files are decrypted in memory and the access material is read
    from there; nothing is written.
  - Only when a renewal is due are the files written for the provider's client,
    under the credential's lease, and resealed with a new data key right after.
    If resealing fails, the plaintext is wiped and the run fails.
- **Wrong or missing key:** the credential cannot be read. Runs fail with
  `credential_unavailable` and the reason "the controller cannot decrypt this
  subscription", and the controller log names the credential and the key
  setting. A failed decryption never reseals anything.
- **Crash recovery and migration:** when the store opens, an abandoned sign-in's
  staging home is removed, and plaintext left in a member's home is sealed.
  That plaintext comes from a crash between unsealing and resealing, or from a
  credential connected before encryption at rest. Without a key it is left in
  place, logged, and refused.

**Rotating the KEK** needs members to reconnect; a re-wrap tool is not part of
this change. Keep a backup of the key file separate from state backups: losing
it means every member reconnects.

**Scope.** Only members' own subscriptions are sealed. Controller accounts,
team accounts and API keys keep the existing 0600 files.

## Audit, revocation and usage

**Audit.** Every event is recorded in `audit/events.jsonl` with the member as
actor, the credential ID as target, and no value:
- `credential.member_connect|login|login_input|login_cancel|delete` (the member's
  own actions);
- `credential.member_used` (`detail.run_id`, the outcome);
- `credential.member_refreshed`;
- `credential.member_reconnect_needed`.

**Revocation.**
- When a member removes a subscription, every unfinished run that uses it is
  cancelled: queued runs never start and running ones are stopped. The reply
  lists them in `cancelled_runs`. An admin removing it on `/v1/credentials/{id}/delete`
  does the same.
- The credential's home (the tokens) is deleted at once.
- Tell the member to also revoke at the provider: Claude account → sessions, or
  ChatGPT → Security → sign out of all devices.
- A member who leaves the org loses the app's token at its next refresh.
  Their stored credential stays until they or an admin remove it.

**Usage.** Run callbacks (`agent_run.finished`) now also carry:
- `usage`: `{tokens_in, tokens_out, tokens_cached, scope}` as the
  native client reported it (`scope` is `run` for Claude, `session_total` for
  Codex);
- `elapsed_seconds`;
- for member runs, `member` and `credential_id`.

## Not implemented (open)

- **Continuation across expiry (E7).** Admission guarantees at least 30 minutes
  of access, and longer when the provider grants it (Claude's full-login access
  token lasts hours). A run that outlives its access token fails on the
  provider's 401; Engine Box does not yet restart it with `resume_id` and fresh
  material.
- **Plan-limit pause (E7).** A provider quota rejection ends the run as
  `failed`; there is no `paused {reason: "plan_limit", resets_at}` yet.
- **"My computer" (E10).** Sign-in on, and runs restricted to, a member's own
  enrolled node are not implemented. Member-runs pools refuse personal nodes
  altogether.
- **Machine time** (`machine_seconds`, `machine_class`) is not in callbacks;
  `elapsed_seconds` is wall time of the run.
- **Account hints.** `account{plan, email_hint}` is not in the session JSON.

## Prompt-injection exfiltration

A prompt-injected agent runs with the member's access token in its own process
environment or `CODEX_HOME`, so it can read that token.

What limits the damage:
- The token is short-lived (Claude full login: hours; Codex access token),
  only the member's own, and never a refresh token.
- The token is scrubbed from returned output (best effort; an agent can encode
  it).
- The agent's workspace is the member's own VM. It cannot reach other members'
  workspaces or the controller's homes, and nothing it leaves behind meets
  another member.
- No long-lived token is ever delivered, unless the operator turns on the
  setup-token fallback.

What remains open:
- No egress allowlist is enforced by Engine Box (operator setup).
- A stolen access token works from anywhere until it expires.
- Engine Box cannot revoke a single access token at the provider; disconnecting
  stops future runs only.
