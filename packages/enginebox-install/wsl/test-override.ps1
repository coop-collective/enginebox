param([Parameter(Mandatory=$true)][string]$Image)
$ErrorActionPreference='Stop'
# The workflow creates only this disposable, ownership-marked distribution.
$name='EngineboxCI'
$restored='EngineboxCIBackup'
$root=Join-Path $env:TEMP ('enginebox-override-proof-'+[guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
$helpers=Join-Path $root 'installer.ps1'
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'install.ps1.in') -Destination $helpers
. $helpers
$before=@((& wsl.exe --list --quiet) | ForEach-Object { ($_ -replace "`0",'').Trim() })
if ($before -notcontains $name -or $before -contains $restored) { throw 'Unexpected disposable WSL fixture state' }
$seed=@'
import pathlib
root=pathlib.Path('/var/lib/enginebox')
p=root/'private/worker.json'
assert not p.exists()
p.parent.mkdir(mode=0o700,exist_ok=True)
p.write_text('{"fixture":"preserve pending identity"}')
(root/'override-workspace-proof.txt').write_text('preserve workspace contents')
'@
$seed | & wsl.exe -d $name -u root --exec python3 -
if ($LASTEXITCODE) { throw 'Could not seed disposable identity/workspace state' }
$backup=Backup-AndRemoveRuntime $name $root
$disk=Join-Path $backup 'enginebox.tar'
$receipt=Get-Content -Raw -LiteralPath (Join-Path $backup 'backup.json') | ConvertFrom-Json
try {
  Invoke-Wsl -Arguments @('--install','--from-file',([IO.Path]::GetFullPath($Image)),'--name',$name,'--location',(Join-Path $root 'fresh'),'--version','2','--vhd-size','20GB','--no-launch')
  Initialize-RuntimeOwnership $name (Join-Path $root 'fresh')
  & (Join-Path $PSScriptRoot 'test-systemd.ps1') -Name $name
  $clean=@'
import pathlib
root=pathlib.Path('/var/lib/enginebox')
assert not (root/'private/worker.json').exists()
assert not (root/'override-workspace-proof.txt').exists()
'@
  $clean | & wsl.exe -d $name -u root --exec python3 -
  if ($LASTEXITCODE) { throw 'Replacement did not start with fresh state' }
  Invoke-Wsl -Arguments @('--unregister',$name)
  # The runner needs space for the exported disk and its independent restore;
  # the clean replacement has already been checked and is no longer needed.
  # Import restores the filesystem into a separate WSL2 virtual disk.
  Invoke-Wsl -Arguments @('--import',$restored,(Join-Path $root 'restored'),$disk,'--version','2')
  $verify=@'
import pathlib
root=pathlib.Path('/var/lib/enginebox')
assert (root/'private/worker.json').read_text()=='{"fixture":"preserve pending identity"}'
assert (root/'override-workspace-proof.txt').read_text()=='preserve workspace contents'
assert (pathlib.Path('/etc/enginebox/settings.json')).exists()
assert (root/'layerfs').is_dir()
'@
  $verify | & wsl.exe -d $restored -u root --exec python3 -
  if ($LASTEXITCODE) { throw 'Backup did not preserve identity/settings/workspace state' }
  if ((Get-FileHash -Algorithm SHA256 -LiteralPath $disk).Hash -ne $receipt.sha256) { throw 'Restoration changed the original backup' }
  $after=@((& wsl.exe --list --quiet) | ForEach-Object { ($_ -replace "`0",'').Trim() })
  foreach ($other in $before) { if ($other -and $other -ne $name -and $after -notcontains $other) { throw 'An unrelated distribution was changed' } }
  Write-Output 'Real WSL override: verified backup, fresh replacement and independent restore of pending identity/settings/workspace state passed.'
} finally {
  $names=@((& wsl.exe --list --quiet) | ForEach-Object { ($_ -replace "`0",'').Trim() })
  if ($names -contains $restored) { Invoke-Wsl -Arguments @('--unregister',$restored) }
  # The workflow removes EngineboxCI separately, including on failure. The
  # fixture backup remains available for diagnostics until the runner is gone.
}
