param([string]$Name='EngineboxCI')
$ErrorActionPreference='Stop'
if ($Name -notin @('EngineboxCI','EngineboxCIBackup')) { throw 'Only disposable CI distributions may be restarted by this test' }
$fixture=Join-Path $env:TEMP ('enginebox-systemd-'+[guid]::NewGuid().ToString('N')+'.ps1')
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'install.ps1.in') -Destination $fixture
try {
  . $fixture
  Invoke-Wsl -Arguments @('--terminate',$Name) | Out-Host
  $boot=Invoke-WslNative -Arguments @('--distribution',$Name,'--user','root','--cd','/','--exec','/usr/bin/true')
  if ($boot.ExitCode -ne 0 -or ($boot.Stderr -join "`n") -match 'Failed to start the systemd user session') { throw "WSL root login did not start cleanly: $(Get-WslFailure $boot)" }
  $state=Invoke-Wsl -Arguments @('--distribution',$Name,'--user','root','--cd','/','--exec','systemctl','is-active','user@0.service')
  if (($state | Out-String).Trim() -ne 'active') { throw 'Root systemd user manager is not active' }
  Invoke-Wsl -Arguments @('--distribution',$Name,'--user','root','--cd','/','--exec','test','-S','/run/user/0/bus') | Out-Host
  Write-Output 'Real WSL cold boot: root user manager and D-Bus session are active without the startup warning.'
} finally { Remove-Item -LiteralPath $fixture -Force }
