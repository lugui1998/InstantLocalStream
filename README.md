# InstantLocalStream

InstantLocalStream is a portable Rust host for low-latency browser screen sharing. The host serves a tokenized HTTP page, negotiates WebRTC sessions, and sends one shared encoded video source to each viewer.

The project currently provides:

- Native Rust control UI built with `eframe`.
- CLI and headless server mode.
- HTTP and Socket.IO-compatible control plane with polling fallback and WebSocket upgrade.
- Vue 3 + TypeScript + Vite static viewer SPA embedded into the Rust binary.
- Token routes with 12-character alphanumeric tokens.
- Capability-driven WebRTC VP8, VP9, and H.264 playback, with VP8 as the compatibility fallback.
- Optional system or selected-window audio capture through native Windows/Linux loopback backends.
- Opus audio in the same WebRTC session, with built-in application exclusion defaults for system audio.
- One-to-four profile adaptive quality with a TCP bootstrap probe, per-viewer media telemetry, targeted group assignment, per-group tuning, reconnect-based migration, and source hot-switching.
- A 14 Mbps fixed default and codec-aware automatic starting floors for screen readability; 1080p/30 starts at no less than 14 Mbps before telemetry-based adaptation.
- A persisted host upload-capacity probe with a conservative recommended viewer limit; the first run measures automatically when no result exists, and later tests run only on request.
- The viewer server starts with the native UI, while video capture, FFmpeg encoding, and native audio input remain cold until **Start Stream** and are torn down again by **Stop Stream**.
- FFmpeg encoding.
- Multiple simultaneous viewers through one shared UDP mux.
- Latest-frame per-viewer media queues that drop stale frames under backpressure.
- Viewer autoplay, per-rendered-frame capture-to-display timing, live playout-buffer delay, acknowledged control RTT, and WebRTC RTT/jitter diagnostics.
- XCap monitor and window discovery.
- Native source cards with immediate background discovery, cache-first progressive previews for visible cards, bounded Windows Graphics Capture thumbnails, live-source frame reuse while streaming, clickable selection, and a cached test-pattern preview.
- FFmpeg platform capture inputs (`gdigrab` on Windows and `x11grab` on X11) plus a deterministic test source. Windows application-window capture uses a stable native window identity, prefers Windows Graphics Capture, coalesces GPU readback to the configured frame rate, and feeds native BGRA directly to FFmpeg. When a captured window finishes resizing, the host automatically rebuilds only the capture and encoder graph at the new native dimensions and asks connected viewers to renegotiate; the stream remains logically running. It automatically falls back to XCap/PrintWindow if WGC does not deliver its first frame. An active minimized target keeps its last valid frame and receives best-effort PrintWindow refreshes until WGC resumes; a new capture asks the user to restore an already-minimized target before starting.
- Configurable cursor capture through `--draw-mouse` / `ILS_DRAW_MOUSE`; isolated window capture includes it only while the pointer is actually over the captured window, not an occluding one.
- Optional build-time embedding of FFmpeg into the executable.
- Native UI preferences persist under the OS temporary directory; access tokens are not persisted.

VP8, VP9, and H.264 are available browser codecs. The default `auto` policy uses VP8 as the validated cross-browser baseline; VP9 and H.264 remain available through explicit codec selection while their first-frame interoperability is tested further. If a negotiated codec delivers no video RTP, the client reports the failure and requests a compatible reassignment instead of waiting forever. H.264 uses a constrained-baseline, low-latency encoder path. AV1 remains deferred until low-latency encoding and device compatibility are better validated.

Audio is disabled by default. Enable system audio for a display or window audio for an application window in the native UI. The **Add default ignore list** action adds currently running matches for Discord, WhatsApp, Telegram, Teams, Zoom, Skype, and Slack; other discovered processes can be added individually.

Changing audio source details, exclusions, or the selected application window reopens the native audio input without replacing connected viewers' WebRTC audio tracks. Enabling or disabling audio changes the negotiated media topology, so the host publishes a new authoritative media-session generation and viewers reconnect automatically with (or without) an Opus audio track.

## Run locally

The default development build expects `ffmpeg` on `PATH`.

Build the viewer bundle before running the Rust application:

```text
.\scripts\build-web.ps1
cargo run -- start --http-port 8080 --media-ports 40000 --source test:0 --codec vp8
```

The packaged application serves the compiled SPA from the Rust binary; Node.js is needed only to build the frontend.

```text
cargo run -- validate --json
cargo run -- list-sources --json
cargo run -- list-windows --json
cargo run -- start --http-port 8080 --media-ports 40000 --source monitor:0
```

Open the generated token URL in a browser. A test pattern can validate the browser and WebRTC path without screen-capture permissions:

```text
cargo run -- start --http-port 8080 --media-ports 40000 --source test:0 --codec vp8 --quality 720p --fps-preset 60
```

The server binds to all local interfaces by default, so the displayed LAN URL works from the host and other devices on the local network. The CLI still accepts `--bind localhost` for an explicitly loopback-only run. `--port N` binds HTTP over TCP and the shared WebRTC UDP mux using the same numeric port. All viewers use that one UDP port. `--media-ports` remains accepted for compatibility, but only its first port is used; prefer a single value such as `--media-ports 40000`.

LAN mode derives a local IPv4 address for the copied URL. The native UI performs public-IP discovery automatically in the background, trying WTFIsMyIP first and a fallback service if necessary, and defaults the share-host selector to Public when no explicit advertised host is configured. You can choose Local, LAN, Public, or Custom for the displayed viewer URL. Custom hosts must be entered without a URL scheme; viewer URLs are HTTP-only for now. For public hosting, set `--advertise-host YOUR_PUBLIC_IP_OR_HOSTNAME` (or `ILS_ADVERTISE_HOST`) only when a manual override is needed.

The native UI starts with `Source` quality and `Source` frame rate. It also offers 144p, 240p, 360p, 480p, 720p HD, 1080p HD, 1440p HD, 2160p 4K, and 4320p 8K presets, plus 5, 10, 24, 30, 60, 75, and 120 FPS. Resolution and FPS entries above the selected source are hidden. Changes made while streaming keep the HTTP/control service available, refresh the shared capture and active encoder tracks when the format changes, and ask connected viewers to renegotiate. A new capture must emit a frame before it replaces the old one; a failed monitor/window switch restores the prior source. A newly selected monitor/window keeps its own aspect ratio. Each negotiated media graph has a fixed raw frame canvas to prevent decoder corruption. During an interactive window resize the current canvas is temporarily padded, then the host uses Windows resize events and the next compositor frame to replace it with a newly sized graph; the browser fits the new canvas into the video element.

## CLI commands

```text
instantlocalstream gui
instantlocalstream --ui
instantlocalstream start --ui
instantlocalstream start
instantlocalstream list-sources --json
instantlocalstream list-windows --json
instantlocalstream validate --json
instantlocalstream public-ip --json
instantlocalstream status --json
instantlocalstream version
```

Configuration can come from command-line arguments or environment variables such as `ILS_HTTP_PORT`, `ILS_MEDIA_PORTS`, `ILS_SOURCE`, and `ILS_TOKEN`.

`--max-viewers` (or `ILS_MAX_VIEWERS`) sets the server's admission limit and defaults to 8. The native UI also exposes this limit and can recommend a conservative value from a short Cloudflare upload-capacity test. The measured upload result is persisted and reused on later launches; use **Retest upload** to measure again. The shared UDP mux means this limit is independent of the UDP port count; practical limits are CPU, upload bandwidth, and network conditions. For example, `--max-viewers 20 --media-ports 40000` permits up to 20 viewers subject to those resource limits. The adaptive implementation supports one to four active transcode groups. A bounded TCP probe selects the initial group, after which rolling WebRTC media telemetry moves individual viewers and tunes each group's active profile.

The native UI's Latency preference is available when bitrate mode is Auto. It adjusts the automatic bitrate target: Low favors responsiveness and bandwidth efficiency, while Quality favors image detail.

`public-ip` queries the primary public IPv4 service and falls back to a second service on demand:

```text
instantlocalstream public-ip --json
instantlocalstream start --bind public --advertise-host 203.0.113.10 --port 8080
```

The executable opens only the native UI when launched normally on Windows; it does not create a companion terminal window. Use `--ui` (or `gui`) to explicitly open the UI from a command-line invocation. `start --ui` opens the UI with the start configuration preloaded; press **Start server** to launch it.

The default share URL has this shape:

```text
http://127.0.0.1:8080/aB3xY7mN2qP9
```

The token must contain exactly twelve ASCII letters or digits.

## Build-time FFmpeg embedding

Set `ILS_FFMPEG_PATH` while building to embed an FFmpeg executable in the application. Build the frontend first when invoking Cargo directly:

```text
$env:ILS_FFMPEG_PATH = 'C:\path\to\ffmpeg.exe'
.\scripts\build-web.ps1
cargo build --release
```

The repository also includes `scripts/build-portable.ps1` for Windows and `scripts/build-portable.sh` for Linux:

```powershell
.\scripts\build-portable.ps1 -FfmpegPath C:\path\to\ffmpeg.exe
```

On Linux, the script requires a statically linked FFmpeg executable, always emits `dist/InstantLocalStream`, and, if `appimagetool` is available, also emits `dist/InstantLocalStream-linux-x86_64.AppImage`.

The application extracts the embedded executable into an application-owned temporary directory when the server starts and removes that directory during normal shutdown. Startup cleanup should handle stale directories after crashes.

The portable build scripts emit `dist/InstantLocalStream.exe` on Windows or `dist/InstantLocalStream` on Linux. The executable contains the configured FFmpeg binary and extracts it only into the application-owned temporary directory at runtime.

FFmpeg redistribution requires a license review. The build configuration determines whether LGPL, GPL, non-free, and codec-patent obligations apply.

## Browser diagnostics

The Vue viewer correlates the RTP timestamp of a frame submitted to the browser compositor with the capture timestamp retained by the host. It reports capture-to-display delay for that exact rendered frame, splits capture-to-receive and receiver-to-display time when the browser exposes `receiveTime`, and shows clock-sync uncertainty. Browsers without rendered-frame metadata retain a clearly labeled estimate. The viewer also reports WebRTC RTT, jitter, observed receive rate, optional available incoming capacity, dropped frames, freezes, jitter-buffer delay, and decode time. It displays the current transcode group and uses a latest-frame policy: the server keeps only the newest queued sample, while the browser receives a zero playout-delay hint when supported.

Bitrate labels distinguish configuration from observation: **Assigned target** and transcode cards show the encoder target, while **Measured receive** is the actual recent RTP receive rate and may be lower for simple content. If FFmpeg remains more than 750 ms behind for eight encoded frames, only that encoder is restarted at the live edge and the recovery count is exposed to clients.

The viewer also reports active connected viewers. Refreshing a page replaces and cleans up its previous WebRTC session instead of consuming another viewer slot.

## Platform notes

The application targets Windows and Linux. XCap discovers monitors and windows; Windows uses `gdigrab` for monitors and Windows Graphics Capture for application windows, while Linux uses `x11grab` when X11 is available. Wayland, desktop portals, graphics drivers, and host permissions remain platform dependencies. A native Windows Graphics Capture picker remains follow-up work.

See [DEVELOPMENT_PLAN.md](DEVELOPMENT_PLAN.md) for the complete architecture, milestones, release requirements, and test plan.
