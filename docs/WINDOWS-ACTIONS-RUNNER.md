# Opt-in Windows Actions verification

Status: workflow routing implemented; personal-machine Actions execution and
runner installation remain unqualified. This is ordinary official GitHub Actions,
not the legacy [Windows node](WINDOWS-WORKERS.md), the WSL workspace installer,
or the Linux Incus JIT/prepared-LayerFS profile in [GITHUB-ACTIONS.md](GITHUB-ACTIONS.md).
An installed Engine Box node does not register an Actions listener.

Without `ENGINEBOX_WINDOWS_RUNNER`, verification uses `windows-2025`. Set that
repository variable to a JSON array only after dedicated CI capacity is ready:

```json
["self-hosted", "enginebox", "windows", "x64", "image:windows-2025-msvc", "msvc", "pwsh"]
```

The variable applies to main and same-repository PRs. Fork PRs always use hosted
Windows; dispatch on other branches uses hosted Windows. GitHub matches every
label. Existing Linux main-only routing and all three matrix/check names remain
unchanged. An opted-in offline runner queues the job; there is no automatic
hosted fallback. Clear the variable to restore hosted routing for subsequent
runs, then cancel/re-run any queued attempt as appropriate.

## Prepare dedicated capacity

Follow [CI.md](CI.md): use a credential-free dedicated Windows CI machine/account,
not a signed-in personal/provider workspace or an existing agent host. Labels,
a different SSH username and an ephemeral registration do not isolate a shared
physical host or enforce resource reservations. Inspect services, scheduled tasks,
active work, disk and toolchains first. Do not stop an existing node/workspace,
change global SSH/firewall settings, or create a runner alongside it as bootstrap.
The personal-PC benchmark establishes build feasibility only; it does not qualify
that host for unattended CI. Shared capacity needs the reservations/isolation
implementation described in CI.md before activation.

Provision Git + git-lfs, PowerShell 7.3+, Node 24, MSVC C++ Build Tools/Windows SDK,
OpenSSL for generated-key test fixtures (Git for Windows includes it under
`usr/bin`), and the root pnpm 12.15.0/Rust toolchain pins. Put the actual native
PowerShell executable on the build account PATH rather than relying on an App
Execution Alias. Run the frozen install and complete
check/test/lint/build suite under the actual dedicated build account. Record
versions and results privately. Keep controller, provider and owner credentials
outside that account and machine. Public-repository same-repo PRs still execute
repository code: grant branch creation only to trusted contributors and restrict
the runner group/repository access. The fork guard is not a sandbox.

## Register one official ephemeral listener

1. Download the Windows x64 archive from the repository's **Settings → Actions →
   Runners → New self-hosted runner** instructions or the selected
   [official runner release](https://github.com/actions/runner/releases).
   Verify its published SHA-256 before extraction into a fresh private directory.
   Record the exact version/hash; this change does not claim a tested version.
2. Install the matching Engine Box CLI on the CI worker. Copy/edit the
   [Windows manifest](../examples/ci/nodes/windows.json) outside Git; use canonical
   CI SSH identity and actual image/capabilities. Set `ENGINEBOX_CI_ROOT` to a
   private directory ACL-limited to that dedicated build account/administrators.
3. Supply a fresh repository registration token through private process environment
   `ENGINEBOX_CI_REGISTRATION_TOKEN`, as in CI.md. Do not put it in YAML, manifests,
   command-line arguments, shell history or logs. Registration tokens are distinct
   from the controller's `ENGINEBOX_CI_GITHUB_TOKEN`; no credential rotation is
   needed. Then run this existing worker command with your private paths:

   ```powershell
   ebox ci github-runner --manifest C:\private\windows-worker.json `
     --directory C:\private\actions-runner --repository coop-collective/enginebox
   ```

4. The wrapper registers `--ephemeral --disableupdate`, holds the worker's CI lock
   and runs one job, with a seven-hour listener bound. It does not install a
   persistent service or automatically reprovision. Use one fresh installation
   per attempt; export diagnostics privately and inspect stale registrations after
   interruption. Reprovision and maintain the official version before every new
   listener. Do not snapshot registration credentials into reusable images.
5. Only once a listener is ready, set the variable above and run one non-deploying
   verify canary. Check actual GitHub conclusion, runner identity, cancellation,
   logs and one-job cleanup. Compare the same SHA and toolchain against hosted
   Windows before enabling regular use. Leave the variable unset until then.

GitHub's [registration guide](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/add-runners)
and [runner reference](https://docs.github.com/en/actions/reference/runners/self-hosted-runners)
are the upstream contracts. No runner token, installation or repository variable
was created by this change. All full checks and release builds retain their scope.

The rechecked hosted baseline [run 41384962825](https://github.com/coop-collective/enginebox/actions/runs/41384962825)
was 1,280 s for Windows: check 84 s, test 388 s, build 711 s. Faster hardware is a
hypothesis until measured, independent of queueing (3 s baseline). Local cold/warm
results and qualification limits are recorded in the PR; private host inventories
and connection details remain outside Git.
