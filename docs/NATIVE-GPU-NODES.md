# Native GPU nodes

This adds a native Rust supervisor (`enginebox-node`), scoped enrollment to the existing fleet controller, Windows installation, and separate Apple VideoToolbox/NVIDIA NVENC qualification profiles. It does not install a second controller or grant a node administrator credentials.

The Windows package runs a background user service through Task Scheduler at sign-in, with the current user's privileges. It is deliberately not a LocalSystem desktop service: Windows services run in Session 0. The owner-configured rendering worker runs in the user's interactive session. Sign-out stops that session; this is not an unattended login-window graphics qualification. No firewall, GPU driver, disk layout, VM or global profile is changed.

## Inspect and install Windows

The [Windows preview release](https://github.com/coop-collective/enginebox/releases/tag/native-node-preview-20261024.1)
supplies [`install-command.txt`](https://github.com/coop-collective/enginebox/releases/download/native-node-preview-20261024.1/install-command.txt): one PowerShell
command that downloads and verifies `install.ps1`, which in turn verifies a pinned
package archive and executable. It launches with `-NoProfile -ExecutionPolicy
Bypass` scoped to that process, so a downloaded unsigned script can run without
changing the user's or machine's persistent execution policy. An organization
enforcing signed scripts through Group Policy still requires its approved signed
installer. This does not add an Authenticode signature to our preview.

The bootstrap finds FFmpeg or offers to install `Gyan.FFmpeg` through WinGet for
the current user, then prompts for the controller origin. Leave the origin blank
to run only the real GPU qualification. Enrollment is still prompted privately
by the installer. A matching deployed controller and valid enrollment invitation
are required to register a node. No GitHub login is needed for release downloads.

For a package already extracted locally, prefix the command with
`powershell.exe -NoProfile -ExecutionPolicy Bypass -File` and keep its existing
arguments. Running the bare unsigned `.ps1` can produce PowerShell's
`UnauthorizedAccess` / "not digitally signed" error before our installer starts.
See [Microsoft's execution-policy scopes](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_execution_policies?view=powershell-5.1).

Download the `enginebox-node-windows-nvidia-<commit>` artifact from the **Build native GPU node installers** workflow. It includes the executable, PowerShell installer, license and checksums. Use the checksum from that trusted workflow/release. These prerelease artifacts are not Authenticode-signed and have not been tested on the owner's RTX machine.

Install an NVIDIA graphics driver and an FFmpeg build containing `h264_nvenc`. Then run from the extracted artifact:

```powershell
$hash = '<enginebox-node.exe SHA-256 from the trusted artifact>'
.\install-native-node.ps1 -Mode Inspect -ExpectedBinarySha256 $hash -FfmpegPath 'C:\Tools\ffmpeg\bin\ffmpeg.exe'
.\install-native-node.ps1 -Mode Install -ExpectedBinarySha256 $hash -FfmpegPath 'C:\Tools\ffmpeg\bin\ffmpeg.exe' -Controller 'https://controller.example.com'
```

Inspect observes Windows, GPU/driver and firmware virtualization prerequisites, then encodes 60 H.264 frames at 1280×720 with NVENC and decodes all 60. An encoder name or `nvidia-smi` result alone cannot pass qualification. NVIDIA L4 and RTX use the same NVENC profile; exact GPU/driver/FFmpeg combinations must pass on their target hardware. No software fallback is allowed.

Native Windows rendering needs no VM. WSL/Docker/Hyper-V are separate backend choices; observing virtualization support does not install or qualify GPU passthrough. The installer reports missing prerequisites before making installation changes.

Install creates a private directory under `%LOCALAPPDATA%\Enginebox\native-node`, prompts privately for the enrollment credential, registers **Enginebox Native GPU Node**, and starts it. The token is passed on stdin, never as a command argument. The directory DACL grants only the current user. Read `private\status.json` and `private\qualification.json` for status/evidence. Installation and GPU execution require a Windows acceptance run; CI compilation and PowerShell parsing do not establish these results.

To supervise a game process, supply `-WorkerConfig C:\private\worker.json` during initial installation. This file contains only a locally approved executable and arguments:

```json
{
  "executable": "C:\\Games\\example\\Game.exe",
  "sha256": "<SHA-256 of this executable>",
  "args": [],
  "directory": "C:\\Games\\example"
}
```

The node checks the executable hash on every start. The worker directory and dependencies are trusted local configuration, not an isolation boundary for hostile projects. A Windows Job Object contains the supervisor and descendants; stopping the task also stops its owned child processes. Unix workers use their own process group with termination/reaping. Revocation, stale session or controller failure stops the worker; task restart rechecks enrollment. No controller-provided shell command, game download or privileged tenant execution is accepted by this initial native role.

Uninstall only removes the owned task and binary:

```powershell
.\install-native-node.ps1 -Mode Uninstall
```

It retains private identity/evidence. Revoke the machine at the controller to retire its identity. The controller's existing node stale threshold removes liveness after heartbeats stop; local status does not substitute for the controller's timestamp.

## Controller enrollment

Deploy the matching controller revision before using this installer. On the owner/controller side, use its existing administrator-authenticated API to create the project (if needed), then:

```http
POST /v1/projects/demo/machine-invitations
Authorization: Bearer <controller-owner-credential>
Content-Type: application/json

{"id":"windows-gpu-1","label":"Windows GPU test host","ownerAccountId":"owner-account","allowControl":false}
```

The response contains a ten-minute enrollment credential. Send only that credential to the machine. The installer generates and privately persists the long-term node identity before exchange. The controller retains the private pending invitation for idempotent owner retries, stores a hash of the claimed node credential, and binds the invitation to one identity; retrying that identity after a lost response is safe. A competing identity is rejected. An identical pending invitation request returns the same credential and expiry. Changed or expired invitations require a fresh ID. Enrollment cannot resurrect a revoked node.

`POST /v1/nodes/windows-gpu-1/revoke` uses owner authorization. Native credentials are restricted to their own identity/session/heartbeat endpoints; they cannot read project storage, claim existing agent jobs, create nodes or access owner operations. HTTPS certificate validation is required, redirects are rejected and a URL cannot contain credentials, query, fragment or a path. Loopback HTTP exists only behind an explicit test flag, never in the installer.

## Apple and Linux profiles

Apple Silicon runs directly on macOS; the Linux image cannot access Metal through Docker Desktop:

```sh
enginebox-node inspect --profile apple-videotoolbox --ffmpeg /opt/homebrew/bin/ffmpeg --qualify
# Pass the invitation on stdin from a private prompt/secret source, never in argv:
enginebox-node enroll --controller https://controller.example.com --config /private/node/node.json --profile apple-videotoolbox --ffmpeg /opt/homebrew/bin/ffmpeg
enginebox-node run --config /private/node/node.json
```

The Apple profile selects `h264_videotoolbox -allow_sw 0`; the NVIDIA profile selects `h264_nvenc -preset p4 -tune ull`. Both verify decoded output. These are synthetic encoder qualifications, separate from game rendering, browser media delivery, audio, latency and capacity tests.

Build the separate Linux NVIDIA image from the repository root:

```sh
docker build -f packages/enginebox-install/containers/Dockerfile.nvidia -t enginebox-node:nvidia .
docker run --rm --gpus all --read-only --tmpfs /tmp:rw,nosuid,size=256m enginebox-node:nvidia
```

The host supplies NVIDIA drivers and NVIDIA Container Toolkit. Use native Windows for the Windows package. WSL CUDA availability is not proof of Vulkan/NVENC game streaming. The image includes node/encoder tools; mount or add the appropriate Linux game runtime separately. For persistent enrollment mount a private writable configuration directory owned by UID 10001. No credentials are baked into the image.

## Current boundary

Validation on October 24, 2026: root `pnpm check`, `pnpm test`, `pnpm lint:rust` and `pnpm build` passed. Native node builds/tests and Windows PowerShell parsing passed on the Windows/Linux/macOS installer matrix. The opt-in Mac lifecycle test used the actual hardware encoder, enrolled a disposable identity with a local controller fixture, launched an owned process, and proved the process stopped after credential revocation. Controller runtime tests cover expiration, competing claims, retry after Durable Object eviction, privilege boundaries and revocation. No private inventory or credentials are checked into Git.

Repeat the Mac hardware/lifecycle check explicitly:

```sh
ENGINEBOX_TEST_FFMPEG=/opt/homebrew/bin/ffmpeg cargo test --locked -p enginebox-install --lib real_apple_encoder -- --ignored
```

This native role enrolls, qualifies encoding, reports liveness and supervises a locally configured worker. Remote game artifact deployment, workload placement, TURN/media ingress, resource admission and trusted project assignment are subsequent Enginebox adapters; existing Linux/LayerFS/agent scheduling is unchanged. `streamingQualified` remains false in node telemetry until an integrated host-specific game/browser acceptance has been recorded. No L4/RTX capacity claim is made from Mac or CI results.

Sources: [Apple encoder selection](https://developer.apple.com/documentation/videotoolbox/kvtvideoencoderspecification_requirehardwareacceleratedvideoencoder), [NVIDIA codec matrix](https://developer.nvidia.com/video-encode-decode-support-matrix), [Windows service sessions](https://learn.microsoft.com/en-us/windows/win32/services/interactive-services).

For CPU/memory/disk-limited Windows registration for CI and native agents, use
[Windows workers](WINDOWS-WORKERS.md). The online installer defaults to that flow;
pass `-Mode Inspect` for a GPU check or `-Workload gpu` for the separate GPU worker.
