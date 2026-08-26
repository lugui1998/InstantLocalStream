# Third-party notices

The application uses the following third-party components:

- `webrtc-rs` 0.20.3, locally patched under `vendor/webrtc` to provide the shared UDP socket hook. Its MIT and Apache-2.0 license texts are included beside that source.
- `flexaudio` 0.2.x, used for native system and process audio capture on Windows and Linux. It is MIT licensed; the exact resolved version and license text must be recorded for a release.
- `opus-rs` 0.1.x, used for pure-Rust Opus encoding. It is MIT licensed; the exact resolved version and license text must be recorded for a release.
- `socketioxide` and its Engine.IO dependencies, used for Socket.IO-compatible control sessions, polling fallback, WebSocket upgrade, acknowledgements, and per-client state. The project is MIT licensed; record the exact Cargo.lock versions and license texts for a release.
- `rust-embed` and `mime_guess`, used to embed and serve the compiled viewer assets from the Rust binary. Record the exact Cargo.lock versions and license texts for a release.
- Vue 3, Vite, TypeScript tooling, and `socket.io-client`, used to compile and run the static viewer SPA. Their npm licenses and exact package-lock versions must be recorded for a release.
- FFmpeg, supplied at build time through `ILS_FFMPEG_PATH`. The portable build must ship the license text and source offer required by the selected FFmpeg build and its enabled libraries. Those notices are intentionally not generated from the development machine.

Before publishing a portable release, add the exact FFmpeg build configuration, license text, source offer, and codec/patent review for the binary used to build that release.
