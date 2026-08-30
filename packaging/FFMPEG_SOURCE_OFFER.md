# FFmpeg source and build information for Instant Local Stream v1.0.0

The Windows and Linux x86-64 release artifacts embed an unmodified, statically linked FFmpeg executable from the GPL build family published by BtbN/FFmpeg-Builds. This variant includes the GPL-only `libx264` encoder required by Instant Local Stream's H.264 mode.

- FFmpeg revision: `n8.1.2-50-g1a748fe2cd`
- FFmpeg source: <https://github.com/FFmpeg/FFmpeg/tree/1a748fe2cd>
- Build release: <https://github.com/BtbN/FFmpeg-Builds/releases/tag/autobuild-2026-08-29-13-12>
- Build scripts and reproduction instructions: <https://github.com/BtbN/FFmpeg-Builds>
- Windows archive: `ffmpeg-n8.1.2-50-g1a748fe2cd-win64-gpl-8.1.zip`
- Linux archive: `ffmpeg-n8.1.2-50-g1a748fe2cd-linux64-gpl-8.1.tar.xz`

The portable applications include the FFmpeg license text extracted from the exact upstream archive. On Windows, run `Instant-Local-Stream-windows-x86_64.exe licenses` to extract it and the other bundled notices. In the Linux AppImage, the notices are under `usr/share/doc/Instant-Local-Stream`. The release workflow verifies both archives against pinned SHA-256 values recorded in the workflow.
