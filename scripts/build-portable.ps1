param(
    [Parameter(Mandatory = $true)]
    [string]$FfmpegPath
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

& (Join-Path $PSScriptRoot "build-web.ps1")
cargo build --release

$artifact = Join-Path (Get-Location) "target\release\instant-local-stream.exe"
$dist = Join-Path (Get-Location) "dist"
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$portable = Join-Path $dist "InstantLocalStream.exe"
Copy-Item -LiteralPath $artifact -Destination $portable -Force
Write-Host "Portable artifact: $portable"
