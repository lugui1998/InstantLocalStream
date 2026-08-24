# InstantLocalStream Development Plan

Status: planning only

## 1. Product definition

InstantLocalStream will let a person capture a monitor or application window, encode it with FFmpeg, and share it through a browser. The host application will run a local HTTP server and a WebRTC media endpoint. Viewers will open a URL in a modern browser.

The first release will target Windows and Linux. Each platform may use a different capture backend and build artifact, while the Rust application core remains shared.

The first release will use HTTP and `ws://` signaling without a server certificate. The application will support local viewing first, LAN viewing next, and optional internet viewing through manual port forwarding.

The application will ship as one executable file per platform. It will not require an installer, a system service, Node.js, a separate FFmpeg installation, or a cloud account.

The same executable will expose both a native UI and a command-line interface. Both interfaces will call the same application services, so tests and automation will exercise the same capture, encoder, signaling, and WebRTC paths as interactive use.

## 2. Goals

### Primary goals

- Capture a complete monitor or a single application window.
- Encode screen video with low latency.
- Play the stream in a normal browser through WebRTC.
- Allow several viewers to connect at the same time.
- Avoid re-encoding the screen for each viewer.
- Run on Windows and Linux.
- Provide a native Rust control UI.
- Provide headless command-line operation for tests and automation.
- Distribute one executable file per platform.
- Display copyable local, LAN, and public viewer URLs.
- Let the user configure the HTTP port and WebRTC media ports.
- Keep the default mode local and safe to start.

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
- Advanced adaptive streaming with several quality layers.
- Full desktop audio support until the video path works.

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
│    └── WebSocket signaling                                   │
│                                                              │
└───────────────┬───────────────────────────┬──────────────────┘
                │ HTTP and WebSocket        │ WebRTC UDP media
                ▼                           ▼
        Browser viewer page             Browser <video>
```

The application has two network planes:

1. The control plane serves the viewer page, exchanges SDP and ICE messages, exposes status, and handles viewer admission.
2. The media plane carries the encoded video through WebRTC. WebRTC handles RTP, RTCP, ICE, DTLS, SRTP, congestion control, and browser playback.

The viewer page will use browser JavaScript or TypeScript because browsers expose WebRTC through the JavaScript API. The host application and its control UI can remain Rust-native.

WebRTC does not define a signaling transport, so the application can use a same-origin WebSocket endpoint for signaling. See [MDN's signaling guide](https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API/Signaling_and_video_calling) and the [WebRTC peer connection documentation](https://developer.mozilla.org/en-US/docs/Web/API/RTCPeerConnection).

## 5. Provisional technology choices

| Area | First choice | Reason | Later alternative |
|---|---|---|---|
| Language | Rust | Shared core, native UI, platform integration, and server in one language | None planned |
| Async runtime | Tokio | Fits capture coordination, process I/O, HTTP, WebSockets, and WebRTC tasks | smol if a future dependency decision favors it |
| HTTP and signaling | Axum plus WebSocket | Small Rust server with one origin for the page and signaling | Another Tokio-compatible HTTP framework |
| Browser media | `webrtc-rs` 0.20.x, pinned to an exact release | Rust WebRTC endpoint and media-track APIs | `str0m` or the WebRTC.rs SFU stack after a focused evaluation |
| Encoder integration | FFmpeg child process | Easy packaging boundary and access to software or hardware encoders | `ffmpeg-next` or a native media pipeline after profiling |
| Windows capture | Windows Graphics Capture | Native monitor and window capture with system permission UI | A lower-level DirectX path if profiling requires it |
| Linux capture | XDG Desktop Portal plus PipeWire | Fits Wayland permissions and supports monitor and window sources | X11-specific fallback for environments without a working portal |
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
- Hardware/software encoder selection.
- Cursor inclusion setting.
- Local, LAN, and public viewer URLs.
- Copy buttons for every viewer URL.
- Current viewer count.
- Capture, encoder, WebRTC, and network status.
- Packet loss, round-trip time, bitrate, and latency diagnostics.
- FFmpeg availability and encoder capability information.
- A licenses and build-information page.

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
- Bind mode: localhost, LAN, or public configuration.
- HTTP port.
- WebRTC UDP port or range.
- Codec, resolution, frame rate, and bitrate.
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
  --media-ports 40000-40010 \
  --codec h264 \
  --width 1920 \
  --height 1080 \
  --fps 60 \
  --bitrate 6000000 \
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

The capture subsystem will expose a platform-independent frame interface with:

- Pixel format.
- Width and height.
- Capture timestamp.
- Frame duration or frame-rate metadata.
- Cursor information when supported.
- Optional GPU-backed handle for a future low-copy path.

The first implementation should prioritize correct timestamps and stable frame delivery. The pipeline should leave room for GPU texture paths later.

The capture path should use a bounded queue. When the encoder or network falls behind, the application should drop stale frames and preserve the newest frame. A growing queue would convert a temporary slowdown into visible latency.

### Windows

Windows Graphics Capture can acquire frames from a display or application window through a system picker. The backend should translate captured frames into the internal frame format and report source changes, permission failures, and closed windows. [Microsoft screen capture documentation](https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture)

### Linux

The Linux backend should treat Wayland as a permission-controlled environment. It should request a ScreenCast portal session, let the desktop environment present the source picker, and consume the resulting PipeWire stream.

The backend should report missing or unavailable components in user-facing language:

- XDG Desktop Portal unavailable.
- PipeWire unavailable.
- User cancelled source selection.
- The chosen window or monitor disappeared.
- The desktop environment does not expose the requested source type.

X11 support should use the same internal frame interface. The project should document the supported desktop environments instead of promising identical behavior on every Linux distribution.

## 8. Encoding plan

FFmpeg will run as a long-lived child process. The host will send captured frames to FFmpeg and read encoded access units from its output.

The WebRTC layer should own RTP packetization, RTCP feedback, encryption, and network pacing. FFmpeg should produce a WebRTC-compatible elementary stream rather than an HLS or MP4 stream.

### Initial codec policy

Start with H.264 Constrained Baseline when a suitable encoder exists. Keep VP8 as the first fallback. Fully compliant WebRTC browsers must support VP8 and H.264 Constrained Baseline for video. [MDN WebRTC codec guide](https://developer.mozilla.org/en-US/docs/Web/Media/Guides/Formats/WebRTC_codecs)

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

The server should route messages and validate their size and session membership. It does not need to implement a second media protocol.

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

The browser can provide WebRTC statistics through `RTCPeerConnection.getStats()`. The host should combine those values with capture and encoder timestamps to estimate where latency accumulates.

## 10. HTTP-only deployment

The first release will serve the viewer page from the host application:

```text
Local:   http://127.0.0.1:PORT
LAN:     http://LAN_IP:PORT
Public:  http://PUBLIC_IP:PORT
```

The viewer will use a same-origin WebSocket for signaling. The viewer does not call browser screen-capture APIs because the native application owns capture.

HTTP mode has a security cost:

- The page can be modified in transit.
- WebSocket signaling is visible and modifiable in transit.
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
- WebRTC UDP port or narrow UDP range.
- Public IPv4 address or hostname.
- Optional advertised address override.

The router must forward both the HTTP port and the WebRTC media port or range. Forwarding the HTTP port alone can load the page while leaving the media connection unreachable.

The application can add STUN configuration later to discover server-reflexive candidates. TURN remains an optional fallback for users whose networks do not permit direct connections. Direct forwarding should remain the preferred path for latency.

## 12. Public IP discovery and share links

The UI should separate address discovery from connectivity verification.

```text
Local URL:   [http://192.168.1.20:8080/?token=...] [Copy]
Public IPv4: [203.0.113.10]                         [Refresh]
Public URL:  [http://203.0.113.10:8080/?token=...]  [Copy]
             [Test instructions]
```

The first implementation can query `https://ipv4.wtfismyip.com/text` when the user presses Refresh or enables public sharing. The application should:

- Apply a short timeout.
- Trim the response.
- Validate strict IPv4 syntax.
- Cache the result.
- Avoid polling.
- Allow a configurable endpoint.
- Allow manual entry if the endpoint fails.
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

## 14. Suggested project boundaries

The source tree should keep platform and media concerns separate:

```text
shared Rust crates or modules
├── app-core          State, commands, events, configuration
├── capture           Platform-independent frame and source interfaces
├── encoder           FFmpeg process and encoder profile abstraction
├── media             Encoded access units and timestamps
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
- FFmpeg H.264 encoding.
- One WebRTC browser viewer.
- A headless start path that uses the same media services as the UI.
- Localhost HTTP and WebSocket signaling.
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

### Phase 5: LAN sharing

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

### Phase 6: Public port-forwarding helper

Deliverables:

- Configurable HTTP port.
- Configurable UDP media range.
- Equivalent CLI options for public mode.
- Public IP lookup and manual override.
- Public URL generation.
- Copy buttons.
- Port-forwarding instructions.
- CGNAT and VPN warnings.

Exit criteria:

- A user with a reachable public IPv4 address can follow the instructions and connect from an external network.
- The UI distinguishes “IP found” from “stream verified.”

### Phase 7: Release hardening

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
- Public IP response parsing.
- Temporary-directory naming and stale-run detection.
- CLI argument parsing and configuration precedence.
- Exit-code mapping.
- JSON event schema validation.

### Integration tests

- HTTP page loads from localhost.
- WebSocket signaling completes an offer/answer exchange.
- A headless browser receives a WebRTC track.
- One encoder feeds multiple viewers.
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

Run tests at several resolutions, frame rates, codecs, and viewer counts. Record bitrate, packet loss, jitter, queue depth, and CPU/GPU use with each latency result.

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
| FFmpeg build licensing | Distribution obligations change with codecs and external libraries | Pin builds, record configuration, ship notices, review before release |
| Hardware encoder differences | Latency and quality vary by GPU and driver | Offer software fallback and expose encoder diagnostics |
| Public IP lookup failure | Generated public URL becomes unavailable | Manual entry, configurable endpoint, cached result, clear status |
| CGNAT or blocked inbound traffic | Port forwarding cannot work | Detect or explain the condition, add TURN later if needed |
| Temp cleanup after crashes | Stale FFmpeg files remain | Startup cleanup with an application-owned namespace |
| Single-file antivirus warnings | Users may distrust or lose access to the executable | Sign releases, publish hashes, avoid opaque runtime behavior, document extraction |
| Viewer count increases upload demand | Host network becomes the bottleneck | Show per-viewer bitrate, add SFU or adaptive layers later |
| HTTP signaling exposure | Attackers can alter the page or negotiate sessions | Random tokens now, HTTPS/WSS later |
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
- [XDG Desktop Portal ScreenCast](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)
- [egui and eframe](https://github.com/emilk/egui)
- [Iced book](https://book.iced.rs/)
- [Slint documentation](https://docs.slint.dev/latest/docs/slint/)
- [Tauri prerequisites](https://tauri.app/start/prerequisites/)
- [Clap command-line parser](https://docs.rs/clap/latest/clap/)
- [AppImage documentation](https://docs.appimage.org/introduction/index.html)
- [Public IP service automation policy](https://wtfismyip.com/automation)
