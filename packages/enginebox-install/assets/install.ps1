# Download and inspect before executing. No elevation is requested implicitly.
[CmdletBinding()]
param(
  [Parameter(Mandatory=$true)][ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$')][string]$Version,
  [ValidatePattern('^[0-9a-f]{64}$')][string]$ArchiveSha256,
  [ValidatePattern('^[0-9a-f]{64}$')][string]$ManifestSha256,
  [string]$Prefix = (Join-Path $env:LOCALAPPDATA 'EngineBox'),
  [ValidateSet('operator','controller')][string]$Role = 'controller',
  [string]$BaseUrl = 'https://github.com/coop-collective/enginebox/releases/download',
  [switch]$Apply,
  [switch]$InstallDependencies
)
$ErrorActionPreference = 'Stop'
if (-not $BaseUrl.StartsWith('https://')) { throw 'HTTPS release source required.' }
$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
$target = switch ($architecture) {
  'X64' { 'x86_64-pc-windows-msvc' }
  'Arm64' { throw 'Windows ARM64 release archives are not yet shipped. This bootstrap currently supports Windows x64 only.' }
  default { throw 'Unsupported Windows architecture.' }
}
Write-Output "Engine Box $Version; target $target; prefix $Prefix; role $Role"
Write-Output 'Windows installs the controller/operator. Linux Incus compute requires a separate Linux host or explicitly provisioned Linux VM.'
if (-not $Apply) { Write-Output 'Plan only. Add -Apply after reviewing the pinned release and trusted hashes.'; return }
if (-not (Get-Command tar.exe -ErrorAction SilentlyContinue)) { throw 'The Windows tar.exe utility is required to extract the verified bootstrap binary.' }
$temp = Join-Path ([System.IO.Path]::GetTempPath()) ('enginebox-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temp | Out-Null
Add-Type -AssemblyName System.Net.Http
function Get-HttpsFile([string]$Url, [string]$Destination, [long]$Limit) {
  $handler = [System.Net.Http.HttpClientHandler]::new()
  $handler.AllowAutoRedirect = $false
  $client = [System.Net.Http.HttpClient]::new($handler)
  $client.Timeout = [TimeSpan]::FromMinutes(15)
  try {
    for ($hop = 0; $hop -lt 9; $hop++) {
      $uri = [Uri]$Url
      if ($uri.Scheme -ne 'https' -or $uri.UserInfo) { throw 'Downloads and every redirect must use credential-free HTTPS.' }
      $response = $client.GetAsync($uri, [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
      if ([int]$response.StatusCode -ge 300 -and [int]$response.StatusCode -lt 400) {
        $Url = [Uri]::new($uri, $response.Headers.Location).AbsoluteUri
        $response.Dispose()
        continue
      }
      $response.EnsureSuccessStatusCode() | Out-Null
      $inputStream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
      $outputStream = [System.IO.File]::Open($Destination, [System.IO.FileMode]::CreateNew)
      try {
        $buffer = [byte[]]::new(65536)
        [long]$total = 0
        while (($read = $inputStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
          $total += $read
          if ($total -gt $Limit) { throw 'Download exceeds size limit.' }
          $outputStream.Write($buffer, 0, $read)
        }
      } finally { $outputStream.Dispose(); $inputStream.Dispose(); $response.Dispose() }
      return
    }
    throw 'Too many download redirects.'
  } finally { $client.Dispose(); $handler.Dispose() }
}
try {
  $archive = Join-Path $temp 'release.tar.gz'
  $manifest = Join-Path $temp 'manifest.json'
  # TLS authenticates the download endpoint; independently supplied hashes authenticate the artifact.
  if (-not $ArchiveSha256 -or -not $ManifestSha256) {
    if ($BaseUrl -ne 'https://github.com/coop-collective/enginebox/releases/download') { throw 'Custom release sources require both independent hashes.' }
    $checksums = Join-Path $temp 'checksums'
    Get-HttpsFile "$BaseUrl/v$Version/enginebox-$Version-$target.sha256" $checksums 65536
    $archiveName = "enginebox-$Version-$target.tar.gz"
    $manifestName = "enginebox-$Version-$target.json"
    foreach ($line in (Get-Content $checksums)) {
      $parts = $line -split '\s+'
      if ($parts.Count -eq 2 -and $parts[1] -eq $archiveName -and -not $ArchiveSha256) { $ArchiveSha256 = $parts[0] }
      if ($parts.Count -eq 2 -and $parts[1] -eq $manifestName -and -not $ManifestSha256) { $ManifestSha256 = $parts[0] }
    }
    if ($ArchiveSha256 -cnotmatch '^[0-9a-f]{64}$' -or $ManifestSha256 -cnotmatch '^[0-9a-f]{64}$') { throw 'Missing or malformed release checksum.' }
  }
  Get-HttpsFile "$BaseUrl/v$Version/enginebox-$Version-$target.tar.gz" $archive 4294967296
  Get-HttpsFile "$BaseUrl/v$Version/enginebox-$Version-$target.json" $manifest 65536
  if ((Get-FileHash $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $ArchiveSha256 -or
      (Get-FileHash $manifest -Algorithm SHA256).Hash.ToLowerInvariant() -ne $ManifestSha256) { throw 'Release integrity check failed. Nothing installed.' }
  & tar.exe -xzf $archive -C $temp 'bin/enginebox-install.exe'
  if ($LASTEXITCODE -ne 0) { throw 'Could not extract the authenticated bootstrap executable.' }
  $installer = Join-Path $temp 'bin/enginebox-install.exe'
  if ((Get-Item $installer).LinkType) { throw 'Bootstrap executable must not be a link.' }
  & $installer --prefix $Prefix plan --role $Role
  if ($LASTEXITCODE -ne 0) { throw 'Host inspection failed.' }
  if ($InstallDependencies) {
    & $installer --prefix $Prefix plan --role $Role --apply-dependencies
    if ($LASTEXITCODE -ne 0) { throw 'Dependency installation failed; inspect a fresh plan.' }
  }
  & $installer --prefix $Prefix stage --version $Version --manifest $manifest --manifest-sha256 $ManifestSha256 --archive $archive
  if ($LASTEXITCODE -ne 0) { throw 'Release staging failed.' }
  if (-not (Test-Path (Join-Path $Prefix 'current.json'))) {
    & $installer --prefix $Prefix activate --version $Version
    if ($LASTEXITCODE -ne 0) { throw 'Initial activation failed.' }
  }
  Write-Output "Staged $Version. Configure private etc files from supervisor.example.json, then run service --config PATH --apply."
  Write-Output 'Existing services are not restarted. Use enginebox-install update for a drained online switch.'
} finally { Remove-Item -LiteralPath $temp -Recurse -Force }
