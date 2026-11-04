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
  [switch]$EngineAccount,
  # With -EngineAccount: agent slot accounts eb-agent-01..NN (0: none) and whether they may reach the internet (never the LAN).
  [ValidateRange(0,8)][int]$AgentSlots=3,
  [switch]$AgentInternet,
  [string]$InstallDirectory=(Join-Path $env:LOCALAPPDATA 'Enginebox\worker-node')
)
$ErrorActionPreference='Stop'; Set-StrictMode -Version Latest
# -EngineAccount runs the node as a dedicated, automatically signed-in local user so engine sessions never run in the
# owner's own account or see the owner's files.
if ($EngineAccount) {
  $AutoLogon=$true
  if (-not $PSBoundParameters.ContainsKey('InstallDirectory')) { $InstallDirectory=Join-Path $env:ProgramData 'Enginebox\engine-node' }
}
$root=[IO.Path]::GetFullPath($InstallDirectory)
$binary=Join-Path $root 'enginebox-node.exe'
$config=Join-Path $root 'private\worker.json'
$receiptPath=Join-Path $root 'installation.json'
$taskName=if ($EngineAccount) { 'Enginebox Engine Node' } else { 'Enginebox CI and Agents Node' }
$engineUser='enginebox'
$engineMarker='Enginebox engine sessions account'
# The Unity Editor's low-privilege account and the agent slot accounts (docs/AGENT-FORKS.md). Only the engine account
# reads their passwords: the installer hands them over in private\accounts.pending.json and the node seals them with
# user-scope DPAPI into private\accounts.dpapi at its next start.
$editorUser='enginebox-editor'
$editorMarker='Enginebox editor account'
$slotMarker='Enginebox agent slot account'
$firewallGroup='Enginebox agent slots'
$userList='HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList'
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
  [DllImport("advapi32.dll")] static extern uint LsaAddAccountRights(IntPtr handle, byte[] sid, UNICODE_STRING[] rights, uint count);
  [DllImport("advapi32.dll")] static extern uint LsaRemoveAccountRights(IntPtr handle, byte[] sid, [MarshalAs(UnmanagedType.U1)] bool all, UNICODE_STRING[] rights, uint count);
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
  // Adds (or removes) one user right such as SeDenyNetworkLogonRight for a SID. Removing a right it does not hold is fine.
  public static void Right(byte[] sid, string right, bool add) {
    var attributes = new ATTRIBUTES(); IntPtr policy;
    uint status = LsaOpenPolicy(IntPtr.Zero, ref attributes, 0x810 /* POLICY_LOOKUP_NAMES|POLICY_CREATE_ACCOUNT */, out policy);
    if (status != 0) throw new System.ComponentModel.Win32Exception(LsaNtStatusToWinError(status));
    var rights = new UNICODE_STRING[] { Make(right) };
    try {
      status = add ? LsaAddAccountRights(policy, sid, rights, 1) : LsaRemoveAccountRights(policy, sid, false, rights, 1);
      if (status != 0 && !(!add && status == 0xC0000034)) throw new System.ComponentModel.Win32Exception(LsaNtStatusToWinError(status));
    } finally { LsaClose(policy); Marshal.FreeHGlobal(rights[0].Buffer); }
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
function Enable-AutoLogon([string]$User=$env:USERNAME,[string]$Password) {
  Assert-Elevated 'Automatic sign-in'
  $computer=Get-CimInstance Win32_ComputerSystem
  if ($computer.PartOfDomain) { throw 'Automatic sign-in is supported for local accounts only; this machine is domain-joined.' }
  $user=$User
  $account=Get-LocalUser -Name $user -ErrorAction SilentlyContinue
  if (-not $account) { throw 'Automatic sign-in is supported for local accounts only.' }
  # Read the password from piped input or a hidden prompt, never from arguments.
  if ($PSBoundParameters.ContainsKey('Password')) { $password=$Password } elseif ([Console]::IsInputRedirected) { $password=[Console]::In.ReadLine() } else {
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
  $sid=$account.SID
  $lockAction=New-ScheduledTaskAction -Execute (Join-Path $env:SystemRoot 'System32\rundll32.exe') -Argument 'user32.dll,LockWorkStation'
  $lockTrigger=New-ScheduledTaskTrigger -AtLogOn -User $sid.Value
  $lockPrincipal=New-ScheduledTaskPrincipal -UserId $sid.Value -LogonType Interactive -RunLevel Limited
  Register-ScheduledTask -TaskName $lockTaskName -Action $lockAction -Trigger $lockTrigger -Principal $lockPrincipal -Settings (New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries) -Description $marker -Force | Out-Null
}
function New-EnginePassword {
  # 32 random characters plus one from each class, so any local password policy accepts it. Nobody is shown it.
  $chars='ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789'
  $bytes=New-Object byte[] 32; [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
  return (-join ($bytes | ForEach-Object { $chars[$_ % $chars.Length] }))+'Aa9-'
}
function Ensure-EngineAccount {
  Assert-Elevated 'The engine account'
  $existing=Get-LocalUser -Name $engineUser -ErrorAction SilentlyContinue
  if ($existing -and $existing.Description -ne $engineMarker) { throw "A local user named $engineUser exists and is not owned by Engine Box." }
  $password=New-EnginePassword
  $secure=ConvertTo-SecureString $password -AsPlainText -Force
  if ($existing) { Set-LocalUser -Name $engineUser -Password $secure -PasswordNeverExpires $true }
  else {
    New-LocalUser -Name $engineUser -Password $secure -PasswordNeverExpires -UserMayNotChangePassword -AccountNeverExpires -FullName 'Engine Box' -Description $engineMarker | Out-Null
    Add-LocalGroupMember -SID 'S-1-5-32-545' -Member $engineUser
  }
  return @{password=$password;sid=(Get-LocalUser -Name $engineUser).SID}
}
function Remove-EngineAccount {
  $existing=Get-LocalUser -Name $engineUser -ErrorAction SilentlyContinue
  if (-not $existing) { return }
  if ($existing.Description -ne $engineMarker) { throw "The local user $engineUser is not owned by Engine Box; it was left in place." }
  Get-CimInstance Win32_UserProfile | Where-Object { $_.SID -eq $existing.SID.Value } | Remove-CimInstance
  Remove-LocalUser -Name $engineUser
}
function Sid-Bytes($Sid) { $bytes=New-Object byte[] $Sid.BinaryLength; $Sid.GetBinaryForm($bytes,0); return ,$bytes }
function Account-Rights([string]$Name) {
  $rights=@('SeDenyNetworkLogonRight','SeDenyRemoteInteractiveLogonRight')
  if ($Name -ne $editorUser) { $rights+=@('SeDenyBatchLogonRight','SeDenyServiceLogonRight') }
  return $rights
}
function Ensure-SideAccount([string]$Name,[string]$Marker,[string]$FullName) {
  # The editor and slot accounts: standard Users, hidden from sign-in, no network/RDP logon; every install rotates the password.
  $existing=Get-LocalUser -Name $Name -ErrorAction SilentlyContinue
  if ($existing -and $existing.Description -ne $Marker) { throw "A local user named $Name exists and is not owned by Engine Box." }
  $password=New-EnginePassword
  $secure=ConvertTo-SecureString $password -AsPlainText -Force
  if ($existing) { Set-LocalUser -Name $Name -Password $secure -PasswordNeverExpires $true }
  else {
    New-LocalUser -Name $Name -Password $secure -PasswordNeverExpires -UserMayNotChangePassword -AccountNeverExpires -FullName $FullName -Description $Marker | Out-Null
    Add-LocalGroupMember -SID 'S-1-5-32-545' -Member $Name
  }
  $sid=(Get-LocalUser -Name $Name).SID
  if (-not (Test-Path -LiteralPath $userList)) { New-Item -Path $userList -Force | Out-Null }
  New-ItemProperty -LiteralPath $userList -Name $Name -Value 0 -PropertyType DWord -Force | Out-Null
  Lsa-Native; foreach ($right in (Account-Rights $Name)) { [EngineboxLsa]::Right((Sid-Bytes $sid),$right,$true) }
  return @{user=$Name;password=$password;sid=$sid}
}
function Remove-SideAccount([string]$Name,[string]$Marker) {
  $existing=Get-LocalUser -Name $Name -ErrorAction SilentlyContinue
  if (-not $existing) { return }
  if ($existing.Description -ne $Marker) { Write-Warning "The local user $Name is not owned by Engine Box; it was left in place."; return }
  # Rights first: deleting the account would leave them on an orphaned SID.
  Lsa-Native; foreach ($right in (Account-Rights $Name)) { [EngineboxLsa]::Right((Sid-Bytes $existing.SID),$right,$false) }
  Remove-ItemProperty -LiteralPath $userList -Name $Name -ErrorAction SilentlyContinue
  Get-NetFirewallRule -Group $firewallGroup -ErrorAction SilentlyContinue | Where-Object { $_.DisplayName -eq "Enginebox agent egress $Name" } | Remove-NetFirewallRule
  Get-CimInstance Win32_UserProfile | Where-Object { $_.SID -eq $existing.SID.Value } | Remove-CimInstance
  Remove-LocalUser -Name $Name
}
function Owned-Slots { return @(Get-LocalUser | Where-Object { $_.Name -match '^eb-agent-\d\d$' -and $_.Description -eq $slotMarker }) }
function Set-AgentFirewall($Slots) {
  # Block rules win over allow rules. Default: a slot reaches loopback only (the node's gateway relay); with
  # -AgentInternet it reaches the internet (package registries) but never the LAN. Per-user (LocalUser) outbound
  # filtering must be confirmed on each machine by qualify-editor-account.ps1 (slot section).
  Get-NetFirewallRule -Group $firewallGroup -ErrorAction SilentlyContinue | Remove-NetFirewallRule
  $blocked=if ($AgentInternet) { @('10.0.0.0/8','172.16.0.0/12','192.168.0.0/16','169.254.0.0/16','100.64.0.0/10','224.0.0.0/4','fc00::/7','fe80::/10','ff00::/8') }
    else { @('0.0.0.0-126.255.255.255','128.0.0.0-255.255.255.255','::2-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff') }
  foreach ($slot in $Slots) {
    New-NetFirewallRule -DisplayName "Enginebox agent egress $($slot.user)" -Name "Enginebox agent egress $($slot.user)" -Group $firewallGroup -Direction Outbound -Action Block -Profile Any -RemoteAddress $blocked -LocalUser "D:(A;;CC;;;$($slot.sid.Value))" | Out-Null
  }
}
function Profile-Path($Sid) { $p=Get-CimInstance Win32_UserProfile | Where-Object { $_.SID -eq $Sid.Value } | Select-Object -First 1; if ($p) { return $p.LocalPath }; return $null }
function Initialize-EditorProfile($Editor,$EngineSid) {
  # Unity runs as the editor account only once qualified; give it the engine account's Unity region pin and accepted terms.
  try {
    $credential=New-Object Management.Automation.PSCredential("$env:COMPUTERNAME\$($Editor.user)",(ConvertTo-SecureString $Editor.password -AsPlainText -Force))
    Start-Process -FilePath (Join-Path $env:SystemRoot 'System32\cmd.exe') -ArgumentList '/c exit 0' -Credential $credential -LoadUserProfile -WindowStyle Hidden -Wait
    $credential=$null
  } catch { Write-Warning "Could not create the $($Editor.user) profile ($($_.Exception.Message)); the node creates it at the editor's first start."; return }
  $editorHome=Profile-Path $Editor.sid; $engineHome=Profile-Path $EngineSid
  if (-not $editorHome) { Write-Warning "No profile for $($Editor.user); Unity settings were not copied."; return }
  $pin=if ($engineHome) { Join-Path $engineHome 'AppData\Local\Unity\config\production.json' } else { $null }
  if ($pin -and (Test-Path -LiteralPath $pin)) {
    $target=Join-Path $editorHome 'AppData\Local\Unity\config'
    New-Item -ItemType Directory -Force -Path $target | Out-Null
    $file=Join-Path $target 'production.json'
    if (Test-Path -LiteralPath $file) { Set-ItemProperty -LiteralPath $file -Name IsReadOnly -Value $false }
    Copy-Item -LiteralPath $pin -Destination $file -Force; Set-ItemProperty -LiteralPath $file -Name IsReadOnly -Value $true
  } else { Write-Warning 'The engine account has no pinned Unity production.json yet; the editor account was not pinned.' }
  $unityKey='Software\Unity Technologies\Unity Editor 5.x'
  $loaded=@()
  try {
    $hives=@{}
    foreach ($pair in @(@('EngineboxEngine',$EngineSid,$engineHome),@('EngineboxEditor',$Editor.sid,$editorHome))) {
      $live="Registry::HKEY_USERS\$($pair[1].Value)"
      if (Test-Path -LiteralPath $live) { $hives[$pair[0]]=$live; continue }
      if (-not $pair[2] -or -not (Test-Path -LiteralPath (Join-Path $pair[2] 'NTUSER.DAT'))) { continue }
      try { & reg.exe load "HKU\$($pair[0])" (Join-Path $pair[2] 'NTUSER.DAT') 2>$null | Out-Null } catch { continue }
      if ($LASTEXITCODE -eq 0) { $loaded+=$pair[0]; $hives[$pair[0]]="Registry::HKEY_USERS\$($pair[0])" }
    }
    $source=if ($hives['EngineboxEngine']) { Join-Path $hives['EngineboxEngine'] $unityKey } else { $null }
    $terms=if ($source -and (Test-Path -LiteralPath $source)) { @((Get-Item -LiteralPath $source).GetValueNames() | Where-Object { $_ -like 'Terms-*' }) } else { @() }
    if ($terms.Count -eq 0 -or -not $hives['EngineboxEditor']) { Write-Warning 'No accepted Unity terms to copy to the editor account yet.'; return }
    $target=Join-Path $hives['EngineboxEditor'] $unityKey
    if (-not (Test-Path -LiteralPath $target)) { New-Item -Path $target -Force | Out-Null }
    $key=Get-Item -LiteralPath $source
    foreach ($name in $terms) { New-ItemProperty -LiteralPath $target -Name $name -Value $key.GetValue($name) -PropertyType $key.GetValueKind($name) -Force | Out-Null }
    $key.Close(); $key=$null
  } finally {
    [gc]::Collect(); [gc]::WaitForPendingFinalizers()
    foreach ($hive in $loaded) { try { & reg.exe unload "HKU\$hive" 2>$null | Out-Null } catch { Write-Warning "Could not unload HKU\$hive." } }
  }
}
function Restrict-ToEngine([string]$Path,$Sid) {
  # One protected full-control entry for the engine account, owned by it: the shape the node enforces for private\.
  $engineSid="*$($Sid.Value)"
  & icacls.exe $Path /setowner $engineSid /C /Q 2>$null | Out-Null
  & icacls.exe $Path /inheritance:r /grant:r "$($engineSid):F" /Q 2>$null | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "Could not restrict $Path to the engine account." }
  foreach ($ace in @((Get-Acl -LiteralPath $Path).Access)) {
    $aceSid=$ace.IdentityReference.Translate([Security.Principal.SecurityIdentifier])
    if ($aceSid -ne $Sid) { & icacls.exe $Path /remove:g "*$($aceSid.Value)" /Q 2>$null | Out-Null }
  }
}
function Reclaim-PrivateDirectory {
  # A repeated engine install finds private\ handed to the engine account; give Administrators access back first.
  $private=Split-Path -Parent $config
  if (-not (Test-Path -LiteralPath $private)) { return }
  & takeown.exe /F $private /R /A /D Y 2>$null | Out-Null
  & icacls.exe $private /grant "*S-1-5-32-544:(OI)(CI)F" /T /C /Q 2>$null | Out-Null
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
  $installed=Get-Content -Raw -LiteralPath $receiptPath | ConvertFrom-Json
  if ($installed.PSObject.Properties['autoLogon']) { Disable-AutoLogon; Write-Output 'Automatic sign-in removed.' }
  if ($installed.PSObject.Properties['editorAccount'] -or $installed.PSObject.Properties['agentSlots']) {
    Assert-Elevated 'Removing the editor and agent slot accounts'
    Get-NetFirewallRule -Group $firewallGroup -ErrorAction SilentlyContinue | Remove-NetFirewallRule
    foreach ($slot in (Owned-Slots)) { Remove-SideAccount $slot.Name $slotMarker }
    Remove-SideAccount $editorUser $editorMarker
    Reclaim-PrivateDirectory
    foreach ($name in @('accounts.dpapi','accounts.pending.json')) { Remove-Item -LiteralPath (Join-Path (Split-Path -Parent $config) $name) -Force -ErrorAction SilentlyContinue }
    Write-Output 'Editor and agent slot accounts, their profiles, rights and firewall rules removed.'
  }
  if ($installed.PSObject.Properties['engineAccount']) { Remove-EngineAccount; Write-Output "Engine account $engineUser and its profile removed." }
  Remove-Item -LiteralPath $binary -Force -ErrorAction SilentlyContinue
  Write-Output 'Worker task and binary removed. Identity and work files retained. Revoke this machine in the dashboard.'
  exit 0
}
if ($AutoLogon) { Assert-Elevated 'Automatic sign-in' }
$engine=$null
$editor=$null; $slots=@()
if ($EngineAccount) {
  $engine=Ensure-EngineAccount; Reclaim-PrivateDirectory
  $editor=Ensure-SideAccount $editorUser $editorMarker 'Engine Box Editor'
  $slots=@(1..8 | Where-Object { $_ -le $AgentSlots } | ForEach-Object { Ensure-SideAccount ('eb-agent-{0:D2}' -f $_) $slotMarker 'Engine Box agent slot' })
  foreach ($stale in (Owned-Slots)) { if ([int]$stale.Name.Substring(9) -gt $AgentSlots) { Remove-SideAccount $stale.Name $slotMarker } }
  Set-AgentFirewall $slots
  $agentTools='C:\Enginebox\engine-tools\agents'
  if (Test-Path -LiteralPath $agentTools) { foreach ($slot in $slots) { & icacls.exe $agentTools /grant "*$($slot.sid.Value):(OI)(CI)RX" /Q 2>$null | Out-Null } }
  Initialize-EditorProfile $editor $engine.sid
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
$sid=if ($engine) { $engine.sid } else { [Security.Principal.WindowsIdentity]::GetCurrent().User }
$acl=New-Object Security.AccessControl.DirectorySecurity
$acl.SetAccessRuleProtection($true,$false)
$acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($sid,'FullControl','ContainerInherit,ObjectInherit','None','Allow')))
if ($engine) {
  # The installing administrator keeps control; the engine account owns nothing else on the machine.
  $admins=New-Object Security.Principal.SecurityIdentifier('S-1-5-32-544')
  $acl.SetOwner($admins)
  $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($admins,'FullControl','ContainerInherit,ObjectInherit','None','Allow')))
  $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule((New-Object Security.Principal.SecurityIdentifier('S-1-5-18')),'FullControl','ContainerInherit,ObjectInherit','None','Allow')))
} else { $acl.SetOwner($sid) }
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
if ($engine) { $receiptData.engineAccount=$engineUser; $receiptData.editorAccount=$editorUser; $receiptData.agentSlots=$slots.Count; $receiptData.agentInternet=[bool]$AgentInternet }
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
$identity=Get-Content -Raw -LiteralPath $config | ConvertFrom-Json
if ($engine) {
  # Registration ran as the installing administrator, which made private\ its own. Hand it to the engine account in
  # the shape the node itself enforces at start: owned by that account, one protected full-control entry.
  $private=Split-Path -Parent $config
  $engineSid="*$($engine.sid.Value)"
  # Children first, while the administrator can still reach them: owned by the engine account and inheriting only.
  & icacls.exe $private /setowner $engineSid /T /C /Q 2>$null | Out-Null
  if ($LASTEXITCODE -ne 0) { throw 'Could not give the engine account ownership of its private files.' }
  & icacls.exe "$private\*" /reset /T /C /Q 2>$null | Out-Null
  if ($LASTEXITCODE -ne 0) { throw 'Could not reset private file permissions to inherit.' }
  # Then the directory itself: one protected full-control entry for the engine account, nothing else.
  & icacls.exe $private /inheritance:r /grant:r "$($engineSid):(OI)(CI)F" /Q 2>$null | Out-Null
  if ($LASTEXITCODE -ne 0) { throw 'Could not restrict the private directory to the engine account.' }
  foreach ($ace in @((Get-Acl -LiteralPath $private).Access)) {
    $aceSid=$ace.IdentityReference.Translate([Security.Principal.SecurityIdentifier])
    if ($aceSid -ne $engine.sid) { & icacls.exe $private /remove:g "*$($aceSid.Value)" /Q 2>$null | Out-Null }
  }
  if (@((Get-Acl -LiteralPath $private).Access).Count -ne 1) { throw 'The private directory kept access rules other than the engine account.' }
  # The editor and slot passwords, for the node to seal with DPAPI at its next start (then it deletes this file).
  $pending=Join-Path $private 'accounts.pending.json'
  $accounts=[ordered]@{schema=1;editor=[ordered]@{user=$editor.user;password=$editor.password};slots=[object[]]@($slots | ForEach-Object { [ordered]@{user=$_.user;password=$_.password} })}
  [IO.File]::WriteAllText($pending,($accounts | ConvertTo-Json -Depth 4 -Compress),(New-Object Text.UTF8Encoding($false)))
  $accounts=$null; $editor.password=$null; foreach ($slot in $slots) { $slot.password=$null }
  Restrict-ToEngine $pending $engine.sid
  if (@((Get-Acl -LiteralPath $pending).Access).Count -ne 1) { throw 'The account password file kept access rules other than the engine account.' }
}
$action=New-ScheduledTaskAction -Execute $binary -Argument ('run-worker --config "{0}"' -f $config) -WorkingDirectory $root
$trigger=New-ScheduledTaskTrigger -AtLogOn -User $sid.Value
# Task Scheduler restarts a task only when it fails to launch, not when the node exits after losing its controller.
# A repeating trigger relaunches it within five minutes; IgnoreNew leaves a running node alone.
$watchdog=New-ScheduledTaskTrigger -Once -At (Get-Date).Date -RepetitionInterval (New-TimeSpan -Minutes 5)
$trigger=@($trigger,$watchdog)
$principal=New-ScheduledTaskPrincipal -UserId $sid.Value -LogonType Interactive -RunLevel Limited
$settings=New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Description $marker -Force | Out-Null
if ($engine) {
  Enable-AutoLogon -User $engineUser -Password $engine.password; $engine.password=$null
  Write-Output "Engine account $engineUser created and set to sign in automatically, then lock. The node starts at its next sign-in: restart Windows (or sign out) to start it."
  $egress=if ($AgentInternet) { 'internet except the LAN' } else { 'loopback only' }
  Write-Output "Editor account $editorUser and $($slots.Count) agent slot account(s) created (hidden, no network or remote sign-in; slot egress: $egress). Their passwords go to the node only. Run qualify-editor-account.ps1 before setting engine.json editor_account."
} else {
  Start-ScheduledTask -TaskName $taskName
  if ($AutoLogon) { Enable-AutoLogon; Write-Output "Automatic sign-in enabled for $env:USERNAME; the console locks right after it. The node starts at every boot without anyone signing in." }
}
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
