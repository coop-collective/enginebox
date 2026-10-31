# Engine sessions (design draft)

Status: design, October 31, 2026. Nothing here is implemented yet.

## Owner decisions (November 1, 2026)

- **Isolation: one VM per session.** Each session runs in its own Hyper-V VM with a GPU partition (GPU-P), not a
  container sharing a kernel. The container spec below becomes the in-VM process spec; the WSL container results
  remain the fallback evidence. Guest OS is being qualified (Windows Server guest is the standard GPU-P path; Windows
  client guests are generally not licensed for third-party hosting). Sessions start from golden images via
  differencing disks and a warm VM pool.
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

An engine session is one tenant's game engine (Unity first; the API is engine-agnostic) running a given
repository and ref in its own container on a GPU node, streamed to the requesting app over MoQ. Gamejam is the first
client: its dev server, and later its Cloudflare deployment, starts sessions as an OAuth client of this controller.
Sessions never run on a bare host.

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
