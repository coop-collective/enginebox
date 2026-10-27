$ErrorActionPreference='Stop'
$directory=Join-Path $env:TEMP ('enginebox-startup-proof-'+[guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $directory | Out-Null
$fixture=Join-Path $directory 'installer.ps1'
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'install.ps1.in') -Destination $fixture
. $fixture
Import-Module ScheduledTasks
$TaskName='Enginebox startup proof '+[guid]::NewGuid().ToString('N')
$statePath=Join-Path $directory 'runs.json'
$script:definition=$null
# Capture the production definition before registering anything. Replace only
# its action with a disposable process; never launch a real WSL worker here.
function Register-ScheduledTask {
  param($TaskName,$Action,$Trigger,$Principal,$Settings,$Description,[switch]$Force)
  $script:definition=New-ScheduledTask -Action $Action -Trigger $Trigger -Principal $Principal -Settings $Settings -Description $Description
}
try {
  $sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
  Register-RuntimeTask $sid
  $child=@'
$ErrorActionPreference='Stop'
$path='__STATE__'
$runs=if (Test-Path -LiteralPath $path) { [int](Get-Content -LiteralPath $path -Raw | ConvertFrom-Json).runs+1 } else { 1 }
@{runs=$runs;session=(Get-Process -Id $PID).SessionId;sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value} | ConvertTo-Json | Set-Content -LiteralPath $path -Encoding UTF8
if ($runs -eq 1) { exit 1 }
exit 0
'@
  $encoded=[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($child.Replace('__STATE__',$statePath.Replace("'","''"))))
  $script:definition.Actions=@(New-ScheduledTaskAction -Execute (Join-Path $env:WINDIR 'System32\WindowsPowerShell\v1.0\powershell.exe') -Argument ('-NoProfile -NonInteractive -EncodedCommand '+$encoded))
  ScheduledTasks\Register-ScheduledTask -TaskName $TaskName -InputObject $script:definition | Out-Null
  Start-ScheduledTask -TaskName $TaskName
  $deadline=(Get-Date).AddSeconds(100)
  $proof=$null
  do {
    Start-Sleep -Seconds 1
    if (Test-Path -LiteralPath $statePath) {
      try { $proof=Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json } catch { continue }
    }
    $task=Get-ScheduledTask -TaskName $TaskName
    $info=Get-ScheduledTaskInfo -TaskName $TaskName
    if ($proof -and $proof.runs -eq 2 -and $task.State -eq 'Ready' -and $info.LastTaskResult -eq 0) { break }
  } while ((Get-Date) -lt $deadline)
  [ordered]@{proof=$proof;taskState=[string]$task.State;lastResult=$info.LastTaskResult;lastRun=$info.LastRunTime.ToString('o')} | ConvertTo-Json -Depth 4 | Write-Output
  if (-not $proof -or $proof.runs -ne 2 -or $proof.session -ne 0 -or $proof.sid -ne $sid -or $info.LastTaskResult -ne 0) { throw 'Passwordless noninteractive startup/crash recovery did not complete' }
  Write-Output 'Real S4U task ran in session 0 and automatically recovered one failed process after one minute. No reboot was performed.'
} finally {
  if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    Stop-ScheduledTask -TaskName $TaskName
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
  }
  Remove-Item -LiteralPath $directory -Recurse -Force
}
