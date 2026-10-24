# Builds, reviews and playtest evidence

Enginebox owns integration candidates, protected checks, build artifacts, review
receipts, recording validation and acceptance. Its Cloudflare Project Durable
Object coordinates a project; R2 holds immutable bytes and durable acceptance
decisions. Gamejam consumes these APIs for its roadmap, review and recording UI.
It does not schedule merges or maintain another production head.

An owner-configured build executor publishes the exact candidate's output once.
An artifact manifest binds the project, generation, source tree, recipe,
toolchain fingerprint and every file's digest. Files use the existing chunk/page
transport; metadata pages keep publication bounded even for many files. Reads
support HTTP byte ranges and verify downloaded chunks. An accepted build is the
same immutable artifact that was tested, with no production rebuild.

## Review and acceptance

The owner API `POST /v1/projects/:project/tasks/:task/approve` accepts
`{operationId, expectedRevision}`. It records a receipt tied to the task's
requirements, result generation and candidate. The response is the current task;
it can remain `testing` with `reviewReceipt` while another batch member, a full
check or required recording is outstanding. The controller attempts promotion
again when a check or recording validation finishes. Selected pre-land checks
never replace the configured full checks. Task-only agent credentials cannot
approve, accept, publish artifacts or claim a trusted worker result.

Automatic promotion requires every candidate change to correspond to a current
task with owner-selected automatic review or a matching owner approval. New
Enginebox integrations and explicitly reviewed candidates use this path. Legacy
manually staged candidates retain explicit owner acceptance. Owner `accept`
remains an explicit whole-candidate decision and still enforces checks and the
recording policy. Acceptance persists its decision in R2 before changing the
project head. Edited requirements or changed result/candidate invalidate review.

## Recording API

All routes are under `/v1/projects/:project` and require project authorization.
An app service additionally authenticates its users and binds `tester` to their
identity; it must enforce creator-only recording mutations before proxying them.
The controller binds each upload to that service identity. A browser's recording
is contributor evidence, not an independent attestation that its pixels came
from the named build; trusted build/CI checks are still required.

| Route | Purpose |
| --- | --- |
| `GET /test-contexts` | Current testing tasks with a published frozen Web artifact |
| `POST /test-sessions` | `{id,taskId,expectedRevision,tester,artifact?}`; freezes the tested build |
| `GET /test-sessions/:id` | Resume a service-owned session |
| `POST /recordings` | `{id,testSessionId,mimeType,captureOrigin,gameAudio,microphone}` |
| `PUT /recordings/:id/chunks/:sequence` | Raw bytes, `x-chunk-sha256` and `x-recording-segment` |
| `POST /recordings/:id/events` | `{operationId,events:[{atMs,type,text}]}` markers and gaps |
| `POST /recordings/:id/finish` | Outcome, feedback and ordered segment/chunk inventory |
| `GET /recordings?taskId=…` | Recent project evidence |
| `GET /recordings/:id` | Capture/upload/validation state and metadata |
| `GET /recordings/:id/media` | Authenticated seekable validated output |
| `GET /artifacts/:id/files/:path` | Frozen build files, including Web entrypoint and assets |

Uploads are capped at 8 MiB/chunk, 256 MiB/session, 10 minutes, 2,048 chunks and
64 segments. The browser's pending queue is capped at 32 MiB. Duplicate chunk
requests acknowledge identical bytes without charging them twice; changed bytes
at an existing sequence fail. Empty aborted captures may finish as `partial`
with zero bytes, duration and segments, and never qualify as evidence.

Finishing a nonempty upload creates a controller-owned `media-finalize` job.
Only an enrolled `mediaProjects` worker can claim it and upload its output.
The worker verifies every input hash, remuxes with fixed FFmpeg arguments, probes
the output and decodes all frames. Remuxing avoids the CPU cost of transcoding.
No shell, remote input URL or user-provided command is executed. A worker result
must reference durably uploaded output chunks before the controller marks media
ready. Losing an uncertain native lease requires reconciliation, as for builds.

`partial`, failed outcomes, gaps, stale requirements and expired media cannot
satisfy a required passing-evidence gate. Audio is opt-in policy: canvas-only
captures can be honest video-only evidence. Configure with owner
`PUT /playtest-policy`:

```json
{"required":true,"requireAudio":false,"minDurationMs":1000,"retentionDays":30}
```

Evidence is optional by default. Required evidence also requires configured full
candidate checks. A required acceptance decision records the policy, recording,
artifact, media hash, tester and measured duration for audit. Default retention
is seven days for abandoned/failed captures and 30 days for completed media.
DO alarms delete recording-owned R2 prefixes, preserving shared source/build
chunks. Input chunks are removed after validated output is available. Metadata
remains for audit, with expired playback unavailable.

## Local development

Start with [LOCAL-FLEET.md](LOCAL-FLEET.md). Docker Compose runs Linux nodes and
local Wrangler DO/R2 on macOS, Windows and Linux. The development image includes
FFmpeg. For a **fresh** fleet, enable the optional media worker:

```sh
pnpm install --frozen-lockfile
pnpm fleet:dev --storage --media
pnpm check:fleet
pnpm test:fleet
```

`agent-b` receives the `demo` media capability and fixed `/usr/bin/ffmpeg` and
`/usr/bin/ffprobe` paths. Existing enrollment is never silently upgraded: choose
a new `EBOX_FLEET_STATE` and `EBOX_FLEET_PORT` to keep an existing preview intact.
No Cloudflare account is needed. Native macOS validation can use absolute local
tool paths; Windows runs this executor in the Linux container. CPU workers can
validate media without starting Unity, Unreal or a GPU editor.

For a standalone node, the owner supplies `mediaProjects:["project"]` at
enrollment and local `media:{ffmpeg,ffprobe,projects:["project"]}` configuration.
Keep owner and worker credentials outside engine source and browser responses.
The current native build path is explicitly trusted/private; it does not provide
hostile-tenant filesystem isolation. See [ENGINE-BUILDS.md](ENGINE-BUILDS.md) and
[LOGICAL-INTEGRATION.md](LOGICAL-INTEGRATION.md) for engine qualification and the
same-Store restriction on logical integration.

Validation includes Worker routing/authentication, durable state recovery,
immutable upload retries, range playback, retention and multi-task approval;
the node test uses actual FFmpeg AV generation, remux, full decode, hash
verification and process cancellation. Set `EBOX_TEST_FFMPEG` and
`EBOX_TEST_FFPROBE` to absolute paths for that test when tools are not in the
standard locations. It explicitly skips where the tools are absent.
