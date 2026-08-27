# InstantLocalStream Development Plan

Status: VP8/VP9/H.264 video baseline, Vue/Vite static viewer, Socket.IO-compatible control sessions, native audio capture, and Opus/WebRTC integration are implemented. The initial adaptive implementation uses a one-to-four-slot encoder budget: only the primary encoder begins running; later groups are created on bootstrap/degradation, merged when near-identical, and drained after becoming unused. All four lightweight slots exist so a live switch between Manual and Auto changes the active budget immediately. A paced raw capture source fans out its newest frame to encoder-only variants. When a live source, resolution, or frame-rate change alters that raw format, the host rebuilds active encoder tracks and asks connected viewers to renegotiate. Replacement encoders must produce their first encoded frame before profile state is committed, and a failed source/profile switch restores the previous capture and group graph. The raw bus always uses one exact output canvas: if a capture backend reports a client area whose aspect differs from the selected window bounds, FFmpeg preserves the source aspect and pads that canvas rather than emitting variable-size YUV frames. On Windows, selected application windows prefer continuous Windows Graphics Capture. A 750 ms first-frame watchdog automatically switches to the same XCap/PrintWindow path used by source previews when WGC is silent, and status exposes the active capture backend. Window selection retains the native HWND for the running UI session rather than trusting a transient enumeration index, so preview refreshes and window-order changes cannot retarget capture. HWNDs are not written to preferences or accepted through the headless CLI because Windows may recycle them. The control server starts on a neutral local test source and still requires an explicit card selection. Stream start is acknowledged only after the host successfully applies the selected source and replacement encoder; the UI no longer reports Running optimistically. A selected window remains in the UI while minimized even if XCap temporarily omits it. An active capture tries a low-rate PrintWindow refresh while WGC is paused and otherwise republishes the last valid frame; starting a new capture is disabled until the window is restored because Windows reports a collapsed, non-content surface while it is minimized. Closing the target raises a capture error through the WGC Closed event instead of replaying the final frame indefinitely. If mouse capture is enabled, the cursor is included only while the OS hit-test says it belongs to the captured window; an occluding window under the pointer suppresses it. A new capture must produce a frame before the switch is committed; failures restore the previous source and viewer tracks. The native source UI warms window thumbnails on its background worker before the Windows tab is selected, and Start Stream remains disabled until an explicit valid source selection exists. The audio graph keeps stable WebRTC tracks and dynamically reopens system/window loopback when audio mode, exclusions, or selected HWND changes. VPx lookahead and alternate-reference buffering are disabled, FFmpeg output is flushed, and encoded access units older than 250 ms are discarded instead of being paced to viewers. Each viewer now receives explicit RTP timestamps; the host retains a bounded per-viewer RTP-to-capture-time map, and the browser correlates `requestVideoFrameCallback()` metadata for the exact frame submitted to the compositor. The displayed capture-to-display value includes the whole host, network, receiver, decoder, and compositor path and shows clock-sync uncertainty; a clearly labeled estimate remains only as fallback. The viewer performs a visible, same-origin streaming bootstrap probe (up to 1 MiB or five seconds), reports a rolling 15-second playback window, and shows the selected codec plus group codec. Automatic codec policy uses VP8 as the validated cross-browser baseline; VP9 and H.264 remain explicit options until first-frame interoperability is proven. A client that receives no video RTP after negotiation now reports codec failure after two bounded checks and requests reassignment instead of waiting forever. Fixed H.264 uses constrained baseline, one slice per picture, and a bounded ordered NAL queue, but its browser packetization path remains under investigation. AV1 remains the next codec milestone.

## 1. Product definition

InstantLocalStream will let a person capture a monitor or application window, encode it with FFmpeg, and share it through a browser. The host application will run a local HTTP server and a WebRTC media endpoint. Viewers will open a URL in a modern browser.

The first release will target Windows and Linux. Each platform may use a different capture backend and build artifact, while the Rust application core remains shared.

The first release will use HTTP and a Socket.IO-compatible control endpoint without a server certificate. The application will support local viewing first, LAN viewing next, and optional internet viewing through manual port forwarding.

The application will ship as one executable file per platform. It will not require an installer, a system service, Node.js, a separate FFmpeg installation, or a cloud account.

The same executable will expose both a native UI and a command-line interface. Both interfaces will call the same application services, so tests and automation will exercise the same capture, encoder, signaling, and WebRTC paths as interactive use.

## 2. Goals

### Primary goals

- Capture a complete monitor or a single application window.
- Encode screen video with low latency.
- Play the stream in a normal browser through WebRTC.
- Allow several viewers to connect at the same time.
- Avoid re-encoding the screen for each viewer.
- Adapt quality independently for viewers when host resources permit.
- Run on Windows and Linux.
- Provide a native Rust control UI.
- Provide headless command-line operation for tests and automation.
- Distribute one executable file per platform.
- Display copyable local, LAN, and public viewer URLs.
- Let the user configure the HTTP port and WebRTC media ports.
- Bind to the local network by default so the host and LAN viewers can use the same displayed URL; retain loopback-only CLI mode for advanced users.

### Measurable targets

The project should measure these targets on a healthy local network rather than treat them as guarantees:

- First picture after a local viewer joins: under two seconds.
- End-to-end local video latency: target roughly 100 to 150 milliseconds.
- One encoded source shared with multiple viewers.
- No unbounded frame queue growth when a viewer slows down.
- A clean shutdown removes temporary runtime files in normal cases.
- A fresh Windows or Linux machine can launch the distributed artifact without installing application dependencies.

The latency target depends on capture timing, encoder settings, browser behavior, network conditions, and display refresh. The application must expose measurements so the team can identify the source of delay.

## 3. Initial scope exclusions

The first release will postpone these features:

- HTTPS and `wss://`.
- Cloud signaling or hosted rooms.
- User accounts.
- TURN relay infrastructure.
- Remote keyboard or mouse control.
- Recording and replay.
- Mobile viewers as a separate native application.
- Multiple simultaneous capture sources.
- Advanced adaptive streaming with several quality layers remains a follow-up milestone; its design is specified below.
- Audio capture and process-level audio exclusions remain a follow-up milestone; the design is specified below.

The architecture should leave room for these features without making them dependencies of the first release.

## 4. High-level architecture

```text
┌──────────────────────────────────────────────────────────────┐
│ Rust host application                                         │
│                                                              │
│  Native UI                                                   │
│    │ commands and events                                     │
│    ▼                                                         │
│  Application state and services                              │
│    ├── Capture backend                                       │
│    ├── Bounded frame pipeline                                │
│    ├── FFmpeg encoder process                                │
│    ├── Encoded media source                                  │
│    ├── WebRTC session manager                                │
│    ├── HTTP server                                            │
│    └── Socket.IO-compatible control sessions                 │
│                                                              │
└───────────────┬───────────────────────────┬──────────────────┘
                │ HTTP and WebSocket        │ WebRTC UDP media
                ▼                           ▼
        Vue/Vite static SPA              Browser <video>
```

The first release uses one shared encoded source. The current adaptive milestone adds a primary and a lower encoder variant, each shared by every viewer assigned to that quality group; it does not create one encoder per viewer. A single FFmpeg capture process converts the selected source to paced `yuv420p` frames and keeps only the newest complete frame for each encoder subscription. Each group encoder scales and encodes that shared source independently, so a slow group cannot build capture latency or produce an independently clocked test pattern.

The application has two network planes:

1. The control plane serves the viewer page, exchanges SDP and ICE messages, exposes status, and handles viewer admission.
2. The media plane carries the encoded video through WebRTC. WebRTC handles RTP, RTCP, ICE, DTLS, SRTP, congestion control, and browser playback.

The native UI starts the viewer server independently from media delivery. The server can admit and count viewers while the stream is idle; Start Stream enables media for existing sessions, and Stop Stream disables media without shutting down HTTP, WebSocket, or viewer admission.

The viewer is a Vue 3 + TypeScript single-page application compiled by Vite into static assets. The host application and its native control UI remain Rust-native; the production binary embeds the generated SPA and serves it at the token route without requiring Node.js at runtime.

WebRTC does not define a signaling transport, so the application uses a same-origin Socket.IO-compatible control endpoint. Socketioxide provides the Engine.IO polling-to-WebSocket upgrade, reconnect heartbeat, bounded packet buffers, acknowledgements, and per-socket state while Axum continues to serve HTTP and WebRTC signaling. See [Socketioxide](https://docs.rs/socketioxide/latest/socketioxide/), [MDN's signaling guide](https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API/Signaling_and_video_calling), and the [WebRTC peer connection documentation](https://developer.mozilla.org/en-US/docs/Web/API/RTCPeerConnection).

## 5. Provisional technology choices

| Area | First choice | Reason | Later alternative |
|---|---|---|---|
| Language | Rust | Shared core, native UI, platform integration, and server in one language | None planned |
| Async runtime | Tokio | Fits capture coordination, process I/O, HTTP, WebSockets, and WebRTC tasks | smol if a future dependency decision favors it |
| HTTP and signaling | Axum plus Socketioxide | One origin for the static SPA, Socket.IO-compatible control sessions, and WebRTC signaling; supports polling fallback and WebSocket upgrade | A lower-level Engine.IO layer or raw WebSocket protocol if compatibility requirements change |
| Viewer client | Vue 3, TypeScript, and Vite | Componentized UI, composable connection/media state, production static bundle, and local/offline packaging | Petite Vue for a deliberately no-build client |
| Browser media | `webrtc-rs` 0.20.x, pinned to an exact release | Rust WebRTC endpoint and media-track APIs | `str0m` or the WebRTC.rs SFU stack after a focused evaluation |
| Encoder integration | FFmpeg child process | Easy packaging boundary and access to software or hardware encoders | `ffmpeg-next` or a native media pipeline after profiling |
| Windows capture | `gdigrab` for monitors; WGC with XCap/PrintWindow fallback for windows | WGC isolates a selected window from occlusion; stable HWND identity survives enumeration reorder | `ddagrab` and native WGC picker |
| Linux capture | FFmpeg `x11grab` with XCap discovery | Direct X11 path with no raw frame copy in Rust | Direct XDG Desktop Portal plus PipeWire backend for Wayland |
| Desktop UI | `egui` and `eframe` | Rust-native, cross-platform, no webview runtime, suitable for a control panel | Slint for a more declarative and polished UI |
| Linux artifact | AppImage | One downloadable executable file with bundled user-space dependencies | Plain ELF release for controlled distributions |

The current `webrtc-rs` project recommends its 0.20.x line for new projects, but the API remains pre-1.0. The media layer must isolate the dependency and pin the version. See [webrtc-rs](https://github.com/webrtc-rs/webrtc).

## 6. Desktop UI plan

### UI responsibilities

The native UI will control the host application. It will provide:

- Start and stop controls.
- Capture-source selection.
- Monitor and window details.
- Resolution and frame-rate controls.
- Bitrate and encoder profile controls.
- Manual or adaptive quality mode.
- Fixed or automatic bitrate selection, including the effective current bitrate.
- Maximum adaptive quality groups: `Auto` or a fixed upper bound.
- Adaptive quality ceiling, latency preference, and current group assignment.
- Hardware/software encoder selection.
- Cursor inclusion setting.
- Local, LAN, and public viewer URLs.
- Copy buttons for every viewer URL.
- Current viewer count.
- Capture, encoder, WebRTC, and network status.
- Packet loss, round-trip time, bitrate, and latency diagnostics.
- Per-viewer quality, group membership, migrations, and adaptation reasons.
- Active group count and host CPU/GPU encoder budget.
- FFmpeg availability and encoder capability information.
- A licenses and build-information page.
- Persisted UI preferences under an OS-owned temporary-directory namespace, excluding access tokens.

### UI architecture

The UI event loop will send commands to the application state and receive snapshots or events in return. It will not read frames, wait for FFmpeg, perform network requests synchronously, or manage WebRTC state directly.

```text
User action
    ▼
UI command
    ▼
Application service
    ▼
Capture, encoder, signaling, or WebRTC task
    ▼
State event
    ▼
UI update
```

This separation keeps UI rendering from blocking the capture and media pipeline.

The command-line interface will use the same application services. It will not start a second implementation of capture, encoding, signaling, or WebRTC.

```text
Native UI ───────┐
                 ├── Application commands and events ── Core services
Command line ────┘
```

### Command-line interface

The CLI will provide a second interface to the same application services. It will support headless operation on machines without a desktop session and make repeatable testing possible.

The provisional command set is:

```text
instantlocalstream gui
instantlocalstream start
instantlocalstream list-sources
instantlocalstream status
instantlocalstream validate
instantlocalstream version
```

The application can launch the UI when the user runs it without a subcommand. The project should decide this default during Phase 0.

The `start` command should support options for:

- Capture source.
- Default LAN-capable binding with an advanced loopback CLI override.
- HTTP port.
- WebRTC UDP port or range.
- Codec, resolution, frame rate, and bitrate.
- Manual or adaptive quality mode.
- Fixed or automatic bitrate mode.
- Adaptive quality ceiling and maximum quality groups (`Auto` or a fixed value).
- Maximum viewers.
- Access token or token source.
- Duration or stop-after-viewer options for tests.
- Log format and log level.
- Headless mode.

Example automation command:

```text
instantlocalstream start \
  --source monitor:0 \
  --bind lan \
  --http-port 8080 \
  --media-ports 40000 \
  --codec vp8 \
  --width 1920 \
  --height 1080 \
  --fps 60 \
  --bitrate 14000000 \
  --json
```

The CLI should write human-readable logs to stderr and machine-readable status or event records to stdout when `--json` is active. The JSON output needs a versioned schema so scripts can depend on it.

The CLI should expose stable exit codes for invalid configuration, unavailable capture, FFmpeg failure, port binding failure, WebRTC failure, and clean shutdown. The exact numeric values belong in the CLI specification before implementation.

Configuration precedence should follow this order:

1. Command-line options.
2. Environment variables.
3. Portable configuration file.
4. Built-in defaults.

The CLI should avoid accepting long-lived secrets as plain command-line arguments because shells and process viewers can expose them. It should accept tokens through an environment variable, a file, or standard input.

The first release should keep `status` and process control local to the host. A local named pipe, Unix socket, or loopback control endpoint can support status queries later. The application should not expose a management API on the public HTTP port.

The current CLI writes a local runtime record under the OS temporary directory. `status --http-port PORT --json` reads that record and verifies the localhost control port, removing stale records after a crash or forced termination.

### Toolkit evaluation

The project should compare `egui/eframe` and Slint before committing to the UI layer.

`egui/eframe` fits a tool with forms, status panels, tables, buttons, and diagnostics. The official project documents native Windows and Linux support through `eframe`. [egui](https://github.com/emilk/egui)

Slint provides declarative markup and native desktop support for Windows and Linux. Its licensing options include GPLv3 and a royalty-free option with attribution, so the project needs a license decision before distribution. [Slint desktop documentation](https://docs.slint.dev/latest/docs/slint/guide/platforms/desktop/), [Slint licensing FAQ](https://slint.dev/faqs)

Iced remains a valid alternative for a strongly typed reactive UI. Its official guide explains the state, message, update, and view model. [Iced guide](https://book.iced.rs/)

Tauri should remain outside the first toolkit choice. It would offer a polished web-style interface, but it adds system webview dependencies such as WebView2 on Windows and WebKit-based dependencies on Linux. That conflicts with the single-file, no-install distribution goal. [Tauri prerequisites](https://tauri.app/start/prerequisites/)

### Native source selection

The UI will expose one shared action called `Choose source`. Each platform backend will decide how to fulfill it.

- Windows can use the Windows Graphics Capture picker.
- Linux Wayland can use the system ScreenCast portal.
- Linux X11 can use the portal when available and may offer an X11-specific fallback.

The Linux portal supports monitor and window source types and returns PipeWire streams after the user grants access. [XDG Desktop Portal ScreenCast](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)

## 7. Capture pipeline

The capture subsystem will expose a platform-independent source interface. FFmpeg will read the selected source directly and send encoded frames to Rust through stdout. Rust will keep source discovery, process lifecycle, timestamps, and error reporting around that process.

The source interface will carry:

- Pixel format.
- Width and height.
- Capture timestamp.
- Frame duration or frame-rate metadata.
- Cursor information when supported.
- Optional GPU-backed handle for a future low-copy path.

The first implementation should prioritize correct source selection and stable FFmpeg input. The pipeline should leave room for GPU texture paths later.

The encoded media path should use a bounded queue. When WebRTC falls behind, the application should drop stale encoded frames and preserve the newest frame. A growing queue would convert a temporary slowdown into visible latency.

### Windows

The current Windows path uses FFmpeg `gdigrab` for monitor capture and continuous Windows Graphics Capture for an application window, so an occluding foreground window does not become part of the selected window stream. The source cards store the selected window's native HWND, while the changing enumeration index is only a legacy/display hint. WGC can pause when the target is minimized, so the capture loop tries XCap/PrintWindow at a reduced rate and preserves the last valid frame when the application cannot render while minimized. The next UX improvement is the native Windows Graphics Capture picker. [FFmpeg devices documentation](https://www.ffmpeg.org/ffmpeg-devices.html), [Microsoft screen capture documentation](https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture)

### Linux

The Linux backend should use FFmpeg's `x11grab` input when X11 is available. Wayland remains a permission-controlled environment. The next Wayland backend should request a ScreenCast portal session, let the desktop environment present the source picker, and consume the resulting PipeWire stream.

The backend should report missing or unavailable components in user-facing language:

- XDG Desktop Portal unavailable.
- PipeWire unavailable.
- User cancelled source selection.
- The chosen window or monitor disappeared.
- The desktop environment does not expose the requested source type.

The project should document the supported desktop environments instead of promising identical behavior on every Linux distribution.

## Audio capture and process exclusions (follow-up)

Implementation status: audio configuration, default exclusions, flexaudio-backed native capture, Opus encoding, WebRTC negotiation, browser playback, and audio status reporting are implemented. Full process exclusion validation and live audio reconfiguration remain follow-up validation work.

Audio is disabled by default. The native UI should expose source-specific controls:

- For a selected monitor: `Capture system audio`.
- For a selected window: `Capture window audio`.
- When audio is enabled: an editable list of applications to exclude.

The three source modes remain mutually exclusive. Audio settings should be applied by restarting only the audio capture/encode pipeline; video, HTTP, signaling, and existing viewer sessions should remain alive where renegotiation permits.

### Windows

System audio should use Windows endpoint loopback. Window audio should identify the selected window's process tree and use application loopback when available. Windows exposes include-tree and exclude-tree process loopback modes, but the API targets a process tree rather than an arbitrary list of applications and requires Windows 10 build 20348 or newer. [Windows process loopback modes](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ne-audioclientactivationparams-process_loopback_mode), [Microsoft application loopback sample](https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample/)

Supporting several exclusions requires a small native mixer or a session-aware capture graph: capture included render sessions, mix them to a common 48 kHz format, and feed the resulting PCM to the Opus encoder. A single FFmpeg loopback input cannot reliably express “system audio except Discord, WhatsApp, and Telegram.”

### Linux

On PulseAudio, system audio can be read from a sink's monitor source. Per-application exclusion requires enumerating sink inputs and either capturing the included streams or mixing the monitor stream with explicit subtraction, with the included-stream mixer being the safer design. On PipeWire, stream metadata such as `application.process.id` can be used to select or exclude application streams directly. [PulseAudio monitoring](https://wiki.freedesktop.org/www/Software/PulseAudio/Documentation/Developer/Clients/WritingVolumeControlUIs/), [PipeWire audio capture](https://docs.pipewire.org/audio-capture_8c-example.html), [PipeWire application process ID](https://docs.pipewire.org/group__pw__keys.html)

Wayland desktops should use the portal/PipeWire backend rather than assuming an X11 or PulseAudio monitor is available. The UI must report when the desktop does not expose per-application audio metadata.

### Default exclusions

The application can ship a user-editable default exclusion list for common communication apps, using executable/package identity rather than window titles:

- Discord
- WhatsApp
- Telegram
- Microsoft Teams
- Zoom
- Skype
- Slack

The UI should offer an `Add default ignore list` action that adds only currently running matches, plus an `Others` list for individually adding discovered processes. These entries should be visible and easy to remove. Matching must be platform-specific and should not silently exclude unrelated applications with similar names.

### WebRTC media

The host should add one Opus audio track at 48 kHz alongside the video track. The browser viewer should attach it to the existing media stream and keep audio muted until the user or browser autoplay policy permits playback. Audio statistics and capture errors should be reported separately from video status. FFmpeg may remain the software Opus encoder boundary, but the native audio capture/mixer must own process selection and timestamps.

## 8. Encoding plan

FFmpeg will run as a long-lived child process. It will capture the selected source, encode it, and write encoded access units to stdout for Rust to packetize into WebRTC.

The WebRTC layer should own RTP packetization, RTCP feedback, encryption, and network pacing. FFmpeg should produce a WebRTC-compatible elementary stream rather than an HLS or MP4 stream.

### Initial codec policy

The working implementation supports VP8 and VP9 through FFmpeg IVF output plus an experimental H.264 Constrained Baseline path through FFmpeg Annex B output. VP8 is the compatibility default; VP9 is an optional bandwidth-saving codec with higher CPU cost. The H.264 encoder uses `libx264`, `yuv420p`, repeated parameter headers, disabled sliced threads, and a short ordered NAL queue so an SPS/PPS sequence cannot be discarded before its picture. It selects level 3.1 at up to 720p/30 and level 4.2 for higher profiles. Although the host produces H.264 access units, the current WebRTC packetization path did not deliver a first decodable frame in the latest Chromium UI test, so H.264 remains explicit-only pending repair. AV1 remains a later experiment because low-latency encoding and device coverage are less predictable. Fully compliant WebRTC browsers must support VP8 and H.264 Constrained Baseline for video. [MDN WebRTC codec guide](https://developer.mozilla.org/en-US/docs/Web/Media/Guides/Formats/WebRTC_codecs)

The native UI presents YouTube-style quality presets: source, 144p, 240p, 360p, 480p, 720p HD, 1080p HD, 1440p HD, 2160p 4K, and 4320p 8K. Frame-rate presets are source, 5, 10, 24, 30, 60, 75, and 120 FPS. Source is the default for both, and the selected source's actual resolution/FPS are shown in the Source menu entries. Presets above the selected source's limits are hidden; Test pattern has no external source limit and defaults to 1080p/60. Manual changes may initially restart only the FFmpeg process while the HTTP server and WebRTC signaling service remain available. Auto mode is the user-facing name for adaptive quality and is required to use multiple shared encoder processes/quality groups; the current one-group bitrate controller is transitional until keyframe handoff or pre-negotiated layer support is complete.

The encoder abstraction should expose:

- Codec.
- Resolution.
- Frame rate.
- Bitrate.
- Keyframe interval.
- Hardware or software mode.
- Preset or speed setting.
- Low-latency mode.
- Pixel format.

Low-latency profiles should avoid B-frames, lookahead, excessive buffering, and long keyframe intervals. The exact FFmpeg options will depend on the encoder selected on each machine.

### Automatic quality and runtime reconfiguration

The configuration model should distinguish the requested policy from the effective encoder profile:

- Quality mode: manual or adaptive.
- Bitrate mode: fixed or automatic.
- Resolution and frame-rate ceilings for adaptive mode.
- Minimum acceptable adaptive quality, where the host can sustain it.
- Maximum adaptive quality groups: `Auto` or a fixed integer greater than or equal to one.
- Latency preference and host resource budget.

Automatic bitrate should begin with a codec-, resolution-, frame-rate-, and content-aware recommendation, then adapt from measured encoder and viewer conditions. It is a starting target, not a guarantee of available network capacity. The current readable-screen floor is 14 Mbps for 1080p at 30 or 60 FPS; lower groups may be reduced only after sustained credible congestion.

Adaptive decisions should sample metrics continuously but change settings over smoothed multi-second windows. Downshifts should happen faster than upshifts, and every change should have a cooldown and hysteresis threshold to avoid oscillation.

The target media architecture is:

```text
Persistent capture at source quality
              │
       bounded frame bus
              │
     scheduler and scaler
              │
       persistent encoder
              │
       encoded access units
```

FPS changes can be implemented by scheduling or dropping source frames before encoding. Bitrate changes should use an encoder runtime-control API when available. Resolution changes should use a live scaler or a new encoder context and begin with a keyframe. Codec changes require a new negotiated codec path.

When a backend cannot reconfigure safely in place, the host should start a candidate encoder beside the current encoder, wait for a keyframe, and switch the encoded sample source while preserving the WebRTC session, codec, SSRC, and timestamp continuity. The old encoder can then be drained and stopped. A short keyframe transition is acceptable; a viewer reconnect or full server restart is not.

### FFmpeg process lifecycle

The host should:

1. Locate the embedded or configured FFmpeg binary.
2. Start one process per active stream.
3. Read stderr for diagnostics without allowing the pipe to fill.
4. Detect process failure and surface a useful error.
5. Stop the process after all WebRTC sessions close.
6. Wait for process exit before deleting its temporary files.

The project should keep the FFmpeg boundary behind an encoder trait. A later `ffmpeg-next` implementation can replace the child process if profiling shows that process I/O or copies add unacceptable latency.

## 9. WebRTC and multiple viewers

The first topology will use one WebRTC connection per viewer while sharing one encoded source:

```text
One capture source
        │
    One encoder
        │
   Encoded source
    ┌───┼───┐
    ▼   ▼   ▼
 Viewer A  Viewer B  Viewer C
```

The host should not encode a separate stream for each viewer. Each viewer receives an independent WebRTC transport, so the host upload cost grows with the number of viewers and the selected bitrate.

### Adaptive quality groups

Implementation status: adaptive mode supports a one-to-four encoder-slot budget. Before its first offer, a client streams a same-origin, incompressible 1 MiB probe body for at most five seconds and reports received bytes, first-body-byte latency, throughput, or timeout through the control session. The video area shows the probe’s received bytes and live rate, then the selected starting-quality step, so the page does not appear stalled while it has no video track. The host uses that hint to select an initial safe group, then relies on WebRTC media telemetry for all later decisions. The host continuously evaluates only the most recent 15 seconds of visible-tab media telemetry, requires at least five samples, and gives an assignment a 30-second settle period before a downgrade or upgrade decision. A downgrade normally requires sustained transport evidence—credible incoming capacity, packet loss, high jitter, or high RTT. Playback counters use a separate conservative rule: repeated freezes can trigger a downgrade, but ordinary dropped frames are diagnostic only and must exceed an extreme rolling threshold before they do. Those counters otherwise defer an upgrade. A sustained degraded window moves a viewer down one profile; a later stable headroom window promotes it one profile. The migration is currently performed by emitting `group.assignment` and recreating that client's WebRTC session, so a brief visible transition is expected. The client correlates each sampled rendered RTP timestamp with the host's retained source-capture timestamp; only browsers lacking rendered-frame RTP metadata use the playout/decode fallback estimate.

`max_quality_groups` is a hard ceiling from one through four. The application starts the primary group and retains the remaining slots only as budget; it does not start their encoders. A bootstrap or sustained-degradation decision may configure and start a stopped slot. Each profile begins with a lower resolution, FPS, and bitrate than the preceding profile, while a timed-out or very slow probe can start the selected safe slot below its nominal ceiling. The controller periodically aggregates each group's visible members and lowers bitrate under moderate pressure, then FPS and resolution under sustained pressure; a stable group recovers bitrate and may climb back toward its group ceiling. Empty non-primary groups drain after a grace period, and near-identical groups can merge by moving their clients before the redundant encoder drains.

### Dynamic group budget and clustering

The configured group count is an upper resource budget, never a startup instruction. The host begins with only the source/primary profile required for the current viewers, then creates a profile only when the rolling telemetry forms a cluster that cannot be served by a small adjustment to an existing group. The current implementation keeps four dormant slots and applies a changed budget live: reducing it moves affected viewers to the primary active group, stops the excess encoders, and asks viewers to renegotiate.

Each group has an explicit lifecycle: `active`, `draining`, or `stopped`. An empty non-primary group enters draining, then stops its encoder and releases its source-bus subscription after a grace period. Only active groups appear in client status. A group can merge into its nearest neighbor when their codec, resolution, frame rate, and bitrate fall within a defined similarity threshold.

The controller first attempts a bounded bitrate/FPS/resolution adjustment. It creates a new group only when the expected improvement for a persistent outlier or cohort exceeds the encoder/capture resource cost. Conversely, it migrates a single outlier down before reducing the quality of a larger stable group. The active variant key is `codec + profile`; codec selection therefore shares the same group budget and lifecycle rather than multiplying every quality group by every codec.

The implemented media topology is one target-quality capture source that publishes bounded raw/source frames to encoder-only variants. The test pattern is generated once at the primary target size and frame rate; lower profiles are scaled/encoded from those same frames. The source reader paces raw frames to its configured rate, which prevents an unbounded `testsrc2` producer from making the synthetic stream appear to run faster than real time.

The adaptive follow-up should replace the single-source topology with a shared variant topology:

```text
One capture source
        │
  bounded frame bus
   ┌────┼────┐
   ▼    ▼    ▼
Group A  Group B  Group C
encoder  encoder  encoder
   │      │      │
   └──────┴──────┴── shared group tracks
          │
    per-viewer assignment
```

Each quality group owns one encoder profile and one shared media variant. A profile may change its bitrate continuously within limits and may change resolution or frame rate in discrete steps. Viewers should be grouped by sustained performance, not by a single noisy measurement.

The controller should maintain two separate decisions:

1. Group adaptation: adjust a group's bitrate, frame rate, or resolution when the condition affects a meaningful portion of its members.
2. Viewer assignment: move an outlier to the nearest existing group, or create a new group when the expected quality improvement justifies the encoder cost.

Quality reductions should be fast; quality increases and migrations upward should require sustained stability. Groups should merge when their profiles and member conditions become similar. A minimum benefit threshold, migration cooldown, and maximum active-group count should prevent fragmentation into one group per viewer.

The maximum group setting is a hard upper bound. `Auto` should calculate a safe bound from source complexity, codec, viewer count, measured CPU/GPU headroom, encoder backlog, memory, and outgoing bandwidth. The actual active group count may be lower than that bound and should change at runtime. Increasing the calculated budget may warm up a new variant; reducing it should migrate viewers and drain the least useful encoder before stopping it.

The resource manager must treat the group count as only one constraint: a 4K60 encoder can cost more than several low-resolution encoders. It should reserve capacity for capture, WebRTC, and the operating system and should stop creating variants when the measured or predicted cost exceeds the budget.

All groups should normally use the same negotiated codec so a viewer can migrate without a new ICE connection or browser session. The preferred migration mechanisms are pre-negotiated simulcast/layered encodings or replacing the server-side sender track at a keyframe. Changing codec still requires renegotiation. The current one-track viewer path should be extended only after browser interoperability tests cover layer selection, resolution changes, keyframes, and timestamp continuity.

The host can later evolve into an SFU-style forwarder if the viewer count or packetization cost requires it. The WebRTC.rs ecosystem includes a Rust SFU that forwards media without decoding and re-encoding. [WebRTC.rs SFU announcement](https://webrtc.rs/blog/2026/07/13/announcing-sfu-v0.20.0-rc.3)

### Signaling messages

The signaling protocol should carry:

- Session identifier.
- Viewer identifier.
- Join request.
- SDP offer.
- SDP answer.
- Trickle ICE candidate.
- Connection state.
- Leave request.
- Restart request.
- Error information.

The server should route messages and validate their size and session membership. It does not need to implement a second media protocol. The current control event names are:

- `session.ready`: protocol version, media capabilities, and the initial status snapshot.
- `status.snapshot`: a complete current status response, also used after reconnect or an explicit `status.request`.
- `status.changed`: a pushed status update for stream state, viewer count, settings, or media errors.
- `status.request`: a client request acknowledged with a current snapshot.
- `control.ping`: an application-level RTT probe acknowledged by the server; the Socket.IO transport heartbeat remains independent.
- `viewer.stats`: bounded browser WebRTC metrics associated with the stable viewer identifier.

The browser must keep a stable `clientId` across control reconnects. The host must keep the control socket identity separate from the WebRTC peer identity so a stale disconnect cannot tear down a newer session. Per-client data belongs in a session registry keyed by `clientId`, with bounded/coalesced status delivery and cleanup on disconnect.

The checked-in `web/` project is built with `npm ci` and `npm run build`; `scripts/build-web.ps1` and `scripts/build-web.sh` provide the reproducible frontend build used by portable packaging. The Rust server embeds `web/dist` and serves hashed assets with long-lived cache headers. No CDN or Node runtime is required by the packaged application.

### Diagnostics

Each viewer session should expose:

- Connection state.
- ICE candidate pair.
- Local or remote address class.
- Round-trip time.
- Jitter.
- Packets lost.
- Sent bitrate.
- Frames sent and dropped.
- Last successful keyframe request.
- Current quality group and effective resolution, frame rate, and bitrate.
- Group migration state and the reason for the last adaptation decision.

Each quality group should expose:

- Active member count.
- Effective encoder profile and target bitrate.
- Encoder CPU/GPU time and backlog.
- Aggregate packet loss, RTT, jitter, queue pressure, and frame drops.
- Predicted cost of retaining or creating the group.

The browser can provide WebRTC statistics through `RTCPeerConnection.getStats()`. The host should combine those values with capture and encoder timestamps to estimate where latency accumulates.

Viewer statistics should be reported over the existing Socket.IO-compatible control session at a bounded interval. The host should combine browser-reported inbound statistics with server-side RTP/RTCP statistics and per-viewer queue drops. Missing browser fields must not be treated as failures; the controller should use the signals available for that browser.

The browser now reports one-second media-path samples: observed inbound bitrate, candidate-pair RTT, packet loss, jitter, optional available incoming bitrate, rendered-video dropped frames, freeze count, average jitter-buffer delay, average decode time, and page visibility. Before the first WebRTC offer it also runs one fresh, same-origin streaming HTTP probe with `Cache-Control: no-store`, identity encoding, an incompressible body, a 1 MiB byte cap, and a five-second deadline. The client counts `ReadableStream` chunks as they arrive, reports first-body-byte latency and observed throughput, and surfaces that progress inside the empty video area. This probe is only an initial placement hint because its TCP path can differ from WebRTC UDP media. Both the client display and host controller retain only a rolling 15-second window. Drop and freeze counters are converted to per-sample deltas, so old playback incidents expire naturally. The client labels this interval explicitly and uses `getVideoPlaybackQuality()` when the browser exposes it, falling back to WebRTC inbound stats otherwise. Deadline-based host pacing accounts for encode/packetization time rather than sleeping an additional whole frame duration, preventing a slowly accumulating source-side latency debt. VPx lookahead and alternate-reference frames are disabled, encoder packets are flushed, and any output whose submitted source frame is older than 250 ms is discarded without pacing. If encoded frames remain at least 750 ms behind for eight consecutive outputs, the affected encoder is restarted against the latest shared source frame; this clears FFmpeg-internal backlog that output dropping alone cannot remove, and the recovery count is exposed in status. Catch-up starts at 100 ms of current playout-buffer delay and increases playback speed more aggressively at larger delays. The status correlates a rendered frame's RTP timestamp to the host capture timestamp and reports capture-to-display delay directly, with capture-to-receive and receiver-to-display components plus clock uncertainty. The sender-timeline/jitter/decode calculation is retained only as a labeled fallback and no longer adds half RTT a second time. Assigned and group bitrates are explicitly labeled encoder targets, while measured receive bitrate is observed RTP traffic and may be lower for simple content. Automatic bitrate now uses codec-aware pixels-per-frame-rate targets plus a readable-screen floor of at least 14 Mbps for 1080p at 30 or 60 FPS, rather than the prior roughly 3.7 Mbps target. The controller gives a new assignment 30 seconds to settle and lowers a healthy group after corroborated transport congestion, repeated freezes, or an extreme 15-second dropped-frame count; ordinary drops are diagnostics and only defer an upgrade. The control-session RTT is useful for control-path health, but it must not be used as a substitute for WebRTC media capacity. When the browser exposes `availableIncomingBitrate`, read it only from the selected/nominated candidate pair and reject values that contradict observed delivery; otherwise react to sustained delivered-bitrate, loss, jitter, and capacity evidence.

The client renders the per-sample dropped-frame deltas from that 15-second window as a compact time-series chart. This makes recent bursts visible while naturally removing incidents that have aged out.

Host settings and per-client assignments are revisioned Socket.IO snapshots. Each client applies only newer `stream.settings` revisions, so quality, frame rate, codec, target bitrate, and group labels remain synchronized with the host even when bootstrap, adaptation, or reconnect messages arrive close together. Authoritative snapshots also carry a media-session generation: a host start or an audio on/off transition causes the viewer to re-offer only when its current WebRTC m-lines no longer match the host, which keeps the browser's audio controls and the actual delivered tracks aligned without reconnecting for routine telemetry.

The viewer measures a control-path round-trip time through an acknowledged control event and reads WebRTC candidate-pair RTT and inbound video jitter when the browser exposes those values. For capture-to-display timing, each outbound viewer track uses an explicit RTP timestamp and retains a bounded map to the raw source frame's wall-clock capture time. `requestVideoFrameCallback()` supplies the RTP timestamp and expected display time for the exact rendered frame; a Socket.IO acknowledgement returns its host capture timestamp and refines the clock offset. The result is smoothed and displayed with half-round-trip clock uncertainty.

### Live-edge policy

Low latency takes priority over smooth playback of stale frames. Every host-side viewer queue has capacity one and is overwritten by the newest encoded sample before write, so server-side backpressure drops obsolete access units. The viewer sets `RTCRtpReceiver.playoutDelayHint` to zero when the browser supports it and seeks to a browser-exposed live edge when one exists. When recent jitter-buffer delay exceeds the browser-reported minimum by a small threshold, the player temporarily raises `playbackRate` from 1.0× up to 1.2× and resets it as soon as the excess clears. A normal WebRTC `MediaStream` is usually not seekable, so a true client-side jump is not universally available; the server queue policy, bounded catch-up rate, browser jitter-buffer hint, and measured capture-to-display delay are the portable controls for keeping the viewer near live.

The viewer must display its assigned quality group, effective profile, group state, assignment reason, and synchronization mode. The control events are `group.assignment` for a targeted reassignment and the existing status snapshots for the available group list.

## 10. Tokenized HTTP and control-plane deployment

The first release will serve the viewer page from the host application at a token path:

```text
Local:   http://127.0.0.1:PORT/TOKEN
LAN:     http://LAN_IP:PORT/TOKEN
Public:  http://PUBLIC_IP:PORT/TOKEN
```

The viewer uses a same-origin Socket.IO-compatible control session for status, diagnostics, session lifecycle, and future commands. The client starts with HTTP polling and upgrades to WebSocket when available. The viewer does not call browser screen-capture APIs because the native application owns capture.

The default token contains twelve ASCII letters or digits. The application also supports `--port N` as a convenience mode that uses the same numeric port for HTTP/TCP and the shared WebRTC UDP mux. All viewer sessions share one UDP port; the configured viewer cap is therefore independent of the port count.

HTTP mode has a security cost:

- The page can be modified in transit.
- Control signaling is visible and modifiable in transit.
- A token in a URL can leak through screenshots, chat logs, browser history, or referrer behavior.
- WebRTC media still uses its own encrypted transport, but that does not protect the HTTP page or signaling channel.

The first release should show a warning when the user enables internet sharing and require a random stream token. HTTPS and `wss://` can wrap the same control plane in a later release.

## 11. LAN and internet connectivity

### Localhost mode

Bind the HTTP server and WebRTC media to loopback. This mode should require no router changes and should start by default.

### LAN mode

Bind the HTTP server to a selected or all local interfaces. Display the reachable LAN URL and explain that the viewer must share the same reachable network.

The application should detect common issues and show them:

- Guest Wi-Fi or client isolation.
- VPN interface selection.
- Multiple network adapters.
- Local firewall blocking the port.
- Viewer using a different subnet.

### Public port-forwarding mode

The user will configure:

- HTTP/TCP port.
- One shared WebRTC UDP port.
- Public IPv4 address or hostname.
- Optional advertised address override.

The router must forward both the HTTP port and the WebRTC media port or range. Forwarding the HTTP port alone can load the page while leaving the media connection unreachable.

The application can add STUN configuration later to discover server-reflexive candidates. TURN remains an optional fallback for users whose networks do not permit direct connections. Direct forwarding should remain the preferred path for latency.

## 12. Public IP discovery and share links

The UI should separate address discovery from connectivity verification. Public-IP discovery is an internal implementation detail: the application queries its fixed primary/fallback services automatically, but it should not expose those service URLs or provide copy/refresh controls for them. The UI may still show the generated viewer URL; manual advertised-host override remains a CLI-only escape hatch.

```text
Local URL:   [http://192.168.1.20:8080/Ab12Cd34Ef56] [Copy]
Public URL:  [http://203.0.113.10:8080/Ab12Cd34Ef56]  [Copy]
             [Test instructions]
```

The implementation queries `https://ipv4.wtfismyip.com/text` internally when the native host UI starts. The service URL is not part of the user-facing UI. The application should:

- Apply a short timeout.
- Trim the response.
- Validate strict IPv4 syntax.
- Cache the result.
- Avoid polling; one lookup at startup is sufficient for the initial release.
- Use WTFIsMyIP as the primary service and a fixed fallback service if it fails.
- Allow a manual CLI override if both services fail.
- Explain that the result may represent a VPN, proxy, carrier NAT, or upstream router.
- Mark the URL as unverified until an external connection succeeds.

The service documents a one-request-per-minute limit for automated use and provides no availability guarantee. [WTF is my IP automation policy](https://wtfismyip.com/automation)

An external IP lookup cannot prove that port forwarding works. If the ISP uses CGNAT, the displayed address may not accept inbound connections even when the user configures the local router.

## 13. Portable single-file distribution

### Release artifacts

The release pipeline should produce:

- `InstantLocalStream-windows-x86_64.exe`
- `InstantLocalStream-linux-x86_64.AppImage`

The project can add ARM builds after the x86-64 paths work.

### Embedded resources

The build process should embed:

- The viewer page and JavaScript bundle.
- UI icons and fonts.
- A pinned FFmpeg binary for the target platform.
- FFmpeg DLLs or shared libraries when the selected build needs them.
- License texts and build metadata.

The host can serve the viewer assets from memory. It should extract FFmpeg only when streaming starts.

### Temporary extraction

The runtime should create a unique application-owned directory under the operating system temporary path:

```text
<temp>/InstantLocalStream/<random-run-id>/
```

The directory should contain a manifest with the application version and embedded asset hashes. The application should use restrictive permissions where the platform supports them and should never build paths from viewer input.

Normal shutdown should stop all viewers, stop FFmpeg, wait for child processes, remove the run directory, and release network sockets.

Startup cleanup should scan only the application-owned temporary namespace and remove stale directories left by older runs. The cleanup should use age and manifest checks so it cannot remove unrelated temporary files.

The program cannot clean files after a power loss, forced termination, or operating-system crash. Startup cleanup provides the recovery path.

### Linux portability boundary

AppImage can bundle user-space application dependencies and run as one downloadable file. It cannot bundle the Linux kernel, graphics drivers, PipeWire, XDG Desktop Portal, or every desktop environment. The application should check these dependencies and explain failures instead of hiding them.

Build the AppImage against the oldest supported Linux distribution so newer systems can run it. [AppImage concepts](https://docs.appimage.org/introduction/concepts.html)

### FFmpeg redistribution

The release process must record the exact FFmpeg source revision, configure options, external libraries, and licenses. FFmpeg uses LGPL for most code but includes optional GPL components and other licensing conditions. The project should choose a build configuration before distributing binaries and should include a licenses screen and source-code notice. [FFmpeg legal guidance](https://ffmpeg.org/legal.html)

The project should also evaluate codec patent obligations before publishing a commercial build. This document does not provide legal advice.

The current build embeds the Vue/Vite viewer assets into the Rust binary. `scripts/build-web.ps1` or `scripts/build-web.sh` must run before `cargo build`; the portable build scripts run it automatically. A release build can embed FFmpeg by setting `ILS_FFMPEG_PATH`; the runtime extracts that executable into an application-owned temporary directory and removes it during normal shutdown.

## 14. Suggested project boundaries

The source tree should keep platform and media concerns separate:

```text
shared Rust crates or modules
├── app-core          State, commands, events, configuration
├── capture           Platform-independent frame and source interfaces
├── encoder           FFmpeg process and encoder profile abstraction
├── media             Encoded access units and timestamps
├── adaptive          Viewer metrics, quality-group assignment, and resource budgeting
├── variants           Shared quality encoders, layer selection, and keyframe handoff
├── transport         WebRTC sessions, tracks, and viewer lifecycle
├── signaling         WebSocket message protocol and admission rules
├── http-server       Static viewer assets and HTTP routes
├── ui                Native control UI and view model
├── cli               Command parsing, headless mode, and automation output
└── packaging         Embedded assets and runtime extraction

platform modules
├── windows-capture   Windows Graphics Capture backend
└── linux-capture     Portal and PipeWire backend
```

The exact Cargo workspace layout can change during implementation. The dependency direction should remain stable: UI and HTTP code send commands to application services, while capture and media services do not depend on UI widgets.

## 15. Development phases

### Phase 0: Decisions and risk spikes

Deliverables:

- Written target-platform policy for Windows and Linux.
- UI toolkit comparison between egui/eframe and Slint.
- Capture feasibility notes for Windows, Wayland, and X11.
- FFmpeg build and licensing decision.
- WebRTC crate version decision.
- Latency measurement method.
- Single-file packaging design.
- CLI subcommands, exit codes, configuration precedence, and JSON schema.

Exit criteria:

- The team can state which dependencies remain system-provided on each platform.
- The team accepts the HTTP-only security model for the first release.

### Phase 1: Portable application shell

Deliverables:

- Native window with start/stop controls.
- Shared application state and command/event model.
- Embedded viewer assets.
- `--help`, `--version`, and configuration validation commands.
- A headless `start` command with a controlled shutdown path.
- One-file Windows build.
- Initial Linux AppImage build.
- Temporary extraction and startup cleanup design.

Exit criteria:

- Both artifacts launch without an installer.
- The application can show a local viewer URL.

### Phase 2: One-viewer video path

Deliverables:

- One monitor capture backend on Windows.
- One Linux capture path, preferably the portal path.
- FFmpeg VP8 and VP9 encoding through IVF for the first browser media path.
- One WebRTC browser viewer.
- Deterministic `test:0` source for browser and CI validation.
- A headless start path that uses the same media services as the UI.
- Localhost HTTP and Socket.IO-compatible control signaling.
- Capture-to-browser latency measurement.

Exit criteria:

- The viewer receives stable video.
- The pipeline keeps bounded queues.
- The application reports capture, encoder, and WebRTC failures.

### Phase 3: Multiple viewers

Deliverables:

- Viewer admission and session IDs.
- One WebRTC connection per viewer.
- Shared encoded source.
- Multi-viewer validation using one shared UDP port.
- Join, leave, reconnect, and stream-stop handling.
- Viewer count and per-viewer statistics.

Exit criteria:

- Several viewers can watch the same stream.
- One slow viewer cannot grow the global capture queue.
- The host stops cleanly after the last viewer leaves.

### Phase 4: Capture and encoder controls

Deliverables:

- Window selection.
- Whole-monitor selection.
- Cursor setting.
- Resolution, frame-rate, bitrate, and keyframe controls.
- Hardware/software encoder selection.
- VP8 fallback where H.264 is unavailable.

Exit criteria:

- The UI prevents unsupported combinations.
- The stream restarts or renegotiates with clear status when settings change.

### Phase 5: Adaptive quality and viewer groups

Deliverables:

- Manual and adaptive quality modes.
- Fixed and automatic bitrate selection with effective-value reporting.
- Automatic bitrate recommendations based on codec, source resolution, frame rate, and content behavior.
- Persistent capture and encoder control path, or a candidate-encoder keyframe handoff when live reconfiguration is unavailable.
- Per-viewer browser and server performance metrics sent to the adaptation controller.
- Shared quality variants with one encoder per active quality group rather than one encoder per viewer.
- Dynamic viewer assignment between quality groups without ICE or browser-session reconnects.
- Group profile adaptation with smoothed sampling, hysteresis, cooldowns, fast downshifts, and slower upshifts.
- Group split, merge, drain, and reuse behavior.
- Maximum quality groups setting with fixed values and a resource-aware `Auto` mode.
- Host resource budgeting for CPU/GPU, memory, encoder backlog, and aggregate outgoing bitrate.
- UI and CLI status for active groups, group membership, effective profiles, resource usage, and adaptation reasons.

Exit criteria:

- Maximum groups set to one behaves as a single global adaptive stream.
- `Auto` chooses a safe group budget and never creates more active encoders than the calculated or configured limit.
- Similar viewers share one quality variant.
- A persistent outlier can move to another group without an ICE restart or viewer-page reconnect.
- A group changes profile without repeatedly restarting FFmpeg or causing unbounded viewer latency.
- Groups merge when their profiles are similar and drain safely when the active-group budget shrinks.
- One slow viewer does not lower the quality of a larger stable group when another suitable group is available.
- Encoder overload causes controlled merging or downshifting instead of runaway process creation.
- The controller records enough evidence to explain every group creation, migration, profile change, and rollback.

### Phase 6: LAN sharing

Deliverables:

- Bind-address selection.
- Equivalent CLI options for LAN mode.
- LAN URL generation.
- Firewall guidance.
- Random access token.
- Viewer limit.
- Diagnostics for interface and candidate selection.

Exit criteria:

- A second device on a normal home LAN can connect with the displayed URL.

### Phase 7: Public port-forwarding helper

Deliverables:

- Configurable HTTP port.
- Configurable single UDP media port.
- Equivalent CLI options for public mode.
- Public IP lookup and manual override.
- Public URL generation.
- Copy buttons.
- Port-forwarding instructions.
- CGNAT and VPN warnings.

Exit criteria:

- A user with a reachable public IPv4 address can follow the instructions and connect from an external network.
- The UI distinguishes “IP found” from “stream verified.”

### Phase 8: Release hardening

Deliverables:

- Windows and Linux clean-machine tests.
- Crash-recovery cleanup.
- FFmpeg licenses and source notices.
- Application licenses and acknowledgements.
- Versioned build manifest.
- Versioned CLI and JSON-output contract.
- Signed Windows executable, if the project obtains a signing certificate.
- Release notes with supported OS and desktop environments.

Exit criteria:

- The release artifacts launch on the declared support targets.
- The team can reproduce each bundled FFmpeg binary.
- The project documents known network and Linux desktop limitations.

## 16. Testing plan

### Unit tests

- Configuration validation.
- Viewer-token generation and comparison.
- Signaling message serialization.
- Session lifecycle state transitions.
- Frame timestamps and queue behavior.
- Encoder profile validation.
- Automatic bitrate recommendation and profile-bound validation.
- Adaptive metric smoothing, hysteresis, cooldown, and downshift/upshift decisions.
- Viewer-to-group assignment, outlier detection, group split/merge, and group-budget decisions.
- `Auto` quality-group budget estimation and resource-cost calculations.
- Encoder handoff keyframe and timestamp-continuity logic.
- Public IP response parsing.
- Temporary-directory naming and stale-run detection.
- CLI argument parsing and configuration precedence.
- Exit-code mapping.
- JSON event schema validation.

### Integration tests

- HTTP page loads from localhost.
- Socket.IO-compatible control signaling completes an offer/answer exchange.
- A headless browser receives a WebRTC track.
- One encoder feeds multiple viewers.
- Two isolated browser contexts receive the same encoded source.
- Multiple viewers with similar conditions share one quality variant.
- Heterogeneous viewers are assigned to different variants.
- Viewer migration between variants does not require ICE or page reconnect.
- The configured group maximum is never exceeded.
- `Auto` group budgeting reacts to measured encoder cost and host headroom.
- Groups split, merge, and drain without orphaned encoders or viewer sessions.
- A group changes bitrate or profile through live control or keyframe handoff.
- A slow outlier does not lower a stable group's profile when another variant is available.
- Encoder overload triggers controlled merging or downshifting.
- Viewer disconnects do not terminate other viewers.
- FFmpeg failure produces a visible error.
- The application restarts a failed encoder.
- The CLI returns documented exit codes.
- The CLI emits valid machine-readable events when requested.

### Latency tests

The test harness should use a known changing visual source or a timestamp overlay. It should record:

- Capture timestamp.
- Encoder output timestamp.
- First network send timestamp.
- Browser decoded-frame timestamp.
- Displayed-frame timestamp where the browser exposes it.

Run tests at several resolutions, frame rates, codecs, viewer counts, and active-group limits. Record bitrate, packet loss, jitter, queue depth, migration time, group count, encoder handoff gaps, and CPU/GPU use with each latency result. Include cases where viewers have deliberately different bandwidth, RTT, packet loss, and decode performance.

### Network tests

- Same host.
- Same LAN over Ethernet.
- Same LAN over Wi-Fi.
- Guest Wi-Fi or client-isolated network.
- Different subnet.
- VPN active.
- IPv4 port forwarding.
- ISP CGNAT.
- Firewall blocking the HTTP port.
- Firewall blocking UDP media ports.

### Packaging tests

- Fresh Windows machine without FFmpeg installed.
- Fresh Linux machine without the application installed.
- AppImage launched from a writable directory.
- AppImage launched from a read-only location.
- Two simultaneous application instances.
- Normal shutdown cleanup.
- Forced termination followed by startup cleanup.
- Missing FFmpeg asset.
- Missing PipeWire or XDG Desktop Portal.
- Headless launch from a non-interactive session.
- Interrupted CLI process followed by startup cleanup.

## 17. Main risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Rust WebRTC API changes | Build or behavior regressions | Pin an exact version, isolate the dependency, maintain browser tests |
| Linux capture differences | Some desktops cannot share the same source types | Use the portal path, document support targets, keep X11 fallback separate |
| Host capture permissions | Screen capture can fail even when monitors enumerate | Report the capture error, keep `test:0` for diagnostics, use native picker/WGC work for production capture |
| FFmpeg build licensing | Distribution obligations change with codecs and external libraries | Pin builds, record configuration, ship notices, review before release |
| Hardware encoder differences | Latency and quality vary by GPU and driver | Offer software fallback and expose encoder diagnostics |
| Runtime encoder controls vary by backend | Adaptive changes may require process restarts or cause gaps | Prefer live control where supported, otherwise use a candidate encoder and keyframe handoff |
| Adaptive controller oscillates | Viewers repeatedly change groups and quality becomes unstable | Smooth metrics, use hysteresis and cooldowns, downshift faster than upshift, and log decisions |
| One poor viewer affects a shared variant | Stable viewers receive unnecessarily low quality | Detect persistent outliers and migrate them to an existing or newly justified lower variant |
| Too many quality groups consume host resources | CPU/GPU, memory, or upload capacity is exhausted | Enforce a hard group ceiling, resource-aware `Auto` budgeting, cost estimates, and safe merge/drain behavior |
| Group migration causes a visible interruption | Viewer sees a freeze or loses the WebRTC session | Keep the codec and negotiated session stable, switch at a keyframe, preserve timestamps, and test track replacement |
| Browser layer-selection behavior differs | Simulcast or layered variants work inconsistently | Start with explicit server-side variant tracks, maintain browser interoperability tests, and use simulcast/SVC only after validation |
| Public IP lookup failure | Generated public URL becomes unavailable | Try the fallback service, retain a manual CLI override, and show a clear status |
| CGNAT or blocked inbound traffic | Port forwarding cannot work | Detect or explain the condition, add TURN later if needed |
| Temp cleanup after crashes | Stale FFmpeg files remain | Startup cleanup with an application-owned namespace |
| Single-file antivirus warnings | Users may distrust or lose access to the executable | Sign releases, publish hashes, avoid opaque runtime behavior, document extraction |
| Viewer count increases upload demand | Host network becomes the bottleneck | Show per-viewer bitrate, add SFU or adaptive layers later |
| HTTP signaling exposure | Attackers can alter the page or negotiate sessions | Random tokens now, HTTPS/WSS later |
| Shared numeric TCP/UDP port | WebRTC peers need separate ICE/DTLS state even when their datagrams share one socket | Use the application UDP mux and forward the single configured UDP port |
| CLI contract changes | Automation scripts break between releases | Version commands and JSON output, document exit codes, add compatibility tests |

## 18. Decisions required before implementation

1. Choose egui/eframe or Slint for the native UI.
2. Set the supported Windows versions.
3. Set the first supported Linux distributions and desktop environments.
4. Decide whether the project will use an LGPL-only FFmpeg build.
5. Choose the first H.264 encoder and VP8 fallback strategy.
6. Choose the exact `webrtc-rs` release.
7. Choose the default HTTP and UDP port ranges.
8. Set the default viewer limit.
9. Choose where portable configuration data will live.
10. Define the default no-argument behavior: UI or CLI help.
11. Define CLI subcommand names, exit codes, and JSON schema.
12. Define the support policy for HTTP-only public sharing.
13. Choose the adaptive quality policy and default latency preference.
14. Define the `Auto` maximum-group budgeting inputs, reserved host headroom, and fixed-value limits.
15. Choose the first runtime-reconfigurable encoder or the dual-encoder keyframe-handoff strategy.
16. Choose the initial quality-variant ladder and whether variants use explicit tracks, simulcast, or SVC.
17. Define the viewer metrics schema, sampling interval, smoothing windows, and adaptation thresholds.

## 19. Reference resources

- [WebRTC 1.0 specification](https://www.w3.org/TR/webrtc/)
- [MDN WebRTC connectivity](https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API/Connectivity)
- [MDN WebRTC signaling](https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API/Signaling_and_video_calling)
- [MDN WebRTC codecs](https://developer.mozilla.org/en-US/docs/Web/Media/Guides/Formats/WebRTC_codecs)
- [webrtc-rs](https://github.com/webrtc-rs/webrtc)
- [WebRTC.rs SFU](https://github.com/webrtc-rs/sfu)
- [FFmpeg documentation](https://ffmpeg.org/documentation.html)
- [FFmpeg legal information](https://ffmpeg.org/legal.html)
- [Windows Graphics Capture](https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture)
- [XCap cross-platform capture](https://docs.rs/xcap/latest/xcap/)
- [XDG Desktop Portal ScreenCast](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)
- [egui and eframe](https://github.com/emilk/egui)
- [Iced book](https://book.iced.rs/)
- [Slint documentation](https://docs.slint.dev/latest/docs/slint/)
- [Tauri prerequisites](https://tauri.app/start/prerequisites/)
- [Clap command-line parser](https://docs.rs/clap/latest/clap/)
- [AppImage documentation](https://docs.appimage.org/introduction/index.html)
- [Public IP service automation policy](https://wtfismyip.com/automation)
