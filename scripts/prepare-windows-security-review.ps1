# @author jvz
# PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
# Read-only provenance collection. Does not run binaries, change Defender, or upload.
param([string]$OutputDirectory = 'dist/security-review')
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    New-Item -ItemType Directory -Force $OutputDirectory | Out-Null
    $files = @('target/x86_64-pc-windows-msvc/release/pureflow.exe', 'target/x86_64-pc-windows-msvc/release/pureflow-collector.exe', 'target/x86_64-pc-windows-msvc/release/bundle/nsis/PureFlow_0.1.1_x64-setup.exe')
    $manifest = foreach ($file in $files) {
        if (!(Test-Path $file)) { [pscustomobject]@{Path=$file;Exists=$false}; continue }
        $item = Get-Item $file
        $signature = Get-AuthenticodeSignature $file
        [pscustomobject]@{Path=$file;Exists=$true;Length=$item.Length;SHA256=(Get-FileHash $file -Algorithm SHA256).Hash;SignatureStatus=$signature.Status.ToString();Signer=if($signature.SignerCertificate){$signature.SignerCertificate.Subject}else{$null}}
    }
    $manifest | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $OutputDirectory 'artifacts.json') -Encoding utf8
    & git rev-parse HEAD | Set-Content (Join-Path $OutputDirectory 'base-commit.txt') -Encoding utf8
    & git diff --stat | Set-Content (Join-Path $OutputDirectory 'source-diff-stat.txt') -Encoding utf8
    $sourceFiles = @('Cargo.toml','Cargo.lock','apps/desktop/package.json','apps/desktop/package-lock.json','apps/desktop/src-tauri/Cargo.toml','apps/desktop/src-tauri/build.rs','apps/desktop/src-tauri/tauri.conf.json','apps/desktop/src-tauri/tauri.windows.conf.json','apps/desktop/scripts/build-helper.mjs','apps/desktop/src-tauri/src/helper.rs')
    Get-FileHash $sourceFiles -Algorithm SHA256 | Select-Object Path,Hash | ConvertTo-Json | Set-Content (Join-Path $OutputDirectory 'source-hashes.json') -Encoding utf8
    Write-Host "Prepared local review material: $OutputDirectory"
    Write-Host 'No binary executed, no security configuration changed, no data uploaded.'
    Write-Host 'Raw captures/SSID/database/private process records are deliberately excluded.'
} finally { Pop-Location }
