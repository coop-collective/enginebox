# Current runtime

This is an extracted environment controller for **already prepared Incus VMs**. Host bootstrap, provider installation, desktop provisioning and a TOML configuration loader are planned. The example TOML is design documentation only.

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
runtime/.venv/bin/ebox list
runtime/.venv/bin/ebox shell ebox-dev
runtime/.venv/bin/ebox exec ebox-dev -- git status --short
runtime/.venv/bin/ebox pause ebox-dev
runtime/.venv/bin/ebox resume ebox-dev
runtime/.venv/bin/ebox hibernate ebox-dev
runtime/.venv/bin/ebox resume ebox-dev
runtime/.venv/bin/ebox snapshot ebox-dev checkpoint-one
runtime/.venv/bin/ebox fork ebox-dev checkpoint-one ebox-copy
runtime/.venv/bin/ebox resume ebox-copy
```

VM names must start with `ebox-`. Pause keeps memory allocated. Hibernate relies on a compatible stateful Incus VM configuration and saves memory to disk. A fork boots independently from the snapshot's disk. The controller does not create the VM/profile or enable stateful support itself.

Snapshotting a running VM requests memory state; snapshotting a cleanly stopped VM captures disk only. Shared external mounts are excluded. A snapshot includes private files and any account state present. Do not turn an authenticated workspace snapshot into a reusable/public base.

Restore replaces later private-disk changes. Stop the target explicitly first:

```sh
runtime/.venv/bin/ebox stop ebox-dev
runtime/.venv/bin/ebox restore ebox-dev checkpoint-one
# For a disk-only snapshot:
runtime/.venv/bin/ebox restore ebox-dev clean-base --disk-only
```

Use native `tmux` or the client session mechanisms for long interactive work. The short `exec` operation has a timeout; terminating the local transport does not guarantee the remote process has stopped.

## MCP and desktop

An operator MCP client can launch the absolute path to `runtime/.venv/bin/enginebox-mcp` with the chosen `ENGINEBOX_HOST` and configuration. It exposes six environment tools plus screenshot, click, typing and key input. It inherits the operator's SSH authority.

A prepared guest can launch the same entry point with `ENGINEBOX_GUEST=1`. That exposes only four desktop tools and does not require a host target. The mode flag restricts the tool list; actual authorization depends on keeping host credentials and privileges out of the guest. X11, `scrot`, `xdotool` and the display must already exist. The computer controls currently assume a 1440×900 desktop.

`ebox desktop ebox-dev` prints connection information for an already configured noVNC service on guest port 6080. SSH mode produces a local tunnel; local mode produces a guest URL. **This command does not install or secure that service.** The original prototype's private-network listener was not isolated from other trusted guests. Safe generic desktop provisioning is an outstanding implementation gate; do not expose an unauthenticated desktop on a shared network.

## Verification and limitations

Run offline tests from the repository root:

```sh
uv sync --project runtime --locked
runtime/.venv/bin/python -m unittest discover -s tests -v
```

These tests validate configuration, command construction, lifecycle guards and MCP mode boundaries without contacting a host. They do not establish native account/inference support or validate this extracted runtime on a real VM.

The original host scripts and live probes were deliberately excluded: they contain machine-specific changes and operate on an existing workspace. The new integration suite must provision disposable targets. Operation locking, complete cancellation, identity sanitization, reboot/backup recovery and generalized storage/network setup remain planned.
