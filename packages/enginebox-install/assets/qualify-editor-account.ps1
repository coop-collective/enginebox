<#
Qualifies a Windows engine machine for running the Unity Editor as the low-privilege `enginebox-editor` account and
agent runs as `eb-agent-NN` slot accounts (docs/AGENT-FORKS.md), before the owner sets engine.json
`"editor_account": "enginebox-editor"`. Run elevated (e.g. over SSH as the administrator) after
`install-worker-node.ps1 -EngineAccount` and one node start (which seals the account passwords).

The checks run inside `enginebox-node.exe engine-qualify-editor`, started by a one-shot scheduled task AS the engine
account in its signed-in desktop session, because only that account can unseal private\accounts.dpapi. It checks:
  accounts   - the editor and slot passwords are sealed (DPAPI, engine account scope) and unseal.
  spawn      - CreateProcessWithLogonW as enginebox-editor, suspended, assigned to a nested UI-limited Job Object,
               resumed: `whoami` reports <machine>\enginebox-editor.
  acl        - as enginebox-editor: reading private\worker.json, private\accounts.dpapi and a node-only session
               file fails (access denied).
  unity      - Unity (and the licensing client when -UnityAccessToken is given) starts as enginebox-editor on
               WinSta0\Default with the editor bridge; bridge /status answers to the engine account; /frame
               advances (it renders); unity.log names a hardware GPU renderer (not WARP / Microsoft Basic Render
               Driver); Play mode is entered; terminating the session Job Object ends the Editor tree; the editor
               account's licensing logs hold no token after scrubbing.
  slot       - CreateProcessWithLogonW as eb-agent-01 (logon flags 0) into a UI-limited job on the private desktop
               WinSta0\eb-agents: it cannot read the dev tree or private\; a TCP connect to the default gateway
               (LAN, port 80/443) is blocked by the per-account firewall rule while 127.0.0.1 works.
Exit code 0 only when every check passes.
#>
[CmdletBinding()]
param(
  [string]$InstallDirectory=(Join-Path $env:ProgramData 'Enginebox\engine-node'),
  [Parameter(Mandatory)][string]$Project,
  [ValidateRange(60,7200)][int]$Seconds=600,
  [switch]$UnityAccessToken
)
$ErrorActionPreference='Stop'; Set-StrictMode -Version Latest
$principal=[Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Run this from an elevated PowerShell (Run as administrator).' }
if ($env:OS -ne 'Windows_NT' -or -not [Environment]::Is64BitOperatingSystem) { throw 'Windows x64 is required.' }
$root=[IO.Path]::GetFullPath($InstallDirectory)
$receiptPath=Join-Path $root 'installation.json'
if (-not (Test-Path -LiteralPath $receiptPath)) { throw "No Engine Box installation at $root." }
$receipt=Get-Content -Raw -LiteralPath $receiptPath | ConvertFrom-Json
foreach ($name in @('engineAccount','editorAccount')) { if (-not $receipt.PSObject.Properties[$name]) { throw "The installation has no $name; re-run install-worker-node.ps1 -EngineAccount." } }
$engineUser=$receipt.engineAccount
$binary=Join-Path $root 'enginebox-node.exe'; $engineConfig=Join-Path $root 'engine.json'
foreach ($file in @($binary,$engineConfig)) { if (-not (Test-Path -LiteralPath $file)) { throw "Missing $file." } }
$Project=[IO.Path]::GetFullPath($Project)
if (-not (Test-Path -LiteralPath (Join-Path $Project 'ProjectSettings\ProjectVersion.txt'))) { throw "$Project is not a Unity project." }
foreach ($path in @($root,$Project)) { if ($path -match '["%]') { throw "Paths may not contain quotes or percent signs: $path" } }
# The checks need the engine account's desktop session (autologon signs it in at boot, then locks).
$signedIn=@(Get-Process -IncludeUserName | Where-Object { $_.UserName -like "*\$engineUser" -and $_.SessionId -ge 1 })
if ($signedIn.Count -eq 0) { throw "$engineUser is not signed in. Restart Windows (automatic sign-in) or sign it in, then retry." }
$engineSid=(Get-LocalUser -Name $engineUser).SID
# A working directory beside private\ (which only the engine account may open): engine account + Administrators.
$dir=Join-Path $root 'qualify'
if (Test-Path -LiteralPath $dir) { Remove-Item -LiteralPath $dir -Recurse -Force }
New-Item -ItemType Directory -Path $dir | Out-Null
$acl=New-Object Security.AccessControl.DirectorySecurity
$acl.SetAccessRuleProtection($true,$false)
$admins=New-Object Security.Principal.SecurityIdentifier('S-1-5-32-544')
$acl.SetOwner($admins)
foreach ($sid in @($engineSid,$admins)) { $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($sid,'FullControl','ContainerInherit,ObjectInherit','None','Allow'))) }
Set-Acl -LiteralPath $dir -AclObject $acl
$result=Join-Path $dir 'result.json'; $log=Join-Path $dir 'node.log'; $runner=Join-Path $dir 'run.cmd'
$arguments="engine-qualify-editor --config `"$engineConfig`" --project `"$Project`" --out `"$result`" --seconds $Seconds"
if ($UnityAccessToken) {
  # Never argv: the token goes to a file only the engine account can read; the node deletes it after reading.
  $secret=Read-Host 'Unity access token for the editor account''s activation' -AsSecureString
  $ptr=[Runtime.InteropServices.Marshal]::SecureStringToBSTR($secret)
  $tokenFile=Join-Path $dir 'unity-token'
  try { [IO.File]::WriteAllText($tokenFile,[Runtime.InteropServices.Marshal]::PtrToStringBSTR($ptr),(New-Object Text.UTF8Encoding($false))) }
  finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($ptr); $secret.Dispose() }
  & icacls.exe $tokenFile /setowner "*$($engineSid.Value)" /C /Q 2>$null | Out-Null
  & icacls.exe $tokenFile /inheritance:r /grant:r "*$($engineSid.Value):F" /Q 2>$null | Out-Null
  & icacls.exe $tokenFile /remove:g "*S-1-5-32-544" /Q 2>$null | Out-Null
  if ($LASTEXITCODE -ne 0) { Remove-Item -LiteralPath $tokenFile -Force; throw 'Could not restrict the token file to the engine account.' }
  $arguments+=" --unity-token-file `"$tokenFile`""
}
[IO.File]::WriteAllText($runner,"@echo off`r`n`"$binary`" $arguments > `"$log`" 2>&1`r`n",([Text.Encoding]::ASCII))
# schtasks.exe, not the ScheduledTasks module (Get/Unregister-ScheduledTask fail on some machines). /IT: only while
# the account is signed in, so the task runs in its interactive session.
$task='Enginebox Qualify Editor'
& schtasks.exe /Create /TN $task /TR "`"$runner`"" /SC ONCE /ST 00:00 /RU $engineUser /IT /F | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Could not register the qualification task.' }
try {
  & schtasks.exe /Run /TN $task | Out-Null
  if ($LASTEXITCODE -ne 0) { throw 'Could not start the qualification task.' }
  Write-Output "Qualifying as $engineUser (up to $($Seconds+120) s)..."
  $deadline=(Get-Date).AddSeconds($Seconds+120); $started=Get-Date
  while (-not (Test-Path -LiteralPath $result) -and (Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 2
    $state=(& schtasks.exe /Query /TN $task /FO CSV /NH | ConvertFrom-Csv -Header TaskName,Next,Status | Select-Object -First 1).Status
    if ($state -ne 'Running' -and ((Get-Date)-$started).TotalSeconds -gt 30) { Start-Sleep -Seconds 2; break }
  }
} finally {
  # Windows PowerShell turns native stderr into errors; a task that already ended is fine.
  try { & schtasks.exe /End /TN $task 2>$null | Out-Null } catch {}
  try { & schtasks.exe /Delete /TN $task /F 2>$null | Out-Null } catch {}
}
$report=$null
if (Test-Path -LiteralPath $result) {
  try { $text=Get-Content -Raw -LiteralPath $result }
  catch { & takeown.exe /F $result /A 2>$null | Out-Null; & icacls.exe $result /grant '*S-1-5-32-544:R' /Q 2>$null | Out-Null; $text=Get-Content -Raw -LiteralPath $result }
  $report=$text | ConvertFrom-Json
}
if (-not $report) {
  Write-Warning 'The node wrote no result. Its output:'
  if (Test-Path -LiteralPath $log) { Get-Content -LiteralPath $log -Tail 60 }
} else {
  $report.checks | ForEach-Object { [pscustomobject]@{check=$_.name;result=$(if ($_.ok) { 'PASS' } else { 'FAIL' });detail=$_.detail} } | Format-Table -AutoSize -Wrap | Out-String -Width 200
  if ($report.PSObject.Properties['unity_log_tail'] -and $report.unity_log_tail) { Write-Output '--- unity.log (last 40 lines) ---'; ($report.unity_log_tail -split "`r?`n") | Select-Object -Last 40 }
}
Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
if (Test-Path -LiteralPath $dir) { & takeown.exe /F $dir /R /A /D Y 2>$null | Out-Null; & icacls.exe $dir /grant '*S-1-5-32-544:(OI)(CI)F' /T /C /Q 2>$null | Out-Null; Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue }
if ($report -and $report.ok -eq $true) { Write-Output 'QUALIFIED: Unity may run as the editor account (set engine.json "editor_account": "enginebox-editor").'; exit 0 }
Write-Output 'NOT QUALIFIED: keep the single-account Editor (no editor_account in engine.json).'
exit 1
