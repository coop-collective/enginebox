# Native agent clients

Engine Box installs the providers' original terminal applications inside a private
Ubuntu 24.04 workspace. It does not replace their agent loops, forward model
requests, or turn subscription allowances into independent worker balances.
The owner runs `codex`, `claude`, or `agent` in the guest terminal. Controller-level
Agent Access connects an owner's accounts and delivers supported access material
to that owner's assigned private workspaces; refresh ownership stays central.

## Guest setup contract

The bootstrap copies `runtime/` into `/opt/enginebox-runtime`, then runs
`bash /opt/enginebox-runtime/guest_setup.sh` as root **inside the guest**.
Do not run that script on a developer's laptop or the Incus host.

| Setting | Default | Effect |
| --- | --- | --- |
| `EBOX_USER`, `EBOX_UID`, `EBOX_GID` | `agent`, `1001`, `1001` | Private non-root account; existing mismatched identities fail |
| `EBOX_HOME`, `EBOX_WORKSPACE` | `/home/agent`, `/workspace` | Private home and working directory |
| `EBOX_INSTALL_CODEX`, `EBOX_INSTALL_CLAUDE`, `EBOX_INSTALL_CURSOR` | `1` | Select native clients independently |
| `EBOX_MCP`, `EBOX_BROWSER` | `1` | Configure native MCP entries/Python test runtime; install headless Chromium and browser MCP |
| `EBOX_DESKTOP` | `0` | Optional X11 desktop and loopback-only noVNC |
| `EBOX_COMPUTER_MCP` | Same as `EBOX_DESKTOP` | Register guest computer tools; requires the desktop |
| `EBOX_UPDATE_CLIENTS` | `0` | Existing clients are preserved; `1` explicitly reinstalls requested clients |
| `EBOX_CODEX_VERSION`, `EBOX_CLAUDE_VERSION` | `0.163.0`, `2.1.302` | Exact versions for new installs/reinstalls |
| `EBOX_NODE_VERSION` | `22.23.3` | Install this official Node distribution if no Node 18+ exists |
| `EBOX_PLAYWRIGHT_MCP_VERSION` | `0.0.83` | Exact official browser MCP package |

The script preserves existing native configuration, login state and other MCP
entries. It adds an Engine Box entry only when that name is absent. Set a flag to
`0` to skip installing/configuring that component; this does not uninstall an
existing component. Existing native clients and a compatible Node installation
are recorded as found rather than silently replaced. The result is saved to
`/opt/enginebox-runtime/installed-versions.json`; account details are excluded.
The bootstrap sets `EBOX_MCP=1` when either browser MCP or computer MCP is requested;
browser installation alone without `EBOX_MCP` does not register client entries.
Browser and computer MCP entries are distinct. Computer MCP runs with
`ENGINEBOX_GUEST=1` and exposes only the guest's screenshot/click/type/key tools;
it receives no host SSH identity or Incus socket. Operator lifecycle MCP is
configured separately. Native client tool-approval settings remain in force.

Codex and Claude support pinned official installs. Claude background updates are
disabled in prepared shell sessions; an owner can deliberately update using the
native CLI. Cursor's official installer currently installs its published version
and its CLI can update itself. Strict Cursor version pinning is **not implemented**.
The manifest records what actually installed. Linux x64 is the integration target;
ARM64 selects corresponding installer artifacts but requires its own integration
run before being considered tested. [Codex installation](https://developers.openai.com/codex/cli),
[Claude installation and updates](https://code.claude.com/docs/en/setup),
[Cursor installation](https://cursor.com/docs/cli/installation).

## Owner sign-in and Agent Access

The current architecture connects accounts once on the controller and delegates
access to assigned private workspaces. Prepared bases and unassigned warm reserves
remain credential-free. Native clients still contact their providers directly;
each workspace consumes the same underlying account allowance. Existing private
workspace logins require explicit adoption, rather than background replacement.

The native commands remain available directly inside a workspace:

```sh
codex login --device-auth
claude auth login --claudeai
NO_OPEN_BROWSER=1 agent login
```

Open the provider's displayed URL in your local browser. Codex device login must
be enabled by the account/workspace; its documentation also describes SSH callback
forwarding. Check status with `codex login status`, `claude auth status`, or
`agent status`. Status output can identify the account; keep it out of public logs.
[Codex authentication](https://developers.openai.com/codex/auth),
[Claude authentication](https://code.claude.com/docs/en/authentication),
[Cursor authentication](https://cursor.com/docs/cli/reference/authentication).

The original workspace sign-in panel was checked against Codex CLI `0.163.0`,
Claude Code `2.1.302` and Cursor Agent `2026.10.16-e3cb207`, including provider
prompts, code entry and cancellation. Central adoption and automatic delivery were
then checked separately in a disposable workspace: all three native clients
reported connected, and Codex/Claude completed inference and exact-session resume
using delegated access. Cursor inference remained quota-blocked from the earlier
test and was not retried.

Claude's documented `claude setup-token` route creates a long-lived subscription
token for `CLAUDE_CODE_OAUTH_TOKEN`; it supports model requests and local MCP, with
different account features from a full interactive login. Cursor documents user
API keys through `CURSOR_API_KEY`. Codex's experimental external-token auth format
and any Cursor browser-token cache adapter require explicit version gates and
regression checks. Live controller qualification also exercised native Codex and Claude renewal, confirmed changed access tokens, and verified delivery to both existing private workspaces after a controller restart. Cursor renewal at expiry and hot replacement inside a running process remain unqualified.

An expired access credential must trigger central renewal or
reconnection, not distribution of refresh credentials. A running native process
may retain its old credential even after a controller updates files.
[Claude token authentication](https://code.claude.com/docs/en/authentication#generate-a-long-lived-token),
[Cursor automation authentication](https://cursor.com/docs/cli/reference/authentication),
[Codex external-token authentication](https://learn.chatgpt.com/docs/app-server#3c-log-in-with-externally-managed-chatgpt-tokens-chatgptauthtokens).

Subscription entitlement, available models and concurrency remain provider/account
decisions. Signing in successfully does not establish successful inference. API
keys can select separate billing; avoid inheriting them into a subscription
workspace. Claude's documented `setup-token` route supports subscription-backed
scripts, with different feature availability from interactive sign-in. Cursor
usage varies by plan/model. The project makes no promise that every plan includes
every model or unlimited parallel work.
[Claude authentication](https://code.claude.com/docs/en/authentication),
[Cursor usage](https://cursor.com/docs/models-and-pricing).

Keep this public repository's own CI free of subscription credentials. OpenAI's
advanced account-auth CI guidance is for trusted private runners and explicitly
excludes public/open-source repository CI. Ordinary cached builds and tests do
not need agent credentials.
[Codex noninteractive mode](https://developers.openai.com/codex/noninteractive).

## Resume and reusable images

Native conversation resume (`codex exec resume`, `claude --resume`,
`agent --resume`) reloads conversation history. VM pause/hibernate preserves
machine execution state. Remote model requests, OAuth expiry and network
connections can still need reconnection after elapsed time; these are separate
acceptance checks.

Prepare reusable bases before account login. A private snapshot can contain
provider credentials, browser cookies and RAM; it must not become a shared base.
Clones use a new virtual machine UUID/MAC. The guest's startup identity service
rotates filesystem machine ID, hostname and SSH host keys when that UUID changes;
DHCP uses MAC rather than the copied machine ID. Identity rotation does **not**
sanitize application credentials. The bootstrap clears machine ID in the owned
pre-login base immediately before stopping/snapshotting, so PID1 also establishes
a fresh identity on the clone's first boot. This is separate from ordinary guest
setup, which must not erase identity in an already running user's workspace.

Bootstrap fingerprints configuration, not installer source or dependency contents.
Reapplying a prepared configuration is a no-op, and an existing
`prepared-<configuration-fingerprint-prefix>` snapshot is retained. To adopt a new
Engine Box release, changed installer recipe or native-client versions, provision
a new base with a new workspace name, validate it, then select that new snapshot
as the fleet template. Reprovisioning the source alone does not update a previously
captured snapshot. Signed-in workspaces are private and are never image upgrades.

After initial cloud-init finishes successfully, guest setup creates
`/etc/cloud/cloud-init.disabled`. Prepared clones keep their rendered netplan and
identity service, avoiding repeated metadata/setup work. Do not invoke guest setup
from a cloud-init `runcmd`: it waits for initial cloud-init completion. Network
readiness services remain enabled; readiness must reflect an operable guest,
not just an early process start. [Cloud-init disable mechanism](https://docs.cloud-init.io/en/latest/howto/disable_cloud_init.html).

## Validation

Bootstrap runs version checks only. Offline checks cover profile isolation and
locking. See [browser validation](BROWSER.md) for an opt-in local fixture test that
uses real Chromium and MCP without model inference. A separately authorized small
native inference run passed code edits, assertions, exact-session resume and
browser MCP for Codex and Claude. Cursor's run was blocked by its provider team
usage limit. Codex's initial `workspace-write` sandbox failed with
`bwrap: Operation not permitted`. After installing the Ubuntu distro bubblewrap
profile, a separate delegated-credential test passed both inference turns with
normal `workspace-write` sandboxing and no bypass. See the
[sandbox setup](CODEX-SANDBOX.md) and
[acceptance record](VALIDATION.md#native-inference-and-conversation-resume)
for command patterns and remaining qualification.

Cursor also needs permission for its own namespace helper on the tested Ubuntu
guest. The [scoped Cursor sandbox setup](CURSOR-SANDBOX.md) now runs a model-free
preflight during preparation. Live operator MCP reached the provider with that
sandbox enabled; the test team's quota still blocked inference and resume.
