# Installed CI toolchains

These six recipes install **Node.js 24.22.1** privately on existing Linux, macOS
and Windows workers (x64 or arm64). SHA-256 values come from the release's
[official checksum manifest](https://nodejs.org/dist/v24.22.1/SHASUMS256.txt).
Each recipe verifies the installed executable's exact version before becoming
ready. Windows also verifies PowerShell 7.3 or later. They require Bash + tar on
Unix, or an existing PowerShell 7.3+ on Windows; they do not install the host OS.

Choose the worker's OS and architecture, review the JSON, then register and enable:

```sh
ebox ci template-register --manifest examples/ci/templates/node24-linux-x64.json
ebox ci template-enable node24-linux-x64
ebox ci templates
```

Enabling also applies to compatible nodes registered later. Use a dedicated CI
account with a private `ENGINEBOX_CI_ROOT`, an up-to-date `ebox`, and no provider
credentials. The dashboard's **Templates → CI toolchain templates** panel provides
the same import, enable, disable, status and retry controls.

To require this verified installation, add a `template` selector to an automation:

```json
{
  "schema_version": 1,
  "request_id": "node-toolchain-smoke-1",
  "event": "workflow_dispatch",
  "sha": "0000000000000000000000000000000000000000",
  "job": {
    "runner": "automation",
    "template": "node24-linux-x64",
    "runs-on": ["enginebox", "linux", "node-24", "image:node-24.22.1"],
    "shell": "bash",
    "run": "node --version",
    "timeout-minutes": 1
  }
}
```

The zero SHA is only for this no-checkout tool smoke test. Supply the exact source
SHA and checkout/project commands for a real build. Choose `shell: "pwsh"` and
the Windows template/labels on Windows. If multiple workers match, specify
`job.node`. Template capabilities replace the node's manually declared toolchain
capabilities for this job; OS and architecture still come from the actual node.

Owner-authored recipes can install CMake, JDKs, Android SDKs, or other private
toolchains using the same format. Keep downloads versioned with SHA-256 and size
bounds; `install` runs in the private installation directory and receives
`ENGINEBOX_TOOLCHAIN` and `ENGINEBOX_ASSETS_DIR`. List relative executable
directories in `paths`, and relative locations in `environment` for supported
variables such as `JAVA_HOME` and `ANDROID_SDK_ROOT`. Every capability needs one
probe with exact expected stdout. The installer inherits a restricted environment
and private HOME; configure paths needed by installer steps explicitly.

These Node recipes do not install Xcode, Android SDK, MSVC, signing identities,
or an official GitHub Actions runner. Those remain separately prepared prerequisites
for the existing native platform build examples. Licensed toolchains and credentials
must be provided by their owner. Enabling a recipe does not modify GitHub runner
labels or supply toolchain paths to an independently launched Actions listener.

To upgrade, edit the pinned URL/hash/probes and increase `revision`. Updating an
enabled template queues its new version automatically. Installation files are
retained across upgrades, retries and disabling; schedule disk cleanup only when
no running jobs reference the old installation. See [CI.md](../../../docs/CI.md)
for recovery, resource checks and the trust boundary.
