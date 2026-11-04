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
admission) is still design. OAuth landed on November 2 ("Who can call it"), and per-session relays with public watch
tokens the same day ("Per-session relays").

### Who can call it

- The owner token, and members through the dashboard (`/api/engine-sessions/{project}[/{id}[/renew|end|watch-token]]`, which
  sends the owner token with `X-EngineBox-Org` and `X-EngineBox-Actor` from the member's session).
- OAuth apps (Gamejam's hosted room) on the public origin, `https://<dashboard>/v1/projects/{p}/engine-sessions…`,
  with the `engine-sessions` scope ([OAuth](OAUTH.md#engine-sessions)). The member who consents ticks "Run live
  engine sessions in project X"; the grant (`{kind:"engine_sessions", project, member}` in `enginebox_grants`)
  records that member and their org. Its access token is a service token with `engine_sessions: [{project}]`,
  `member` and the operations `engine_sessions.start|get|renew|end|watch`; `service_allows` admits exactly the six
  project routes (start, list, read, renew, end, watch-token) on a granted project, and the controller takes the actor from the
  token's `member` (never from `X-EngineBox-Actor`) and the org from the token. So the token is that member: starts
  go to their own engine machine with their own Unity account, and only their reads carry the `stream` block.
  Other members' sessions are `404`; the owner-wide routes are `403` (and never forwarded by the proxy).
- Other service tokens are refused.

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
  (with a per-session relay, `audience.token` is a subscribe-only token and `control.token` a publish+subscribe token
  for that relay alone; on the shared relay they are `MOQ_SUBSCRIBE_TOKEN` and, when configured, `MOQ_CONTROL_TOKEN`).
  Reads also show `relay`, the session's relay id (null on the shared relay). The owner, other members and every
  other response never carry a token; the node's publish token never leaves the poll.
- `POST …/{id}/watch-token {}` → `200 {relay_url, url, token, expires_at, expires_in, namespace, session, tag, width,
  height}`: a subscribe-only token for the session's own relay, for viewers of a public room. No control key or
  control token. Callers: the owner token, the starter, and a token with `engine_sessions.watch` acting as the
  starter (Gamejam asks server-side for each public viewer). `409` when the session is not `ready` or has no relay of
  its own; others' sessions are `404`. See "Per-session relays".
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
  the subscribe token can watch any session's namespace. With the broker configured, each session gets its own relay
  instead and `publish_token` is for that relay only ("Per-session relays").

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
   package whose editor script would enter Play mode after import when Unity runs with `-enginebox-autoplay`; since
   November 3, 2026 sessions open the full Editor instead and the flag is not passed). The next session's
   `checkout -f` restores the manifest.
3. `bridge.json {port, token, scene, width, height, playOnly:false}` and `bridge.token` are written to
   `work\engine\sessions\<id>` (the node's owner-only directory). `port` is a free loopback port picked just before
   (bind `127.0.0.1:0`). Another program can take it before Unity binds; the bridge then retries its start on every
   Editor update and reads `bridge.json` again each time. So while the node waits for the bridge (step 5, and before
   every stream host start in step 6), a port that stays held for three checks (about 6 s) by something that does not
   answer the bridge's authenticated `/status` gets a new free port written into `bridge.json`, at most five times per
   session ("could not keep a local port" after that; `bridge_port_moves` in the receipt). Unity's own
   player-connection listener is left as it is.
4. `<Editor>\Data\Resources\Licensing\Client\Unity.Licensing.Client.exe --activate-all --accessToken <token>
   --include-personal`. **This is the one place a secret is in argv**; the client has no other input. Its output is
   discarded.
5. `Unity.exe -acceptSoftwareTermsForThisRunOnly -projectPath <project> -jam-editor-config <bridge.json> -logFile <session>\unity.log`
   in the node's own interactive session (the `enginebox` account, session 1); then the node polls the bridge's
   `/status` until it answers and is not compiling (default limit one hour, `startup_timeout_seconds`). The session
   opens in the Editor: the controlling client switches between the Editor and Play mode, saves the scene and reads
   the hierarchy and Inspector over its WebRTC peer, as the Unity Jam Games tab always did.
6. `enginebox-stream-host.exe --moq-session <session>\moq-session.json` with
   `{schema:1, token:<publish>, controlKey, session, namespace, tag, durationSeconds:600, width, height,
   capture:{kind:"unity-editor", origin:"http://127.0.0.1:<port>", tokenFile, ffmpeg, editor:true}}` (`editor`: the
   full editor bridge rather than play-only), where `<port>` is always the
   port the bridge is answering on at that start (the node waits up to two minutes for it). Its first
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
     "startup_timeout_seconds": 3600,
     "unity_region": "global"
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
- For per-session relays and public watching, the same connection also holds `CLOUDFLARE_MOQ_API_TOKEN` (a
  Cloudflare API token with MoQ edit on the account) and `CLOUDFLARE_ACCOUNT_ID`. Both or neither: one without the
  other fails every start with a clear reason.
- Each member saves their Unity account on the Agents page (PR #92).
- The repository is hosted or GitHub-linked in the project, or has an `engine-repo-<name>` connection.
- `engine-world-<project>` (org `secret_source`, `ROOM_ORIGINS`): the rooms OAuth apps' world sessions may name
  ([world sessions](#world-sessions-mode-world)).

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

### Unity licensing notes

- The licensing client writes its command line, access token included, to `%LOCALAPPDATA%\Unity\Unity.Licensing.Client.log`.
  The node replaces the token with `<redacted>` in those logs right after activation, whether or not it succeeded
  (`licensing_logs_scrubbed` in the receipt).
- **Editor terms.** A new engine account has never accepted Unity's Editor Software Terms, and the Editor waits at that
  dialog (window class `EditorSoftwareTermsWindowClass`) before writing its log. Before each launch the node reads the
  `Terms-<hash>` key the Editor build checks (from `Editor\Unity.dll`) and records it in the engine account's
  EditorPrefs (`HKCU\Software\Unity Technologies\Unity Editor 5.x`, `<key>_h<djb2-xor>` = 1), as Accept does; the member
  accepted the terms with the Unity account the session uses (`unity_terms_key` in the receipt). It also passes
  `-acceptSoftwareTermsForThisRunOnly`, which newer Editors honour.
- **Region.** Unity's config CDN (`public-cdn.cloud.unity3d.com/config/production`) answers by IP; from mainland China or
  Hong Kong it redirects to Unity China, whose license server does not know global Unity IDs (`Token not found in
  cache`). The global license, activation and identity hosts work from those networks. With `"unity_region": "global"`
  the node pins the licensing client's cached `%LOCALAPPDATA%\Unity\config\production.json` to the global services
  (a copy shipped with the node) and leaves it read-only (`unity_region_pinned` in the receipt).
- **Crash leftovers.** The node always stops a session's Editor, which leaves scene backups and a lock file in the
  project's `Temp` ("Scene Backup Detected" next time). The node removes `Temp` before each launch; imports stay in
  `Library`.
- **Blocking dialogs.** If a known blocking dialog ("Scene Backup Detected", "Enter Safe Mode?", "Project Upgrade
  Required", the Software Terms, …) stays up for 20 s during startup, the session fails with its title instead of
  waiting out `startup_timeout_seconds`.
- **Editor bridge.** Unity writes `.meta` files into `file:` packages, and a missing one in a read-only package keeps the
  Editor at "Opening project" forever. The node copies `editor_bridge` into its own `work\engine\packages`
  (keyed by content, refreshed when the bridge changes) and references the copy.

### Per-session relays (`packages/enginebox-controller/src/moq.rs`)

Cloudflare MoQ tokens are scoped to a relay and to operations (`publish`, `subscribe`), never to a namespace, so a
public viewer holding a relay-wide subscribe token could watch every session. With `CLOUDFLARE_MOQ_API_TOKEN` and
`CLOUDFLARE_ACCOUNT_ID` on `cloudflare-moq`, every session gets a relay of its own:

- **Creation.** When the node first polls a placed session, the controller creates a relay
  (`POST /accounts/{account}/moq/relays {name:"enginebox-session-<id>"}`), revokes the two year-long tokens Cloudflare
  creates with it, and mints the node's publish+subscribe token (7 days; deleting the relay ends it sooner). The node
  receives it in `secrets.moq.publish_token` as before; `relay_url` is unchanged, because a token's `sub` claim names
  its relay and every relay is reached at `https://draft-16.cloudflare.mediaoverquic.com/<token>` (checked against
  Cloudflare on November 2: a token of one relay receives that relay's objects, a token of another relay does not, and
  a revoked token is refused).
- **If creation fails, the start fails** (`failed`, "Engine Box could not create this session's MoQ relay
  (Cloudflare MoQ relay creation failed (HTTP …)). Start the session again."). There is no fall back to the shared
  relay: its subscribe token would let a public viewer watch every session. Without the two fields at all, sessions
  keep using the shared relay as before, and `watch-token` answers `409`.
- **Tokens.** The starter's read mints a subscribe-only and a publish+subscribe (control) token for the relay, 4 hours
  each, reused while an hour remains. `watch-token` returns one shared subscribe-only token, 30 minutes, replaced once
  less than 10 minutes remain, so every answer is good for at least 10 minutes; viewers ask again to refresh. A replaced
  token keeps working until it expires and is then revoked. Cloudflare holds at most ten tokens per relay; the
  controller revokes expired ones first and keeps at most eight (never the node's).
- **Release.** When the session ends, fails or is lost, the controller deletes the relay (which revokes every token);
  a failed deletion is retried a minute later on the next request. Deleting a relay does not close established peers,
  so stopping the node's stream host remains the real teardown.
- **Records.** `<state>/engine-sessions/relays/<session>.json` holds the relay id, the ids (`jti`), roles and expiry of
  the tokens minted on it and when it was released; never a token. It sits beside the session record so releases that
  predate relays still read `engine-sessions/<id>.json`. Token secrets live only in controller memory: after a restart
  the starter and watch tokens are minted again.
- Errors and logs carry HTTP status and Cloudflare error codes only, never a token or a response body.

### WebRTC fallback (`packages/enginebox-controller/src/engine_sessions/peers.rs`, `turn.rs`)

Added November 3, 2026. Browsers reach the MoQ relay over WebTransport, which needs QUIC on UDP 443. Some networks drop
exactly that: measured through ExpressVPN (Lightway, Network Lock), forced-QUIC loads of any site fail while TCP loads
work, and Cloudflare TURN answers on udp:3478, tcp:80/3478, tls:5349 and tls:443 but not udp:443. Such viewers fall back
to WebRTC with media relayed through **Cloudflare TURN on both sides** (`iceTransportPolicy: "relay"`, TURN over TLS on
443 included). MoQ stays the primary path; the engine machine still opens no port.

- **Configuration.** The platform org's `secret_source` connection `cloudflare-turn` with `TURN_KEY_ID` (32 hex) and
  `TURN_API_TOKEN` (the TURN key's API token). Both or neither; without it nothing below is offered and peers are `503`.
- **Viewers** (owner token, the starter, or an OAuth token with `engine_sessions.watch` acting as the starter; same
  visibility as `watch-token`; the session must be `ready`):
  - `POST …/{id}/peers {mode:"watch"|"control"}` → `201 {peer:{id, mode, state, expires_at}, client_token, ice_servers,
    ice_transport_policy:"relay", ice_expires_at, width, height}`. Control is the starter's (or the owner token's), and a
    new control peer ends their previous one. At most 8 open peers per session (`429`), 64 overall. ICE servers are
    minted per peer for an hour (Cloudflare STUN/TURN URLs only, port 53 dropped).
  - `POST …/peers/{pid}/offer {client_token, sdp}` (one complete offer, ≤ 64 KiB), `…/poll {client_token}` →
    `{peer, answer}`, `…/renew {client_token}` (lease 60 s, capped at the ICE expiry), `…/end {client_token}`. A wrong
    `client_token` is `403`; an ended peer `410`.
- **Stream host** (`Authorization: Bearer <host token>`, through the dashboard at `/api/node/engine-peers/…`, which
  forwards to the controller's `/v1/node/engine-peers/…`): `POST poll {}` → `{now, sessions:[{id, project, nodeId:<engine
  session>, nodeEpoch:<generation>, mode, state, expires, iceServers, iceTransportPolicy, iceExpires}]}` (the stream host's
  machine-session shape), `GET {pid}/signals?after=N` (the offer as signal 1), `POST {pid}/signals {id, signal:{type:
  "answer", sdp}}` (one answer, idempotent on `id`; a different second answer ends the peer, `410`), `POST {pid}/end
  {id}`. A bad token is `401`; refusals about one peer are `410` so the host keeps its other peers.
- **Host token.** With TURN configured, the node's `secrets` gain `webrtc:{host_token}` (`ehp_<64 hex>`), minted once per
  session generation. Only its SHA-256 is kept, on the session record (`peer_host_hash`). The node writes it, the
  controller origin, project, session id and generation into the owner-only `moq-session.json` as `webrtc`, and the
  stream host polls about every second while MoQ runs. A connected control peer keeps the stream host's control
  liveness, as a MoQ controlling client does.
- **Lifetime.** Peers live in controller memory only: a restart, the session leaving `ready` or changing generation,
  a lapsed lease, or either side ending it ends the peer, and the viewer opens a new one. The stream host's ten-minute
  restart ends its peers too.
- `engine.peer_open` is audited with the mode.

### World sessions (`mode: "world"`)

Added November 4, 2026, for Gamejam's hosted shared world. A world session runs a room's **world host**
(Gamejam's `gamejam-world-host`) on the member's engine machine instead of the Editor and stream host. The world host
dials the room (`wss://<room>/engine/host`), builds Windows players with the activated Editor in batch mode and runs
them inside the session's Job Object; viewers' media goes browser ↔ player through Cloudflare TURN. The engine machine
still opens no port, and no MoQ relay is created.

- **Start.** `POST /v1/projects/{p}/engine-sessions` with the usual `request_id`, `repository`, `ref`, `project_path`
  and `engine`, plus `mode:"world"` and `world:{room_origin, enroll_code}`. For Gamejam: repository `gamejam`, ref
  `main`, `project_path:"unity-examples/multiplayer-jam"`. `mode` defaults to `editor`; `world` is refused without
  `mode:"world"` and required with it.
  - `room_origin` is exactly an https origin (`https://host[:port]`, lower case, no path, slash, query or
    credentials).
  - **Allowed rooms.** The owner token (and members through the dashboard) may name any https origin. A service token
    (an OAuth app such as Gamejam's Account) may name only an origin the owner listed for the project: an org
    `secret_source` connection **`engine-world-<project>`** yielding **`ROOM_ORIGINS`**, comma- or space-separated
    https origins (invalid entries are ignored). Anything else is `403` naming that connection, and is audited.
    To allow Gamejam's room in project `queen-of-hearts`:

    ```json
    POST /v1/connections
    {"name": "engine-world-queen-of-hearts", "kind": "secret_source",
     "source": {"provider": "stored", "environment": "production"},
     "secrets": {"ROOM_ORIGINS": "https://gamejam.coopcollective.dev"}}
    ```

    (create it in the org the app's grant acts for; edit or delete it to change or revoke rooms; it is read on every
    start).
  - `enroll_code` is the room's one-time host enrollment code, 32–128 letters, digits, `-` or `_`. It is held **only in
    controller memory** and delivered only in the node's secrets: never in the session record, the request digest,
    reads, lists, audit or logs. A retry with the same `request_id` and a different code is `409`. A controller restart
    before the node received it fails the session (`WORLD_CODE_LOST`: "start the world again"); the code is dropped when
    the session ends.
  - Audited as `engine.world_start` (with `room_origin`, never the code), refusals included.
- **Reads** carry `mode` and `world:{room_origin}`; there is no `stream` block, and `watch-token` and `peers` answer
  `409` for a world.
- **ICE.** `POST …/{id}/ice {}` → `200 {ice_servers, ice_transport_policy:"relay", expires_at}` (unix seconds, one
  hour) from the platform `cloudflare-turn` connection (Cloudflare STUN/TURN URLs only, TURN over TLS 443 included,
  port 53 dropped). Owner token, the starter, or an OAuth token with `engine_sessions.watch` acting as the starter, for a
  world that is not ending or over. Without TURN it is `503`; an editor session is `409`. The room hands these to its
  players (`join`) and browsers (`session`), both with `iceTransportPolicy:"relay"`.
- **Node channel.** The poll's `spec` carries `mode` (editor sessions too) and, for a world, `world:{room_origin}`;
  `secrets` are `{repository, unity, world:{enroll_code}}` (no `moq`, no `webrtc`). Older nodes cannot parse a world's
  secrets and fail the session rather than start an Editor.
- **Node.** `engine.json` gains optional `"world_host": "C:\\Enginebox\\engine-tools\\gamejam-world-host.exe"`
  (absolute, existing, named `gamejam-world-host[.exe]`); the heartbeat's `tools.engine.world` is `true` when it is set.
  A world session checks out the ref and activates Unity exactly as above (steps 1, 3–4; no packages or bridge), then:
  1. Lends the persistent build cache `work\engine\world-builds` to the session (a rename into
     `work\engine\sessions\<id>\world\world-builds`) and moves it back after the session's processes end; the startup
     sweep moves it back too, before removing session directories.
  2. Writes the owner-only `work\engine\sessions\<id>\world-session.json`:
     `{schema:1, room_origin, enroll_code, engine_session:<id>, generation, unity_exe:<engine.json's Unity for the
     version>, project_dir:<checkout>/<project_path>, work_dir:<session>\world}`.
  3. Starts `world_host --session <that file>` (nothing else in argv) in the session's Job Object, working directory
     `work_dir`, stdout read line by line, stderr discarded. The first line that is the JSON object
     `{"event":"world-host-connected"}` reports `ready`.
  4. Restarts the host whenever it exits while the session runs, after a second; five quick (under 30 s) non-zero
     exits in a row fail the session, like the stream host. The enrollment code is one-time, so a restarted host must
     re-enroll with credentials the room accepts (the room's contract).
  Stop, lease loss and epoch changes end the Job Object as for editor sessions. Placement is unchanged: one session per
  engine machine, so a world and the Live Editor do not run at once; a machine without `world_host` fails a world it is
  given with a clear reason.

### Agent forks (`packages/enginebox-controller/src/agent_forks/`)

Added November 4, 2026 (controller side; the node side is a separate change). Room members run their own coding agent
(Claude Code or Codex) on a **fork** of a live editor session's dev tree; a **submission** freezes the fork's head,
checks it, waits for the room owner if any path is code, three-way merges it into the dev head and applies it to the
live tree, which the Editor hot-refreshes. Owner decisions (2026-11-04): other members' agents may run on the owner's
Windows machine only in isolated slot accounts, at most three at once, never with the owner's keys; Gamejam Account
vouches for the signed-in user per request on agent routes only, and that person's own Xava Inference (xinf)
connection pays; a merge that breaks compilation is reverted automatically and the author and owner see the compiler
errors; builders edit the shared scene live, and code goes through forks and owner approval.

**Who calls.** The owner token (the dashboard names the member), and OAuth apps with the `agent-forks` scope
([OAuth](OAUTH.md#agent-forks)), whose token acts as the consenting room owner, or as a **room principal** the app
vouches for (below). A member reaches only sessions they started; a room principal reaches only the grant member's
sessions in the grant's project, sees and drives only its own forks (others' forks are `404`; the list shows them only
as claims: owner name, title, state, intended paths), uses only its own xinf connection, and never approves, rejects,
reverts or publishes (`403`). `viewer` principals read the list and dev status only.

**API.** Base `/v1/projects/{p}/engine-sessions/{sid}`; ids are 32 hex; errors `{error}` plus `code` where a client
acts on it. Mutations are JSON (≤ 64 KiB); reads take no body. Only the listed queries are accepted.

| Route | Who | Body → result |
|---|---|---|
| `POST …/forks` | builder | `{request_id, prompt (≤ 16 KiB), provider:"claude"\|"codex", model?, budget_usd? (0.1–20, default 2), title?, paths?:[≤32 globs]}` → `202 {fork:{id, state:"creating", base}, run:{id, state:"queued"}}`; `409` with `code` `gateway_required`, `insufficient_balance` (+`topup_url`), `fork_limit` (3 open forks per person per session), `forks_unsupported` (the machine reports no fork provider), `isolation_required` (the machine has no slot accounts: only the room owner). Idempotent per `request_id`. |
| `GET …/forks[?all=1]` | anyone in the room | `{forks:[…]}`: own forks in full (`id, owner{principal,name}, title, state, base, head, paths, paths_touched_count, updated_at`), others' open forks as claims; `all=1` (room owner) lists everything in full |
| `GET …/forks/{fid}` | fork owner, room owner | `{id, state, base, head, changed:[{path, status, class, bytes, lfs}], runs:[{id, state, provider, model, budget_usd, spent_usd, requests, started_at, ended_at, error}], submission, rebase_conflict, has_diff, …}` |
| `GET …/forks/{fid}/diff[?path=&limit=]` | same | `text/x-diff` (≤ 2 MiB; one file with `path`) |
| `POST …/forks/{fid}/runs` | fork owner | `{request_id, prompt, resume?, budget_usd?}` → `202 {run}`; `resume` continues the agent's native session |
| `GET …/forks/{fid}/runs/{rid}/events[?after=N]` | fork owner, room owner | `{events:[{seq, at, kind:"text"\|"tool"\|"status"\|"usage", data}], next}` (redacted, newest 512 KiB kept) |
| `POST …/forks/{fid}/runs/{rid}/cancel` | fork owner, room owner | `{}` → `{run}` |
| `POST …/forks/{fid}/submit` | fork owner | `{request_id, message, expected_head, auto_merge?:["scene","asset","config"]}` → `202 {submission:{id, state:"checking"}}` |
| `POST …/forks/{fid}/rebase` | fork owner | `{request_id}` → `202 {fork}` (onto the current dev head) |
| `POST …/forks/{fid}/close` | fork owner, room owner | `{keep_snapshot?}` → `{fork}` |
| `GET …/submissions[?state=]` | room owner: all; others: own | `{submissions:[{id, fork, owner, message, state, fork_head, classes:{scene, asset, config, code, protected, refused}, findings:[{rule, path, line?}], needs_approval, approved_by, reason, conflict_paths, held_reason, dev_head_before, dev_head_after, reverted, …}]}` |
| `POST …/submissions/{id}/approve` | room owner | `{request_id, expected_fork_head}` → `202 {submission}` (merged against the then-current dev head) |
| `POST …/submissions/{id}/reject` | room owner | `{reason?}` → `{submission}` |
| `GET …/dev` | anyone in the room | `{head, applied, applied_at, pending:[ids], held?:{reason}, compile:{state, errors:[{file, line, message}]}, live_code_pending:[paths], last_revert, notices:[…], published}` |
| `POST …/dev/revert` | room owner | `{request_id, submission}` → `202 {submission}` (a new revert commit through the apply path) |
| `POST …/dev/publish` | room owner | `{request_id, branch:"jam/<name>"}` → `202 {op_id}`; needs a repository Engine Box hosts or links (a one-shot write token goes to the node only for this op), else `409 publish_unavailable` |
| `POST /v1/projects/{p}/gateway/connect` | anyone | `{return_to}` → `{authorize_url}` |
| `GET\|PUT\|DELETE /v1/projects/{p}/gateway` | anyone (own only) | `{connected, kind:"oauth"\|"key", status, monthly_cap_usd, balance_usd?, spent_this_month_usd?, remaining_budget_usd?, topup_url}`; `PUT {key:"xk_live_…"}` (write-only); `DELETE` disconnects (revokes at xinf) |

States. Fork: `creating → idle ⇄ running → submitted → idle`, `closing → closed`, `failed` (session ended, moved to
another generation, or the node could not create it). Run: `queued → dispatched → running → succeeded | failed |
cancelled`. Submission: `checking → rejected | awaiting_approval | merging → applying → applied | held | conflict |
failed`; `held` (Play mode, unsaved overlapping scene, live edits) becomes `applied` when the node reports the commit
on disk. A fork with an open submission takes no runs.

**Limits.** 3 open forks per person per session; 3 concurrent runs per engine machine (fewer if the node reports fewer
slots), dispatched round robin by person (the person served longest ago first); run budget $2 default, $20 max
(enforced by the node's relay from xinf's per-request cost); 30-minute run limit: after it (plus a minute) the run is
failed, its bearer is no longer delivered and the node gets a `cancel` op.

**Checks on submit** (controller authoritative; `classify.rs`). Each changed path is classified, first match wins
(`P` = the session's `project_path`):

| Class | Paths | Merge |
|---|---|---|
| refused | invalid or Windows-hostile names (`..`, `\`, `:`, reserved names, trailing dot/space), a `.git` component, symlinks/submodules (modes 120000/160000), credential paths (`.ssh`, `.aws`, `.azure`, `.claude`, `.codex`, `.cursor` directories; `.env*` except `.example/.sample/.template`; `*.pem/.key/.pfx/.p12`; `id_<x>` key files; `.npmrc`, `.pypirc`, `.netrc`, `auth.json`, `*credentials*.json`), `P/ProjectSettings/ProjectVersion.txt`, `P/{Library,Temp,Logs,obj,UserSettings}/**`, anything outside `P` except root `README*` and `docs/**` | never |
| protected | `.gitattributes`, `.lfsconfig`, `.gitmodules`, `.gitignore` anywhere, root `.github/**`, `package.json` and lockfiles outside `P`, files in `P` outside `Assets/`, `ProjectSettings/`, `Packages/` | room owner |
| code | `*.cs`, `*.asmdef`, `*.asmref`, `*.rsp`, `*.dll`, `*.so`, `*.dylib`, `*.bundle/**`, `*.jslib`, `*.jspre`, `*.mm`, `*.m`, `*.cpp`, `*.c`, `*.h`, `*.java`, `*.kt`, `*.aar`, `*.jar`, `*.py`, `*.ps1`, `*.sh`, `*.bat`, `*.exe`; anything under an `Editor/` or `Plugins/` directory; `P/Packages/**`; the `.meta` of any of them | room owner |
| config | `P/ProjectSettings/**`, `*.inputactions` | automatic |
| scene | `*.unity`, `*.prefab`, `*.scenetemplate` | automatic |
| asset | everything else under `P/Assets/`, root `README*`, `docs/**` | automatic |

Then: files over 100 MiB not in LFS, more than 5 000 paths or more than 500 MiB added are refused; the node's
findings are merged with the controller's own **secret scan** of the fork's diff artifact (added lines: `xk_live_`,
`xat_/xrt_/xak_`, `ebr_`, `ebst_`, `ehp_`, `sk-ant-`, `sk-`, GitHub tokens, `AKIA…`, private key blocks, JWT shapes,
high-entropy 40+ character strings in `*.json`, `*.asset`, `*.txt`, and an exact match of every bearer Engine Box
delivered to that fork's runs). Any finding or refused path rejects the submission (findings name rule, path and line,
never the value). Code or protected paths, or an automatic class the caller left out of `auto_merge` (room policy),
wait for the room owner; the owner's own submissions too. An empty change set is rejected.

**Compile failures (owner decision 3).** When `engine-report`'s `detail.dev.compile` says `errors` for the applied
head, that head is not the last head that compiled, and an applied (or held) submission produced it, the controller
queues a `revert` op for that submission (once), records `reverted:{state:"reverting", auto:true, reason, errors}` on
it and a `notices` entry `{kind:"auto_revert", submission, author, errors}` on the dev status, and audits
`agent_fork.revert` (actor `enginebox`). Compile errors from live edits (no submission produced the head) are only
reported. A revert the node cannot apply leaves a `revert_failed` notice for the owner.

**Model access: each person's own xinf connection** (`gateway.rs`). Engine Box is a confidential OAuth client of
Xava Inference. `POST …/gateway/connect {return_to}` answers xinf's authorize URL (`scope=inference account:read`,
`resource=https://xavainf.com`, PKCE S256, a one-time `state` held in memory for ten minutes); the person consents on
xinf and picks a monthly cap; xinf returns the browser to `https://<engine box>/v1/gateway/xinf/callback`, which the
dashboard forwards to the controller (`POST /v1/gateway/xinf/callback {state, code|error}`, owner token only); the
controller exchanges the code and 303s the browser to `return_to?xinf=connected|denied|failed`. `return_to` must be
on one of the app's registered redirect origins (the dashboard's origin for owner-token callers). Credentials are
stored per (org, person) in `agent-credentials/gateway/<digest>.json`, sealed with AES-256-GCM under
`agent-credentials/gateway-seal.key` (created on first use): no record, read, log or audit entry holds a token or
key. Room principals are keyed by user and app, so one connection serves all of that app's rooms. Runs use only the
person's own connection (never shared, team, API-key or controller accounts, never the owner's); without one the fork
is refused `gateway_required`, and an xinf balance or app cap under $0.05 is `insufficient_balance` with xinf's top-up
URL. A refresh xinf refuses marks the connection `reconnect`. Phase 2 (not implemented): org members' own Claude or
Codex subscriptions first.

**Room principals** (`principal.rs`). The app (Gamejam Account) registers an Ed25519 public key on its OAuth client
(`enginebox_principal_key`); every access token minted for the grant carries it. Per request the app sends:

```
EngineBox-Principal:       gj:<room>:<user>        (each 1-64 of A-Z a-z 0-9 _ -)
EngineBox-Principal-Name:  <percent-encoded UTF-8, 1-64 characters>
EngineBox-Principal-Role:  builder | viewer
EngineBox-Principal-Time:  <unix seconds>
EngineBox-Principal-Nonce: <16-64 of A-Z a-z 0-9 _ ->
EngineBox-Principal-Sig:   base64url(Ed25519(private key, message)), no padding
message = join("\n", "enginebox-principal-v1", METHOD, path, raw query or "", hex(sha256(body bytes or "")),
               principal, name header as sent, role, time, nonce, access token id (the 32 hex after "ebst_"))
```

The controller verifies the signature with the grant's key, refuses assertions more than 60 s from its clock and any
nonce already accepted for that key in the window (memory; a restart forgets nonces but the window still bounds
replays), and accepts the headers only on agent-fork routes of the grant's project (`400` elsewhere; `403` when the
app registered no key). Without the headers the grant's member acts. Engine Box holds only the public key, so nothing
on Engine Box can mint an assertion.

**Node protocol** (the node side implements it; engine channel, fenced by node, epoch and the fork's generation, which
is the session generation the fork was created in):

- `engine-poll`: each session row gains `forks:[{id, generation, desired:"open"|"closed", base, keep_snapshot,
  ops:[Op]}]` and `dev:{capture_on_save:true, head, ops:[publish ops]}`. A fork stays listed while it is open or has
  ops (a closed fork's `revert` op). `Op = {op_id, kind, …payload}`:
  `run:{run_id, provider, model, prompt, resume_id, timeout_seconds, budget_usd}` plus, only while the run is
  dispatched or running, `secrets:{gateway:{access_token, expires_at, base_url, kind:"oauth"|"key"}}` (re-delivered,
  refreshed when under 15 minutes remain; `expires_at` 0 for keys);
  `snapshot:{expected_head, submission}` (freeze for a submission: the node must report the same head when the tree is
  unchanged); `merge:{submission, fork_head, expected_dev_head, message, approved_by}`; `rebase:{onto}`;
  `revert:{submission, commit, parent, expected_dev_head, message, auto}` (three-way: undo `commit` against the dev
  head, i.e. merge-tree with merge base `commit`, theirs `parent`); `cancel:{run_id}`;
  `publish:{branch, head, request_id}` with `secrets:{repository:{remote, token, auth, expires_at}}`.
- `POST /v1/node/fork-report {session, epoch, fork, generation, op_id?, state, error?, detail?}`. Without `op_id`,
  fork state `idle` (created; `detail.base`, `detail.head`), `failed` or `closed` (acknowledged). With `op_id`, op
  state `running` (a run started), `applying`/`held` (merge in the dev head, not yet on disk) or final `done`,
  `failed`, `conflict`, `cancelled`. `detail`: `head`, `base`, `changed:[{path, status:"A"|"M"|"D"|"R", mode (100644 or
  "100644"), lfs, bytes}]` (the node's `class` is ignored), `findings:[{rule, path, line?}]`,
  `run:{native_session_id, spent_usd, requests, exit}`, `merge:{dev_head, conflict_paths, stale}` (`stale:true` with
  the current `dev_head` when the compare-and-swap lost: the controller retries up to three times),
  `apply:{applied, held_reason}`, `conflict_paths` (rebase). Repeated reports of a finished op answer `{done:true}`.
  Up to 1 MiB.
- `POST /v1/node/fork-events {session, epoch, fork, generation?, run_id, events:[{kind, data, at?}]}` (≤ 1000 per call;
  each `data` ≤ 16 KiB; redacted again here).
- `POST /v1/node/fork-artifact {session, epoch, fork, generation?, head, kind:"diff", body}` (≤ 2 MiB; scanned for
  secrets before it is stored; Engine Box's own bearers are redacted from the stored copy).
- `engine-report`'s `detail.dev:{head, applied, held, compile:{state:"ok"|"compiling"|"errors", errors:[{file, line,
  message}] (≤ 50), head?}, live_code_pending:[paths], publish:{op_id, state:"done"|"failed", commit, error}}` (≤ 64
  KiB, outside the 4 KiB session detail bound; send it with the session's current state).
- The node advertises forks in its heartbeat: `tools.engine.forks:{provider:"git"|"layerfs", slots, free_slots,
  isolation:"accounts"|"sandbox"|"none"}`. Without it forks are refused; with `isolation:"none"` only the room owner
  runs agents.
- The dashboard node proxy and the supervisor's node gate allow the three new routes (the supervisor change needs a
  supervisor upgrade, not just a controller handoff).

**Records.** `engine-sessions/forks/<fork>.json`, `engine-sessions/fork-submissions/<id>.json`,
`engine-sessions/dev/<session>.json`, run events `engine-sessions/fork-events/<fork>-<run>.json`, diffs
`engine-sessions/fork-diffs/<fork>.diff` (directories older releases do not read). Run prompts sit in the fork record
until the node finishes the op; bearers and write tokens are never written. Audit: `agent_fork.create`, `.run`,
`.submit`, `.approve`, `.reject`, `.apply`, `.revert`, `.publish`, `.gateway`.

**Platform configuration.** Register Engine Box at xinf as a confidential OAuth client (redirect URI
`https://enginebox.coopcollective.dev/v1/gateway/xinf/callback`, scopes `inference account:read`) and store it in the
platform org's `secret_source` connection **`xava-inference`**: `XINF_CLIENT_ID`, `XINF_CLIENT_SECRET`,
`XINF_REDIRECT_URI`. Without it OAuth connects answer `409`; pasted keys still work.

**Not yet.** The node side (fork provider, slot runner, relay) is a separate change; per-run xinf child keys (the relay
forwards the person's access token); notifications beyond the dev status' `notices` and the submissions list (the app
polls them); org members' own Claude/Codex subscriptions (phase 2); automatic publish at session end; multi-member
live scene control.

### Known v1 limits

- One session per engine machine; no capacity measurement; no `UsageEvent`.
- On the shared relay (no broker configured), streams without `MOQ_CONTROL_TOKEN` are view-only and restart about every
  15 s (see above), and the control token is relay-wide, so hand it only to members you trust with the relay. Per-session
  relays give every starter a control token for their own relay.
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

Implemented as described under "Who can call it" and in [OAuth](OAUTH.md#engine-sessions): scope
`engine-sessions`; consent row "Run live engine sessions in project X"; grant `{kind:"engine_sessions", project,
member}`; service tokens gain `engine_sessions`, `member` and operations `engine_sessions.start|get|renew|end`. The
grant does not also require a repository grant: the controller checks the repository in the token's org at start
and mints the node's read token itself.

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

Implemented as "Per-session relays" above (`moq.rs`, the `Broker` trait): the platform `cloudflare-moq` connection
holds `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_MOQ_API_TOKEN`; each session gets one relay with a node publish token, the
starter's subscribe and control tokens, and a rotating public watch token; ending deletes the relay.

## Observability

`engine-sessions/<id>/events.jsonl`, audit entries for start/end/refusal, `UsageEvent engine_session_seconds{gpu}`.
Node receipt without secrets: checkout/LFS bytes, Library hit, import time, first keyframe, encoder fps/bitrate,
`nvidia-smi` samples, cleanup. Compute shows engine slots.

## Work plan

1. Protocol and capability: `enginebox-node-protocol/src/lib.rs`, `enginebox-install/src/wsl.rs`, `compute.rs`.
2. Controller store, API, fencing: new `engine_sessions.rs`; `lib.rs`, `service_allows`; `contributors.rs`.
3. Admission and quotas: `tenancy.rs`, `metering.rs`, `audit.rs`.
4. OAuth: `service_tokens.rs`, dashboard OAuth and `contributors.ts` allowlists.
5. MoQ relay broker: `moq.rs` (done November 2, "Per-session relays").
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
