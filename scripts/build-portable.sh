#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 || ( $# -eq 3 && ${3:-} != "--use-prebuilt-web" ) ]]; then
  echo "usage: $0 /path/to/ffmpeg /path/to/ffmpeg-license [--use-prebuilt-web]" >&2
  exit 2
fi

export ILS_FFMPEG_PATH="$(realpath "$1")"
ffmpeg_license="$(realpath "$2")"
if [[ ! -s "$ILS_FFMPEG_PATH" ]]; then
  echo "FFmpeg file is empty: $ILS_FFMPEG_PATH" >&2
  exit 1
fi
"$ILS_FFMPEG_PATH" -version >/dev/null
if [[ ! -s "$ffmpeg_license" ]]; then
  echo "FFmpeg license file is empty: $ffmpeg_license" >&2
  exit 1
fi
ldd_output="$(ldd "$ILS_FFMPEG_PATH" 2>&1 || true)"
if ! grep -Eq "not a dynamic executable|statically linked" <<<"$ldd_output"; then
  echo "Linux portable builds require a statically linked FFmpeg executable" >&2
  exit 1
fi
if [[ "${3:-}" == "--use-prebuilt-web" ]]; then
  if [[ ! -s web/dist/index.html ]]; then
    echo "Prebuilt viewer is missing: $(pwd)/web/dist/index.html" >&2
    exit 1
  fi
  if ! find web/dist/assets -maxdepth 1 -type f -print -quit 2>/dev/null | grep -q .; then
    echo "Prebuilt viewer assets are missing: $(pwd)/web/dist/assets" >&2
    exit 1
  fi
else
  bash "$(dirname "$0")/build-web.sh"
fi
cargo build --release --locked
mkdir -p dist
cp target/release/instant-local-stream dist/Instant-Local-Stream

appdir="dist/Instant-Local-Stream.AppDir"
rm -rf "$appdir"
mkdir -p "$appdir/usr/bin"
cp target/release/instant-local-stream "$appdir/usr/bin/Instant-Local-Stream"
cp packaging/linux/AppRun "$appdir/AppRun"
cp packaging/linux/Instant-Local-Stream.desktop "$appdir/Instant-Local-Stream.desktop"
cp packaging/linux/Instant-Local-Stream.svg "$appdir/Instant-Local-Stream.svg"
chmod +x "$appdir/AppRun" "$appdir/usr/bin/Instant-Local-Stream"
license_dir="$appdir/usr/share/doc/Instant-Local-Stream"
mkdir -p "$license_dir"
cp LICENSE "$license_dir/LICENSE"
cp THIRD_PARTY_NOTICES.md "$license_dir/THIRD_PARTY_NOTICES.md"
cp packaging/THIRD_PARTY_LICENSES-RUST.txt "$license_dir/THIRD_PARTY_LICENSES-RUST.txt"
cp packaging/THIRD_PARTY_LICENSES-NPM.txt "$license_dir/THIRD_PARTY_LICENSES-NPM.txt"
cp packaging/FFMPEG_SOURCE_OFFER.md "$license_dir/FFMPEG_SOURCE_OFFER.md"
cp "$ffmpeg_license" "$license_dir/FFMPEG-LICENSE.txt"

if command -v appimagetool >/dev/null 2>&1 && command -v linuxdeploy >/dev/null 2>&1; then
  APPIMAGE_EXTRACT_AND_RUN=1 linuxdeploy --appdir "$appdir"
  runtime_args=()
  if [[ -n "${APPIMAGETOOL_RUNTIME_FILE:-}" ]]; then
    runtime_args=(--runtime-file "$APPIMAGETOOL_RUNTIME_FILE")
  fi
  APPIMAGE_EXTRACT_AND_RUN=1 appimagetool "${runtime_args[@]}" "$appdir" dist/Instant-Local-Stream-linux-x86_64.AppImage
  printf 'Portable AppImage: %s\n' "$(pwd)/dist/Instant-Local-Stream-linux-x86_64.AppImage"
else
  printf 'Standalone portable executable: %s\n' "$(pwd)/dist/Instant-Local-Stream"
  printf 'AppImage not generated: install linuxdeploy and appimagetool to enable it.\n'
fi
