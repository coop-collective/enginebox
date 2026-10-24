# Container stream host

`enginebox-stream-host` connects an owned Linux container to an Enginebox fleet. The fleet authorizes project-scoped viewers; this process captures its own Xvfb display and sends VP8 video over WebRTC. Control uses the same peer's authenticated data channel. No public listening port, host desktop capture, Docker socket, provider credential, or home-directory mount is required.

The portable profile uses software rendering and a 30fps capture target. Actual decoded FPS is measurable in the viewer; this target is not a claim about the game's render rate. This initial profile is **video-only**. Linux GPU passthrough, Windows native capture, and macOS GPU passthrough are not qualified by this image. Docker Desktop on macOS/Windows runs the Linux CPU profile inside its Linux VM.

## Build and configure

Build the generic image from this directory, selecting `--target runtime`. It contains the host, FFmpeg, Xvfb, Mesa and `tini`; add an engine and trusted project in a derived image. The optional `container/Godot.Dockerfile` provides the repository-owned Godot profile.

```sh
docker build --target runtime -t enginebox-stream-host:local packages/stream-host
```

Copy `example.json` and configure the controller origin, project, absolute executable, argument array, and working directory. The browser/controller cannot supply commands or filesystem paths. `width`/`height` must match the owned application's viewport (default 1280×720, bounds 160×90 through 1920×1080); `maxPeers` is 1–8, default 4. Only explicit `allowLocalController:true` enables HTTP for disposable local development. Production uses HTTPS with normal certificate verification and no redirect following.

The engine command runs with a cleared environment. Only owned HOME/XDG_RUNTIME_DIR, PATH, DISPLAY, software-rendering settings, and explicitly allowlisted locale/.NET/SDL settings are supplied. Xvfb always starts a fresh display, disables TCP, and does not use inherited DISPLAY. Its Unix socket is inside the container's namespace. Do not mount a host X socket or host `/tmp` into this image.

## Enroll and run

The native host accepts a short-lived invitation on **stdin**, as either the invitation string or JSON `{controller,project,invitation}`. It generates its host secret locally, writes a mode 0600 identity atomically before redeeming the invitation, and retries the same identity after a lost acknowledgement. The private state directory is mode 0700 and exclusively locked while a process owns it. An existing identity is bound to its original controller and project.

```sh
docker volume create enginebox-stream-state
# Save the invitation outside the repository with private permissions, or pipe it
# directly from the Account service. Never place it in command-line arguments.
docker run --rm -i --read-only --cap-drop ALL --security-opt no-new-privileges \
  --tmpfs /tmp:rw,nosuid,nodev,size=256m \
  --mount type=volume,source=enginebox-stream-state,target=/var/lib/enginebox-stream \
  --mount type=bind,source=/absolute/stream-host.json,target=/etc/enginebox/stream-host.json,readonly \
  enginebox-stream-host:local --enroll-only < /private/invitation.json

docker run -d --name enginebox-stream --restart unless-stopped \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  --pids-limit 256 --memory 4g --cpus 4 --tmpfs /tmp:rw,nosuid,nodev,size=512m \
  --mount type=volume,source=enginebox-stream-state,target=/var/lib/enginebox-stream \
  --mount type=bind,source=/absolute/stream-host.json,target=/etc/enginebox/stream-host.json,readonly \
  enginebox-stream-host:local
```

Use a derived image containing the configured engine/project for these commands. The binary CLI is `enginebox-stream-host [--enroll-only] [config.json] [private-state-directory]`; `--version` needs no credentials. `--enroll-only` exits before hello or engine launch. Keep the state volume on restart; create a new invitation only when deliberately registering a new host. Normal `docker stop enginebox-stream` stops only this owned container; removing its volume discards its identity.

The engine and daemon are trusted relative to each other inside this single-user container: an engine running as the same UID can read the project-scoped native identity. This is not an isolation boundary for hostile game code. The identity has no owner, provider, storage, job-execution, or other-project authority. Stronger untrusted-code isolation requires a separate sandbox boundary.

## Session, input, and failure contract

Enrollment uses `/v1/node/enroll`; each daemon process uses a fresh UUID in `/v1/node/hello`, then carries its returned epoch/session on every request. It heartbeats every 5s and polls `/v1/node/machine-sessions/poll` every 2s. Each session carries exact project/node/epoch, watch/control mode, expiry, and temporary ICE settings. Production TURN credentials come from the controller broker; the provider's TURN API key never enters this container. Relay policy requires a TURN server and works without inbound host port mappings.

The browser sends one offer through the project session mailbox. The host gathers a full answer asynchronously and retries it with one stable operation ID. Incoming trickle ICE is supported. Offers are bounded 64 KiB, candidates 4 KiB, controller responses 1 MiB, frames 4 MiB, peer count 8, and captured-frame fanout 2 frames. Slow peers drop old frames; failed/closed peers release their slot. Reconnect uses a fresh controller admission and offer, not a stale SDP replay.

Only one `input` data channel is accepted for a control lease. Messages are JSON text at most 1024 bytes and 120 events/second. Pointer messages use `{type:'pointermove'|'pointerdown'|'pointerup',x,y,button?:0|1|2}` with finite pixel coordinates inside the decoded viewport. Keys use `{type:'keydown'|'keyup',key}` with `KeyA`–`KeyZ`, `Digit0`–`Digit9`, Space, Enter, Escape, Backspace, Tab, arrow keys, ShiftLeft, or ControlLeft. Unknown fields/events/keys and out-of-range coordinates are rejected. Watch sessions cannot inject input. Held keys/buttons are released when the control lease ends.

Local monotonic permits stop media/input at lease expiry even if the controller cannot be reached. Missing sessions, failed authority checks, revocation, and stale process epochs close streams. An ended individual session (HTTP 410) closes only that peer. The old process never reacquires an epoch after fencing. No active peers stops the encoder, engine, and Xvfb; child process groups receive TERM followed by a bounded KILL/reap. Engine/capture failure closes the sessions honestly.

## Verification

The Rust crate is independently locked. To avoid the root workspace's pnpm-managed Cargo source override when developing this standalone image, invoke Cargo from a directory outside the repository:

```sh
cargo test --locked --manifest-path /absolute/enginebox/packages/stream-host/Cargo.toml
```

The repeatable browser test creates an owned Docker network, private Coturn relay, native X11 fixture, state volume, and two real Chromium peers. It publishes **no host ports**. It checks relay-selected RTP frames, visible native input, denied watch/invalid input, reusable peer slots, lease expiry, last-peer cleanup, persistent enrollment across restart, and authority revocation. It cleans only its own resources and can retain evidence outside Git:

```sh
docker build --target test-runtime -t enginebox-stream-host:qualification packages/stream-host
docker build -t enginebox-stream-relay:qualification -f packages/stream-host/test/relay.Dockerfile packages/stream-host/test
docker build -t enginebox-stream-browser:qualification -f packages/stream-host/test/browser.Dockerfile .
pnpm install --frozen-lockfile
STREAM_EVIDENCE_DIR=/private/stream-evidence node packages/stream-host/test/start-fleet.mjs
```

`start-fleet.mjs` starts a real local Wrangler controller with unique disposable
SQLite/R2 state, a fresh project and an owner credential confined to the test
driver. No Cloudflare login or production token is needed. It removes its
controller, private connection file, state, and exact Docker resources after the
test. `--check-controller` checks local startup/authentication/cleanup without
Docker. On Linux the temporary authenticated listener binds a host interface so
Docker's host gateway can reach it; use an isolated development or CI machine.

For an isolated media-only protocol fixture, use `node
packages/stream-host/test/run.mjs`. Neither fixture credentials nor its test-only
native window are shipped in the runtime target. Native amd64/arm64 image builds
and real-controller relay checks run in `.github/workflows/stream-host.yml`; a
successful single-architecture build is not proof of another architecture.

The Godot profile uses the same runner with `STREAM_HOST_TEST_IMAGE`,
`STREAM_ENGINE_CONFIG` (operator-owned command and dimensions), and
`STREAM_SCENE_KIND=godot`, under 4 CPU / 4 GiB limits. The optional
`STREAM_GAMEJAM_PROOF_ROOT` mounts the Gamejam prototype read-only and invokes its
actual viewer proof; this is not a runtime dependency.

The [local ARM64 qualification receipt](test/evidence/godot-arm64-2026-10-24.json)
records the complete Godot → TURN → Gamejam flow on 2026-10-24. Two 1280×720
viewers measured 30.53 / 30.52 decoded fps over 3.24 seconds. Real native input
appeared in both streams; host restart, lease expiry, revocation, and owned
cleanup passed. These are decoded video measurements for the control-check
scene, not a performance guarantee for arbitrary games. The Rust regressions
also cover delayed-response lease aging, releasing a held native key while
controller HTTP is blocked, and terminating descendants after their direct
parent exits. The full Godot profile is now included in both architecture CI
jobs; those latest CI results remain pending at this receipt's checkpoint.
