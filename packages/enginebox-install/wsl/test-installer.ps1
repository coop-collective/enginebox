$ErrorActionPreference='Stop'
$path=Join-Path $PSScriptRoot 'install.ps1.in'
$tokens=$null; $errors=$null
[System.Management.Automation.Language.Parser]::ParseFile($path,[ref]$tokens,[ref]$errors) | Out-Null
if ($errors.Count) { $errors | Format-List; throw 'PowerShell syntax failure' }
# Load helpers from a .ps1 copy; no host setup may execute during this test.
$fixture=Join-Path $env:TEMP ('enginebox-installer-'+[Guid]::NewGuid()+'.ps1')
Copy-Item -LiteralPath $path -Destination $fixture
try {
  . $fixture
  if ((Assert-Origin 'https://example.com/') -ne 'https://example.com') { throw 'Origin normalization failed' }
  foreach ($bad in @('http://example.com','https://name:secret@example.com','https://example.com/api','https://example.com/?x=1')) {
    $rejected=$false;try { Assert-Origin $bad | Out-Null } catch { $rejected=$true }
    if (-not $rejected) { throw "Unsafe controller accepted: $bad" }
  }
  if ((Read-Number 'CPU' 4 2 1 8) -ne 4) { throw 'Explicit allowance ignored' }
  $rejected=$false;try { Read-Number 'CPU' 9 2 1 8 | Out-Null } catch { $rejected=$true }
  if (-not $rejected) { throw 'Physical resource ceiling ignored' }
  Write-Output 'PowerShell 5.1 parsing, controller validation and resource bounds passed.'
} finally { Remove-Item -LiteralPath $fixture -Force }

# Exercise the actual installer orchestration with Windows effects replaced by
# fixtures. Real WSL/container behavior is covered by the separate WSL job.
# ScheduledTasks exports CDXML functions. Load it before overriding its effects;
# autoloading it later would replace the fixture functions with real commands.
Import-Module ScheduledTasks
$testRoot=Join-Path $env:TEMP ('enginebox-wsl-flow-'+[Guid]::NewGuid())
New-Item -ItemType Directory $testRoot | Out-Null
$flowScript=Join-Path $testRoot 'install.ps1'
$fixtureImage=Join-Path $testRoot 'image-fixture'
[IO.File]::WriteAllText($fixtureImage,'fixture-rootfs')
$fixtureHash=(Get-FileHash -Algorithm SHA256 -LiteralPath $fixtureImage).Hash
(Get-Content -Raw -LiteralPath $path).Replace('@@ROOTFS_URL@@','https://fixture.invalid/enginebox.wsl').Replace('@@ROOTFS_SHA256@@',$fixtureHash) | Set-Content -LiteralPath $flowScript -Encoding UTF8
$global:EngineboxWslFixture=@{ version='2.6.0'; calls=@(); prompts=@(); registrations=@(); starts=0; updates=0; enables=0; configuration=$null; installed=$true; enrolled=$true; claim=$null; code=$null; downloads=0; controllerChoice=$null; secret=$null; linuxCpu=16; linuxMemory=16GB; committed=$false; overrideChoice='n'; pendingId=$null; exportFailure=$false; exportInvalid=$false; exportSharing=0; delays=0; enrollmentError=$null; stopped=0 }
function global:Start-Sleep { param($Seconds) if ($Seconds -ne 2) { throw 'Unexpected retry delay' }; $global:EngineboxWslFixture.delays++ }
function global:Get-CimInstance { [pscustomobject]@{NumberOfLogicalProcessors=16;TotalPhysicalMemory=32GB} }
function global:Get-ScheduledTask { return $null }
function global:Register-ScheduledTask {
  param($TaskName,$Action,$Trigger,$Principal,$Description,$Settings,[switch]$Force)
  $global:EngineboxWslFixture.registrations+=@($Action.Execute)
}
function global:Stop-ScheduledTask { param($TaskName) $global:EngineboxWslFixture.stopped++ }
function global:Start-ScheduledTask { param($TaskName) $global:EngineboxWslFixture.starts++ }
function global:Read-Host {
  param($Prompt,[switch]$AsSecureString)
  if ($AsSecureString) {
    if (-not $global:EngineboxWslFixture.secret) { throw 'Unexpected secret prompt' }
    return ConvertTo-SecureString $global:EngineboxWslFixture.secret -AsPlainText -Force
  }
  $global:EngineboxWslFixture.prompts+=@($Prompt)
  if ($Prompt -eq 'Override existing? [Y/n]') { return $global:EngineboxWslFixture.overrideChoice }
  if ($Prompt -like 'Controller address*' -and $global:EngineboxWslFixture.controllerChoice) { return $global:EngineboxWslFixture.controllerChoice }
  return ''
}
function global:Start-Process {
  param($FilePath,$ArgumentList,$Verb,[switch]$Wait,[switch]$PassThru)
  if ($FilePath -eq 'wsl.exe' -and $ArgumentList -eq '--install --no-distribution --web-download' -and $Verb -eq 'RunAs') {
    $global:EngineboxWslFixture.enables++
    return [pscustomobject]@{ExitCode=3010}
  }
  if ($FilePath -ne 'wsl.exe' -or $ArgumentList -ne '--update --web-download' -or $Verb -ne 'RunAs') { throw 'Unexpected elevated host operation' }
  $global:EngineboxWslFixture.updates++
  $global:EngineboxWslFixture.version='2.6.0'
  [pscustomobject]@{ExitCode=0}
}
function global:Invoke-WebRequest {
  param([switch]$UseBasicParsing,$Uri,$OutFile)
  if ($Uri -ne 'https://fixture.invalid/enginebox.wsl') { throw 'Unexpected download' }
  $global:EngineboxWslFixture.downloads++
  [IO.File]::WriteAllText($OutFile,'fixture-rootfs')
}
function global:wsl.exe {
  $global:LASTEXITCODE=0
  $global:EngineboxWslFixture.calls+=@(($args -join ' '))
  switch ($args[0]) {
    '--version' { 'WSL version: '+$global:EngineboxWslFixture.version }
    '--list' { if ($global:EngineboxWslFixture.installed) { 'Enginebox' }; 'Ubuntu' }
    '--status' { return }
    '--terminate' { if ($args[1] -ne 'Enginebox') { throw 'Unrelated distribution stopped' } }
    '--export' {
      if ($args[1] -ne 'Enginebox' -or $args[3] -ne '--vhd') { throw 'Unexpected export' }
      if ($global:EngineboxWslFixture.exportSharing -gt 0) { $global:EngineboxWslFixture.exportSharing--; $global:LASTEXITCODE=1; 'Wsl/Service/ERROR_SHARING_VIOLATION'; return }
      if ($global:EngineboxWslFixture.exportFailure) { $global:LASTEXITCODE=1; return }
      $stream=[IO.File]::Create($args[2])
      try {
        $magic=if ($global:EngineboxWslFixture.exportInvalid) { 'invalid!' } else { 'vhdxfile' }
        $bytes=[Text.Encoding]::ASCII.GetBytes($magic);$stream.Write($bytes,0,$bytes.Length);$stream.SetLength(1MB)
      } finally { $stream.Dispose() }
    }
    '--unregister' {
      if ($args[1] -ne 'Enginebox') { throw 'Unrelated distribution removed' }
      $global:EngineboxWslFixture.installed=$false; $global:EngineboxWslFixture.enrolled=$false; $global:EngineboxWslFixture.pendingId=$null
    }
    '--install' {
      if ($args.Count -ne 12 -or $args[1] -ne '--from-file' -or $args[3] -ne '--name' -or $args[4] -ne 'Enginebox' -or $args[7] -ne '--version' -or $args[8] -ne '2' -or $args[9] -ne '--vhd-size' -or $args[10] -ne '20GB' -or $args[11] -ne '--no-launch') { throw ('Incorrect import argv: '+($args -join ' ')) }
      $global:EngineboxWslFixture.installed=$true
    }
    '--distribution' {
      switch ($args[-1]) {
        '-' {
          if ($args[-2] -ne 'python3') { throw 'Unexpected probe' }
          $probe=$input | Out-String
          if ($probe.Contains('/proc/meminfo')) {
            @{cpu=$global:EngineboxWslFixture.linuxCpu;memory_bytes=$global:EngineboxWslFixture.linuxMemory;committed=$global:EngineboxWslFixture.committed} | ConvertTo-Json -Compress
          } elseif ($probe.Contains('/var/lib/enginebox/private/worker.json')) {
            @{id=$global:EngineboxWslFixture.pendingId;enrolled=$global:EngineboxWslFixture.enrolled} | ConvertTo-Json -Compress
          } else { throw 'Unexpected probe body' }
        }
        '/etc/enginebox/installation-owner' { [Security.Principal.WindowsIdentity]::GetCurrent().User.Value }
        'configure' { $global:EngineboxWslFixture.configuration=($input | Out-String | ConvertFrom-Json) }
        'enrolled' { if (-not $global:EngineboxWslFixture.enrolled) { $global:LASTEXITCODE=1 }; return }
        'claim' { $global:EngineboxWslFixture.claim=($input | Out-String).Trim() }
        'enginebox-docker.service' { return }
        'sync' { return }
        'enroll' {
          $global:EngineboxWslFixture.code=($input | Out-String).Trim()
          if ($global:EngineboxWslFixture.enrollmentError) {
            $global:LASTEXITCODE=1;Write-Error $global:EngineboxWslFixture.enrollmentError; return
          }
          $global:EngineboxWslFixture.enrolled=$true
        }
        default { throw 'Unexpected WSL mutation' }
      }
    }
    default { throw 'Unexpected WSL operation' }
  }
}
try {
  & $flowScript -InstallDirectory (Join-Path $testRoot 'ready')
  $f=$global:EngineboxWslFixture
  if ($f.prompts.Count -ne 5 -or $f.configuration.controller -ne 'https://enginebox.coopcollective.dev' -or $f.configuration.cpu -ne 4 -or $f.configuration.memory_gib -ne 8) { throw 'Installer defaults or prompts were not applied' }
  if ($f.configuration.disk_gib -lt 20 -or $f.configuration.disk_gib -gt 50) { throw 'Disk default outside bounds' }
  if ($f.starts -ne 1 -or [IO.Path]::GetFileName($f.registrations[-1]) -ne 'wsl.exe' -or $f.updates -ne 0) { throw ('Worker must start only in WSL: '+($f | ConvertTo-Json -Compress)) }
  $global:EngineboxWslFixture.version='2.3.26'
  & $flowScript -InstallDirectory (Join-Path $testRoot 'updated') -Controller 'https://custom.example' -Cpu 2 -MemoryGiB 4 -DiskGiB 20
  $f=$global:EngineboxWslFixture
  if ($f.updates -ne 1 -or $f.configuration.controller -ne 'https://custom.example' -or $f.configuration.cpu -ne 2 -or $f.configuration.memory_gib -ne 4 -or $f.configuration.disk_gib -ne 20) { throw 'WSL upgrade or custom allowances failed' }
  $before=$f.prompts.Count
  & $flowScript -InstallDirectory (Join-Path $testRoot 'updated') -Resume
  if ($f.prompts.Count -ne $before -or $f.updates -ne 1) { throw 'Resume must retain existing choices' }
  $f.installed=$false; $f.enrolled=$false
  $code=('a'*32)+'.'+('b'*64)
  & $flowScript -InstallDirectory (Join-Path $testRoot 'fresh') -Cpu 2 -MemoryGiB 4 -DiskGiB 20 -EnrollmentCode $code
  if ($f.downloads -ne 1 -or $f.claim -ne [Security.Principal.WindowsIdentity]::GetCurrent().User.Value -or $f.code -ne $code -or -not $f.enrolled) { throw 'Fresh distribution import/claim/registration failed' }
  $f.enrolled=$false; $f.controllerChoice='https://override.example'; $f.secret=('c'*32)+'.'+('d'*64)
  & $flowScript -InstallDirectory (Join-Path $testRoot 'override') -Controller 'https://original.example' -Cpu 2 -MemoryGiB 4 -DiskGiB 20 -EnrollmentCode $code
  if ($f.configuration.controller -ne $f.controllerChoice -or $f.code -ne $f.secret) { throw 'A controller override must not receive another controller''s registration code' }
  $f.controllerChoice=$null; $f.secret=$null
  # Recover the exact old failure: a saved Windows receipt requests 8 GiB,
  # while an unconfigured WSL VM only has a 4 GiB memory cap and two CPUs.
  $f.linuxMemory=3.8GB; $f.linuxCpu=2; $f.committed=$false
  $beforeDownloads=$f.downloads
  & $flowScript -InstallDirectory (Join-Path $testRoot 'ready') -Resume
  $saved=Get-Content -Raw -LiteralPath (Join-Path $testRoot 'ready/setup.json') | ConvertFrom-Json
  if ($f.configuration.memory_gib -ne 4 -or $f.configuration.cpu -ne 2 -or $saved.memory_gib -ne 4 -or $saved.cpu -ne 2 -or $f.downloads -ne $beforeDownloads) { throw 'Pending setup must recover within WSL limits without downloading again' }
  $before=$f.prompts.Count
  & $flowScript -InstallDirectory (Join-Path $testRoot 'ready') -Resume
  if ($f.prompts.Count -ne $before) { throw 'Adjusted allowances must persist across resumption' }
  # Never lower an established identity/runtime budget or claim readiness when
  # WSL cannot supply the minimum. Both failures retain installation state.
  $f.committed=$true; $beforeStarts=$f.starts; $f.configuration=$null
  $failed=$false
  try { & $flowScript -InstallDirectory (Join-Path $testRoot 'committed') -Cpu 4 -MemoryGiB 8 -DiskGiB 20 } catch { $failed=$_.Exception.Message -like '*saved allowance*' }
  if (-not $failed -or $f.starts -ne $beforeStarts -or $null -ne $f.configuration) { throw 'Established runtime must fail closed when WSL limits shrink' }
  $saved=Get-Content -Raw -LiteralPath (Join-Path $testRoot 'committed/setup.json') | ConvertFrom-Json
  if ($saved.memory_gib -ne 8 -or $saved.cpu -ne 4) { throw 'Established budget was changed' }
  $f.committed=$false; $f.linuxMemory=2GB
  $failed=$false
  try { & $flowScript -InstallDirectory (Join-Path $testRoot 'too-small') -Cpu 2 -MemoryGiB 4 -DiskGiB 20 } catch { $failed=$_.Exception.Message -like '*at least 4 GiB*' }
  if (-not $failed -or $f.starts -ne $beforeStarts -or $null -ne $f.configuration) { throw 'Insufficient WSL memory must report actionable guidance before configuration' }
  if ($f.calls | Where-Object { $_ -match '--shutdown|--terminate|--unregister' }) { throw 'Capacity recovery must not interrupt other WSL distributions' }
  $f.linuxMemory=16GB; $f.linuxCpu=16
  # Enter accepts Override existing by default. A complete private backup must
  # precede unregistering, and the new runtime must take the fresh invitation.
  $f.overrideChoice=''; $f.enrolled=$false; $f.pendingId='a'*32; $f.calls=@(); $f.exportSharing=2
  $replacement=Join-Path $testRoot 'replacement'
  $newCode=('e'*32)+'.'+('f'*64)
  & $flowScript -InstallDirectory $replacement -Cpu 2 -MemoryGiB 4 -DiskGiB 20 -EnrollmentCode $newCode
  $backups=@(Get-ChildItem -LiteralPath (Join-Path $replacement 'backups') -Directory)
  if ($backups.Count -ne 1 -or $f.code -ne $newCode -or -not $f.enrolled -or $f.stopped -ne 1 -or $f.delays -ne 2) { throw 'Default override must wait for disk release, back up and register a fresh runtime' }
  $backup=Join-Path $backups[0].FullName 'enginebox.vhdx'
  $receipt=Get-Content -Raw -LiteralPath (Join-Path $backups[0].FullName 'backup.json') | ConvertFrom-Json
  if ((Get-FileHash -Algorithm SHA256 -LiteralPath $backup).Hash -ne $receipt.sha256) { throw 'Missing verified backup receipt' }
  $exportIndex=[array]::IndexOf($f.calls,($f.calls | Where-Object { $_ -like '--export *' } | Select-Object -First 1))
  $removeIndex=[array]::IndexOf($f.calls,'--unregister Enginebox')
  if ($exportIndex -lt 0 -or $removeIndex -le $exportIndex) { throw 'Replacement removed state before export' }
  # Startup resumption never repeats override, even with the default Yes answer.
  $f.calls=@(); $beforeStops=$f.stopped
  & $flowScript -InstallDirectory $replacement -Resume
  if ($f.stopped -ne $beforeStops -or ($f.calls | Where-Object { $_ -like '--export*' -or $_ -like '--unregister*' })) { throw 'Resume must not repeat replacement' }
  foreach ($failure in @('exportFailure','exportInvalid')) {
    $f[$failure]=$true; $f.enrolled=$false; $f.pendingId='a'*32; $f.calls=@(); $beforeStarts=$f.starts
    $failed=$false
    try { & $flowScript -InstallDirectory (Join-Path $testRoot $failure) -Cpu 2 -MemoryGiB 4 -DiskGiB 20 -EnrollmentCode $newCode } catch { $failed=$true }
    if (-not $failed -or -not $f.installed -or $f.pendingId -ne ('a'*32) -or $f.starts -ne $beforeStarts -or ($f.calls | Where-Object { $_ -like '--unregister*' })) { throw 'An incomplete backup must retain the existing distribution' }
    $f[$failure]=$false
  }
  # Choosing No retains the old identity and explains a mismatched code locally.
  $f.overrideChoice='n'; $f.calls=@(); $failed=$false
  try { & $flowScript -InstallDirectory (Join-Path $testRoot 'retain') -Cpu 2 -MemoryGiB 4 -DiskGiB 20 -EnrollmentCode $newCode } catch { $failed=$_.Exception.Message -like '*Saved machine ID*' }
  if (-not $failed -or $f.pendingId -ne ('a'*32) -or ($f.calls | Where-Object { $_ -like '--unregister*' })) { throw 'Declining override must retain the pending identity' }
  # Show and save the underlying native error, with credentials removed.
  $f.enrollmentError='Controller returned HTTP 409; credential '+$code
  $failed=$false; $errorsRoot=Join-Path $testRoot 'error-detail'
  try { & $flowScript -InstallDirectory $errorsRoot -Cpu 2 -MemoryGiB 4 -DiskGiB 20 -EnrollmentCode $code } catch { $failed=$_.Exception.Message -like '*HTTP 409*' -and -not $_.Exception.Message.Contains($code) }
  $detail=Get-Content -Raw -LiteralPath (Join-Path $errorsRoot 'registration-error.txt')
  if (-not $failed -or $detail.Contains($code) -or -not $detail.Contains('[redacted credential]') -or $f.enrolled) { throw 'Registration failure detail must be useful and exclude credentials' }
  $f.enrollmentError=$null; $f.pendingId=$null; $f.enrolled=$true
  $f.version=''; $f.calls=@(); $beforeStarts=$f.starts
  $rebootRoot=Join-Path $testRoot 'reboot'
  & $flowScript -InstallDirectory $rebootRoot -Cpu 2 -MemoryGiB 4 -DiskGiB 20 -EnrollmentCode $code
  if ($LASTEXITCODE -ne 3010 -or $f.enables -ne 1 -or $f.starts -ne $beforeStarts -or ($f.calls | Where-Object { $_ -like '*configure*' })) { throw 'WSL prerequisite reboot must save setup without starting workloads' }
  $saved=Get-Content -Raw -LiteralPath (Join-Path $rebootRoot 'setup.json')
  if ($saved.Contains($code) -or -not (Test-Path -LiteralPath (Join-Path $rebootRoot 'install.ps1'))) { throw 'Resume must retain its installer and must not retain the code' }
  $global:LASTEXITCODE=0
  Write-Output 'Fresh import and stdin registration, defaults, custom controller/resources, WSL upgrade/reboot, constrained-memory recovery, immutable registered budgets, minimum capacity and WSL-only startup passed.'
} finally {
  Remove-Item -LiteralPath $testRoot -Recurse -Force
  foreach ($name in @('Get-CimInstance','Get-ScheduledTask','Register-ScheduledTask','Start-ScheduledTask','Stop-ScheduledTask','Start-Sleep','Read-Host','Start-Process','Invoke-WebRequest','wsl.exe')) { Remove-Item ('Function:\global:'+$name) -ErrorAction SilentlyContinue }
  Remove-Variable EngineboxWslFixture -Scope Global
}
