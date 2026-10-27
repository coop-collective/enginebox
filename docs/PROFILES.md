# Workspace profiles

A profile is a named, owner-managed description of how a native CLI runs in a workspace: how many turns it may take, which permission profile it uses, which tools it may call without asking, which plugins, skills and MCP servers it gets, which setting sources it loads, default environment variables, and optionally which template the workspace must come from. Different roles get different profiles: a coding agent and a black-box verifier can share a fleet and a credential while running with different tooling, permissions and base images.

A run names a profile with `"profile":"<name>"` on `POST /v1/hosts/{host}/instances/{name}/agent-runs` (see [FLEET.md](FLEET.md#agent-runs-v2)). A run without a profile behaves exactly as before.

A [pool](QUEUE.md#pools) may name a profile: every run started on `POST /v1/pools/{pool}/agent-runs` then uses it (a run may omit `profile` or repeat the same name; another name is refused), and a service token granted the pool may use that profile without a separate `profiles` grant. A pool's `template`, when the profile also names one, must be the same. The profile is resolved again at admission if the controller restarted while the run was queued, so a queued run uses the profile's revision current when it starts; a profile deleted meanwhile fails the run without starting it.

## Model

- **Owner-managed.** Only the owner token can create, replace, read or delete profiles and upload profile files. Every record carries `"owner":"owner"`; when tenants exist this field becomes the tenant, as an additive change.
- **Revisioned.** Each `POST /v1/profiles` with an existing name replaces it and increments `revision`. A run records the `{name, revision}` it started with.
- **Per provider.** `providers` maps `claude`, `codex` and/or `cursor` to a provider block. A run must use a provider the profile configures.
- **No flags.** A profile never names a CLI option. It picks `permission_profile` from a fixed set and supplies data; Engine Box translates that data into each CLI's documented options. Sandbox and permission bypasses (`--dangerously-*`, `--yolo`, `--force`, `bypassPermissions`, `danger-full-access`, `--approve-mcps`) cannot be produced by any profile, and the launcher refuses any argv that contains one.
- **Content-addressed files.** Plugins and skills are lists of files, each `{path, sha256, executable?}`. Files are uploaded first (`POST /v1/profile-files`, at most 1 MiB each) and stored under their SHA-256; a profile can only reference stored digests. The controller re-verifies each file's digest when it resolves a run and the guest verifies it again before writing. Nothing is fetched from a URL at run time.
- **Private per run.** Before the CLI starts, the guest writes the profile's files and configuration into `~/.local/state/enginebox/agent-runs/profiles/<run_id>/` (mode 0700, files 0600, `executable` files 0700) and nowhere else, points the CLI at it, and removes the directory before releasing the run's writer lease. Runs never share a profile directory, so nothing carries over between runs or between profiles.
- **Repositories alongside profiles.** A run may also name a [hosted repository](REPOSITORIES.md#runs-with-a-repository). Its git config is written the same way, into `~/.local/state/enginebox/agent-runs/repositories/<run_id>/gitconfig` (0700/0600), and removed with the run; with a profile, the guest command is `run-repository` and the profile travels inside that envelope. Profile and repository env may not set `GIT_CONFIG*` or `GIT_ASKPASS` on a repository run.
- **Secrets stay write-only.** `env` values and `callback_secret` are never returned; reads show `env_keys` and `has_callback_secret`.

## Provider block

| Field | Meaning | Claude Code | Codex | Cursor |
|---|---|---|---|---|
| `max_turns` | 1–500 | `--max-turns` (default 20, as before) | refused: bound the run with `timeout_seconds` | refused |
| `permission_profile` | `edit` (default) or `read_only` | `edit`: `--permission-mode acceptEdits`; `read_only`: `--permission-mode dontAsk` plus `--disallowedTools Write,Edit,MultiEdit,NotebookEdit,Bash` | `-c sandbox_mode="workspace-write"` or `"read-only"`, `approval_policy="never"` | `edit`: `--sandbox enabled`; `read_only`: adds `--mode ask` |
| `allowed_tools` | 1–64 tool names such as `Read`, `Bash(npm test:*)`, `mcp__docs__*` | `--allowedTools` (default `Read,Write,Edit,Bash,mcp__enginebox-browser__*` for `edit`, `Read,Glob,Grep` for `read_only`). `read_only` refuses Write, Edit, MultiEdit, NotebookEdit and Bash. | refused | refused |
| `plugins` | bundles `{name, files}` | one `--plugin-dir` per plugin | refused (Codex plugins stay disabled) | one `--plugin-dir` per plugin |
| `skills` | bundles `{name, files}`, each with `SKILL.md` at its root | wrapped in one generated plugin, loaded with `--plugin-dir` | copied to `CODEX_HOME/skills/<name>/`; bundled system skills off | refused |
| `mcp_servers` | `{name, command, args, env_vars}` (stdio) or `{name, url, headers_from_env}` (HTTP) | a generated `mcp.json` passed with `--mcp-config` | `[mcp_servers.<name>]` in the generated `config.toml`, tools approved | refused |
| `strict_mcp` | use only the profile's MCP servers | `--strict-mcp-config` | the workspace browser server is left out of the generated config | refused |
| `network_access` | let sandboxed commands reach the network (git clone/push to the run's repository, package installs); edit profile only, off by default | refused | `[sandbox_workspace_write] network_access = true` | refused |
| `setting_sources` | distinct values from `user`, `project`, `local` | `--setting-sources` | refused | refused |
| `env` | default variables (same names, bounds and denylist as a run's `env`) | merged under the run's `env` | same | same |

MCP rules: names match `^[A-Za-z0-9_-]{1,64}$` and may not start with `enginebox-`; `url` must be HTTPS (plain HTTP only for `localhost`/`127.0.0.1`), without credentials or fragment; header and variable *values* always come from the run's environment (`headers_from_env` maps a header to a variable name; `env_vars` lists names passed to a stdio server), never from the profile.

Codex profile runs get a generated `CODEX_HOME` inside the run directory: a `config.toml` with `approval_policy = "never"`, the sandbox, every feature that reaches past the sandbox or the MCP boundary disabled (apps, browser and computer use, plugins, remote plugins, skill MCP dependency install, tool suggestions), `[skills.bundled] enabled = false`, and the profile's MCP servers. The workspace's delegated `auth.json` is copied in for the run and removed with it; `sessions` links to `~/.codex/sessions` when that directory exists, so `resume` keeps working.

Claude profiles that leave `user` out of `setting_sources` would lose the delegated access token, which lives in user settings; Engine Box passes it to that run itself as `CLAUDE_CODE_OAUTH_TOKEN`, and scrubs it from returned output like any other secret.

Top-level fields: `name` (`^[a-z][a-z0-9-]{0,62}$`), `description` (at most 512 bytes), optional `template` (a registered template ID; the workspace must have been created from it), `providers`, optional `callback_secret` (32–256 printable ASCII characters; signs [callbacks](FLEET.md#agent-runs-v2) for owner runs with this profile).

## Examples

Upload a skill, then reference its digest:

```sh
curl -sS -H "Authorization: Bearer $OWNER" -H 'Content-Type: application/json' \
  -d "{\"content_base64\":\"$(base64 < review/SKILL.md | tr -d '\n')\"}" \
  https://controller.example/v1/profile-files
# {"sha256":"9f2c…","size":812}
```

A **coder** profile: long Claude sessions with the usual edit policy, the team's plugin and a docs MCP server; Codex with the same skill.

```json
{
  "name": "coder",
  "description": "Builds one change per run.",
  "providers": {
    "claude": {
      "max_turns": 400,
      "permission_profile": "edit",
      "allowed_tools": ["Read", "Write", "Edit", "Bash", "mcp__docs__*"],
      "setting_sources": ["project", "local"],
      "plugins": [{"name": "team-tools", "files": [
        {"path": ".claude-plugin/plugin.json", "sha256": "4b1e…"},
        {"path": "hooks/format.sh", "sha256": "77ad…", "executable": true}
      ]}],
      "mcp_servers": [{"name": "docs", "url": "https://docs.example.com/mcp",
                       "headers_from_env": {"Authorization": "DOCS_AUTHORIZATION"}}]
    },
    "codex": {
      "skills": [{"name": "review", "files": [{"path": "SKILL.md", "sha256": "9f2c…"}]}],
      "mcp_servers": [{"name": "docs", "url": "https://docs.example.com/mcp",
                       "headers_from_env": {"Authorization": "DOCS_AUTHORIZATION"}}]
    }
  }
}
```

A **verifier** profile: read-only, only its own MCP server, and only on workspaces created from a template that has the game engine installed. The coder's workspaces do not need the engine and cannot use this profile.

```json
{
  "name": "verifier",
  "description": "Runs the locked checks against a build; never edits.",
  "template": "engine-godot",
  "callback_secret": "…32+ printable characters…",
  "providers": {
    "claude": {
      "max_turns": 120,
      "permission_profile": "read_only",
      "allowed_tools": ["Read", "Glob", "Grep", "mcp__checks__*"],
      "strict_mcp": true,
      "mcp_servers": [{"name": "checks", "command": "/opt/checks/bin/server", "env_vars": ["CHECKS_TOKEN"]}],
      "env": {"CHECKS_MODE": "headless"}
    }
  }
}
```

Grant a service token only the verifier, with its own callback key, then start a run:

```json
POST /v1/service-tokens
{"label":"verifier-runner","scopes":[{"host_id":"primary","name":"ebox-verify-1"}],
 "operations":["agent_runs.start","agent_runs.get"],"profiles":["verifier"],"callback_secret":"…"}

POST /v1/hosts/primary/instances/ebox-verify-1/agent-runs
{"provider":"claude","prompt":"Run the checks for build 42.","profile":"verifier",
 "env":{"CHECKS_TOKEN":"…"},"idempotency_key":"verify-build-42",
 "callback":{"url":"https://ci.example.com/hooks/enginebox"}}
```

The same token naming `coder` gets HTTP 403.

## Deliberately later

- **Tenant ownership.** Profiles, files and grants are owner-managed; tenant-owned profiles (and per-tenant listing and quotas) follow tenant identities ([TENANT-AGENT-STATE.md](TENANT-AGENT-STATE.md)).
- **A credential vault and secret references.** Secrets reach a run only through its `env` or a profile's default `env`; there are no `secret_refs`, file injection or rotation yet. Callback keys are stored with their token or profile record (owner-only files), not in a vault.
- **Prepared snapshot images per profile.** A profile references a template; it does not build one. Bootstrap commands, package installs and warm pools per profile revision are not implemented, so an engine must already be in the template.
- **Referencing files already in an image.** Files are uploaded to the controller; referencing a path baked into a template (with its digest checked in the guest) is not supported.
- **Network egress rules, resources and budgets per profile**, Cursor MCP/skills, Codex turn limits, file garbage collection (uploaded files are kept after a profile is deleted), and callback events other than the terminal one (`started`, heartbeats).
