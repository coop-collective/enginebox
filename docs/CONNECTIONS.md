# Connections catalog

A **connection** is a named, reusable record in an org's catalog: an MCP server, or a source of secret values. [Profiles](PROFILES.md) reference connections by name (`"connections": ["kitbash", "d1-readonly-canvalytics"]`), so one connection is set up once and shared by any number of profiles; editing it changes every profile that uses it from the next run on.

Connections and profiles are runtime catalog state, owned by an org and changed through the API, the fleet MCP tools or the dashboard. They are not part of `fleet.toml` or the supervisor configuration, which stay infrastructure-only (hosts, capacity, storage). Teams that want profiles reviewed in pull requests keep a [catalog file](#profiles-as-code) in a repository and apply it.

## Kinds

| Kind | Fields | At run time |
|---|---|---|
| `mcp_http` | `url` (HTTPS, or HTTP on localhost; no credentials or fragment), `headers` (header name → [value reference](#value-references)) | an HTTP MCP server named after the connection in the provider's generated config (Claude `mcp.json`, Codex `config.toml`); header values travel in the run's env |
| `mcp_stdio` | `command` (an absolute path under `/usr/bin/`, `/usr/local/bin/` or `/opt/`: a binary the template installs, never a package runner that downloads code), `args`, `env` (variable → value reference), optional `template` | a stdio MCP server in the generated config; its variables are passed to it. With `template`, a profile using the connection must name the same template |
| `secret_source` | `source`: `{provider, project?, environment?, integration?}`; `names` for Kitbash | every value it yields is set in the run's env under its own name |
| `agent_account` | reserved | refused until the credential-pool work ([ORGS.md](ORGS.md#agent-credentials)) uses it |

`secret_source` providers:

- **`stored`**: values written once through this API (`secrets` on create or update). Yields exactly its stored names.
- **`kitbash`**: values Kitbash delivers to the controller for a project and environment. See [Kitbash](#kitbash).

Cloudflare Secrets Store values cannot be read back through Cloudflare's API, so a Cloudflare-backed source is a Kitbash delivery: Kitbash holds the values in Secrets Store and seals a copy to the controller.

Names match `^[a-z][a-z0-9-]{0,62}$`, are unique within an org, may not start with `enginebox-`, and never change (create a new connection instead). The kind never changes either.

### Value references

A header or variable value is never written in the record. It is one of:

| Reference | Meaning |
|---|---|
| `{"env": "HQ_AUTHORIZATION"}` | the run's own `env` supplies it at start (for per-run tokens, as Ideator passes its run token) |
| `{"secret": "KITBASH_TOKEN"}` | this connection's stored write-only value |
| `{"secret_source": "kitbash-staging", "name": "CLOUDFLARE_API_TOKEN"}` | a value another `secret_source` connection of the same org yields |

`prefix` (up to 32 printable characters, for example `"Bearer "`) is prepended to a secret value. Secret names and `env` names follow the [per-run environment](FLEET.md#per-run-environment) rules and denylist, because they become variable names.

## Secrets are write-only

- Values are accepted only in `secrets` (`{"NAME": "value"}`) on `POST /v1/connections` and `POST /v1/connections/{name}`. `remove_secrets: ["NAME"]` drops one.
- They are stored per connection in `connections/secrets/<id>.json` (mode 0600 in the owner-only controller state), apart from the record.
- No API, MCP tool or dashboard view returns them. Reads show `secret_names`. Responses to a write list the names set or removed.
- They are never logged, never in an audit entry, never in an error message. Refusals that come from a JSON shape error return a fixed message, because a deserializer can quote its input.
- A stored secret must be referenced by one of the connection's headers or variables (or, for a stored `secret_source`, it is what the source yields).
- At run time values go only into the run's env, which reaches the native CLI on the guest helper's stdin (never argv, never the prompt), and are scrubbed from returned output like every run env value ([FLEET.md](FLEET.md#per-run-environment)).

Records carry `created_by`, `created_at`, `updated_by`, `updated_at`, `revision` and `last_used_at` (set when a run resolves the connection, at most once a minute).

## API

Owner token only. Service tokens, including OAuth access tokens, get 403 on every route here. The dashboard holds the owner token and forwards writes only for org owners and admins, naming the org in `X-EngineBox-Org` and the member in `X-EngineBox-Actor` (recorded as `created_by`/`updated_by` and in the audit log; without it the actor is `owner`).

| Route | Purpose |
|---|---|
| `GET /v1/connections` | the org's connections with `secret_names` and `used_by` (profiles) |
| `GET /v1/connections/{name}` | one connection |
| `POST /v1/connections` | create: `{name, kind, description?, …kind fields, secrets?}` → 201 |
| `POST /v1/connections/{name}` | change fields (only those sent) and secrets; bumps `revision` when something changed |
| `POST /v1/connections/{name}/delete` | `{}`; refused while a profile uses it |
| `POST /v1/connections/{name}/test` | `{}`; see below |
| `POST /v1/profiles/{name}/connections/attach` | `{connection, run_kinds?}` |
| `POST /v1/profiles/{name}/connections/detach` | `{connection}` |
| `POST /v1/profiles/{name}/connections/policy` | `{connection_policy}`: replace the run-kind policy (env values and the callback key are kept) |
| `POST /v1/catalog/apply` | `{document, dry_run?, prune?}`; see [profiles as code](#profiles-as-code) |
| `GET /v1/audit` | the org's newest audit entries (at most 500) |

Refusals answer 400 (404 unknown, 409 name taken or concurrent change) with `{"error": "…"}` naming the rule. A profile save (`POST /v1/profiles`) that names an unknown connection is refused the same way.

**Test** resolves every reference (reporting `unresolved` header or variable names and `needs_run_env` for env references) and then:

- `mcp_http`: sends one MCP `initialize` POST (no redirects, 10 s) with the resolved headers and reports `reachable`, `http_status` and `auth` (`accepted` for 2xx with every header resolved, `rejected` for 401/403, else `unknown`). It never returns the response body. For orgs other than the platform org the host must resolve only to public addresses.
- `secret_source`: reports which yielded names are `present` and `missing`.
- `mcp_stdio`: values only; the command is checked when a run starts in the workspace.

## Run-time resolution

When a run names a profile, the controller resolves the profile's connections in the profile's own org and merges them into that provider's block before anything starts:

1. The [policy](#policy-per-kind-of-run) picks which connections apply to the run's `run_kind`.
2. Each MCP connection becomes an entry in the provider's `mcp_servers`, named after the connection. Header values that come from secrets are carried by a generated variable (`CONN_<NAME>_<HEADER>`, uppercased, `-` as `_`; a digest-based name when that exceeds 64 characters) that the header maps to with `headers_from_env`. Env references map the header straight to the run's variable.
3. Each `secret_source` sets its values under their own names.
4. The results join the profile's `env`. A connection may not set a variable the profile's `env` already sets, two connections may not set the same variable, and an MCP connection may not share a name with one of the profile's inline `mcp_servers`. The merged set stays within the per-run bounds and denylist, and the run's own `env` still wins on a name clash, as before.
5. Cursor has no MCP support in profiles: MCP connections are skipped for Cursor runs (recorded as `"skipped": "provider_has_no_mcp"`); secrets still apply.

Anything that cannot be resolved (unknown connection, missing stored secret, missing delivered value, a clash) refuses the start with HTTP 409 and a message naming the connection and the rule. A queued run that a restart re-resolves at admission fails with `{"stage":"resolve","reason":"profile_unavailable"}` instead.

Resolution happens at acceptance and again at admission after a controller restart, so a run uses the connection revisions current when it starts. The run's details record `profile.run_kind` and `profile.connections` (`{name, revision, kind}`, plus `names` for secret sources), never a value. Resolved values exist in controller memory and the guest's stdin only; the run queue file holds only the run's own env, and drops it at admission.

## Policy per kind of run

A start request may state `run_kind` ([FLEET.md](FLEET.md#agent-runs-v2)); Ideator passes its node kind. The profile's `connection_policy` maps run kinds to the subset of its `connections` that apply:

```json
{
  "connections": ["kitbash", "staging-secrets", "cf-readonly", "db-readonly", "deploy-token"],
  "connection_policy": {
    "planner": [],
    "builder": ["staging-secrets"],
    "debug": ["cf-readonly", "db-readonly"],
    "deploy": ["deploy-token"],
    "*": ["kitbash"]
  }
}
```

- A listed kind gets exactly its list (`planner: []` gets nothing).
- A kind not listed gets the `*` entry when there is one, else every connection.
- A run without `run_kind` gets every connection (the default).
- Lists may name only the profile's own connections.

`attach` without `run_kinds` adds the connection to `connections` (and to `*` when that entry exists); explicit kinds do not gain it. `attach` with `run_kinds` adds it to those kinds' lists, and first creates a `*` entry holding the profile's other connections if there is none, so unlisted kinds do not gain it. `detach` removes it from `connections` and every list. For anything else, replace the profile.

## MCP tools

The fleet MCP server (`enginebox-fleet-mcp`, configured with the owner token file; set `ENGINEBOX_ORG` to act for another org) adds:

| Tool | Route |
|---|---|
| `connection_list` | `GET /v1/connections` |
| `connection_create` | `POST /v1/connections` |
| `connection_update` | `POST /v1/connections/{name}` |
| `connection_delete` | `POST /v1/connections/{name}/delete` |
| `connection_test` | `POST /v1/connections/{name}/test` |
| `profile_get` | `GET /v1/profiles/{name}` |
| `profile_attach_connection` | `POST /v1/profiles/{name}/connections/attach` |
| `profile_detach_connection` | `POST /v1/profiles/{name}/connections/detach` |
| `catalog_apply` | `POST /v1/catalog/apply` |

They need the owner token: an OAuth access token or service token gets 403 from the controller. An agent can kick off installing an MCP server for a team ("connect Kitbash to the coder profile") with `connection_create` and `profile_attach_connection`. It should give a secret as a reference to a `secret_source` (a Kitbash delivery) instead of pasting the value, because a pasted value passes through the agent's context even though Engine Box never echoes it.

## Audit

Every catalog write appends one line to `audit/events.jsonl` (0600; rotated once to `events.1.jsonl` past 4 MiB): `at`, `org_id`, `actor`, `action` (`connection.create`, `connection.update`, `connection.delete`, `connection.test`, `profile.attach_connection`, `profile.detach_connection`, `catalog.apply`, `catalog.seed`), `target`, `outcome` (`applied` or `refused`) and `detail` (secret names set or removed, the refusal reason, the number of applied changes). Never a value. `GET /v1/audit` returns the org's newest entries.

## Profiles as code

A catalog file declares connections and profiles for one org:

```json
{
  "schema": "enginebox.catalog/v1",
  "connections": [
    {"name": "hq", "kind": "mcp_http", "url": "https://ideator.example.com/mcp",
     "headers": {"Authorization": {"env": "HQ_AUTHORIZATION"},
                 "X-HQ-Machine": {"env": "HQ_MACHINE"},
                 "X-HQ-Org": {"env": "HQ_ORG"}}},
    {"name": "kitbash-staging", "kind": "secret_source",
     "source": {"provider": "kitbash", "integration": "ideator-staging", "project": "ideator", "environment": "staging"},
     "names": ["CLOUDFLARE_API_TOKEN"]}
  ],
  "profiles": [
    {"$comment": "Keys starting with $ are notes.",
     "name": "ideator-agent", "description": "Ideator agent runs.", "template": "native-dev",
     "providers": {
       "codex": {"permission_profile": "edit", "network_access": true,
                 "skills": [{"name": "hq-loop", "files": [{"path": "SKILL.md", "sha256": "…"}]}],
                 "env": {"GIT_AUTHOR_NAME": "engine-box-runners[bot]"}},
       "claude": {"max_turns": 300, "allowed_tools": ["Read", "Write", "Edit", "Bash", "Glob", "Grep", "mcp__hq__*"],
                  "skills": [{"name": "hq-loop", "files": [{"path": "SKILL.md", "sha256": "…"}]}]}},
     "connections": ["hq", "kitbash-staging"],
     "connection_policy": {"planner": ["hq"]}}
  ]
}
```

- **Connections** take the declarative fields only (`name`, `kind`, `description`, `url`, `headers`, `command`, `args`, `env`, `template`, `source`, `names`). `secrets` is refused: a file holds references, never values. Values are set once with `connection_update` or delivered by Kitbash.
- **Profiles** take `name`, `description`, `template`, `providers` (the [profile](PROFILES.md#provider-block) provider blocks; skill and plugin files are referenced by digest and must be uploaded first with `POST /v1/profile-files`), `connections` and `connection_policy`. A profile's callback key is not part of the file; apply keeps an existing one.
- **Applying** (`POST /v1/catalog/apply {document, dry_run, prune}`, the `catalog_apply` MCP tool, or `ebox catalog apply FILE [--dry-run] [--prune]`) is idempotent. It validates the whole document first, then creates or updates what differs and reports each record as `create`, `update`, `unchanged` or `delete`. An unchanged profile keeps its revision. `dry_run` returns that diff without writing. Records the file does not mention are left alone unless `prune` is set; prune refuses to delete a connection that a remaining profile still uses. A failure after the first write (a concurrent edit, for example) stops the apply; running it again converges.
- **Self-hosted seed**: set `ENGINEBOX_CATALOG_SEED=/etc/enginebox/catalog.json` (an absolute path to a regular file of at most 1 MiB; accepted in the supervisor's `environment.json`) and the controller applies it to the platform (Default) org on every activation. `ENGINEBOX_CATALOG_SEED_PRUNE=1` also prunes. A seed that fails to apply is reported on stderr and in the audit log (`catalog.seed`) and does not stop activation.

Ideator's `packages/plugin/enginebox/ideator-agent.profile.json` maps onto this format directly. Its publish script already turns skill names into uploaded, digest-referenced bundles; it can emit `{"schema": "enginebox.catalog/v1", "profiles": [<that profile>]}` and call `catalog_apply` instead of `POST /v1/profiles`. Moving its inline `hq` MCP server into a shared `hq` connection (as above) is optional: inline `mcp_servers` keep working.

## Kitbash

Kitbash is the first real connection pair: its MCP server, and the values it delivers to the controller. Secrets live in Cloudflare Secrets Store, managed through Kitbash, and in GitHub Actions secrets. Infisical is retired.

### The Kitbash MCP connection

Kitbash's broker serves a remote MCP endpoint at `https://<broker hostname>/mcp` with a bearer key (Kitbash [BROKER-SETUP.md](https://github.com/coop-collective/kitbash/blob/main/docs/BROKER-SETUP.md#mcp-connection-and-repeat-setup); Coop Collective's broker is `https://kitbash.coopcollective.dev/mcp`). It has no OAuth server, so the connection carries the key as a header:

```json
{"name": "kitbash", "kind": "mcp_http", "url": "https://kitbash.coopcollective.dev/mcp",
 "headers": {"Authorization": {"secret": "KITBASH_MCP_TOKEN", "prefix": "Bearer "}}}
```

Set the key once with `connection_update {"name": "kitbash", "secrets": {"KITBASH_MCP_TOKEN": "…"}}` from the operator's private `.kitbash/mcp.json`, or deliver it with Kitbash (below) and reference it with `{"secret_source": "kitbash-ops", "name": "KITBASH_MCP_TOKEN", "prefix": "Bearer "}`. Kitbash rotates the key on each setup run; update the connection when that happens. Profiles then attach `kitbash` and allow `mcp__kitbash__*`.

### Kitbash secret sources

A `secret_source` with `"provider": "kitbash"` reads what Kitbash's controller receiver staged on the controller host. The operator sets the delivery root once: `ENGINEBOX_KITBASH_ROOT=/opt/enginebox-control/kitbash` (absolute; in the supervisor's `environment.json`). A connection names an **integration** inside that root, never a path:

```json
{"name": "canvalytics-staging", "kind": "secret_source",
 "source": {"provider": "kitbash", "integration": "canvalytics-staging", "project": "canvalytics", "environment": "staging"},
 "names": ["CLOUDFLARE_API_TOKEN", "DATABASE_URL"]}
```

The receiver's layout (Kitbash `packages/secrets-broker/scripts/controller-receiver.mjs`), generalised:

```text
<root>/<integration>/config.json     {integration, project, target, kind, names, outputDirectory, enginebox_org?}
<root>/<integration>/receipt.json    {deliveryId, integration, status: "staged", directory, names}
<root>/<integration>/credentials/<deliveryId>/<file>    one 0600 file per value
```

When a run resolves the connection, the controller:

1. checks that `config.json` names this integration, that `project` and `target` match the connection's `project` and `environment` (when set), and that the delivery belongs to the connection's org (`enginebox_org`; absent means the platform org);
2. requires `receipt.json` to be `staged` and its `directory` to be exactly `<root>/<integration>/credentials/<deliveryId>`, whatever else it says;
3. reads each listed name from a regular, non-symlink, owner-only file of at most 16 KiB. For the GitHub App kinds (`runners`, `repos`) the files are `<kind>-client-secret`, `<kind>-private-key` and `<kind>-webhook-secret`, mapped to names by `config.names`. For the generic `env` kind each file is named after its value (`CLOUDFLARE_API_TOKEN`).

Values are read at resolution and never copied into controller state. A missing delivery, file or value refuses the run with the integration's name. `connection_test` reports which names are present.

### What Kitbash needs to deliver per project

For each project and environment whose secrets runs should get (for example `canvalytics` / `staging`):

1. Register the values in Kitbash for that project and target (`kitbash_register_secrets`), stored in Cloudflare Secrets Store. Use `generate: true` for application keys Kitbash can generate, and `generate: false` for provider-issued ones such as a Cloudflare API token.
2. *(Proposed Kitbash change; the flags below do not exist yet.)* Enroll an **`env`-kind** controller delivery named `<project>-<environment>` on the controller host (`pnpm run setup:controller --kind env --integration canvalytics-staging --project canvalytics --target staging --directory /opt/enginebox-control/kitbash`, plus `--enginebox-org org_…` for a tenant org). The generic `env` kind is the generalisation this design needs from Kitbash: the existing receiver supports only the GitHub App kinds. It writes one file per registered name, with no GitHub webhook step, and `config.json` lists the delivered names.
3. Upload or deliver the values (`kitbash_upload_secrets` with `delivery: "<integration>"`). The receiver timer stages them under `credentials/<deliveryId>/`.
4. Create the `secret_source` connection (or put it in the catalog file) and attach it to profiles, limited by run kind (`debug` gets read-only tokens, `deploy` gets the deploy token).

Rotation is a new delivery: the receiver refuses a different delivery ID without an explicit migration, so rotating means re-enrolling and then pointing the connection's `integration` at the new directory.

## GitHub Actions secrets

`POST /v1/actions-secrets` (MCP `github_actions_secret_set`) creates or updates an Actions secret on a repository selected in the dashboard's GitHub panel, or on one of its deployment environments:

```json
{"repository": "coop-collective/canvalytics", "environment": "production", "name": "CLOUDFLARE_API_TOKEN",
 "from": {"connection": "canvalytics-production", "name": "CLOUDFLARE_API_TOKEN"}}
```

- **Gated.** Owner token and platform org only (the GitHub App is the platform org's). Service and OAuth tokens get 403. The dashboard limits it to platform-org owners.
- **Values** come from a `secret_source` connection (`from`, preferred: the value never passes through the caller) or from a write-only `value` in the body. They are never echoed or logged.
- **Encryption.** The controller mints an installation token limited to that repository and `secrets: write`, fetches the repository's (or environment's) Actions public key, encrypts the value as a libsodium sealed box (X25519 + XSalsa20-Poly1305, the `crypto_box` crate), and PUTs `{encrypted_value, key_id}`.
- **Audit.** `github.actions_secret_set` with target `owner/repo[:environment]/NAME` and the source (`request` or `connection:<name>`).
- **App permission.** The repository GitHub App needs **Secrets: read and write** (repository permission). Add it in the App settings and approve it on the installation ([GITHUB-SECRETS.md](GITHUB-SECRETS.md#1-configure-one-app-and-installation)). Without it the token request fails and the call reports that the permission is missing.

## Migration and rollback

- **Existing profiles are unchanged.** `connections` and `connection_policy` are written only when non-empty, so every stored profile keeps its exact bytes and resolves exactly as before. Inline `mcp_servers` and `env` keep working alongside connections.
- **Runs without `run_kind`** behave as before. The queue entry gains `run_kind` only when one is set.
- **New state**: `connections/` (records and `secrets/`) and `audit/` under the controller state directory, created on activation.
- **Rollback**: a release before connections ignores `connections/` and `audit/`. It refuses profiles that carry `connections` or `connection_policy` (unknown fields) and queue entries with `run_kind`: detach connections from profiles and let runs with a kind finish before rolling back.

## Deliberately later

- OAuth for `mcp_http` connections (today: headers from secrets or the run's env).
- `agent_account` connections (the credential-pool work).
- A per-template manifest allowlist of stdio MCP binaries (today: fixed path prefixes plus the optional `template` binding).
- Secret rotation and versioned values; per-connection quotas.
- Reading Kitbash values through the broker's API instead of a staged delivery.
