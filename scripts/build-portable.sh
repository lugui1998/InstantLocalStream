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
if ! ldd "$ILS_FFMPEG_PATH" 2>&1 | grep -q "not a dynamic executable"; then
  echo "Linux portable builds require a statically linked FFmpeg executable" >&2
  exit 1
fi
bash "$(dirname "$0")/build-web.sh"
cargo build --release
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

if command -v appimagetool >/dev/null 2>&1; then
  appimagetool "$appdir" dist/InstantLocalStream-linux-x86_64.AppImage
  printf 'Portable AppImage: %s\n' "$(pwd)/dist/InstantLocalStream-linux-x86_64.AppImage"
else
  printf 'Standalone portable executable: %s\n' "$(pwd)/dist/InstantLocalStream"
  printf 'AppImage not generated: install appimagetool to enable it.\n'
fi
