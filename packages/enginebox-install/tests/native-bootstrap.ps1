# Real Windows PowerShell 5.1 regression: downloaded unsigned scripts, bounded
# archive extraction, checksum rejection and preservation of machine/user policy.
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
$root=Join-Path ([IO.Path]::GetTempPath()) ('enginebox-bootstrap-test-'+[guid]::NewGuid())
$assets=Join-Path $PSScriptRoot '..\assets'
New-Item -ItemType Directory -Path $root | Out-Null
try {
  $package=Join-Path $root 'package'
  New-Item -ItemType Directory -Path $package | Out-Null
  foreach ($name in @('enginebox-node.exe','README.md','LICENSE','NOTICE','SHA256SUMS')) {
    [IO.File]::WriteAllText((Join-Path $package $name),'owned test fixture')
  }
  # A fixture installer intentionally records nothing and accepts the real API.
  [IO.File]::WriteAllText((Join-Path $package 'install-native-node.ps1'),'param($Mode,$ExpectedBinarySha256,$FfmpegPath,$Controller,$WorkerConfig); exit 0')
  $output=Join-Path $root 'output'
  & (Join-Path $assets 'package-native-node.ps1') -PackageDirectory $package -OutputDirectory $output -Tag 'native-node-test'
  $bootstrap=Join-Path $output 'install.ps1'
  $before=@{machine=Get-ExecutionPolicy -Scope LocalMachine;user=Get-ExecutionPolicy -Scope CurrentUser}
  # Mark the downloaded script as Internet-zone, reproducing the reported error.
  Set-Content -LiteralPath $bootstrap -Stream Zone.Identifier -Value "[ZoneTransfer]`nZoneId=3"
  $blocked=Join-Path $root 'blocked.txt'
  $oldPreference=$ErrorActionPreference; $ErrorActionPreference='Continue'
  & powershell.exe -NoProfile -ExecutionPolicy RemoteSigned -File $bootstrap *> $blocked
  $blockedExit=$LASTEXITCODE; $ErrorActionPreference=$oldPreference
  if ($blockedExit -eq 0 -or (Get-Content -Raw $blocked) -notmatch 'digitally signed') { throw 'Unsigned Internet-zone regression was not reproduced.' }
  # Dot-source the helper in this process, then override only the test downloader.
  . $bootstrap
  $script:fixtureArchive=Join-Path $output 'enginebox-node-windows-x64.zip'
  function Invoke-WebRequest { param($Uri,$OutFile,[switch]$UseBasicParsing); Copy-Item -LiteralPath $script:fixtureArchive -Destination $OutFile }
  $destination=Join-Path $root 'extracted'
  New-Item -ItemType Directory -Path $destination | Out-Null
  $hash=(Get-FileHash $script:fixtureArchive -Algorithm SHA256).Hash
  Get-VerifiedNodePackage -Uri 'https://fixture.example/package.zip' -Sha256 $hash -Destination $destination
  if (-not (Test-Path (Join-Path $destination 'enginebox-node.exe'))) { throw 'Verified download was not extracted.' }
  $bad=Join-Path $root 'bad'; New-Item -ItemType Directory -Path $bad | Out-Null
  $rejected=$false
  try { Get-VerifiedNodePackage -Uri 'https://fixture.example/package.zip' -Sha256 ('0'*64) -Destination $bad } catch { $rejected=$_.Exception.Message -match 'checksum mismatch' }
  if (-not $rejected -or (Test-Path (Join-Path $bad 'install-native-node.ps1'))) { throw 'Corrupt package was not rejected before extraction.' }
  # Run the exact generated one-paste command with its download redirected to our
  # owned fixture. The real child is Windows PowerShell 5.1 with a process policy.
  $script:fixtureBootstrap=Join-Path $root 'fixture.ps1'
  [IO.File]::WriteAllText($script:fixtureBootstrap,'param(); Write-Output "process-policy-proof"; exit 0')
  Set-Content -LiteralPath $script:fixtureBootstrap -Stream Zone.Identifier -Value "[ZoneTransfer]`nZoneId=3"
  function Invoke-WebRequest { param($Uri,$OutFile,[switch]$UseBasicParsing); Copy-Item -LiteralPath $script:fixtureBootstrap -Destination $OutFile; Set-Content -LiteralPath $OutFile -Stream Zone.Identifier -Value "[ZoneTransfer]`nZoneId=3" }
  $command=Get-Content -Raw (Join-Path $output 'install-command.txt')
  $command=$command.Replace((Get-FileHash -Algorithm SHA256 $bootstrap).Hash.ToLowerInvariant(),(Get-FileHash -Algorithm SHA256 $script:fixtureBootstrap).Hash.ToLowerInvariant())
  $result=& ([ScriptBlock]::Create($command))
  if ($LASTEXITCODE -ne 0 -or $result -notcontains 'process-policy-proof') { throw 'Process-scoped launcher failed.' }
  if ((Get-ExecutionPolicy -Scope LocalMachine) -ne $before.machine -or (Get-ExecutionPolicy -Scope CurrentUser) -ne $before.user) { throw 'Persistent execution policy changed.' }
  Write-Output 'PASS: unsigned-download reproduction, verified extraction, tamper rejection, process-scoped launch, unchanged persistent policy.'
} finally { Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue }
