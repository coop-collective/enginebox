[CmdletBinding()]
param(
  [ValidateSet('Install','Uninstall')][string]$Mode='Install',
  [string]$Controller,
  [string]$EnrollmentCode,
  [ValidateRange(1,256)][int]$Cpu=4,
  [ValidateRange(2,4096)][int]$MemoryGiB=8,
  [ValidateRange(10,65536)][int]$DiskGiB=50,
  [string]$ExpectedBinarySha256,
  [switch]$SkipDependencies,
  [string]$InstallDirectory=(Join-Path $env:LOCALAPPDATA 'Enginebox\worker-node')
)
$ErrorActionPreference='Stop'; Set-StrictMode -Version Latest
$root=[IO.Path]::GetFullPath($InstallDirectory)
$binary=Join-Path $root 'enginebox-node.exe'
$config=Join-Path $root 'private\worker.json'
$receiptPath=Join-Path $root 'installation.json'
$taskName='Enginebox CI and Agents Node'
$marker='Enginebox contributor schema 1'
function Assert-PlainPath([string]$Path) {
  $cursor=$Path
  while ($cursor) {
    if ((Test-Path -LiteralPath $cursor) -and ((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Installation path contains a junction or symlink.' }
    $parent=Split-Path -Parent $cursor; if ($parent -eq $cursor) { break }; $cursor=$parent
  }
}
function Owned-Task {
  $task=Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
  if ($task -and ($task.Description -ne $marker -or $task.Actions[0].Execute -ne $binary)) { throw 'An unrelated scheduled task owns this name.' }
  return $task
}
function Find-Tool([string]$Name) {
  $cmd=Get-Command ($Name+'.exe') -ErrorAction SilentlyContinue
  if ($cmd) { return $cmd.Source }
  $candidates=@((Join-Path $env:LOCALAPPDATA "Microsoft\WinGet\Links\$Name.exe"),(Join-Path $env:ProgramFiles "PowerShell\7\$Name.exe"),(Join-Path $env:ProgramFiles "Git\cmd\$Name.exe"))
  foreach ($candidate in $candidates) { if (Test-Path -LiteralPath $candidate) { return $candidate } }
  return $null
}
if ($env:OS -ne 'Windows_NT' -or -not [Environment]::Is64BitOperatingSystem -or $env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { throw 'Windows x64 is required.' }
Assert-PlainPath $root
if ($root.Contains('"')) { throw 'Installation path contains a quote.' }
$task=Owned-Task
if (Test-Path -LiteralPath $receiptPath) {
  $receipt=Get-Content -Raw -LiteralPath $receiptPath | ConvertFrom-Json
  if ($receipt.marker -ne $marker -or $receipt.root -ne $root) { throw 'Installation ownership mismatch.' }
} elseif ((Test-Path -LiteralPath $root) -and @(Get-ChildItem -LiteralPath $root -Force).Count -gt 0) { throw 'Installation directory is not empty and has no ownership receipt.' }
if ($Mode -eq 'Uninstall') {
  if (-not (Test-Path -LiteralPath $receiptPath)) { throw 'No owned installation exists.' }
  if ($task) { Stop-ScheduledTask -TaskName $taskName; Unregister-ScheduledTask -TaskName $taskName -Confirm:$false }
  Remove-Item -LiteralPath $binary -Force -ErrorAction SilentlyContinue
  Write-Output 'Worker task and binary removed. Identity and work files retained. Revoke this machine in the dashboard.'
  exit 0
}
if (-not $Controller) { $Controller=Read-Host 'Controller HTTPS origin' }
$uri=$null
if (-not [Uri]::TryCreate($Controller,[UriKind]::Absolute,[ref]$uri) -or $uri.Scheme -ne 'https' -or $uri.UserInfo -or $uri.Query -or $uri.Fragment -or $uri.AbsolutePath -ne '/') { throw 'Controller must be an HTTPS origin.' }
$computer=Get-CimInstance Win32_ComputerSystem
$logical=[int]$computer.NumberOfLogicalProcessors
$memory=[math]::Floor([double]$computer.TotalPhysicalMemory/1GB)
if ($Cpu -gt $logical -or $MemoryGiB -gt ($memory-1)) { throw "Resource allowance exceeds this machine. Choose at most $logical CPUs and $($memory-1) GiB memory in the dashboard and create a new command." }
$drive=[IO.DriveInfo]::new([IO.Path]::GetPathRoot($root))
if (-not (Test-Path -LiteralPath $config) -and $drive.AvailableFreeSpace -lt (($DiskGiB+1)*1GB)) { throw 'Insufficient free disk space for this allowance plus 1 GiB reserve.' }
$source=Join-Path $PSScriptRoot 'enginebox-node.exe'
if ($ExpectedBinarySha256 -notmatch '^[a-fA-F0-9]{64}$' -or (Get-FileHash -Algorithm SHA256 -LiteralPath $source).Hash -ne $ExpectedBinarySha256) { throw 'Worker executable checksum mismatch.' }
if (Test-Path -LiteralPath $config) {
  $existing=Get-Content -Raw -LiteralPath $config | ConvertFrom-Json
  if ([Uri]$existing.controller -ne $uri -or $existing.budget.cpu -ne $Cpu -or $existing.budget.memory_gib -ne $MemoryGiB -or $existing.budget.disk_gib -ne $DiskGiB) { throw 'Existing registration has a different host or budget. Revoke it and use a fresh installation directory.' }
}
New-Item -ItemType Directory -Force -Path $root | Out-Null
$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User
$acl=New-Object Security.AccessControl.DirectorySecurity
$acl.SetOwner($sid); $acl.SetAccessRuleProtection($true,$false)
$acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($sid,'FullControl','ContainerInherit,ObjectInherit','None','Allow')))
Set-Acl -LiteralPath $root -AclObject $acl
if ($task) {
  Stop-ScheduledTask -TaskName $taskName
  $deadline=(Get-Date).AddSeconds(15)
  while ((Get-ScheduledTask -TaskName $taskName).State -eq 'Running') {
    if ((Get-Date) -gt $deadline) { throw 'Previous node task did not stop.' }; Start-Sleep -Milliseconds 200
  }
}
Copy-Item -LiteralPath $source -Destination $binary -Force
@{marker=$marker;root=$root;binarySha256=$ExpectedBinarySha256} | ConvertTo-Json | Set-Content -LiteralPath $receiptPath -Encoding UTF8
if (-not (Test-Path -LiteralPath $config) -or -not (Get-Content -Raw -LiteralPath $config | ConvertFrom-Json).enrolled) {
  if (-not $EnrollmentCode) {
    $secret=Read-Host 'One-time registration code from the dashboard' -AsSecureString
    $ptr=[Runtime.InteropServices.Marshal]::SecureStringToBSTR($secret)
    try { $EnrollmentCode=[Runtime.InteropServices.Marshal]::PtrToStringBSTR($ptr) } finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($ptr); $secret.Dispose() }
  }
  # Consume the code before tool downloads so long installs cannot expire it.
  $EnrollmentCode | & $binary register-worker --controller ($uri.GetLeftPart([UriPartial]::Authority)) --config $config --cpu $Cpu --memory-gib $MemoryGiB --disk-gib $DiskGiB
  $EnrollmentCode=$null
  if ($LASTEXITCODE -ne 0) { throw 'Registration failed. Retry the same command before expiry; an existing pending identity is retained.' }
}
$action=New-ScheduledTaskAction -Execute $binary -Argument ('run-worker --config "{0}"' -f $config) -WorkingDirectory $root
$trigger=New-ScheduledTaskTrigger -AtLogOn -User $sid.Value
$principal=New-ScheduledTaskPrincipal -UserId $sid.Value -LogonType Interactive -RunLevel Limited
$settings=New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Description $marker -Force | Out-Null
Start-ScheduledTask -TaskName $taskName
$identity=Get-Content -Raw -LiteralPath $config | ConvertFrom-Json
if (-not $SkipDependencies) {
  $required=@()
  if ($identity.workloads.ci) { $required+=@(@{name='git';id='Git.Git'},@{name='pwsh';id='Microsoft.PowerShell'}) }
  if ($identity.workloads.agents) { $required+=@(@{name='codex';id='OpenAI.Codex'}) }
  foreach ($tool in $required) {
    if (Find-Tool $tool.name) { continue }
    $winget=Get-Command winget.exe -ErrorAction SilentlyContinue
    if (-not $winget) { Write-Warning "Registered. Install $($tool.id) to enable this workload; WinGet is unavailable."; continue }
    & $winget.Source install --id $tool.id --exact --source winget --accept-source-agreements --accept-package-agreements
    if ($LASTEXITCODE -ne 0) { Write-Warning "Registered, but $($tool.id) installation failed. The dashboard will report the missing tool." }
  }
}
if ($identity.workloads.agents -and (Find-Tool 'codex')) {
  $answer=Read-Host 'Sign in to native Codex for this worker now? [Y/n]'
  if ($answer -notmatch '^(n|no)$') {
    $providerRoot=Join-Path $root 'work\providers\codex'; New-Item -ItemType Directory -Force -Path $providerRoot | Out-Null
    $previousCodexConfig=$env:CODEX_HOME
    try { $env:CODEX_HOME=$providerRoot; & (Find-Tool 'codex') login } finally { $env:CODEX_HOME=$previousCodexConfig }
  }
}
Write-Output "Registered this machine for its selected CI/agent workloads. It starts at sign-in. Status: $root\private\status.json"
Write-Output "Shared limits: $Cpu CPUs, $MemoryGiB GiB RAM; workspace disk watchdog: $DiskGiB GiB. Check readiness in the dashboard."
