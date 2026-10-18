# Native client preparation

Reviewed October 18, 2026 against official documentation and the provider installers. Installation recipes described here are planned. The repository's runtime does not install or authenticate an agent client.

Engine Box prepares a private Linux workspace, then the owner runs the official CLI directly. No replacement harness, prompt proxy or pooled provider account is required. The initial infrastructure target is Ubuntu x86_64 even where a provider supports additional architectures.

## Provider contracts

| Client | Install/version policy | Native login on a remote workspace | Native conversation continuation |
|---|---|---|---|
| Codex (`codex`) | Official standalone installer; supports a selected release and noninteractive installation. Record the release and verified download. | `codex login --device-auth` where the account/workspace enables device login; official browser callback flow is another option. | `codex resume` interactively, or `codex exec resume` for supported noninteractive use. |
| Claude Code (`claude`) | Official native installer supports exact versions. Set and record update policy because native installation can auto-update. | `claude auth login`; subscription-backed scripted use also has an official `claude setup-token` option, kept outside base images/config. | `claude --resume` or `claude --continue`. |
| Cursor Agent (`agent`) | Official installer, with installed version recorded. Strict version pinning and updater control remain unresolved in the reviewed CLI docs. | `NO_OPEN_BROWSER=1 agent login`, then open the supplied URL in the owner's browser. | `agent --resume` or `agent --continue`. |

Primary sources: [Codex installation](https://developers.openai.com/codex/cli), [authentication](https://developers.openai.com/codex/auth) and [noninteractive execution](https://developers.openai.com/codex/noninteractive); [Claude setup](https://code.claude.com/docs/en/setup), [authentication](https://code.claude.com/docs/en/authentication) and [CLI reference](https://code.claude.com/docs/en/cli-reference); [Cursor installation](https://cursor.com/docs/cli/installation), [authentication](https://cursor.com/docs/cli/reference/authentication) and [parameters](https://cursor.com/docs/cli/reference/parameters).

Recipes should use official release/install mechanisms rather than redistribute provider binaries in this repository. Codex and Claude provide version-selection options; resolve supported syntax from the selected installer when implementing. Cursor's version policy is an explicit investigation item. A manifest must not claim reproducibility solely because a mutable installer URL was recorded.

## Account and state boundaries

Account eligibility and billing are provider-specific. Codex account sign-in and an OpenAI API key use different billing paths. Claude needs an eligible account mode; an environment API key can change the billing path. Cursor CLI usage depends on the customer's account/model terms. Do not hardcode a promise that every subscription or model is included. [Codex authentication](https://developers.openai.com/codex/auth), [Claude authentication](https://code.claude.com/docs/en/authentication), [Cursor pricing](https://cursor.com/docs/models-and-pricing).

Keep native account state in the private guest home. Do not silently copy credentials from the operator laptop, share homes between jobs, log tokens or include them in images/cache exports. Clean bases are prepared before authentication. Private checkpoints may contain account state and RAM and must remain private.

The optional CI module optimizes ordinary builds/tests and needs no model account. Public OSS repository CI must remain credential-free. OpenAI's documented advanced account-auth CI workflow is intended for trusted runners and excludes public/open-source repository CI. [Codex noninteractive guidance](https://developers.openai.com/codex/noninteractive).

Native conversation resume reloads session history in a new client process. VM pause/hibernate preserves machine execution state. A suspended network connection can still expire, and provider-side sessions, token refresh and quotas continue to follow provider behavior.

## MCP preparation

Use the provider's native configuration interface. Keep the Engine Box entry distinct and preserve unrelated user entries on repeated setup.

| Client | Supported configuration surface |
|---|---|
| Codex | `codex mcp add`; user configuration under `~/.codex/config.toml`. |
| Claude Code | `claude mcp add --scope user`; user configuration under `~/.claude.json`. |
| Cursor Agent | `~/.cursor/mcp.json`; CLI MCP inspection commands. |

Sources: [Codex MCP](https://developers.openai.com/codex/mcp), [Claude MCP](https://code.claude.com/docs/en/mcp), [Cursor MCP](https://cursor.com/docs/cli/mcp).

Guest entries launch `enginebox-mcp` with `ENGINEBOX_GUEST=1` and expose only four desktop tools. They receive no host SSH identity or Incus socket. Operator MCP is configured separately and can administer environments. Native tool approval settings remain in force.

## Acceptance

The account-free preparation test installs selected clients in a clean disposable VM, checks `--version`/`--help`, records versions, exercises native MCP registration and a direct tool call, and repeats setup without destroying user configuration. Cache and lifecycle tests use deterministic local processes.

The owner-account test is separate: complete each native login, inspect status privately, run one bounded prompt when authorized, and verify native continuation before/after restart or hibernation. Never publish tokens or account-identifying command output. Use documented diagnostics for quota/expiry behavior; do not burn an allowance just to force a limit.

For upgrades, validate candidate versions in a disposable workspace before replacing a recipe manifest. A binary already existing on PATH is not proof that it matches the requested version.
