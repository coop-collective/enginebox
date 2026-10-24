$ErrorActionPreference='Stop'
$path=Join-Path $PSScriptRoot 'install.ps1.in'
$tokens=$null; $errors=$null
[System.Management.Automation.Language.Parser]::ParseFile($path,[ref]$tokens,[ref]$errors) | Out-Null
if ($errors.Count) { $errors | Format-List; throw 'PowerShell syntax failure' }
# Load helpers from a .ps1 copy; no host setup may execute during this test.
$fixture=Join-Path $env:TEMP ('enginebox-installer-'+[Guid]::NewGuid()+'.ps1')
Copy-Item -LiteralPath $path -Destination $fixture
try {
  . $fixture
  if ((Assert-Origin 'https://example.com/') -ne 'https://example.com') { throw 'Origin normalization failed' }
  foreach ($bad in @('http://example.com','https://name:secret@example.com','https://example.com/api','https://example.com/?x=1')) {
    $rejected=$false;try { Assert-Origin $bad | Out-Null } catch { $rejected=$true }
    if (-not $rejected) { throw "Unsafe controller accepted: $bad" }
  }
  if ((Read-Number 'CPU' 4 2 1 8) -ne 4) { throw 'Explicit allowance ignored' }
  $rejected=$false;try { Read-Number 'CPU' 9 2 1 8 | Out-Null } catch { $rejected=$true }
  if (-not $rejected) { throw 'Physical resource ceiling ignored' }
  Write-Output 'PowerShell 5.1 parsing, controller validation and resource bounds passed.'
} finally { Remove-Item -LiteralPath $fixture -Force }
