$ErrorActionPreference='Stop'
$path=Join-Path $PSScriptRoot 'install.ps1.in'
$tokens=$null; $errors=$null
[System.Management.Automation.Language.Parser]::ParseFile($path,[ref]$tokens,[ref]$errors) | Out-Null
if ($errors.Count) { $errors | Format-List; throw 'PowerShell syntax failure' }
# Load helpers from a .ps1 copy; no host setup may execute during this test.
$fixture=Join-Path $env:TEMP ('enginebox-installer-'+[Guid]::NewGuid()+'.ps1')
$nativeFixtureDirectory=$fixture+'.native'
$nativeFixturePath=Join-Path $nativeFixtureDirectory 'wsl.exe'
$savedPath=$env:PATH
New-Item -ItemType Directory -Path $nativeFixtureDirectory | Out-Null
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
  # Reproduce Windows PowerShell 5.1's actual native stderr behavior. A
  # PowerShell function mock alone does not prove NativeCommandError handling.
  Add-Type -OutputAssembly $nativeFixturePath -OutputType ConsoleApplication -TypeDefinition @'
using System;
public static class EngineboxNativeStderrFixture {
  public static int Main(string[] args) {
    Console.Error.WriteLine("wsl: Failed to start the systemd user session for 'root'.");
    if (args.Length > 0 && args[0] == "stdin") {
      using (var buffer = new System.IO.MemoryStream()) {
        Console.OpenStandardInput().CopyTo(buffer);
        Console.Out.WriteLine(Convert.ToBase64String(buffer.ToArray()));
      }
    } else { Console.Out.WriteLine("{\"ok\":true}"); }
    return args.Length > 0 && args[0] == "fail" ? 7 : 0;
  }
}
'@
  $reproduced=$false
  try { $null=(& $nativeFixturePath ok 2>&1) } catch { $reproduced=$true }
  if (-not $reproduced) { throw 'Native stderr fixture did not reproduce the PowerShell 5.1 failure' }
  # Resolve a real native executable, not a function shim: a shim introduces
  # another PowerShell scope that can change the native-pipeline encoding.
  $env:PATH=$nativeFixtureDirectory+[IO.Path]::PathSeparator+$savedPath
  if ((Get-Command wsl.exe).Source -ne $nativeFixturePath) { throw 'Native fixture did not take precedence on PATH' }
  $result=Invoke-WslNative -Arguments @('ok')
  if ($result.ExitCode -ne 0 -or -not (($result.Stdout -join "`n") | ConvertFrom-Json).ok -or -not ($result.Stderr -join "`n").Contains('systemd user session')) { throw 'Native stdout and stderr must stay separate' }
  $json=Invoke-Wsl -Arguments @('ok') | Out-String | ConvertFrom-Json
  if (-not $json.ok -or $ErrorActionPreference -ne 'Stop') { throw 'Successful native stderr must not abort or corrupt structured output' }
  $rejected=$false
  try { Invoke-Wsl -Arguments @('fail') | Out-Null } catch { $rejected=$_.Exception.Message -like '*exit 7*' -and $_.Exception.Message -like '*systemd user session*' }
  if (-not $rejected) { throw 'Nonzero native exits must still stop setup with their error output' }
  $savedEncoding=$OutputEncoding
  $savedConsoleInput=[Console]::InputEncoding
  try {
    $OutputEncoding=[Text.UnicodeEncoding]::new()
    [Console]::InputEncoding=[Text.UTF8Encoding]::new($true)
    $value='fixture-'+[char]0x00e9
    $encoded=Invoke-WslNative -Arguments @('stdin') -InputText $value
    $bytes=[Convert]::FromBase64String(($encoded.Stdout -join '').Trim())
    $expected=[Text.UTF8Encoding]::new($false).GetBytes($value+[Environment]::NewLine)
    if ($encoded.ExitCode -ne 0 -or [Convert]::ToBase64String($bytes) -ne [Convert]::ToBase64String($expected)) { throw ('Native stdin fixture bytes differ: actual='+[BitConverter]::ToString($bytes)+' expected='+[BitConverter]::ToString($expected)) }
    if ($OutputEncoding.CodePage -ne 1200) { throw 'Native stdin changed the caller encoding' }
    if ([Console]::InputEncoding.CodePage -ne 65001 -or [Console]::InputEncoding.GetPreamble().Length -ne 3) { throw 'Native stdin changed the caller console encoding' }
  } finally { $OutputEncoding=$savedEncoding; [Console]::InputEncoding=$savedConsoleInput }
  $env:PATH=$savedPath
  Write-Output 'PowerShell 5.1 parsing, controller validation and resource bounds passed.'
} finally {
  $env:PATH=$savedPath
  Remove-Item -LiteralPath $fixture -Force
  Remove-Item -LiteralPath $nativeFixturePath -Force -ErrorAction SilentlyContinue
  Remove-Item -LiteralPath $nativeFixtureDirectory -Force -ErrorAction SilentlyContinue
}

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
$global:EngineboxWslFixture=@{ version='2.6.0'; calls=@(); prompts=@(); registrations=@(); starts=0; updates=0; enables=0; configuration=$null; installed=$true; enrolled=$true; claim=$null; code=$null; downloads=0; controllerChoice=$null; secret=$null; linuxCpu=16; linuxMemory=16GB; committed=$false; overrideChoice='n'; pendingId=$null; exportFailure=$false; exportInvalid=$false; enrollmentError=$null; stopped=0 }
$global:EngineboxWslFixture.claimError=$null
$global:EngineboxWslFixture.claimWarning=$false
$global:EngineboxWslFixture.warnEveryCall=$false
$global:EngineboxWslFixture.locationOverride=$null
$global:EngineboxWslFixture.retryChoice='n'
$global:EngineboxWslFixture.recoverEnrollmentOnRetry=$false
function global:Get-ItemProperty {
  param($Path,$ErrorAction)
  if ($Path -ne 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Lxss\*') { throw 'Unexpected registry lookup' }
  $location=if ($global:EngineboxWslFixture.locationOverride) { $global:EngineboxWslFixture.locationOverride } else { Join-Path $root 'distro' }
  [pscustomobject]@{DistributionName='Enginebox';BasePath=('\\?\'+$location)}
}
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
  if ($Prompt -eq 'Retry registration without reinstalling? [Y/n]') {
    if ($global:EngineboxWslFixture.recoverEnrollmentOnRetry) { $global:EngineboxWslFixture.enrollmentError=$null }
    return $global:EngineboxWslFixture.retryChoice
  }
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
function global:tar.exe {
  $global:LASTEXITCODE=0
  if ($args[0] -ne '-tf' -or -not (Test-Path -LiteralPath $args[1])) { throw 'Unexpected archive validation' }
  if ($global:EngineboxWslFixture.exportInvalid) { $global:LASTEXITCODE=1; return }
  './etc/enginebox/installation-owner'; './etc/enginebox/wsl-runtime'; './usr/local/bin/enginebox-node'; './var/lib/enginebox/'
}
function global:wsl.exe {
  $global:LASTEXITCODE=0
  $global:EngineboxWslFixture.calls+=@(($args -join ' '))
  if ($global:EngineboxWslFixture.warnEveryCall) { Write-Error "wsl: Failed to start the systemd user session for 'root'." }
  if ($args[0] -eq '--distribution' -and $args -contains '-c') {
    if ($args[4] -ne '--cd' -or $args[5] -ne '/' -or $args[8] -ne '-c' -or $args[9] -notlike 'import base64;exec*') { throw 'Ownership must use explicit Linux argv and working directory' }
    if ($global:EngineboxWslFixture.claimError) {
      $global:LASTEXITCODE=1; Write-Error $global:EngineboxWslFixture.claimError; return
    }
    if ($global:EngineboxWslFixture.claimWarning) { Write-Error "wsl: Failed to start the systemd user session for 'root'." }
    $global:EngineboxWslFixture.claim=$args[-1]
    $args[-1]
    return
  }
  switch ($args[0]) {
    '--version' { 'WSL version: '+$global:EngineboxWslFixture.version }
    '--list' { if ($global:EngineboxWslFixture.installed) { 'Enginebox' }; 'Ubuntu' }
    '--status' { return }
    '--terminate' { if ($args[1] -ne 'Enginebox') { throw 'Unrelated distribution stopped' } }
    '--export' {
      if ($args[1] -ne 'Enginebox' -or $args.Count -ne 3) { throw 'Expected filesystem export without a VHD lock' }
      if ($global:EngineboxWslFixture.exportFailure) { $global:LASTEXITCODE=1; return }
      $stream=[IO.File]::Create($args[2])
      try {
        $magic='archive-fixture'
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
  $f.installed=$false; $f.enrolled=$false; $f.warnEveryCall=$true
  $code=('a'*32)+'.'+('b'*64)
  & $flowScript -InstallDirectory (Join-Path $testRoot 'fresh') -Cpu 2 -MemoryGiB 4 -DiskGiB 20 -EnrollmentCode $code
  if ($f.downloads -ne 1 -or $f.claim -ne [Security.Principal.WindowsIdentity]::GetCurrent().User.Value -or $f.code -ne $code -or -not $f.enrolled) { throw 'Fresh distribution import/claim/registration failed' }
  $f.warnEveryCall=$false
  # A failed initial ownership claim retains the imported runtime and can be
  # retried without another import. The underlying WSL error must be visible.
  $f.installed=$false; $f.enrolled=$false; $f.claimError='Wsl/Service/E_UNEXPECTED'; $beforeStarts=$f.starts
  $claimRoot=Join-Path $testRoot 'interrupted-claim'; $failed=$false
  try { & $flowScript -InstallDirectory $claimRoot -Cpu 2 -MemoryGiB 4 -DiskGiB 20 -EnrollmentCode $code } catch { $failed=$_.Exception.Message -like '*Wsl/Service/E_UNEXPECTED*' }
  if (-not $failed -or -not $f.installed -or $f.starts -ne $beforeStarts) { throw 'Claim failure must retain the runtime and show the WSL error' }
  $f.claimError=$null; $f.claimWarning=$true; $f.calls=@(); $beforeDownloads=$f.downloads
  & $flowScript -InstallDirectory $claimRoot -Resume -EnrollmentCode $code
  if (-not $f.enrolled -or $f.downloads -ne $beforeDownloads -or ($f.calls | Where-Object { $_ -like '--install*' -or $_ -like '--unregister*' })) { throw 'Interrupted ownership must recover without reinstalling or removing state' }
  $f.claimWarning=$false
  $f.locationOverride=Join-Path $testRoot 'unrelated'; $f.calls=@(); $failed=$false
  try { & $flowScript -InstallDirectory $claimRoot -Resume } catch { $failed=$_.Exception.Message -like '*outside this installation*' }
  if (-not $failed -or ($f.calls | Where-Object { $_ -like '--distribution*' })) { throw 'A different registered location must be refused before Linux execution' }
  $f.locationOverride=$null
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
  $f.overrideChoice=''; $f.enrolled=$false; $f.pendingId='a'*32; $f.calls=@(); $f.warnEveryCall=$true
  $replacement=Join-Path $testRoot 'replacement'
  $newCode=('e'*32)+'.'+('f'*64)
  & $flowScript -InstallDirectory $replacement -Cpu 2 -MemoryGiB 4 -DiskGiB 20 -EnrollmentCode $newCode
  $f.warnEveryCall=$false
  $backups=@(Get-ChildItem -LiteralPath (Join-Path $replacement 'backups') -Directory)
  if ($backups.Count -ne 1 -or $f.code -ne $newCode -or -not $f.enrolled -or $f.stopped -ne 1) { throw 'Default override must back up and register a fresh runtime' }
  $backup=Join-Path $backups[0].FullName 'enginebox.tar'
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
  # A rejected registration retries only enrollment. It keeps the saved ID,
  # does not re-import Linux, and accepts a renewed secret for that exact ID.
  $f.enrollmentError='Controller returned HTTP 400'; $f.retryChoice=''; $f.recoverEnrollmentOnRetry=$true
  $f.secret=('a'*32)+'.'+('c'*64); $f.calls=@(); $beforeDownloads=$f.downloads
  & $flowScript -InstallDirectory $errorsRoot -Resume -EnrollmentCode $code
  if (-not $f.enrolled -or $f.code -ne $f.secret -or $f.pendingId -ne ('a'*32) -or $f.downloads -ne $beforeDownloads -or ($f.calls | Where-Object { $_ -like '--install*' -or $_ -like '--unregister*' })) { throw 'Registration retry must retain the installed runtime and saved identity' }
  if (@($f.calls | Where-Object { $_ -like '* configure' }).Count -ne 1) { throw 'Registration retry repeated runtime configuration' }
  $f.retryChoice='n'; $f.recoverEnrollmentOnRetry=$false; $f.enrollmentError=$null
  # Pasting a dashboard password is rejected locally, then the user can paste
  # the raw registration code into the same prompt without reinstalling.
  $f.enrolled=$false; $f.pendingId=$null; $f.secret=$code; $f.calls=@()
  & $flowScript -InstallDirectory $errorsRoot -Resume -EnrollmentCode 'fixture-dashboard-password'
  if (-not $f.enrolled -or $f.code -ne $code -or ($f.calls | Where-Object { $_ -like '--install*' -or $_ -like '--unregister*' })) { throw 'Invalid local code should re-prompt without reinstallation' }
  $f.enrolled=$false; $f.pendingId='a'*32; $f.calls=@()
  & $flowScript -InstallDirectory $errorsRoot -Resume -EnrollmentCode $code -EnrollmentExpires 1
  if (-not $f.enrolled -or $f.pendingId -ne ('a'*32) -or $f.downloads -ne $beforeDownloads) { throw 'Expired code must be replaced without changing identity or downloading Linux' }
  $f.secret=$null
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
  foreach ($name in @('Get-ItemProperty','Get-CimInstance','Get-ScheduledTask','Register-ScheduledTask','Start-ScheduledTask','Stop-ScheduledTask','Read-Host','Start-Process','Invoke-WebRequest','tar.exe','wsl.exe')) { Remove-Item ('Function:\global:'+$name) -ErrorAction SilentlyContinue }
  Remove-Variable EngineboxWslFixture -Scope Global
}
