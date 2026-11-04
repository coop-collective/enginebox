# Agent forks: the engine machine (node) side

Room members run their own coding agent (Claude Code or Codex) on a **fork** of a live editor session's dev tree,
on the engine machine beside the Editor. A submission three-way merges the fork into the dev head and applies it to
the live tree, which the Editor hot-refreshes. This page covers the node (`packages/enginebox-install/src/forks/`).
The controller side (API, policy, submissions, xinf OAuth) and the node protocol are in
[ENGINE-SESSIONS.md → Agent forks](ENGINE-SESSIONS.md) (controller PR #128).

## Pieces

| Module | What it does |
|---|---|
| `forks/mod.rs` | `ForkProvider` trait, `DevTree`, `ForkTree`, `Change`, `ForkError` (`conflict`, `stale`, `refused`, `held`, `unsupported`) |
| `forks/git.rs` | `GitTreeProvider` (Windows, macOS, Linux without LayerFS) |
| `forks/layerfs.rs` | `LayerFsProvider`: **unsupported** (see below) |
| `forks/prescan.rs` | Pre-scan before git reads a tree: symlinks, junctions and reparse points, hard links, `.git` names, Windows-reserved names, trailing dot or space, `:` (alternate data streams), case-only collisions |
| `forks/classify.rs` | Path classes and the secret scan (the controller is authoritative; the node computes the same for previews, live capture and its own pre-merge check) |
| `forks/relay.rs` | Loopback gateway relay to Xava Inference |
| `forks/harness.rs` | Per-run home and harness configuration for Claude Code and Codex |
| `forks/slots.rs`, `forks/windows.rs` | Slot accounts, sealed passwords, ACLs, the agent desktop, `CreateProcessWithLogonW` into a limited Job Object |
| `forks/runner.rs` | One agent run: relay token, harness, transcript events, timeout and cancellation, snapshot |
| `forks/submit.rs` | Submission checks, merge, apply under the Editor's source lock, `/refresh`, compile result, revert |
| `forks/executor.rs` | The node protocol: `engine-poll` `forks`/`dev`, `fork-report`, `fork-events`, `fork-artifact`, `engine-report` `detail.dev` |
| `editor_account.rs` | Opt-in: Unity as `enginebox-editor`, and the qualification checks |

## Git layout and hardening

The engine session's checkout is now a work tree of a **node-private** repository. A checkout from an older node,
with `.git` inside it, is moved once:

```
work\engine\repos\<key>.git     node only: objects, LFS store, refs/eb/dev/<session>, refs/eb/forks/<fork>
work\engine\repos\<key>.index   the dev tree's index
work\engine\repos\.node\        empty hooks directory and empty global config for every node git call
work\engine\checkouts\<key>\    the dev tree (no .git; Library cache kept)
work\engine\forks\<fork>\tree   the fork (the agent's only writable tree)
work\engine\forks\<fork>\home   the run home
work\engine\forks\<fork>\meta   node only: per-fork index, state.json
```

Every node git call:
- names `GIT_DIR`, `GIT_WORK_TREE` and `GIT_INDEX_FILE` explicitly;
- sets `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=<empty>`, `GIT_LITERAL_PATHSPECS=1` and a cleared environment;
- passes `-c core.hooksPath=<empty> -c core.fsmonitor=false -c core.symlinks=true -c protocol.file.allow=never`.

A test plants `core.fsmonitor`, `core.hooksPath` and hooks in the repository config and checks that nothing runs.
Three-way merges run with `GIT_ATTR_SOURCE=<empty tree>`, so no `.gitattributes` can pick the built-in `union` driver.
Forks need **Git 2.42 or newer**. The node checks the version when it registers a dev tree and records it in the
session receipt as `forks_git`.

The operations:
- **create**: `read-tree <base>` into the fork index, then `checkout-index -a -u`. LFS content smudges from the
  local store.
- **snapshot**: the pre-scan, then `add -A` into a copy of the fork index and `write-tree`. It also checks sizes:
  files over 100 MiB not in LFS, more than 5,000 paths, or more than 500 MiB added are refused, as are symlink or
  submodule modes and case collisions. Then `commit-tree -p <base>` and `refs/eb/forks/<fork>`. An unchanged tree
  keeps the same head.
- **merge**: `merge-tree --write-tree --merge-base=<fork base> <dev head> <fork head>`. Exit 1 is a conflict. Then
  `commit-tree` with both parents and a compare-and-swap `update-ref`.
- **apply**: `read-tree -m -u <from> <to>` on the dev index, inside the bridge's source-lock handshake. Git refuses
  when a live edit is in the way. The node then captures, merges again and retries once; otherwise it rewinds the
  dev head and reports `held`, retrying every 15 seconds for up to 30 minutes.
- **capture_live**: driven by the bridge's `(instance, saveSeq)` (checked every 3 s) and run before every merge.
  Commits the Editor's saved content: scene, asset and config classes. Code and protected paths stay uncommitted
  and are reported as `live_code_pending`; refused paths are skipped. The bridge and session entries the node adds
  to `Packages/manifest.json` are `skip-worktree`, so they are never captured.
- **rebase**: snapshot, then a three-way merge of the new dev head into the fork, `read-tree -m -u` into the fork
  tree, and a new snapshot on the new base. A conflict leaves the fork untouched.
- **revert** (the controller's `revert` op, including the automatic one after compile errors): undoes a merge
  commit on top of whatever followed it (`merge-tree` with base `<commit>`, theirs `<parent>`), applied the same way
  as a merge.
- **publish** (`dev.ops`): `git lfs push` and `git push <head>:refs/heads/jam/<name>` with the one-shot repository
  token in `GIT_CONFIG_*` only. The result goes in `engine-report` `detail.dev.publish`.

## Isolation

| | Windows | macOS / Linux without slot users |
|---|---|---|
| Heartbeat `isolation` | `accounts` | `none` (the controller lets only the room owner run agents) |
| Run as | `eb-agent-01..03` via `CreateProcessWithLogonW` (logon flags 0: the slot's profile is never loaded) on the private desktop `WinSta0\eb-agents` | the node's account, own process group, cleared environment; Codex keeps `sandbox_mode=workspace-write` |
| Limits | own Job Object: `KILL_ON_JOB_CLOSE`, 4 GiB job memory, 25 % CPU hard cap, 64 processes, UI limits (desktop, handles, atoms, clipboard, system parameters, display, exit), no breakaway | process group, killed at the end |
| Files | fork `tree` and `home` ACL'd to node full and slot modify for the run only. `OWNER RIGHTS` is limited to `READ_CONTROL`, so a slot cannot re-ACL its own files. Revoke resets the tree to node-only and node-owned | node's own files (only the starter's forks) |
| Network | installer firewall rule per slot SID: block everything except loopback (`-AgentInternet`: block LAN, link-local, CGNAT and multicast only) | none |

Per-slot Unix uids behind a root `enginebox-spawn` helper are WS-11 (not done).

**Accounts.** `install-worker-node.ps1 -EngineAccount [-AgentSlots 0-8] [-AgentInternet]`:
- Creates `enginebox-editor` and `eb-agent-01..NN` as Users. They are hidden from sign-in and denied network and
  remote-desktop logon; slots are also denied batch and service logon.
- Gives each slot a firewall rule.
- Copies the engine account's Unity region pin and accepted Unity terms to the editor profile.
- Writes the passwords to `private\accounts.pending.json`, which only the engine account can read. At start the
  node seals them with user-scope DPAPI into `private\accounts.dpapi` and deletes the pending file.
- Re-install rotates every password; uninstall removes everything.

**Tools.** `engine.json` names the pinned harness binaries:

```json
"agents": {"claude": "C:\\Enginebox\\engine-tools\\agents\\claude.exe",
           "codex": "C:\\Enginebox\\engine-tools\\agents\\codex.exe",
           "git_bash": "C:\\Program Files\\Git\\bin\\bash.exe", "slots": 3}
```

## Model access: the gateway relay

One loopback listener (`127.0.0.1:<dynamic>`) per node. It accepts only these routes:
`POST /v1/messages`, `/v1/messages/count_tokens`, `/v1/responses`, `/v1/chat/completions`, and `GET /v1/models`.
- **Auth.** Each run gets a bearer `ebr_<64 hex>`; the node keeps only its digest. The relay forwards to
  `https://xavainf.com` with the run's xinf access token, which arrives in `secrets.gateway` of the run op while the
  run is in flight. It is re-delivered on later polls and refreshed in memory.
- **Headers.** Client auth, cookie and forwarding headers are stripped. Responses stream back unchanged, without
  `set-cookie`.
- **Limits per run.**
  - The run deadline and the token's expiry. A `kind:"key"` gateway has `expires_at: 0`, meaning no expiry.
  - The budget, summed from xinf's `x-xm-cost-usd`. Once it is spent, the next request gets a 402 shaped like
    Anthropic's or OpenAI's.
  - 4 concurrent requests and 600 per minute; xinf's own 402 and 429 pass through.
- **Guards.** Requests with a foreign `Host` or any `Origin` are refused.
- **Logging and run end.** The relay logs no token and no body. At run end the bearer is revoked and spend and
  request counts go into the run report.

The run token is written only into the run home's harness file, never into argv or the environment:
- Claude Code reads it from `home\.claude\settings.json`: `env.ANTHROPIC_BASE_URL` and `env.ANTHROPIC_AUTH_TOKEN`,
  with `CLAUDE_CONFIG_DIR` pointing there.
- Codex reads it from `home\.codex\config.toml`: provider `xinf` with `base_url = relay/v1`,
  `wire_api = "responses"` and `experimental_bearer_token`. There is no `auth.json`, and Codex runs OpenAI models
  only.

The prompt goes in on stdin after a node-owned preamble. The harness file is deleted when the run ends; the home
(native sessions for `resume`) stays until the fork closes. Run timeout: 30 min by default, 2 h at most.

## Unity as `enginebox-editor` (opt-in, Windows)

Default: Unity runs as the node account, as before. With `"editor_account": "enginebox-editor"` in `engine.json`
the node:
- runs the licensing client and Unity as that account (`CreateProcessWithLogonW` with its profile,
  `WinSta0\Default`) inside the session's Job Object;
- shares only the dev tree, the staged bridge package and `work\engine\editor\<session>\` with it. That directory
  holds the bridge config and token, the session package, `unity.log` and the handshake files;
- scrubs the licensing logs in the editor's profile by starting `enginebox-node scrub-licensing-logs` as the
  editor. The scrubber removes whatever follows `--accessToken`, so it never sees the token.

The stream host, git and every secret stay with the node.

**Qualify first.** Run `qualify-editor-account.ps1 -Project <a copy of the Unity project> [-UnityAccessToken]`
elevated on the machine. It runs `enginebox-node engine-qualify-editor` as the engine account in its desktop session
(a one-shot `schtasks /IT` task) and prints a PASS/FAIL table. The checks:

- **Accounts:** the sealed accounts are readable.
- **Editor sign-in:** `whoami` as the editor works through a job-assigned, UI-limited process.
- **Editor access:** the editor cannot read `private\worker.json` or `accounts.dpapi`.
- **Unity:**
  - Licensing works when a token is given, and the scrubber then leaves no token in the logs.
  - The bridge answers the node.
  - Frames advance, and `/frame` returns a JPEG to the node.
  - Play mode starts, and `unity.log` names a hardware GPU renderer (not WARP or the Basic Render Driver).
  - `TerminateJobObject` ends the Editor.
- **Slots:**
  - The private desktop can be created.
  - `whoami` as `eb-agent-01` with no profile works.
  - The slot cannot read the node identity or the project.
  - Loopback works, and a LAN address is blocked.

Flip the flag only after every check passes. If sharing the desktop fails, the contract's fallback (autologon
`enginebox-editor`, node in an S4U task, a launcher over a named pipe) is not implemented.

## Linux (LayerFS)

`LayerFsProvider` answers `unsupported` for every operation, and the node never selects it.

**TODO (WS-10):** map the trait onto `enginebox-storage-layerfs`:
- `capture_live` = `checkpoint dev-<session>`
- `create` = `github_fork`/`fork --from dev-<session>` in a LayerFS sandbox container
- `snapshot` = `checkpoint` + `workspace_export`
- `merge` = native `branch.merge` with an `expected_main`-style guard

This needs LayerFS to merge into a branch whose workspace is open and mounted. Until then: "dev = main" through
`project_promote --merge expected_main`, with an `updateMode:"files"` follower as `apply`.
