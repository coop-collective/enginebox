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
$testRoot=Join-Path $env:TEMP ('enginebox-wsl-flow-'+[Guid]::NewGuid())
New-Item -ItemType Directory $testRoot | Out-Null
$flowScript=Join-Path $testRoot 'install.ps1'
$fixtureImage=Join-Path $testRoot 'image-fixture'
[IO.File]::WriteAllText($fixtureImage,'fixture-rootfs')
$fixtureHash=(Get-FileHash -Algorithm SHA256 -LiteralPath $fixtureImage).Hash
(Get-Content -Raw -LiteralPath $path).Replace('@@ROOTFS_URL@@','https://fixture.invalid/enginebox.wsl').Replace('@@ROOTFS_SHA256@@',$fixtureHash) | Set-Content -LiteralPath $flowScript -Encoding UTF8
$global:EngineboxWslFixture=@{ version='2.6.0'; calls=@(); prompts=@(); registrations=@(); starts=0; updates=0; configuration=$null; installed=$true; enrolled=$true; claim=$null; code=$null; downloads=0 }
function global:Get-CimInstance { [pscustomobject]@{NumberOfLogicalProcessors=16;TotalPhysicalMemory=32GB} }
function global:Get-ScheduledTask { return $null }
function global:Register-ScheduledTask {
  param($TaskName,$Action,$Trigger,$Principal,$Description,$Settings,[switch]$Force)
  $global:EngineboxWslFixture.registrations+=@($Action.Execute)
}
function global:Start-ScheduledTask { param($TaskName) $global:EngineboxWslFixture.starts++ }
function global:Read-Host {
  param($Prompt,[switch]$AsSecureString)
  if ($AsSecureString) { throw 'An already registered fixture must not prompt for a secret' }
  $global:EngineboxWslFixture.prompts+=@($Prompt)
  return ''
}
function global:Start-Process {
  param($FilePath,$ArgumentList,$Verb,[switch]$Wait,[switch]$PassThru)
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
    '--install' {
      if ($args.Count -ne 12 -or $args[1] -ne '--from-file' -or $args[3] -ne '--name' -or $args[4] -ne 'Enginebox' -or $args[7] -ne '--version' -or $args[8] -ne '2' -or $args[9] -ne '--vhd-size' -or $args[10] -ne '20GB' -or $args[11] -ne '--no-launch') { throw ('Incorrect import argv: '+($args -join ' ')) }
      $global:EngineboxWslFixture.installed=$true
    }
    '--distribution' {
      switch ($args[-1]) {
        '/etc/enginebox/installation-owner' { [Security.Principal.WindowsIdentity]::GetCurrent().User.Value }
        'configure' { $global:EngineboxWslFixture.configuration=($input | Out-String | ConvertFrom-Json) }
        'enrolled' { if (-not $global:EngineboxWslFixture.enrolled) { $global:LASTEXITCODE=1 }; return }
        'claim' { $global:EngineboxWslFixture.claim=($input | Out-String).Trim() }
        'enroll' { $global:EngineboxWslFixture.code=($input | Out-String).Trim(); $global:EngineboxWslFixture.enrolled=$true }
        default { throw 'Unexpected WSL mutation' }
      }
    }
    default { throw 'Unexpected WSL operation' }
  }
}
try {
  & $flowScript -InstallDirectory (Join-Path $testRoot 'ready')
  $f=$global:EngineboxWslFixture
  if ($f.prompts.Count -ne 4 -or $f.configuration.controller -ne 'https://enginebox.coopcollective.dev' -or $f.configuration.cpu -ne 4 -or $f.configuration.memory_gib -ne 8) { throw 'Installer defaults or prompts were not applied' }
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
  Write-Output 'Fresh import and stdin registration, default prompts, custom controller/resources, WSL upgrade, resumed settings and WSL-only startup passed.'
} finally {
  Remove-Item -LiteralPath $testRoot -Recurse -Force
  foreach ($name in @('Get-CimInstance','Get-ScheduledTask','Register-ScheduledTask','Start-ScheduledTask','Read-Host','Start-Process','Invoke-WebRequest','wsl.exe')) { Remove-Item ('Function:\global:'+$name) -ErrorAction SilentlyContinue }
  Remove-Variable EngineboxWslFixture -Scope Global
}
