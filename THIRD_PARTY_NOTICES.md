# Third-party notices for Instant Local Stream v1.0.0

Instant Local Stream project code is distributed under the PolyForm
Noncommercial License 1.0.0. The complete project license is provided in
`LICENSE`. Third-party components remain subject to their own licenses listed
below; the project license does not replace or modify those terms.

This notice summarizes the direct production dependencies resolved for
v1.0.0. `Cargo.lock` and `web-package-lock.json`, shipped with the release,
are the authoritative inventories of all direct and transitive versions.
Each component remains subject to its own license and copyright notices.
Complete harvested license and copyright texts are provided in
`THIRD_PARTY_LICENSES-RUST.txt` and `THIRD_PARTY_LICENSES-NPM.txt` with every
release. They are also bundled inside the Windows archive and Linux AppImage.

## Rust dependencies

The following resolved direct dependencies are licensed under MIT or
Apache-2.0, at the recipient's option:

- `anyhow` 1.0.104
- `async-trait` 0.1.92
- `clap` 4.6.6
- `eframe` 0.36.1
- `futures-util` 0.3.34
- `png` 0.18.1
- `rand` 0.9.5
- `reqwest` 0.12.28
- `rtc` 0.20.3
- `serde` 1.0.229
- `serde_json` 1.0.151
- `thiserror` 2.0.20
- `uuid` 1.25.0
- `windows` 0.62.2
- `webrtc` 0.20.3, locally patched under `vendor/webrtc` to support the
  shared UDP socket hook; its upstream MIT and Apache-2.0 texts remain beside
  the vendored source.

The following resolved direct dependencies are MIT licensed:

- `axum` 0.8.9
- `bytes` 1.12.1
- `flexaudio` 0.2.0
- `mime_guess` 2.0.5
- `rust-embed` 8.12.0
- `socketioxide` 0.18.7
- `tokio` 1.53.1
- `tracing` 0.1.44
- `tracing-subscriber` 0.3.23

Additional direct dependencies:

- `opus-rs` 0.1.32 — BSD-3-Clause
- `xcap` 0.9.8 — Apache-2.0

## Embedded viewer

The compiled viewer includes these direct runtime dependencies:

- `socket.io-client` 4.8.3 — MIT
- `uplot` 1.6.32 — MIT
- `vue` 3.5.41 — MIT

The frontend build toolchain and its complete transitive resolution are
recorded in `web-package-lock.json`. Packages in that lockfile use permissive
licenses including MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, and ISC; consult
each package's bundled metadata for its copyright notice and exact terms.

## FFmpeg and codecs

The Windows and Linux portable artifacts embed an unmodified FFmpeg
executable from BtbN/FFmpeg-Builds and invoke it as a separate process. The
selected GPL build includes `libx264`. Each release includes:

- the license text extracted from the exact platform archive;
- the complete `ffmpeg -version` output, including configuration and library
  versions;
- pinned source, build, archive, and reproduction information in
  `FFMPEG_SOURCE_OFFER.md`.

FFmpeg and its enabled libraries are governed by their respective licenses.
Codec use may also be subject to patent or regulatory requirements in your
jurisdiction.
