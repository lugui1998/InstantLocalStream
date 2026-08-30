# InstantLocalStream

Share your screen or an application window with any device that has a web browser. InstantLocalStream runs on your computer and sends video over WebRTC, with no app or account required for viewers.

**[Download the latest release](https://github.com/lugui1998/InstantLocalStream/releases/latest)**

Available for Windows x86-64 and Linux x86-64.

## How it works

1. Open InstantLocalStream.
2. Choose a monitor or application window.
3. Click **Start Stream**.
4. Copy the link and open it on another device.

The link contains a private 12-character access token. Anyone with the link can watch the stream, so share it with care.

## Download

### Windows

[Download `InstantLocalStream-windows-x86_64.exe`](https://github.com/lugui1998/InstantLocalStream/releases/latest/download/InstantLocalStream-windows-x86_64.exe)

Open the downloaded file. Windows SmartScreen may warn about builds that do not have a code-signing certificate.

### Linux

[Download `InstantLocalStream-linux-x86_64.AppImage`](https://github.com/lugui1998/InstantLocalStream/releases/latest/download/InstantLocalStream-linux-x86_64.AppImage)

Make the AppImage executable, then run it:

```bash
chmod +x InstantLocalStream-linux-x86_64.AppImage
./InstantLocalStream-linux-x86_64.AppImage
```

Linux screen capture uses X11. Wayland support depends on your desktop, portal, and capture permissions.

## Features

- Monitor and application-window capture
- VP8, VP9, and H.264 video
- Optional system or application audio
- Automatic quality adjustment for each viewer
- Low-latency WebRTC playback
- Live latency, bitrate, and connection statistics
- Support for multiple viewers through one UDP port
- Built-in test pattern for connection checks

Audio starts disabled. You can enable it from the host window before starting the stream.

## Sharing outside your computer

The default **Local** mode limits access to the host computer. Choose **LAN** to share with devices on the same network.

Public internet sharing requires firewall and router configuration. InstantLocalStream serves viewer links over HTTP, so use a trusted network or place the host behind a secure reverse proxy. Do not publish the token link.

The host accepts eight viewers by default. Your upload speed, selected resolution, and computer performance determine how many viewers it can serve.

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

InstantLocalStream uses FFmpeg and other third-party components. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) and the [FFmpeg source information](packaging/FFMPEG_SOURCE_OFFER.md) for details.
