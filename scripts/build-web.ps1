$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
Push-Location (Join-Path $root "web")
try {
    npm ci
    npm run build
}
finally {
    Pop-Location
}
