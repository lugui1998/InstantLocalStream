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
  if ! command -v pkg-config >/dev/null 2>&1 || ! pkg-config --exists libpipewire-0.3; then
    echo "PipeWire development files are required to build the portable AppImage." >&2
    exit 1
  fi
  pipewire_library="$(pkg-config --variable=libdir libpipewire-0.3)/libpipewire-0.3.so.0"
  pipewire_prefix="$(pkg-config --variable=prefix libpipewire-0.3)"
  pipewire_libdir="$(pkg-config --variable=libdir libpipewire-0.3)"
  pipewire_datadir="$(pkg-config --variable=datadir libpipewire-0.3)"
  pipewire_modules="$pipewire_libdir/pipewire-0.3"
  spa_plugins="$pipewire_libdir/spa-0.2"
  pipewire_config="$pipewire_datadir/pipewire"
  pipewire_license="$pipewire_prefix/share/doc/pipewire/COPYING"
  if [[ ! -s "$pipewire_library" ]]; then
    echo "PipeWire runtime library was not found: $pipewire_library" >&2
    exit 1
  fi
  for runtime_tree in "$pipewire_modules" "$spa_plugins" "$pipewire_config"; do
    if [[ ! -d "$runtime_tree" ]]; then
      echo "PipeWire runtime tree was not found: $runtime_tree" >&2
      exit 1
    fi
  done
  if [[ ! -s "$pipewire_license" ]]; then
    echo "PipeWire license was not found: $pipewire_license" >&2
    exit 1
  fi
  mkdir -p "$appdir/usr/lib" "$appdir/usr/share"
  cp -a "$pipewire_modules" "$appdir/usr/lib/"
  cp -a "$spa_plugins" "$appdir/usr/lib/"
  cp -a "$pipewire_config" "$appdir/usr/share/"
  cp "$pipewire_license" "$license_dir/PIPEWIRE-LICENSE.txt"
  # linuxdeploy's standard exclusion list assumes PipeWire is installed on the
  # target system. Force-deploy it because this executable links to it at startup.
  APPIMAGE_EXTRACT_AND_RUN=1 linuxdeploy --appdir "$appdir" --library "$pipewire_library"
  if [[ ! -s "$appdir/usr/lib/libpipewire-0.3.so.0" ]]; then
    echo "linuxdeploy did not bundle the required PipeWire runtime library." >&2
    exit 1
  fi
  for required_runtime_file in \
    "$appdir/usr/lib/libpipewire-0.3.so.0" \
    "$appdir/usr/lib/pipewire-0.3/libpipewire-module-protocol-native.so" \
    "$appdir/usr/lib/pipewire-0.3/libpipewire-module-client-node.so" \
    "$appdir/usr/lib/pipewire-0.3/libpipewire-module-adapter.so" \
    "$appdir/usr/lib/pipewire-0.3/libpipewire-module-metadata.so" \
    "$appdir/usr/lib/spa-0.2/support/libspa-support.so" \
    "$appdir/usr/lib/spa-0.2/audioconvert/libspa-audioconvert.so" \
    "$appdir/usr/share/pipewire/client.conf"; do
    if [[ ! -s "$required_runtime_file" ]]; then
      echo "PipeWire runtime dependency was not bundled: $required_runtime_file" >&2
      exit 1
    fi
  done
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
