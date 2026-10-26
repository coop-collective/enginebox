$ErrorActionPreference='Stop'
$fixture=Join-Path $env:TEMP ('enginebox-registration-'+[guid]::NewGuid().ToString('N')+'.ps1')
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'install.ps1.in') -Destination $fixture
try {
  . $fixture
  $source=[Convert]::ToBase64String([IO.File]::ReadAllBytes((Join-Path $PSScriptRoot 'test-node.py')))
  $stage="import base64,pathlib;pathlib.Path('/tmp/enginebox-node-proof.py').write_bytes(base64.b64decode('$source'))"
  Invoke-Wsl -Arguments @('--distribution','EngineboxCI','--user','root','--cd','/','--exec','python3','-c',$stage) | Out-Host
  $code=[guid]::NewGuid().ToString('N')+'.'+[guid]::NewGuid().ToString('N')+[guid]::NewGuid().ToString('N')
  $sha=[Security.Cryptography.SHA256]::Create()
  try { $hash=([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($code)))).Replace('-','').ToLowerInvariant() } finally { $sha.Dispose() }
  # Exercise the installer's real PowerShell 5.1 -> WSL stdin transport. Linux
  # verifies the expected hash and passes those same bytes to the Rust CLI.
  Invoke-Wsl -Arguments @('--distribution','EngineboxCI','--user','root','--cd','/','--exec','python3','/tmp/enginebox-node-proof.py','--stdin-code-sha256',$hash) -InputText $code | Out-Host
  Write-Output 'Real Windows PowerShell 5.1 registration input reached WSL and the Rust node unchanged.'
} finally { Remove-Item -LiteralPath $fixture -Force }
