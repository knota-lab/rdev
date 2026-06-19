$ErrorActionPreference = "Stop"

cargo build --release

$metadata = cargo metadata --format-version 1 --no-deps | ConvertFrom-Json
$targetDir = $metadata.target_directory
$releaseDir = Join-Path $targetDir "release"
$source = Join-Path $releaseDir "rdev.exe"
$target = Join-Path $releaseDir "rdev-dev.exe"

if (-not (Test-Path $source)) {
    throw "release binary not found: $source"
}

Copy-Item $source $target -Force
Write-Host "dev binary: $target"
