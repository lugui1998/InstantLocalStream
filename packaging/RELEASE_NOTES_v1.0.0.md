# Instant Local Stream v1.0.0

The first stable release of Instant Local Stream: a portable host application
for sharing a monitor or application window to modern browsers over WebRTC.

## Highlights

- Native Windows and Linux x86-64 host applications.
- Monitor and application-window capture.
- Validated VP8 video with adaptive quality per viewer; VP9 and H.264 are
  available as experimental codec modes.
- Optional system or application audio.
- Low-latency browser playback with live connection statistics.
- Automatic bitrate scaling from a 14 Mbps 1080p30 baseline for higher
  resolutions and frame rates.
- Public sharing selected by default on port `8475`, with Local, LAN, and
  Custom alternatives.
- A built-in test source and validation command for troubleshooting.

## Downloads

- Windows: `Instant-Local-Stream-windows-x86_64.exe`
- Linux: `Instant-Local-Stream-linux-x86_64.AppImage`

Both applications include the pinned FFmpeg build used by the release. Verify
downloads with `SHA256SUMS.txt`; FFmpeg license and source/build provenance are
provided as separate release assets.

## Notes

- Windows builds are currently unsigned, so Microsoft Defender SmartScreen may
  show a warning on first launch.
- Linux screen capture targets X11. Wayland behavior depends on the desktop,
  portal, and capture permissions.
- LAN and public viewer links use HTTP and contain a bearer token. Use a trusted
  network or a secure reverse proxy, and do not publish the token link.
- Public sharing requires forwarding the selected port to the host for both TCP
  and UDP, plus allowing that port through the host firewall.
- Project source is available under the PolyForm Noncommercial License 1.0.0.
  Commercial use requires separate permission from the licensor.
