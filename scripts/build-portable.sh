#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 /path/to/ffmpeg" >&2
  exit 2
fi

export ILS_FFMPEG_PATH="$(realpath "$1")"
if [[ ! -s "$ILS_FFMPEG_PATH" ]]; then
  echo "FFmpeg file is empty: $ILS_FFMPEG_PATH" >&2
  exit 1
fi
"$ILS_FFMPEG_PATH" -version >/dev/null
ldd_output="$(ldd "$ILS_FFMPEG_PATH" 2>&1 || true)"
if ! grep -Eq "not a dynamic executable|statically linked" <<<"$ldd_output"; then
  echo "Linux portable builds require a statically linked FFmpeg executable" >&2
  exit 1
fi
bash "$(dirname "$0")/build-web.sh"
cargo build --release --locked
mkdir -p dist
cp target/release/instant-local-stream dist/InstantLocalStream

appdir="dist/InstantLocalStream.AppDir"
rm -rf "$appdir"
mkdir -p "$appdir/usr/bin"
cp target/release/instant-local-stream "$appdir/usr/bin/InstantLocalStream"
cp packaging/linux/AppRun "$appdir/AppRun"
cp packaging/linux/InstantLocalStream.desktop "$appdir/InstantLocalStream.desktop"
cp packaging/linux/InstantLocalStream.svg "$appdir/InstantLocalStream.svg"
chmod +x "$appdir/AppRun" "$appdir/usr/bin/InstantLocalStream"

if command -v appimagetool >/dev/null 2>&1 && command -v linuxdeploy >/dev/null 2>&1; then
  APPIMAGE_EXTRACT_AND_RUN=1 linuxdeploy --appdir "$appdir"
  runtime_args=()
  if [[ -n "${APPIMAGETOOL_RUNTIME_FILE:-}" ]]; then
    runtime_args=(--runtime-file "$APPIMAGETOOL_RUNTIME_FILE")
  fi
  APPIMAGE_EXTRACT_AND_RUN=1 appimagetool "${runtime_args[@]}" "$appdir" dist/InstantLocalStream-linux-x86_64.AppImage
  printf 'Portable AppImage: %s\n' "$(pwd)/dist/InstantLocalStream-linux-x86_64.AppImage"
else
  printf 'Standalone portable executable: %s\n' "$(pwd)/dist/InstantLocalStream"
  printf 'AppImage not generated: install linuxdeploy and appimagetool to enable it.\n'
fi
