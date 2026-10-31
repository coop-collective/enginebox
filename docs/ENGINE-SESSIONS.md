# Engine sessions (design draft)

Status: v1 implemented November 1, 2026 for personal Windows engine machines (see "What v1 implements");
the rest is design, October 31, 2026.

## Owner decisions (November 1, 2026)

- **Node kinds and isolation (revised November 1).** WSL's paravirtual GPU tops out at OpenGL 4.6 over D3D12 with
  no Vulkan (no Unreal, no HDRP, no ray tracing), so it is not the foundation. Two node kinds behind one API:
  - **Personal Windows machines** run only their owner's sessions, natively (DirectX 12, Vulkan, Unreal, the Windows
    stream host), under a dedicated `enginebox` Windows account that signs in automatically with a generated
    password, separated from the owner's files. Other orgs' sessions are never placed on personal machines.
  - **Cloud Linux GPU servers (NVIDIA L4 first)** run sessions as containers with the native driver through NVIDIA
    Container Toolkit (Vulkan, OpenGL, NVENC). The cloud instance is the tenant boundary: one instance per org.
  - Per-session Hyper-V GPU-P VMs and WSL GPU containers stay possible later tiers, not v1.
- **Editor and player sessions.** Editor sessions run the engine editor on a project. Player sessions run a build
  artifact (EngineBox engine builds), are lighter, and can be placed on any eligible node with headroom; the artifact
  is cached per node, so one game's players spread across machines when one is loaded. Viewers cost no node GPU (MoQ
  relay fan-out).
- **Licensing: each org brings its own Unity credentials** as its own `secret_source` connection; activation is per
  (node, org) inside that org's VMs.
- **Saves: LayerFS workspaces.** The session's project is a LayerFS workspace exported into the VM.
- **Capacity is measured, not configured.** No fixed `sessions_per_gpu`. A calibration run profiles each
  (project, engine version, mode): peak VRAM, mean/p95 GPU and encoder utilisation, frame time at the target
  resolution, first-import time. Admission requires the GPU's remaining VRAM, 3D headroom and free NVENC sessions
  (GeForce drivers cap concurrent encodes per GPU) to cover the profile plus a margin; the VM's GPU-P VRAM share is
  sized from it. Live per-VM telemetry (frame time over budget, encoder queue) tightens later admissions. Viewers and
  WebGL players cost no node GPU (MoQ relay fan-out, client-side play); only Editor sessions and cloud-rendered play
  are admitted against GPU capacity.

## What v1 implements (November 1, 2026)

v1 is the personal-machine path only: a member starts the Unity Editor for a repository on **their own** Windows
engine machine and gets a MoQ stream back. First target: Queen of Hearts on the owner's RTX 4080 machine. Everything
below the "Design" heading that v1 does not list here (cloud Linux nodes, containers, LayerFS exports, measured
admission, the per-session relay broker, OAuth) is still design.

### Who can call it

- The owner token, and members through the dashboard (`/api/engine-sessions/{project}[/{id}[/renew|end]]`, which
  sends the owner token with `X-EngineBox-Org` and `X-EngineBox-Actor` from the member's session).
- Service tokens are refused. **TODO (OAuth):** a grant `{kind:"engine_sessions", project}` with operations
  `engine_sessions.start|get|renew|end`, the `engine-sessions` OAuth scope, and the routes in the dashboard's
  `RESOURCE_ROUTES`, so Gamejam's dev server can start sessions as an OAuth client. The controller side is one more
  arm in `service_allows` plus mapping the token's principal to the starting member.

### API (`packages/enginebox-controller/src/engine_sessions.rs`)

- `POST /v1/projects/{p}/engine-sessions`
  `{request_id, repository, ref, project_path?, engine:{kind:"unity", version}, scene?, width?, height?, ttl_seconds?, mode?:"editor"}`
  → `202 {id, state, lease, lease_expires_at, node, generation, …}`. Idempotent on `(org, member, request_id)`: the
  same body returns the same session (and lease); a different body is `409`. `ttl_seconds` 60–14400 (default 3600);
  `width`×`height` up to 1920×1080 (default 1280×720), the stream host's and bridge's bounds. Refusals carry a
  reason: `409 "Add your Unity account on the Agents page to run Unity engine sessions."`, `409` no engine machine of
  yours, `404` unknown repository, `429` more than four open sessions.
- `GET /v1/projects/{p}/engine-sessions[/{id}]`: the member's own sessions (the owner token without a member sees
  all of the org's). When `ready`, **only the starting member's** read adds
  `stream:{relay_url, url, namespace, session, tag, width, height, audience:{token}, control:{control_key, token}}`
  (`control.token` only when the owner configured `MOQ_CONTROL_TOKEN`, below). The
  owner, other members and every other response never carry a token; the node's publish token never leaves the poll.
- `POST …/{id}/renew {lease, ttl_seconds}`, `POST …/{id}/end {}` (or `{lease}`).
- Owner: `GET /v1/engine-sessions`, `POST /v1/engine-sessions/{id}/end`.
- `POST /v1/contributors/{node}/engine-owner {member}` (owner token; the dashboard's `…/{id}/engine-owner` sends the
  claiming admin). A personal machine runs only its engine owner's sessions.

States: `queued → placed → preparing → starting → ready → ending → ended`, plus `failed` and `lost`. Placement is
lazy (on every read and poll): a queued session goes to an online, enrolled Windows node with `workloads.engine`, in
the session's org (`tenancy`), whose `engine_owner` is the starting member, that reports the requested Unity version
in its heartbeat and holds no other session (one per machine in v1). A lease that runs out moves a running session
to `ending` (the node is told to stop) and a queued one to `ended`; a node that goes offline, restarts (new epoch) or
does not confirm a stop within two minutes leaves its session `lost`. Records live in
`<state>/engine-sessions/<id>.json` (private) and never hold a minted secret; `engine.session_start`,
`engine.session_end` and `engine.machine_owner` are audited.

### Node channel

`POST /v1/node/engine-poll {session, epoch}` and `POST /v1/node/engine-report {session, epoch, id, generation, state,
error?, detail?}` run beside the single job slot and never touch it (`n.active`). Both require the node's current
session and epoch, an engine machine and a fresh heartbeat; reports must also match the session's node, placement
epoch and generation, and only move forward. The poll returns
`{sessions:[{id, generation, desired:"run"|"stop", state, lease_expires_at, spec, secrets?}]}` where
`spec = {repository, ref, project_path, engine, width, height, scene, namespace, session, tag}` and, only while the
session is `placed` on that epoch,
`secrets = {repository:{remote, token, auth, expires_at}, unity:{access_token}, moq:{relay_url, publish_token, control_key}}`.
Secrets are minted on demand and cached in memory per (session, generation) until the node reports progress:

- **Repository:** a hosted or GitHub-linked repository of the project gets a one-hour read token from
  `repositories.rs`. Otherwise (v1) an owner-created org `secret_source` connection named
  `engine-repo-<repository>` with `GIT_REMOTE_URL` (https, no credentials) and `GIT_READ_TOKEN` (sent as GitHub-style
  Basic `x-access-token`; a fine-grained read-only PAT works).
- **Unity:** `Credentials::unity_access_token(org, starting member)`; the password stays on the controller. No
  account: the start is refused, and if it disappears before delivery the session fails with the same message.
- **MoQ:** the platform org's `secret_source` connection `cloudflare-moq` with `MOQ_PUBLISH_TOKEN` and
  `MOQ_SUBSCRIBE_TOKEN` (operator tokens of the draft-16 Cloudflare relay, which the stream host fixes at
  `https://draft-16.cloudflare.mediaoverquic.com/<token>`). Each session gets its own namespace (`eb-<id>`),
  session UUID, tag and 32-byte control key. These operator tokens are relay-wide, not per namespace: anyone holding
  the subscribe token can watch any session's namespace. The per-session relay broker (below) replaces this.

The dashboard's node proxy allows `engine-poll` and `engine-report`.

### Windows executor (`packages/enginebox-install/src/engine.rs`)

`enginebox-node run-worker` on a machine with `workloads.engine` sweeps leftovers at start (processes recorded in
`config\engine-pids.json` that still match their creation time, and every `work\engine\sessions\*` directory), adds
`tools.engine {ready, unity:[versions], running, error?}` to its heartbeat, polls the engine channel every heartbeat and
reports in order. If the engine channel fails for a minute, it stops its sessions. For each session:

1. `git` (Git for Windows with git-lfs) checks out the exact ref into `work\engine\checkouts\<remote hash>`: `init`
   / `remote set-url`, `lfs install --local --skip-smudge`, `fetch --no-tags --depth=1 origin <ref>`,
   `checkout --detach -f FETCH_HEAD`, `clean -ffdq` (ignored files such as Unity's `Library` stay, so imports are
   reused), `lfs pull`. The token reaches git only as `GIT_CONFIG_KEY_n=http.<remote>.extraHeader` in the
   environment, with the credential helper disabled and prompts off; error text is redacted.
2. The project's `ProjectVersion.txt` must match the requested version (no upgrade dialog). `Packages/manifest.json`
   gains `dev.unityjam.editor-bridge` (`file:` the configured bridge) and `dev.enginebox.session` (`file:` a generated
   package whose editor script enters Play mode after import when Unity runs with `-enginebox-autoplay`; the bridge
   streams only in Play mode). The next session's `checkout -f` restores the manifest.
3. `bridge.json {port, token, scene, width, height, playOnly:true}` and `bridge.token` are written to
   `work\engine\sessions\<id>` (the node's owner-only directory).
4. `<Editor>\Data\Resources\Licensing\Client\Unity.Licensing.Client.exe --activate-all --accessToken <token>
   --include-personal`. **This is the one place a secret is in argv**; the client has no other input. Its output is
   discarded.
5. `Unity.exe -projectPath <project> -jam-editor-config <bridge.json> -enginebox-autoplay -logFile <session>\unity.log`
   in the node's own interactive session (the `enginebox` account, session 1); then the node polls the bridge's
   `/status` until `playing` and `view:"play"` (default limit one hour, `startup_timeout_seconds`).
6. `enginebox-stream-host.exe --moq-session <session>\moq-session.json` with
   `{schema:1, token:<publish>, controlKey, session, namespace, tag, durationSeconds:600, width, height,
   capture:{kind:"unity-editor", origin:"http://127.0.0.1:<port>", tokenFile, ffmpeg}}`. Its first
   `moq-source-running` line reports `ready`. The stream host's permit is fixed at 600 s and it also ends cleanly
   when no controlling client keeps it alive, so the node restarts it while the session runs (viewers reconnect);
   five quick non-zero exits in a row fail the session.
7. Stop, lease loss, an epoch change or a controller outage terminate the session's Job Object (every descendant),
   remove the session directory and write `config\engine-receipts\<id>.json` (commit, timings, outcome; no secrets),
   then report `ended` or `failed`.

### Configuring an engine machine

1. Register it as an engine machine (Compute page; PR #90's `-EngineAccount` installer) and, signed in as yourself,
   claim it (`POST /api/contributors/{id}/engine-owner {}`; the controller route is `…/engine-owner {member}`).
2. Write `engine.json` beside the node's `config` directory (`C:\ProgramData\Enginebox\engine-node\engine.json`),
   readable by the `enginebox` account:

   ```json
   {
     "schema": 1,
     "unity": {"6000.6.5f1": "C:\\Program Files\\Unity\\Hub\\Editor\\6000.6.5f1\\Editor\\Unity.exe"},
     "stream_host": "C:\\Enginebox\\engine-tools\\enginebox-stream-host.exe",
     "ffmpeg": "C:\\Enginebox\\engine-tools\\ffmpeg\\bin\\ffmpeg.exe",
     "editor_bridge": "C:\\Enginebox\\engine-tools\\unity-editor-bridge",
     "git": "C:\\Program Files\\Git\\cmd\\git.exe",
     "startup_timeout_seconds": 3600
   }
   ```

   Every path is absolute and must exist; each editor needs its bundled licensing client; `editor_bridge` must be the
   `dev.unityjam.editor-bridge` package; the stream host must be built with `--features moq`. Put the tools where the
   `enginebox` account can read them (not under another user's profile). `enginebox-node engine-check --config <file>`
   prints what the heartbeat will report. The file is re-read for each session.
3. `enginebox-node engine-rehearse --config <engine.json> --work <scratch dir> --spec <spec.json> --remote <url>`
   runs steps 1–3 against a scratch directory (a `file:///` clone works), starts the licensing client without a token,
   and proves the Job Object ends a process tree. It never activates Unity or publishes.

### Platform configuration

- `cloudflare-moq` (platform org, `secret_source`, provider `stored`): `MOQ_PUBLISH_TOKEN` (the node's),
  `MOQ_SUBSCRIBE_TOKEN` (viewers), and optionally `MOQ_CONTROL_TOKEN`, a separate relay token with publish rights for
  the starting member's browser to act as the controlling client (it publishes `<namespace>-input` liveness and input
  with the control key). Without a controlling client the stream host ends itself about 15 s after start
  (`control-liveness-expired`) and the node restarts it, so a steady stream needs `MOQ_CONTROL_TOKEN`.
- Each member saves their Unity account on the Agents page (PR #92).
- The repository is hosted or GitHub-linked in the project, or has an `engine-repo-<name>` connection.

### Qualified so far (RTX 4080 Windows machine, November 1, 2026)

- `enginebox-install` engine tests and clippy pass on Windows; `engine-check` accepts the machine's Unity
  6000.6.5f1, FFmpeg 7.1 (Gyan full), Gamejam's editor bridge and Git for Windows with git-lfs 3.5.1.
- `engine-rehearse` on a copy of Queen of Hearts: exact commit checked out with all 979 LFS files materialised in
  about 10 s, manifest gains both packages, the bundled licensing client runs, the session Job Object ends a
  process tree in milliseconds and the sweep removes the session directory. The original checkout was untouched.
- The Gamejam stream host (PR #38 branch, built with `--features moq`) accepted a session file in exactly the
  node's format with owner-only ACLs and published 535 VP8 frames (1.8 MB) from a stand-in bridge to the draft-16
  relay, then stopped cleanly on `control-liveness-expired`.
- Not yet run: Unity activation and the Editor (the owner's Unity account is not saved yet), a controller-driven
  session end to end, and a browser viewer.

### Known v1 limits

- One session per engine machine; no capacity measurement; no `UsageEvent`.
- Without `MOQ_CONTROL_TOKEN` streams are view-only and restart about every 15 s (see above). The control token is
  relay-wide, so hand it only to members you trust with the relay until per-session tokens exist.
- The stream host restart every ten minutes is visible to viewers.
- The node exits on a failed heartbeat (existing behavior); its process Job Object then ends every session.

## Design

An engine session is one tenant's game engine (Unity first; the API is engine-agnostic) running a given
repository and ref in its own container on a GPU node, streamed to the requesting app over MoQ. Gamejam is the first
client: its dev server, and later its Cloudflare deployment, starts sessions as an OAuth client of this controller.
Sessions run in a container (cloud) or under a dedicated engine account on the owner's own machine (personal).

## Foundations reused

- **Node side:** the contributor worker channel (`packages/enginebox-install/src/contributor.rs`,
  `packages/enginebox-controller/src/contributors.rs`, `/v1/node/*`): outbound connection, scoped node credential,
  epoch fencing. Its single job slot (`n.active`) cannot hold long-lived sessions, so engine sessions use a parallel
  channel (`engine-poll` / `engine-report`) that never touches `n.active`.
- **API and leases:** shaped like branch previews (`previews.rs`): the controller is the authenticated front door,
  leases are renewable, and the request never chooses host, image or command.
- The environment service (`packages/environment-service`) is not used: it is a single-host Linux supervisor with no
  GPU support.

## Measured GPU basis (RTX 4080 Windows node, WSL 3.0.3, driver 610.88)

A plain container given `--device /dev/dxg -v /usr/lib/wsl:/usr/lib/wsl:ro` and `LD_LIBRARY_PATH=/usr/lib/wsl/lib`
encodes H.264, HEVC and AV1 with NVENC and renders OpenGL 4.6 in hardware through Mesa `GALLIUM_DRIVER=d3d12`
under Xvfb and EGL surfaceless. Vulkan is unavailable (Ubuntu's Mesa ships no `dzn`), so the Unity Linux Editor runs
with `-force-glcore`. Unity 6000.6.5f1 Linux starts in such a container with all libraries resolved. These ran with
no Windows user signed in. Hyper-V is installed and the GPU is partitionable (32 partitions).

## API

Service tokens with an `engine_sessions` grant reach these routes (also added to the dashboard `RESOURCE_ROUTES`):

- `POST /v1/projects/{p}/engine-sessions`
  `{request_id, repository, ref, project_path, engine:{kind:"unity", version}, mode:"editor"|"play", scene?, width,
  height, ttl_seconds}` → `202 {id, state, lease, lease_expires_at}`; idempotent on `request_id`.
- `GET /v1/projects/{p}/engine-sessions[/{id}]`; when `ready`, adds
  `stream:{relay_url, namespace, session_uuid, tag, control:{token, control_key}, audience:{token}}`, only to the
  creating grant.
- `POST …/{id}/renew {lease, ttl_seconds}`, `POST …/{id}/end`. Owner: `GET /v1/engine-sessions`,
  `POST /v1/engine-sessions/{id}/end`.

States: `queued → placed → preparing → starting → ready → ending → ended`, plus `failed` and `lost`. Lease expiry or
the org hard cap ends a session.

## Admission

The org owns the node or holds a capacity grant with `capacity_class:"engine-gpu"` (`tenancy.rs`). Enforce
`max_concurrent_runs`, `cpu`, `memory_gib`, `monthly_hours` (first enforcement of these limits). Placement needs the
engine version installed, a qualified GPU profile and a free slot; slots = `gpus × sessions_per_gpu`; CPU/RAM
reservations fit the node budget minus its LayerFS workspaces.

## OAuth

Scope `engine-sessions`; consent row "Run engine sessions in project X"; grant `{kind:"engine_sessions", project}`.
Service tokens gain `engine_sessions` and operations `engine_sessions.start|get|renew|end`. Start also requires
repository `read`. Files: `service_tokens.rs`, `packages/dashboard/lib/oauth/{server,controller,store,pages}.ts`.

## Node protocol

`Workloads` gains `engine: bool` (`#[serde(default)]`); engine-only invitations validate. `engine-poll` returns
`{sessions:[{id, generation, desired:"run"|"stop", spec, secrets}]}`; secrets (repository read token, relay publish
token, control key) go only to the current epoch's node. Reports must match node + epoch + generation. A new `hello`
marks that node's sessions `lost`; on start the node removes every container labelled `enginebox.session`. Heartbeat
gains `engine:{gpu_profile, gpus, engines, images, slots, running}` within the 4 KiB tools limit.

GPU profiles are node-local, owner-set in `/etc/enginebox/engine.json` and qualified by a probe:
`wsl-dxg` (`--device /dev/dxg`, `/usr/lib/wsl` read-only, `GALLIUM_DRIVER=d3d12`) and `nvidia-ctk`
(`--gpus device=<uuid>`, `NVIDIA_DRIVER_CAPABILITIES=graphics,video,utility`).

## Container

- Image from an owner catalog (engine, version → digest, qualified profiles); the request never names one.
- `--user 10001 --read-only --cap-drop ALL --security-opt no-new-privileges --pids-limit 4096`, `--cpus/--memory`
  (swap = memory), `--shm-size 2g`, tmpfs `/tmp` and HOME.
- Mounts: `/project` (the session's own export), `/project/Library` (a Library slot), licensing identity (read-only),
  `/run/session.json` (0600).
- Network: one bridge per session, no inter-container traffic, egress to public addresses only (a `DOCKER-USER` rule
  drops RFC 1918 and the WSL host).
- Entrypoint: Xvfb → Unity `-force-glcore -projectPath /project` with the editor bridge
  (`dev.unityjam.editor-bridge`, added to the exported `Packages/manifest.json`) → `enginebox-stream-host` MoQ
  session with `unity-editor` capture.

## Licensing

One stable identity per (node, org, licence connection): generated `/etc/machine-id`, hostname and MAC, stored under
`/var/lib/enginebox/engine/licensing/<key>/`. A one-shot activator container with that identity runs
`Unity.Licensing.Client --activate-all --username --password --include-personal` when no valid activation exists or
the `unity-license` connection revision changed. Credentials reach only the activator (stdin), never a session.
Sessions mount the activated state read-only. Node revocation or licence replacement runs `--deactivate-all`.

## Checkout and Library cache

The controller mints a one-hour single-repository read token (`repositories.rs::mint_for`). The node keeps a bare
mirror and an LFS object cache per (org, repository) (`repo_source.rs` git env, `enginebox-storage-layerfs/src/lfs.rs`
verification) and exports the exact commit to `sessions/<id>/project` without `.git`. Library slots:
`library/<org>/<repo>/<path-hash>/<unity-version>/<n>`, `flock`-held, LRU-evicted under the disk budget.

## MoQ credentials

The controller holds the Cloudflare account ID and MoQ API token as the platform `secret_source` `cloudflare-moq`.
Per session, `moq.rs` (behind a `RelayApi` trait) creates one relay with three tokens: node publish, controlling
client publish+subscribe, shared audience subscribe; plus a 32-byte control key. End deletes tokens and relay; since
deletion does not close established peers, killing the container is the real teardown.

## Observability

`engine-sessions/<id>/events.jsonl`, audit entries for start/end/refusal, `UsageEvent engine_session_seconds{gpu}`.
Node receipt without secrets: checkout/LFS bytes, Library hit, import time, first keyframe, encoder fps/bitrate,
`nvidia-smi` samples, cleanup. Compute shows engine slots.

## Work plan

1. Protocol and capability: `enginebox-node-protocol/src/lib.rs`, `enginebox-install/src/wsl.rs`, `compute.rs`.
2. Controller store, API, fencing: new `engine_sessions.rs`; `lib.rs`, `service_allows`; `contributors.rs`.
3. Admission and quotas: `tenancy.rs`, `metering.rs`, `audit.rs`.
4. OAuth: `service_tokens.rs`, dashboard OAuth and `contributors.ts` allowlists.
5. MoQ relay broker: new `moq.rs` and the `cloudflare-moq` connection check.
6. Node executor: new `enginebox-install/src/engine.rs`, parallel loop in `contributor.rs`, `runtime.sh` network
   rules and slice sizing.
7. Licensing activator.
8. Gamejam: Unity container image, an H.264 track in MoQ protocol v1, renewable session leases in stream-host, the
   OAuth client in the dev server.
9. Docs and installer GPU allowance.

## Known conflicts to resolve

- `WINDOWS-WSL.md`: WSL is "not qualification for mutually hostile tenants"; containers sharing `/dev/dxg` and one
  kernel are soft isolation.
- Gamejam MoQ v1 is VP8-only; NVENC cannot encode VP8.
- stream-host's fixed outer permit cannot be extended by liveness.
- A controller blip stops the node's whole Docker daemon (`stop_runtime`), killing every session.
- WSL defaults (4 cores, 8 GiB, 50 GiB) cannot hold Unity (this project's Library alone is 5.4 GB).
- `deny_unknown_fields` on `Workloads`/`Invitation` and the `nodeActions` allowlist force a controller-first rollout.
