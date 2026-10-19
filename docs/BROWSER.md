# Browser access inside a workspace

Each prepared workspace includes Microsoft's original `@playwright/mcp` package,
pinned to `0.0.83`, with its matching Chromium download. Native Codex, Claude and
Cursor each receive an `enginebox-browser` stdio MCP entry. No browser service or
Chrome debugging port is exposed over the network. This is a new browser inside
the guest; it never attaches to the owner's laptop browser.

The official MCP supplies navigation, accessibility snapshots, screenshots,
clicking, typing, JavaScript evaluation and the other tools it documents. Engine
Box only selects a private profile and locks it against concurrent use. Browser
pages can contain untrusted instructions; agents retain their normal permission
model. Browser access is not a network security boundary.
[Microsoft Playwright MCP](https://github.com/microsoft/playwright-mcp).

Call `browser_snapshot` without a filename after navigation or an action when you
need the current accessibility tree and element references. This returns the tree
inline in the tool response. Automatic post-action snapshots in the pinned
upstream version write files instead; the launcher disables those redundant
automatic snapshots with the documented `--snapshot-mode none` option. Explicit
snapshot requests remain available. Nothing rewrites the upstream MCP protocol.

## Persistent profiles and concurrency

Profiles and output live under:

```text
~/.local/share/enginebox/browser/codex/
~/.local/share/enginebox/browser/claude/
~/.local/share/enginebox/browser/cursor/
```

Each directory is private to the guest user. The three clients do not contend for
one Chromium profile or share login cookies by default. A second simultaneous
session of the same client should configure a distinct `--profile NAME`; opening
an already locked profile fails with an actionable error. Cookies/storage survive
closing and reopening that profile. Tabs and an in-flight HTTP request are not
guaranteed to survive a fresh browser process.

The launcher runs the installed package directly, without `npx` fetching packages
on each tool call. Root installs versioned package directories at
`/opt/enginebox-browser/releases/`, preserves the generated package lock there,
and points `current` at the configured release. Browser artifacts are installed
under `/opt/enginebox-browser/chromium`. Chrome runs as the ordinary guest user
with its sandbox enabled. On Ubuntu 24.04 an AppArmor profile grants user-namespace
creation to that browser executable; the global user-namespace policy is unchanged.

## Native MCP setup

The setup adds missing `enginebox-browser` entries using native Codex/Claude MCP
commands and Cursor's documented JSON format. Existing entries are left intact.
The command is:

```sh
/usr/local/bin/enginebox-guest browser --client codex
```

Use `claude` or `cursor` for the corresponding client. An owner's MCP settings can
append `--profile task-2` for another persistent browser. A direct terminal launch
speaks MCP on stdin/stdout, so it is not an interactive browser shell.
[Codex MCP](https://developers.openai.com/codex/mcp),
[Claude MCP](https://code.claude.com/docs/en/mcp),
[Cursor MCP](https://cursor.com/docs/cli/mcp).

## Optional desktop

Headless Chromium is the default. With the template's `config.desktop` enabled, the guest has a virtual
display at `:99`; configure the browser entry with `--headed` to display that
profile there. Close its existing MCP session first. A headless browser cannot
become visible without relaunching; persisted cookies/storage remain available.

The optional VNC and noVNC listeners bind guest loopback only. A human viewer needs
an authenticated tunnel terminating inside the guest before opening noVNC. Merely
forwarding to the VM's private address does not reach a loopback-only listener.
The current bootstrap does not install or authorize an SSH login service for this
purpose. Automated screenshot/click/type tools can still control the virtual
desktop. Direct human viewer transport is separate work until that guest tunnel
is configured. Neither browser CDP nor noVNC should be published unauthenticated.

## Validation

Rust browser/profile tests live under `packages/enginebox-agents/tests/`. Run `pnpm exec cargo test -p enginebox-agents` for private-profile ownership, concurrent-writer locking, native child cleanup and protocol boundaries. A live browser acceptance run must use a disposable guest and profile, serve a local fixture, navigate/type/click through the official MCP, verify a screenshot and restart MCP to check cookie/storage persistence. Unit tests alone do not prove that browser installation works on a new platform.

The v0.2 Python implementation passed that live Chromium fixture on October 19, 2026 in Ubuntu 24.04 x64 in 2.672 seconds. That is historical evidence; it is not automatically qualification of the Rust launcher. See [validation](VALIDATION.md) for release-specific evidence.

An optional existing browser installation can be selected for a test with
`ENGINEBOX_BROWSER_PACKAGE=/absolute/package/directory`; that directory must have
the pinned MCP package plus `chromium-path.txt`. `ENGINEBOX_BROWSER_HOME` selects
a private absolute profile root. Neither override changes the native client.
