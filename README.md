# Instant Local Stream

Share your screen or an application window with any device that has a web browser. Instant Local Stream runs on your computer and sends video over WebRTC, with no app or account required for viewers.

Available for Windows x86-64 and Linux x86-64.

## Download

### Windows

[Download `Instant-Local-Stream-windows-x86_64.exe`](https://github.com/lugui1998/InstantLocalStream/releases/latest/download/Instant-Local-Stream-windows-x86_64.exe)

Open the downloaded executable. Windows SmartScreen may warn because the application does not have a code-signing certificate.

### Linux

[Download `Instant-Local-Stream-linux-x86_64.AppImage`](https://github.com/lugui1998/InstantLocalStream/releases/latest/download/Instant-Local-Stream-linux-x86_64.AppImage)

Make the AppImage executable, then run it:

```bash
chmod +x Instant-Local-Stream-linux-x86_64.AppImage
./Instant-Local-Stream-linux-x86_64.AppImage
```

Linux screen capture uses X11. Wayland support depends on your desktop, portal, and capture permissions. Audio capture requires a working host PipeWire service and session manager; the AppImage bundles its PipeWire client runtime.


## How it works

1. Open Instant Local Stream.
2. Choose a monitor or application window.
3. Click **Start Stream**.
4. Copy the link and open it on another device.

The link contains a private 12-character access token. Anyone with the link can watch the stream, so share it with care.

## Sharing public stream

**Public** is the default sharing mode. To make the viewer link reachable from the internet, configure your router to forward the selected port to the host computer for both **TCP and UDP**, and allow that port through the host firewall. The default port is `8475` unless you change it in the app.

## Features

- Monitor and application-window capture
- VP8 video, with experimental VP9 and H.264 modes
- Optional system or application audio
- Automatic quality adjustment for each viewer
- Low-latency WebRTC playback
- Live latency, bitrate, and connection statistics
- Support for multiple viewers through one UDP port
- Built-in test pattern for connection checks


## Build from source

You need Rust 1.95, Node.js 22, and FFmpeg on `PATH`.

```bash
git clone https://github.com/lugui1998/InstantLocalStream.git
cd InstantLocalStream
```

Windows:

```powershell
.\scripts\build-web.ps1
cargo run
```

Linux:

```bash
bash scripts/build-web.sh
cargo run
```

Run `cargo run -- --help` to see the command-line options. Contributors can find architecture and implementation notes in [DEVELOPMENT_PLAN.md](DEVELOPMENT_PLAN.md).

## Third-party software

Instant Local Stream uses FFmpeg and other third-party components. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) and the [FFmpeg source information](packaging/FFMPEG_SOURCE_OFFER.md) for details.

The portable downloads contain their required license notices. On Windows, run `Instant-Local-Stream-windows-x86_64.exe licenses` to extract readable copies.

## Disclosure

AI tools were used while creating this project.

## License

Instant Local Stream is source-available under the [PolyForm Noncommercial License 1.0.0](LICENSE). Personal and other noncommercial uses are permitted; commercial use requires separate permission from the licensor.
