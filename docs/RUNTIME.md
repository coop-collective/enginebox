# Current runtime

These are the lower-level commands for prepared Incus VMs. Use [bootstrap](BOOTSTRAP.md) to create a workspace and [fleet operations](FLEET.md) for capacity-aware creation and lifecycle controls through the API, MCP or dashboard.

## Configuration

Set variables explicitly in the operator shell or the operator MCP process environment:

| Variable | Default | Purpose |
|---|---|---|
| `ENGINEBOX_HOST` | Required in operator mode | SSH config alias/target or `local`. No owner server is selected implicitly. |
| `ENGINEBOX_PROJECT` | `enginebox` | Incus project for inventory and lifecycle operations. |
| `ENGINEBOX_GUEST_UID` | `1001` | Guest execution UID. |
| `ENGINEBOX_GUEST_GID` | `1001` | Guest execution GID. |
| `ENGINEBOX_GUEST_USER` | `agent` | Guest username exposed to the process. |
| `ENGINEBOX_GUEST_HOME` | `/home/agent` | Guest home directory. |
| `ENGINEBOX_GUEST_WORKSPACE` | `/workspace` | Guest working directory. |
| `ENGINEBOX_GUEST_DISPLAY` | `:99` | Optional X11 display. |
| `ENGINEBOX_GUEST` | `0` | Set to `1` for a guest-local desktop MCP server with no host lifecycle tools. |

The configured guest user/UID/GID/directories must already exist and agree. These variables select execution settings; they do not provision accounts. The operator uses `sudo -n incus` locally or through SSH and therefore has administrative authority. Only trusted operator clients should receive the lifecycle MCP tools.

## Operations

```sh
export ENGINEBOX_HOST=my-server
export ENGINEBOX_PROJECT=enginebox
ebox list
ebox shell ebox-dev
ebox exec ebox-dev -- git status --short
export ENGINEBOX_FLEET_HOST=primary
export ENGINEBOX_API_TOKEN_FILE="$HOME/.local/state/enginebox/api.token"
ebox pause ebox-dev
ebox resume ebox-dev
ebox hibernate ebox-dev
ebox resume ebox-dev
```

VM names must start with `ebox-`. Pause keeps memory allocated. Hibernate relies on a compatible stateful Incus VM configuration and saves memory to disk. A fork boots independently from the snapshot's disk. Bootstrap creates the VM/profile and configures stateful support; these lower-level commands assume that preparation is complete. Normal lifecycle commands require a registered `--fleet-host` (or `ENGINEBOX_FLEET_HOST`) and use the controller API, returning jobs to poll. Direct administrator maintenance requires explicit `--direct` and an SSH host; it takes the host lock and rejects active native leases, but bypasses fleet capacity admission and post-resume credential attachment. Quiesce fleet work before using that maintenance mode.

Snapshotting a running VM requests memory state; snapshotting a cleanly stopped VM captures disk only. Shared external mounts are excluded. A snapshot includes private files and any account state present. Do not turn an authenticated workspace snapshot into a reusable/public base.

Restore replaces later private-disk changes. Stop the target explicitly first:

```sh
ebox stop ebox-dev
# Wait for the stop job to succeed, then quiesce fleet work.
unset ENGINEBOX_FLEET_HOST
ebox --direct restore ebox-dev checkpoint-one
# For a disk-only snapshot:
ebox --direct restore ebox-dev clean-base --disk-only
```

`ebox --direct snapshot ebox-dev checkpoint-one` and `ebox --direct fork ebox-dev checkpoint-one ebox-copy` are administrator operations. A fork of a private snapshot copies its credentials and private files; it is not a clean template or an automatically admitted fleet allocation. Use the fleet allocation API for a fresh workspace.

Use native `tmux` or the client session mechanisms for long interactive work. The short `exec` operation has a timeout; terminating the local transport does not guarantee the remote process has stopped.

## MCP and desktop

An operator MCP client launches `enginebox-fleet-mcp` with the controller API URL and token; see [fleet MCP](FLEET.md). Lifecycle actions use the controller's capacity checks and ownership locks. Direct `ebox` administration remains available to an operator with SSH authority.

A prepared guest launches `enginebox-mcp` with `ENGINEBOX_GUEST=1`. That exposes only four desktop tools and does not require a host target. The mode flag restricts the tool list; actual authorization depends on keeping host credentials and privileges out of the guest. X11, `scrot`, `xdotool` and the display must already exist. The computer controls currently assume a 1440×900 desktop.

`ebox desktop ebox-dev` prints connection information for an existing guest-network noVNC listener on port 6080. **This legacy tunnel does not reach the new optional desktop recipe's guest-loopback-only listener.** Interactive viewer transport is still pending. Headless [browser MCP](BROWSER.md) works independently, and guest computer MCP can control the optional virtual desktop locally. Do not expose an unauthenticated desktop listener to work around this limitation.

## Verification and limitations

Run offline tests from the repository root:

```sh
pnpm install --frozen-lockfile
pnpm test
```

These tests validate configuration, command construction, lifecycle guards, fleet capacity and MCP mode boundaries without contacting a host. They do not establish native account/inference support. Live browser qualification requires an explicitly prepared disposable guest.

Machine-specific inventory, credentials and live evidence remain outside Git. Bootstrap/fleet share a host lock; prepared disk clones regenerate guest identity. Complete cancellation, reboot/backup recovery and generalized shared mount/cache services remain release work.
