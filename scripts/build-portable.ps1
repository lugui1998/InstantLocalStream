param(
    [Parameter(Mandatory = $true)]
    [string]$FfmpegPath,
    [Parameter(Mandatory = $true)]
    [string]$FfmpegLicensePath,
    [switch]$UsePrebuiltWeb
)

$ErrorActionPreference = "Stop"
$ffmpegItem = Get-Item -LiteralPath $FfmpegPath
$resolvedFfmpeg = if ($ffmpegItem.ResolvedTarget) {
    $ffmpegItem.ResolvedTarget
} else {
    $ffmpegItem.FullName
}
if ((Get-Item -LiteralPath $resolvedFfmpeg).Length -le 0) {
    throw "FFmpeg file is empty: $resolvedFfmpeg"
}
$null = & $resolvedFfmpeg -version
if ($LASTEXITCODE -ne 0) {
    throw "FFmpeg could not be executed: $resolvedFfmpeg"
}
$env:ILS_FFMPEG_PATH = $resolvedFfmpeg
$ffmpegLicenseItem = Get-Item -LiteralPath $FfmpegLicensePath
$resolvedFfmpegLicense = if ($ffmpegLicenseItem.ResolvedTarget) {
    $ffmpegLicenseItem.ResolvedTarget
} else {
    $ffmpegLicenseItem.FullName
}
if ((Get-Item -LiteralPath $resolvedFfmpegLicense).Length -le 0) {
    throw "FFmpeg license file is empty: $resolvedFfmpegLicense"
}
$env:ILS_FFMPEG_LICENSE_PATH = $resolvedFfmpegLicense

if ($UsePrebuiltWeb) {
    $webIndex = Join-Path (Get-Location) "web\dist\index.html"
    $webAssets = Join-Path (Get-Location) "web\dist\assets"
    if (-not (Test-Path -LiteralPath $webIndex -PathType Leaf)) {
        throw "Prebuilt viewer is missing: $webIndex"
    }
    if (-not (Get-ChildItem -LiteralPath $webAssets -File -ErrorAction SilentlyContinue | Select-Object -First 1)) {
        throw "Prebuilt viewer assets are missing: $webAssets"
    }
} else {
    & (Join-Path $PSScriptRoot "build-web.ps1")
}
cargo build --release --locked
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed with exit code $LASTEXITCODE"
}

$artifact = Join-Path (Get-Location) "target\release\instant-local-stream.exe"
$dist = Join-Path (Get-Location) "dist"
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$portable = Join-Path $dist "Instant-Local-Stream.exe"
Copy-Item -LiteralPath $artifact -Destination $portable -Force
Write-Host "Portable artifact: $portable"
