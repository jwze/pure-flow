# @author jvz
# PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
# Build the ordinary-permission GUI plus separately elevated, fixed collector.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:OS -ne 'Windows_NT') { throw 'Windows is required.' }
$root = Split-Path -Parent $PSScriptRoot
$helperStage = Join-Path $root 'apps/desktop/src-tauri/helper/pureflow-collector.exe'
$stageBackup = $null
$stagePrepared = $false
$previousLto = $env:CARGO_PROFILE_RELEASE_LTO
Push-Location $root
try {
    if (Test-Path -LiteralPath $helperStage -PathType Leaf) {
        $backupCandidate = [IO.Path]::GetTempFileName()
        try {
            Copy-Item -LiteralPath $helperStage -Destination $backupCandidate -Force
            # Publish only a complete backup; a failed copy must never be restored.
            $stageBackup = $backupCandidate
        } catch {
            Remove-Item -LiteralPath $backupCandidate -Force
            throw
        }
    }
    # Windows-only result of the measured thin/fat comparison; workspace and
    # macOS release profiles are deliberately unchanged. Helper inherits it.
    $env:CARGO_PROFILE_RELEASE_LTO = 'fat'
    # A clean checkout has no ignored helper staging. Tauri resource validation
    # during cargo check needs it, so build the real helper before those checks.
    $stagePrepared = $true
    & node (Join-Path $root 'apps/desktop/scripts/build-helper.mjs')
    if ($LASTEXITCODE -ne 0) { throw 'Collector prerequisite build failed.' }
    & cargo fmt --all --check
    if ($LASTEXITCODE -ne 0) { throw 'Formatting failed.' }
    & cargo check --all-targets --workspace --features windows-collector --locked --target x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw 'Rust checks failed.' }
    & cargo clippy --workspace --all-targets --features windows-collector --locked --target x86_64-pc-windows-msvc -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'Clippy failed.' }
    Push-Location apps/desktop
    try {
        & npm.cmd ci
        if ($LASTEXITCODE -ne 0) { throw 'npm ci failed.' }
        & npm.cmd run tauri -- build --target x86_64-pc-windows-msvc --bundles nsis
        if ($LASTEXITCODE -ne 0) { throw 'Tauri build failed.' }
        # Byte inspection only: never launch GUI/helper/installer to validate.
        $builtRoot = Join-Path $root 'target/x86_64-pc-windows-msvc/release'
        $bundledHelper = Join-Path $builtRoot 'helper/pureflow-collector.exe'
        $helperSha = [Security.Cryptography.SHA256]::Create()
        try {
            $expected = ([BitConverter]::ToString($helperSha.ComputeHash([IO.File]::ReadAllBytes($bundledHelper)))).Replace('-', '').ToLowerInvariant()
        } finally { $helperSha.Dispose() }
        $guiBytes = [IO.File]::ReadAllBytes((Join-Path $builtRoot 'pureflow.exe'))
        if (-not [Text.Encoding]::ASCII.GetString($guiBytes).Contains($expected)) {
            throw 'GUI is missing the exact bundled collector SHA-256; refusing package delivery.'
        }
    } finally { Pop-Location }
    $id = & cargo pkgid -p pureflow-desktop
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read desktop version.' }
    $version = ($id.Trim() -split '@')[-1]
    if ($version -notmatch '^\d+\.\d+\.\d+$') { throw 'Invalid version.' }
    $dist = Join-Path $root 'dist'
    # The release directory is reserved for the two deliverables below. Remove
    # stale verification logs, extracted probes, and prior review material so a
    # finished Windows package has the same clean shape as the macOS package.
    if (Test-Path -LiteralPath $dist) {
        Remove-Item -LiteralPath $dist -Recurse -Force
    }
    New-Item -ItemType Directory -Force $dist | Out-Null
    $asset = "PureFlow_${version}_x64-setup.exe"
    $source = Join-Path $root "target/x86_64-pc-windows-msvc/release/bundle/nsis/$asset"
    if (-not (Test-Path $source)) { throw 'Installer was not generated.' }
    $destination = Join-Path $dist $asset
    Copy-Item $source $destination -Force
    $sha = [Security.Cryptography.SHA256]::Create()
    $stream = [IO.File]::OpenRead($destination)
    try { $hash = ([BitConverter]::ToString($sha.ComputeHash($stream))).Replace('-', '').ToLowerInvariant() }
    finally { $stream.Dispose(); $sha.Dispose() }
    [IO.File]::WriteAllText("$destination.sha256", "$hash  $asset`n", [Text.Encoding]::ASCII)
    Write-Host "Installer: $destination"
    Write-Host 'GUI runs without elevation. Only its collector asks for UAC; no service is installed.'
    Write-Host 'Local history survives upgrades/uninstall unless explicitly removed by the user.'
} finally {
    $env:CARGO_PROFILE_RELEASE_LTO = $previousLto
    try {
        if ($null -ne $stageBackup) {
            # If restoration fails, retain the only backup and report the error.
            Copy-Item -LiteralPath $stageBackup -Destination $helperStage -Force
            Remove-Item -LiteralPath $stageBackup -Force
        } elseif ($stagePrepared -and (Test-Path -LiteralPath $helperStage -PathType Leaf)) {
            Remove-Item -LiteralPath $helperStage -Force
        }
    } finally { Pop-Location }
}
