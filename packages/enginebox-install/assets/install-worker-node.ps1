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
  [switch]$AutoLogon,
  [string]$InstallDirectory=(Join-Path $env:LOCALAPPDATA 'Enginebox\worker-node')
)
$ErrorActionPreference='Stop'; Set-StrictMode -Version Latest
$root=[IO.Path]::GetFullPath($InstallDirectory)
$binary=Join-Path $root 'enginebox-node.exe'
$config=Join-Path $root 'private\worker.json'
$receiptPath=Join-Path $root 'installation.json'
$taskName='Enginebox CI and Agents Node'
$marker='Enginebox contributor schema 1'
$lockTaskName='Enginebox Lock After Sign-in'
$winlogon='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
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
function Lsa-Native {
  if ('EngineboxLsa' -as [type]) { return }
  Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices;
public static class EngineboxLsa {
  [StructLayout(LayoutKind.Sequential)] struct UNICODE_STRING { public ushort Length; public ushort MaximumLength; public IntPtr Buffer; }
  [StructLayout(LayoutKind.Sequential)] struct ATTRIBUTES { public int Length; public IntPtr RootDirectory; public IntPtr ObjectName; public int Attributes; public IntPtr SecurityDescriptor; public IntPtr SecurityQualityOfService; }
  [DllImport("advapi32.dll")] static extern uint LsaOpenPolicy(IntPtr system, ref ATTRIBUTES attributes, int access, out IntPtr handle);
  [DllImport("advapi32.dll")] static extern uint LsaStorePrivateData(IntPtr handle, ref UNICODE_STRING key, IntPtr data);
  [DllImport("advapi32.dll")] static extern uint LsaClose(IntPtr handle);
  [DllImport("advapi32.dll")] static extern int LsaNtStatusToWinError(uint status);
  [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)] static extern bool LogonUser(string user, string domain, string password, int type, int provider, out IntPtr token);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
  static UNICODE_STRING Make(string value) { var u = new UNICODE_STRING(); u.Buffer = Marshal.StringToHGlobalUni(value); u.Length = (ushort)(value.Length * 2); u.MaximumLength = (ushort)(u.Length + 2); return u; }
  // LOGON32_LOGON_INTERACTIVE: the same check Winlogon applies, so a wrong password is refused before it is stored.
  public static bool Verify(string user, string domain, string password) { IntPtr token; if (!LogonUser(user, domain, password, 2, 0, out token)) return false; CloseHandle(token); return true; }
  // Stores (or with null deletes) the LSA secret Winlogon reads for automatic sign-in.
  public static void Store(string name, string value) {
    var attributes = new ATTRIBUTES(); IntPtr policy;
    uint status = LsaOpenPolicy(IntPtr.Zero, ref attributes, 0x20 /* POLICY_CREATE_SECRET */, out policy);
    if (status != 0) throw new System.ComponentModel.Win32Exception(LsaNtStatusToWinError(status));
    var key = Make(name); IntPtr data = IntPtr.Zero; UNICODE_STRING secret = new UNICODE_STRING();
    try {
      if (value != null) { secret = Make(value); data = Marshal.AllocHGlobal(Marshal.SizeOf(secret)); Marshal.StructureToPtr(secret, data, false); }
      status = LsaStorePrivateData(policy, ref key, data);
      if (status != 0 && !(value == null && status == 0xC0000034)) throw new System.ComponentModel.Win32Exception(LsaNtStatusToWinError(status));
    } finally {
      LsaClose(policy); Marshal.FreeHGlobal(key.Buffer);
      if (secret.Buffer != IntPtr.Zero) { Marshal.Copy(new byte[secret.Length], 0, secret.Buffer, secret.Length); Marshal.FreeHGlobal(secret.Buffer); }
      if (data != IntPtr.Zero) Marshal.FreeHGlobal(data);
    }
  }
}
'@
}
function Assert-Elevated([string]$Purpose) {
  $principal=[Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
  if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw "$Purpose requires an elevated PowerShell (Run as administrator)." }
}
function Owned-LockTask {
  $lock=Get-ScheduledTask -TaskName $lockTaskName -ErrorAction SilentlyContinue
  if ($lock -and $lock.Description -ne $marker) { throw 'An unrelated scheduled task owns the lock task name.' }
  return $lock
}
function Disable-AutoLogon {
  Assert-Elevated 'Removing automatic sign-in'
  Lsa-Native; [EngineboxLsa]::Store('DefaultPassword',$null)
  Set-ItemProperty -LiteralPath $winlogon -Name AutoAdminLogon -Value '0'
  Remove-ItemProperty -LiteralPath $winlogon -Name DefaultPassword -ErrorAction SilentlyContinue
  if (Owned-LockTask) { Unregister-ScheduledTask -TaskName $lockTaskName -Confirm:$false }
}
function Enable-AutoLogon {
  Assert-Elevated 'Automatic sign-in'
  $computer=Get-CimInstance Win32_ComputerSystem
  if ($computer.PartOfDomain) { throw 'Automatic sign-in is supported for local accounts only; this machine is domain-joined.' }
  $user=$env:USERNAME
  if (-not (Get-LocalUser -Name $user -ErrorAction SilentlyContinue)) { throw 'Automatic sign-in is supported for local accounts only.' }
  # Read the password from piped input or a hidden prompt, never from arguments.
  if ([Console]::IsInputRedirected) { $password=[Console]::In.ReadLine() } else {
    $secret=Read-Host "Windows password for $user (stored as an LSA secret for automatic sign-in)" -AsSecureString
    $ptr=[Runtime.InteropServices.Marshal]::SecureStringToBSTR($secret)
    try { $password=[Runtime.InteropServices.Marshal]::PtrToStringBSTR($ptr) } finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($ptr); $secret.Dispose() }
  }
  if ($null -eq $password) { throw 'No password was provided.' }
  Lsa-Native
  if (-not [EngineboxLsa]::Verify($user,$env:COMPUTERNAME,$password)) { throw "Windows rejected the password for $user; automatic sign-in was not changed." }
  [EngineboxLsa]::Store('DefaultPassword',$password); $password=$null
  Remove-ItemProperty -LiteralPath $winlogon -Name DefaultPassword -ErrorAction SilentlyContinue
  Set-ItemProperty -LiteralPath $winlogon -Name DefaultUserName -Value $user
  Set-ItemProperty -LiteralPath $winlogon -Name DefaultDomainName -Value $env:COMPUTERNAME
  Remove-ItemProperty -LiteralPath $winlogon -Name AutoLogonCount -ErrorAction SilentlyContinue
  Set-ItemProperty -LiteralPath $winlogon -Name AutoAdminLogon -Value '1'
  # Windows Hello "passwordless" mode ignores password sign-in, including Winlogon's automatic one.
  $passwordless='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\PasswordLess\Device'
  if (Test-Path -LiteralPath $passwordless) { Set-ItemProperty -LiteralPath $passwordless -Name DevicePasswordLessBuildVersion -Value 0 -Type DWord }
  # An unattended engine host must not sleep; the display may still turn off.
  & powercfg.exe /change standby-timeout-ac 0; & powercfg.exe /change hibernate-timeout-ac 0
  # Lock the console right after the automatic sign-in. Engines keep rendering in the locked session.
  Owned-LockTask | Out-Null
  $sid=[Security.Principal.WindowsIdentity]::GetCurrent().User
  $lockAction=New-ScheduledTaskAction -Execute (Join-Path $env:SystemRoot 'System32\rundll32.exe') -Argument 'user32.dll,LockWorkStation'
  $lockTrigger=New-ScheduledTaskTrigger -AtLogOn -User $sid.Value
  $lockPrincipal=New-ScheduledTaskPrincipal -UserId $sid.Value -LogonType Interactive -RunLevel Limited
  Register-ScheduledTask -TaskName $lockTaskName -Action $lockAction -Trigger $lockTrigger -Principal $lockPrincipal -Settings (New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries) -Description $marker -Force | Out-Null
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
  if ((Get-Content -Raw -LiteralPath $receiptPath | ConvertFrom-Json).PSObject.Properties['autoLogon']) { Disable-AutoLogon; Write-Output 'Automatic sign-in removed.' }
  Remove-Item -LiteralPath $binary -Force -ErrorAction SilentlyContinue
  Write-Output 'Worker task and binary removed. Identity and work files retained. Revoke this machine in the dashboard.'
  exit 0
}
if ($AutoLogon) { Assert-Elevated 'Automatic sign-in' }
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
$previousAutoLogon=(Test-Path -LiteralPath $receiptPath) -and [bool](Get-Content -Raw -LiteralPath $receiptPath | ConvertFrom-Json).PSObject.Properties['autoLogon']
$receiptData=@{marker=$marker;root=$root;binarySha256=$ExpectedBinarySha256}
if ($AutoLogon -or $previousAutoLogon) { $receiptData.autoLogon=$true }
$receiptData | ConvertTo-Json | Set-Content -LiteralPath $receiptPath -Encoding UTF8
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
if ($AutoLogon) { Enable-AutoLogon; Write-Output "Automatic sign-in enabled for $env:USERNAME; the console locks right after it. The node starts at every boot without anyone signing in." }
$identity=Get-Content -Raw -LiteralPath $config | ConvertFrom-Json
if (-not $SkipDependencies) {
  $required=@()
  if ($identity.workloads.ci) { $required+=@(@{name='git';id='Git.Git'},@{name='pwsh';id='Microsoft.PowerShell'}) }
  foreach ($tool in $required) {
    if (Find-Tool $tool.name) { continue }
    $winget=Get-Command winget.exe -ErrorAction SilentlyContinue
    if (-not $winget) { Write-Warning "Registered. Install $($tool.id) to enable this workload; WinGet is unavailable."; continue }
    & $winget.Source install --id $tool.id --exact --source winget --accept-source-agreements --accept-package-agreements
    if ($LASTEXITCODE -ne 0) { Write-Warning "Registered, but $($tool.id) installation failed. The dashboard will report the missing tool." }
  }
}
if ($identity.workloads.agents) { Write-Warning 'Host agent execution is disabled. Use the Enginebox WSL2 installer for template workspaces.' }
Write-Output "Registered this machine for its selected CI/agent workloads. It starts at sign-in. Status: $root\private\status.json"
Write-Output "Shared limits: $Cpu CPUs, $MemoryGiB GiB RAM; workspace disk watchdog: $DiskGiB GiB. Check readiness in the dashboard."
