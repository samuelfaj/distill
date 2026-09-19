# Modified for Distill by Samuel Fajreldines, 2026. Source-only installation.
$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path "$PSScriptRoot/../../../..").Path
Push-Location $repoRoot
try {
    cargo build --release -p distill-pager-bin --bin distill
    if ($LASTEXITCODE -ne 0) { throw 'Distill build failed' }
    $installDir = if ($env:DISTILL_BIN_DIR) { $env:DISTILL_BIN_DIR } else { Join-Path $HOME '.local/bin' }
    $targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
    New-Item -ItemType Directory -Force $installDir | Out-Null
    Copy-Item (Join-Path $targetDir 'release/distill.exe') (Join-Path $installDir 'distill.exe') -Force
    Write-Host "Distill installed at $installDir/distill.exe. Add this directory to PATH if needed."
} finally { Pop-Location }
