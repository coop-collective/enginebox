[CmdletBinding()]
param(
  [Parameter(Mandatory=$true)][string]$PackageDirectory,
  [Parameter(Mandatory=$true)][string]$OutputDirectory,
  [Parameter(Mandatory=$true)][ValidatePattern('^[a-z0-9][a-z0-9.-]{1,80}$')][string]$Tag
)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
$package=[IO.Path]::GetFullPath($PackageDirectory)
$output=[IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Force -Path $output | Out-Null
$archive=Join-Path $output 'enginebox-node-windows-x64.zip'
if (Test-Path -LiteralPath $archive) { throw 'Output already exists; use a fresh packaging directory.' }
$files=@('enginebox-node.exe','install-native-node.ps1','README.md','LICENSE','NOTICE','SHA256SUMS') | ForEach-Object {
  $file=Join-Path $package $_
  if (-not (Test-Path -LiteralPath $file -PathType Leaf)) { throw "Missing package file: $_" }
  $file
}
Compress-Archive -LiteralPath $files -DestinationPath $archive
$archiveHash=(Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
$binaryHash=(Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $package 'enginebox-node.exe')).Hash.ToLowerInvariant()
$base="https://github.com/coop-collective/enginebox/releases/download/$Tag"
$template=Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'install-native-node-online.ps1.in')
$installer=$template.Replace('@@ARCHIVE_URL@@',"$base/enginebox-node-windows-x64.zip").Replace('@@ARCHIVE_SHA256@@',$archiveHash).Replace('@@BINARY_SHA256@@',$binaryHash)
$installerPath=Join-Path $output 'install.ps1'
[IO.File]::WriteAllText($installerPath,$installer,[Text.UTF8Encoding]::new($false))
$installerHash=(Get-FileHash -Algorithm SHA256 -LiteralPath $installerPath).Hash.ToLowerInvariant()
$command=@'
& { $p=Join-Path $env:TEMP ('enginebox-'+[guid]::NewGuid()+'.ps1'); try { Invoke-WebRequest -UseBasicParsing '@@URL@@' -OutFile $p; if ((Get-FileHash -Algorithm SHA256 $p).Hash -ne '@@HASH@@') { throw 'Installer checksum mismatch' }; & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $p } finally { Remove-Item -LiteralPath $p -Force -ErrorAction SilentlyContinue } }
'@
[IO.File]::WriteAllText((Join-Path $output 'install-command.txt'),$command.Replace('@@URL@@',"$base/install.ps1").Replace('@@HASH@@',$installerHash),[Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText((Join-Path $output 'SHA256SUMS'),"$archiveHash  enginebox-node-windows-x64.zip`n$installerHash  install.ps1`n",[Text.UTF8Encoding]::new($false))
Write-Output "Packaged verified bootstrap for $Tag"
