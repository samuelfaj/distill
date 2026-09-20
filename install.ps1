# Install or upgrade Distill from its public GitHub releases.
$ErrorActionPreference = 'Stop'
$repo = 'samuelfaj/distill'
$installDir = if ($env:DISTILL_INSTALL_DIR) { $env:DISTILL_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'distill' }
$version = if ($env:DISTILL_VERSION) { $env:DISTILL_VERSION } else { 'latest' }

$asset = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
  'X64' { 'distill-windows-x86_64' }
  'Arm64' { 'distill-windows-aarch64' }
  default { throw "Unsupported CPU architecture: $_" }
}

if ($version -eq 'latest') {
  $release = Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$repo/releases/latest"
  $version = $release.tag_name
}
$version = $version -replace '^v', ''
if ($version -notmatch '^[0-9A-Za-z][0-9A-Za-z.+-]*$') { throw 'Invalid release version.' }

$base = "https://github.com/$repo/releases/download/v$version"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) "distill-install-$([guid]::NewGuid())"
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  $download = Join-Path $tmp "$asset.exe"
  Invoke-WebRequest -UseBasicParsing "$base/$asset.exe" -OutFile $download
  $sums = (Invoke-WebRequest -UseBasicParsing "$base/SHA256SUMS").Content
  $expected = ($sums -split "`n" | Where-Object { $_ -match "\s$([regex]::Escape($asset)).exe$" } |
    ForEach-Object { ($_ -split '\s+')[0] } | Select-Object -First 1)
  if (-not $expected) { throw 'SHA256SUMS does not list this release asset.' }
  $actual = (Get-FileHash -Algorithm SHA256 $download).Hash
  if ($actual -ne $expected.ToUpperInvariant()) {
    throw 'Checksum verification failed. Your existing installation was not changed.'
  }
  & $download --version | Out-Null

  $downloads = Join-Path $installDir 'downloads'
  $bin = Join-Path $installDir 'bin'
  New-Item -ItemType Directory -Force -Path $downloads, $bin | Out-Null
  $binary = Join-Path $downloads "distill-$version-windows.exe"
  if (Test-Path $binary) {
    if ((Get-FileHash -Algorithm SHA256 $binary).Hash -ne $actual) {
      throw "Existing $binary differs from the verified release; installation stopped."
    }
  } else {
    Copy-Item $download "$binary.new" -Force
    Move-Item "$binary.new" $binary -Force
  }
  Copy-Item $binary (Join-Path $bin 'distill.exe') -Force
  Set-Content -Path (Join-Path $installDir '.distill-release') -Value $repo -NoNewline
  Write-Host "`nDistill $version installed. Restart any open Distill sessions."
  Write-Host "Add this directory to your PATH:`n  $bin"
  Write-Host 'For future upgrades, run: distill update'
} finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
