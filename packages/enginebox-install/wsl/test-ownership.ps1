param([Parameter(Mandatory=$true)][string]$Location)
$ErrorActionPreference='Stop'
$fixture=Join-Path $env:TEMP ('enginebox-ownership-'+[guid]::NewGuid().ToString('N')+'.ps1')
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'install.ps1.in') -Destination $fixture
try {
  . $fixture
  # This workflow-owned distribution has just been imported with --no-launch.
  # Exercise the actual first boot through Windows PowerShell 5.1, not a mock.
  Initialize-RuntimeOwnership 'EngineboxCI' $Location
  $sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
  $owner=(& wsl.exe -d EngineboxCI -u root --cd / --exec cat /etc/enginebox/installation-owner | Out-String).Trim()
  if ($LASTEXITCODE -or $owner -ne $sid) { throw 'Cold-start ownership was not recorded' }
  $before=(& wsl.exe -d EngineboxCI -u root --cd / --exec stat -c '%i:%Y' /etc/enginebox/installation-owner | Out-String).Trim()
  if ($LASTEXITCODE) { throw 'Could not inspect ownership marker' }
  Initialize-RuntimeOwnership 'EngineboxCI' $Location
  $after=(& wsl.exe -d EngineboxCI -u root --cd / --exec stat -c '%i:%Y' /etc/enginebox/installation-owner | Out-String).Trim()
  if ($LASTEXITCODE -or $before -ne $after) { throw 'Repeated ownership setup rewrote existing state' }
  function Invoke-FixturePython([string]$Source) {
    $encoded=[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($Source))
    & wsl.exe -d EngineboxCI -u root --cd / --exec python3 -c "import base64;exec(base64.b64decode('$encoded'))" $sid
    if ($LASTEXITCODE) { throw 'Disposable ownership fixture failed' }
  }
  Invoke-FixturePython "import pathlib;pathlib.Path('/etc/enginebox/installation-owner').write_text('S-1-5-21-0')"
  $rejected=$false
  try { Initialize-RuntimeOwnership 'EngineboxCI' $Location } catch { $rejected=$_.Exception.Message -like '*different Windows owner*' }
  $foreign=(& wsl.exe -d EngineboxCI -u root --cd / --exec cat /etc/enginebox/installation-owner | Out-String).Trim()
  if ($LASTEXITCODE -or -not $rejected -or $foreign -ne 'S-1-5-21-0') { throw 'A foreign ownership marker was overwritten' }
  Invoke-FixturePython "import pathlib;pathlib.Path('/etc/enginebox/installation-owner').unlink();pathlib.Path('/etc/enginebox/settings.json').write_text('{}')"
  $rejected=$false
  try { Initialize-RuntimeOwnership 'EngineboxCI' $Location } catch { $rejected=$_.Exception.Message -like '*Configured runtime has no ownership marker*' }
  if (-not $rejected) { throw 'Configured state without ownership must not be adopted' }
  Invoke-FixturePython "import pathlib;assert not pathlib.Path('/etc/enginebox/installation-owner').exists();assert pathlib.Path('/etc/enginebox/settings.json').read_text()=='{}';pathlib.Path('/etc/enginebox/settings.json').unlink()"
  Initialize-RuntimeOwnership 'EngineboxCI' $Location
  $rejected=$false
  try { Initialize-RuntimeOwnership 'EngineboxCI' ($Location+'-unrelated') } catch { $rejected=$_.Exception.Message -like '*outside this installation*' }
  if (-not $rejected) { throw 'Ownership accepted a different Windows installation path' }
  Write-Output 'Windows PowerShell 5.1 real WSL cold-start ownership, idempotent retry, foreign owner/configured state refusal and registration path isolation passed.'
} finally { Remove-Item -LiteralPath $fixture -Force }
