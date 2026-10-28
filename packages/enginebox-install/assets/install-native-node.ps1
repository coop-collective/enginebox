# Run from the verified native-node CI/release artifact. No administrator token is accepted.
[CmdletBinding()]
param(
  [ValidateSet('Inspect','Install','Uninstall')][string]$Mode = 'Inspect',
  [string]$Controller,
  [string]$FfmpegPath,
  [string]$WorkerConfig,
  [string]$ExpectedBinarySha256,
  [switch]$RequireQuic,
  [string]$InstallDirectory = (Join-Path $env:LOCALAPPDATA 'Enginebox\native-node')
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$taskName = 'Enginebox Native GPU Node'
$marker = 'Enginebox native-node schema 1'
$root = [IO.Path]::GetFullPath($InstallDirectory)
$receiptPath = Join-Path $root 'install-receipt.json'
$configPath = Join-Path $root 'private\node.json'
$nodePath = Join-Path $root 'enginebox-node.exe'

function Assert-OwnedTask {
  $task = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
  if ($task -and ($task.Description -ne $marker -or $task.Actions[0].Execute -ne $nodePath)) {
    throw 'A different task owns this name; refusing to modify it.'
  }
  return $task
}
function Assert-PlainPath([string]$Path) {
  $cursor=$Path
  while ($cursor) {
    if (Test-Path -LiteralPath $cursor) {
      if ((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw 'Installation path contains a symlink or junction.'
      }
    }
    $parent=Split-Path -Parent $cursor
    if ($parent -eq $cursor) { break }; $cursor=$parent
  }
}
if ($env:OS -ne 'Windows_NT' -or -not [Environment]::Is64BitOperatingSystem) { throw 'Windows x64 is required.' }
if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { throw 'This package is for Windows x64, not ARM64.' }
Assert-PlainPath $root
if ($root.Contains('"')) { throw 'Installation path contains an unsupported quote.' }

if ($Mode -eq 'Uninstall') {
  if (-not (Test-Path -LiteralPath $receiptPath)) { throw 'No installation ownership receipt exists.' }
  $receipt=Get-Content -Raw -LiteralPath $receiptPath | ConvertFrom-Json
  if ($receipt.marker -ne $marker -or $receipt.root -ne $root) { throw 'Installation ownership mismatch.' }
  $task=Assert-OwnedTask
  if ($task) { Stop-ScheduledTask -TaskName $taskName; Unregister-ScheduledTask -TaskName $taskName -Confirm:$false }
  # Keep private identity/evidence so a reinstall can reconnect; server revocation is explicit.
  if (Test-Path -LiteralPath $nodePath) { Remove-Item -LiteralPath $nodePath }
  Remove-Item -LiteralPath $receiptPath
  Write-Output 'Node task and binary removed. Private identity retained; revoke the node at its controller to retire it permanently.'
  exit 0
}

$os=Get-CimInstance Win32_OperatingSystem
$cpu=Get-CimInstance Win32_Processor | Select-Object -First 1
$computer=Get-CimInstance Win32_ComputerSystem
$gpu=@(Get-CimInstance Win32_VideoController | Select-Object Name,DriverVersion)
if (-not $FfmpegPath) {
  $ffmpeg=Get-Command ffmpeg.exe -ErrorAction SilentlyContinue
  if ($ffmpeg) { $FfmpegPath=$ffmpeg.Source }
}
$prerequisites=[ordered]@{
  windows=$os.Caption; build=$os.BuildNumber; gpu=$gpu
  virtualizationFirmwareEnabled=$cpu.VirtualizationFirmwareEnabled
  secondLevelAddressTranslation=$cpu.SecondLevelAddressTranslationExtensions
  hypervisorPresent=$computer.HypervisorPresent
  profile='nvidia-nvenc'; virtualMachineRequired=$false
  interactiveSessionRequired=$true; ffmpegPresent=[bool]($FfmpegPath -and (Test-Path -LiteralPath $FfmpegPath))
}
$prerequisites | ConvertTo-Json -Depth 4
if (-not $prerequisites.ffmpegPresent) { throw 'Install FFmpeg with h264_nvenc support and supply -FfmpegPath. VM/WSL installation is not required for native rendering.' }
$source=Join-Path $PSScriptRoot 'enginebox-node.exe'
if (-not (Test-Path -LiteralPath $source)) { throw 'Run this installer beside enginebox-node.exe from the downloaded native-node artifact.' }
if (-not $ExpectedBinarySha256 -or $ExpectedBinarySha256 -notmatch '^[a-fA-F0-9]{64}$') {
  throw 'Supply -ExpectedBinarySha256 from the trusted CI/release checksum before executing the node binary.'
}
if ((Get-FileHash -Algorithm SHA256 -LiteralPath $source).Hash -ne $ExpectedBinarySha256) { throw 'Node binary checksum mismatch.' }
$networkArgs=@('network-check','--profile','streaming')
if ($Controller) { $networkArgs+=@('--controller',$Controller) }
if ($RequireQuic) { $networkArgs+='--require-quic' }
& $source @networkArgs
if ($LASTEXITCODE -ne 0) { throw 'Required network preflight failed; installation has not changed this host.' }
Write-Output 'Network results describe this connection now. Authenticated media, TURN allocation and engine streaming still require qualification. Rerun network-check after VPN/network changes.'
& $source inspect --profile nvidia-nvenc --ffmpeg $FfmpegPath --qualify
if ($LASTEXITCODE -ne 0) { throw 'NVIDIA hardware encode/decode qualification failed; installation has not changed this host.' }
if ($Mode -eq 'Inspect') { exit 0 }

$uri=$null
if (-not [Uri]::TryCreate($Controller,[UriKind]::Absolute,[ref]$uri) -or $uri.Scheme -ne 'https' -or $uri.UserInfo -or $uri.Query -or $uri.Fragment -or $uri.AbsolutePath -ne '/') {
  throw 'Controller must be an HTTPS origin such as https://controller.example.com.'
}
$null=Assert-OwnedTask
if ((Test-Path -LiteralPath $root) -and -not (Test-Path -LiteralPath $receiptPath) -and @(Get-ChildItem -LiteralPath $root -Force).Count -gt 0) { throw 'Installation directory is nonempty and unowned; choose an empty directory.' }
if ((Test-Path -LiteralPath $nodePath) -and -not (Test-Path -LiteralPath $receiptPath)) { throw 'Unowned node binary exists; refusing to overwrite.' }
if (Test-Path -LiteralPath $receiptPath) {
  $receipt=Get-Content -Raw -LiteralPath $receiptPath | ConvertFrom-Json
  if ($receipt.marker -ne $marker -or $receipt.root -ne $root) { throw 'Installation ownership mismatch.' }
}
New-Item -ItemType Directory -Force -Path $root | Out-Null
$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User
$acl=New-Object Security.AccessControl.DirectorySecurity
$acl.SetOwner($sid); $acl.SetAccessRuleProtection($true,$false)
$rule=New-Object Security.AccessControl.FileSystemAccessRule($sid,'FullControl','ContainerInherit,ObjectInherit','None','Allow')
$acl.AddAccessRule($rule); Set-Acl -LiteralPath $root -AclObject $acl
$existingTask=Assert-OwnedTask
if ($existingTask) { Stop-ScheduledTask -TaskName $taskName }
Copy-Item -LiteralPath $source -Destination $nodePath -Force
@{marker=$marker;root=$root;binarySha256=$ExpectedBinarySha256} | ConvertTo-Json | Set-Content -LiteralPath $receiptPath -Encoding UTF8

if (-not (Test-Path -LiteralPath $configPath) -or -not (Get-Content -Raw -LiteralPath $configPath | ConvertFrom-Json).enrolled) {
  $secret=Read-Host 'Paste the ten-minute node enrollment credential' -AsSecureString
  $ptr=[Runtime.InteropServices.Marshal]::SecureStringToBSTR($secret)
  try {
    $enrollArgs=@('enroll','--controller',$uri.GetLeftPart([UriPartial]::Authority),'--config',$configPath,'--profile','nvidia-nvenc','--ffmpeg',[IO.Path]::GetFullPath($FfmpegPath))
    if ($WorkerConfig) { $enrollArgs+=@('--worker',[IO.Path]::GetFullPath($WorkerConfig)) }
    [Runtime.InteropServices.Marshal]::PtrToStringBSTR($ptr) | & $nodePath @enrollArgs
    if ($LASTEXITCODE -ne 0) { throw 'Enrollment failed. No scheduled task was created; retry with the same credential before expiry.' }
  } finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($ptr); $secret.Dispose() }
} else {
  $config=Get-Content -Raw -LiteralPath $configPath | ConvertFrom-Json
  if ([Uri]$config.controller -ne $uri) { throw 'Existing node belongs to another controller.' }
}
$action=New-ScheduledTaskAction -Execute $nodePath -Argument ('run --config "{0}"' -f $configPath) -WorkingDirectory $root
$trigger=New-ScheduledTaskTrigger -AtLogOn -User $sid.Value
$principal=New-ScheduledTaskPrincipal -UserId $sid.Value -LogonType Interactive -RunLevel Limited
$settings=New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Description $marker -Force | Out-Null
Start-ScheduledTask -TaskName $taskName
Write-Output "Installed the native node user service. It starts at sign-in. Private status and qualification: $root\private."
