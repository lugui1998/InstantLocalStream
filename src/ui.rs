use std::collections::{HashMap, HashSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::io::Cursor;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use eframe::egui;

use crate::capture::{self, CapturePreview, CaptureSourceInfo};
use crate::config::{
    AppConfig, DEFAULT_AUDIO_EXCLUSIONS, FPS_PRESETS, QUALITY_PRESETS, fps_value, generate_token,
    local_ipv4, quality_height,
};
use crate::media::CaptureSettings;
use crate::network::UploadSpeedTestProgress;
use crate::preferences::{self, HostNetworkTestResult, UserPreferences};
use crate::server;

const PREVIEW_QUEUE_CAPACITY: usize = 4;
const PREVIEW_EVENT_CAPACITY: usize = 12;
const PREVIEW_EVENTS_PER_FRAME: usize = 3;
const MAX_CACHED_PREVIEWS: usize = 64;
const SOURCE_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const SOURCE_EVENT_DEBOUNCE: Duration = Duration::from_millis(150);
const LIVE_PREVIEW_RETRY_DELAY: Duration = Duration::from_secs(2);
const STREAM_START_ACK_TIMEOUT: Duration = Duration::from_secs(30);
const STREAM_STOP_ACK_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_WINDOW_WIDTH: f32 = 950.0;
const SOURCE_CARD_WIDTH: f32 = 280.0;
const SOURCE_CARD_HEIGHT: f32 = 210.0;
const SOURCE_PREVIEW_HEIGHT: f32 = 158.0;
const SOURCE_CAROUSEL_GAP: f32 = 8.0;

#[cfg(windows)]
static SOURCE_CHANGE_PENDING: AtomicBool = AtomicBool::new(false);

pub fn run(config: AppConfig) -> Result<()> {
    run_internal(config, true)
}

pub fn run_without_preferences(config: AppConfig) -> Result<()> {
    run_internal(config, false)
}

fn run_internal(mut config: AppConfig, load_preferences: bool) -> Result<()> {
    let saved_preferences = load_preferences.then(preferences::load).flatten();
    if let Some(saved) = &saved_preferences {
        saved.apply_to(&mut config);
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([DEFAULT_WINDOW_WIDTH, 650.0])
            .with_min_inner_size([760.0, 620.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Instant Local Stream",
        options,
        Box::new(move |creation_context| {
            creation_context.egui_ctx.set_visuals(viewer_visuals());
            Ok(Box::new(HostUi::new(
                config.clone(),
                creation_context.egui_ctx.clone(),
                saved_preferences
                    .as_ref()
                    .and_then(|saved| saved.host_network_test.clone()),
            )))
        }),
    )
    .map_err(|error| anyhow::anyhow!("UI failed: {error}"))
}

fn viewer_visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = egui::Color32::from_rgb(13, 16, 20);
    visuals.panel_fill = egui::Color32::from_rgb(13, 16, 20);
    visuals.faint_bg_color = egui::Color32::from_rgb(20, 25, 31);
    visuals.extreme_bg_color = egui::Color32::from_rgb(5, 6, 7);
    visuals.widgets.noninteractive.bg_stroke.color = egui::Color32::from_rgb(48, 57, 68);
    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(20, 25, 31);
    visuals.selection.bg_fill = egui::Color32::from_rgb(105, 137, 68);
    visuals.selection.stroke.color = egui::Color32::from_rgb(201, 243, 107);
    visuals.hyperlink_color = egui::Color32::from_rgb(201, 243, 107);
    visuals
}

struct HostUi {
    config: AppConfig,
    status: HostStatus,
    command_tx: mpsc::Sender<UiCommand>,
    event_rx: mpsc::Receiver<UiEvent>,
    control_slot: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<server::ServerCommand>>>>,
    source_request_tx: mpsc::SyncSender<SourceRequest>,
    preview_request_tx: mpsc::SyncSender<PreviewRequest>,
    preview_event_rx: mpsc::Receiver<PreviewEvent>,
    preview_scope: Arc<AtomicU64>,
    discovery_generation: HashMap<&'static str, u64>,
    discovery_inflight: HashSet<&'static str>,
    #[cfg(windows)]
    source_watcher: Option<SourceChangeWatcher>,
    next_source_refresh: Instant,
    preview_epoch: u64,
    preview_inflight: HashSet<(u64, PreviewKey)>,
    preview_failed_epoch: HashMap<PreviewKey, u64>,
    preview_retry_after: HashMap<PreviewKey, Instant>,
    visible_preview_keys: HashSet<PreviewKey>,
    source_tab: SourceTab,
    source_selected: bool,
    display_non_window_elements: bool,
    sources: Vec<CaptureSourceInfo>,
    source_previews: HashMap<PreviewKey, CachedPreview>,
    test_preview: Option<egui::TextureHandle>,
    test_preview_signature: Option<TestPreviewSignature>,
    test_preview_inflight: Option<(u64, TestPreviewSignature)>,
    test_preview_failed_signature: Option<TestPreviewSignature>,
    source_error: Option<String>,
    last_saved_preferences: Option<UserPreferences>,
    viewer_url_mode: ViewerUrlMode,
    custom_viewer_host: String,
    public_ipv4: Option<String>,
    public_ip_error: Option<String>,
    copied_until: Option<Instant>,
    port_input: String,
    host_network_test: Option<HostNetworkTestResult>,
    network_test_inflight: bool,
    network_test_error: Option<String>,
    network_test_progress: Option<UploadSpeedTestProgress>,
    last_sent_max_viewers: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceTab {
    Displays,
    Windows,
    TestPattern,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewerUrlMode {
    Local,
    Lan,
    Public,
    Custom,
}

#[derive(Clone, PartialEq, Eq)]
enum HostStatus {
    StartingServer,
    Ready,
    StartingStream,
    Running,
    StoppingStream,
    StreamFailed(String),
    Failed(String),
}

impl HostStatus {
    fn server_alive(&self) -> bool {
        matches!(
            self,
            Self::StartingServer
                | Self::Ready
                | Self::StartingStream
                | Self::Running
                | Self::StoppingStream
                | Self::StreamFailed(_)
        )
    }

    fn stream_active(&self) -> bool {
        matches!(
            self,
            Self::StartingStream | Self::Running | Self::StoppingStream
        )
    }

    fn label(&self) -> String {
        match self {
            Self::StartingServer => "Starting viewer server…".to_owned(),
            Self::Ready => String::new(),
            Self::StartingStream => "Starting stream…".to_owned(),
            Self::Running => "Streaming".to_owned(),
            Self::StoppingStream => "Stopping stream…".to_owned(),
            Self::StreamFailed(error) => format!("Stream failed: {error}"),
            Self::Failed(error) => format!("Unavailable: {error}"),
        }
    }
}

enum UiCommand {
    StartServer(Box<AppConfig>),
    StartStream(Box<CaptureSettings>),
    StopStream,
    Shutdown,
    LookupPublicIp,
    TestUploadSpeed,
}

enum UiEvent {
    ServerReady,
    StreamStarted,
    StreamStopped,
    StreamFailed(String),
    Failed(String),
    PublicIp(String),
    PublicIpFailed(String),
    UploadSpeedTestProgress(UploadSpeedTestProgress),
    UploadSpeedTestFinished(std::result::Result<u64, String>),
}

enum SourceRequest {
    Discover {
        generation: u64,
        kind: &'static str,
        display_non_window_elements: bool,
    },
    Shutdown,
}

enum PreviewRequest {
    Source {
        scope: u64,
        epoch: u64,
        key: PreviewKey,
        source: CaptureSourceInfo,
    },
    LiveSource {
        scope: u64,
        epoch: u64,
        key: PreviewKey,
        source: CaptureSourceInfo,
        snapshot_rx: mpsc::Receiver<Option<server::CapturePreviewSnapshot>>,
    },
    TestPattern {
        scope: u64,
        signature: TestPreviewSignature,
    },
    Shutdown,
}

enum PreviewEvent {
    SourcesDiscovered {
        generation: u64,
        kind: &'static str,
        sources: Vec<CaptureSourceInfo>,
    },
    SourceDiscoveryFailed {
        generation: u64,
        kind: &'static str,
        error: String,
    },
    SourceReady {
        scope: u64,
        epoch: u64,
        key: PreviewKey,
        source_size: (u32, u32),
        source_name: String,
        image: egui::ColorImage,
    },
    SourceFailed {
        scope: u64,
        epoch: u64,
        key: PreviewKey,
        sticky: bool,
    },
    TestPatternReady {
        scope: u64,
        signature: TestPreviewSignature,
        image: egui::ColorImage,
    },
    TestPatternFailed {
        scope: u64,
        signature: TestPreviewSignature,
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PreviewKey {
    kind: String,
    identity: PreviewIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum PreviewIdentity {
    NativeWindow { native_id: u64, pid: Option<u32> },
    Indexed { index: usize, name: String },
}

impl PreviewKey {
    fn for_source(source: &CaptureSourceInfo) -> Self {
        let identity = match source.native_id {
            Some(native_id) => PreviewIdentity::NativeWindow {
                native_id,
                pid: source.pid,
            },
            None => PreviewIdentity::Indexed {
                index: source.index,
                name: source.name.clone(),
            },
        };
        Self {
            kind: source.kind.clone(),
            identity,
        }
    }

    fn texture_name(&self) -> String {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        format!("source-preview-{:016x}", hasher.finish())
    }
}

struct CachedPreview {
    texture: egui::TextureHandle,
    captured_epoch: u64,
    source_size: (u32, u32),
    source_name: String,
    last_used: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TestPreviewSignature {
    width: u32,
    height: u32,
    fps: u32,
}

impl HostUi {
    fn new(
        config: AppConfig,
        repaint_context: egui::Context,
        host_network_test: Option<HostNetworkTestResult>,
    ) -> Self {
        let mut config = config;
        if config.source.kind == "test" {
            if config.quality == "source" {
                config.quality = "1080p".to_owned();
            }
            if config.fps_preset == "source" {
                config.fps_preset = "60".to_owned();
            }
            if config.adaptive_quality_ceiling == "source" {
                config.adaptive_quality_ceiling = "1080p".to_owned();
            }
            if config.adaptive_fps_ceiling == "source" {
                config.adaptive_fps_ceiling = "60".to_owned();
            }
        }
        let source_tab = match config.source.kind.as_str() {
            "window" => SourceTab::Windows,
            "test" => SourceTab::TestPattern,
            _ => SourceTab::Displays,
        };
        let (viewer_url_mode, custom_viewer_host, public_ipv4) = initial_viewer_url_state(&config);
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let (source_request_tx, source_request_rx) = mpsc::sync_channel(PREVIEW_QUEUE_CAPACITY);
        let (preview_request_tx, preview_request_rx) = mpsc::sync_channel(PREVIEW_QUEUE_CAPACITY);
        let (preview_event_tx, preview_event_rx) = mpsc::sync_channel(PREVIEW_EVENT_CAPACITY);
        let preview_scope = Arc::new(AtomicU64::new(1));
        let source_events = preview_event_tx.clone();
        let source_repaint = repaint_context.clone();
        thread::spawn(move || source_worker_loop(source_request_rx, source_events, source_repaint));
        let capture_scope = Arc::clone(&preview_scope);
        thread::spawn(move || {
            preview_worker_loop(
                preview_request_rx,
                preview_event_tx,
                capture_scope,
                repaint_context,
            )
        });
        let shutdown = Arc::new(Mutex::new(None));
        let control_slot = Arc::new(Mutex::new(None));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_control = Arc::clone(&control_slot);
        thread::spawn(move || worker_loop(command_rx, event_tx, thread_shutdown, thread_control));
        let initial_preferences = UserPreferences::from_config(&config, host_network_test.clone());
        let port_input = config.http_port.to_string();
        let mut app = Self {
            config,
            status: HostStatus::StartingServer,
            command_tx,
            event_rx,
            control_slot,
            source_request_tx,
            preview_request_tx,
            preview_event_rx,
            preview_scope,
            discovery_generation: HashMap::new(),
            discovery_inflight: HashSet::new(),
            #[cfg(windows)]
            source_watcher: start_source_change_watcher(),
            next_source_refresh: Instant::now() + SOURCE_REFRESH_INTERVAL,
            preview_epoch: 1,
            preview_inflight: HashSet::new(),
            preview_failed_epoch: HashMap::new(),
            preview_retry_after: HashMap::new(),
            visible_preview_keys: HashSet::new(),
            source_tab,
            // The config's default source is only a placeholder.  This native
            // UI requires an explicit source-card (or test-pattern) choice.
            source_selected: false,
            display_non_window_elements: false,
            sources: Vec::new(),
            source_previews: HashMap::new(),
            test_preview: None,
            test_preview_signature: None,
            test_preview_inflight: None,
            test_preview_failed_signature: None,
            source_error: None,
            last_saved_preferences: Some(initial_preferences),
            viewer_url_mode,
            custom_viewer_host,
            public_ipv4,
            public_ip_error: None,
            copied_until: None,
            port_input,
            network_test_inflight: host_network_test.is_none(),
            host_network_test,
            network_test_error: None,
            network_test_progress: None,
            last_sent_max_viewers: None,
        };
        // Discover both classes cheaply in the background so tab switches can
        // render cards immediately. Thumbnails are requested separately and
        // only for cards that intersect the visible UI clip rectangle.
        app.request_source_discovery("monitor");
        app.request_source_discovery("window");
        // Bring the control/viewer server online with a neutral local source.
        // Persisted HWNDs are intentionally not trusted before the user makes
        // the explicit selection required by this UI.
        let _ = app
            .command_tx
            .send(UiCommand::StartServer(Box::new(server_bootstrap_config(
                &app.config,
            ))));
        let _ = app.command_tx.send(UiCommand::LookupPublicIp);
        if app.network_test_inflight {
            let _ = app.command_tx.send(UiCommand::TestUploadSpeed);
        }
        app
    }

    fn poll_preview_events(&mut self, ctx: &egui::Context) {
        let mut processed = 0;
        while processed < PREVIEW_EVENTS_PER_FRAME {
            let Ok(event) = self.preview_event_rx.try_recv() else {
                break;
            };
            processed += 1;
            match event {
                PreviewEvent::SourcesDiscovered {
                    generation,
                    kind,
                    mut sources,
                } => {
                    if self.discovery_generation.get(kind).copied() != Some(generation) {
                        continue;
                    }
                    self.discovery_inflight.remove(kind);
                    self.source_error = None;
                    let selected_native_exists = self
                        .config
                        .source
                        .native_id
                        .is_some_and(capture::native_window_exists);
                    if kind == "window" {
                        retain_temporarily_unenumerated_windows(
                            &mut sources,
                            &self.sources,
                            self.display_non_window_elements,
                            capture::native_window_exists,
                        );
                    }
                    retain_selected_native_source(
                        &mut sources,
                        &self.sources,
                        self.source_selected,
                        &self.config.source.kind,
                        self.config.source.native_id,
                        selected_native_exists,
                        self.display_non_window_elements,
                    );
                    replace_sources_for_kind(&mut self.sources, kind, sources);
                    let valid_keys = self
                        .sources
                        .iter()
                        .map(PreviewKey::for_source)
                        .collect::<HashSet<_>>();
                    self.source_previews
                        .retain(|key, _| valid_keys.contains(key));
                    self.preview_failed_epoch
                        .retain(|key, _| valid_keys.contains(key));
                    self.preview_retry_after
                        .retain(|key, _| valid_keys.contains(key));
                }
                PreviewEvent::SourceDiscoveryFailed {
                    generation,
                    kind,
                    error,
                } => {
                    if self.discovery_generation.get(kind).copied() == Some(generation) {
                        self.discovery_inflight.remove(kind);
                        self.source_error = Some(error);
                    }
                }
                PreviewEvent::SourceReady {
                    scope,
                    epoch,
                    key,
                    source_size,
                    source_name,
                    image,
                } => {
                    self.preview_inflight.remove(&(scope, key.clone()));
                    if scope != self.current_preview_scope()
                        || !self
                            .sources
                            .iter()
                            .any(|source| PreviewKey::for_source(source) == key)
                    {
                        continue;
                    }
                    let now = Instant::now();
                    if let Some(cached) = self.source_previews.get_mut(&key) {
                        cached.texture.set(image, egui::TextureOptions::LINEAR);
                        cached.captured_epoch = cached.captured_epoch.max(epoch);
                        cached.source_size = source_size;
                        cached.source_name = source_name;
                        cached.last_used = now;
                    } else {
                        let texture = ctx.load_texture(
                            key.texture_name(),
                            image,
                            egui::TextureOptions::LINEAR,
                        );
                        self.source_previews.insert(
                            key.clone(),
                            CachedPreview {
                                texture,
                                captured_epoch: epoch,
                                source_size,
                                source_name,
                                last_used: now,
                            },
                        );
                    }
                    self.preview_failed_epoch.remove(&key);
                    self.preview_retry_after.remove(&key);
                    let mut protected = HashSet::new();
                    if self.source_selected
                        && let Some(selected) = self.sources.iter().find(|source| {
                            source_matches_selection(
                                source,
                                &self.config.source.kind,
                                self.config.source.index,
                                self.config.source.native_id,
                            )
                        })
                    {
                        protected.insert(PreviewKey::for_source(selected));
                    }
                    for visible in &self.visible_preview_keys {
                        if protected.len() >= MAX_CACHED_PREVIEWS {
                            break;
                        }
                        protected.insert(visible.clone());
                    }
                    prune_preview_cache(&mut self.source_previews, MAX_CACHED_PREVIEWS, &protected);
                }
                PreviewEvent::SourceFailed {
                    scope,
                    epoch,
                    key,
                    sticky,
                } => {
                    self.preview_inflight.remove(&(scope, key.clone()));
                    if scope == self.current_preview_scope() {
                        if sticky {
                            self.preview_failed_epoch.insert(key, epoch);
                        } else {
                            self.preview_retry_after
                                .insert(key, Instant::now() + LIVE_PREVIEW_RETRY_DELAY);
                        }
                    }
                }
                PreviewEvent::TestPatternReady {
                    scope,
                    signature,
                    image,
                } => {
                    if self.test_preview_inflight == Some((scope, signature)) {
                        self.test_preview_inflight = None;
                    }
                    if scope != self.current_preview_scope() {
                        continue;
                    }
                    self.source_error = None;
                    if let Some(texture) = self.test_preview.as_mut() {
                        texture.set(image, egui::TextureOptions::LINEAR);
                    } else {
                        self.test_preview = Some(ctx.load_texture(
                            "test-pattern-preview",
                            image,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                    self.test_preview_signature = Some(signature);
                    self.test_preview_failed_signature = None;
                }
                PreviewEvent::TestPatternFailed {
                    scope,
                    signature,
                    error,
                } => {
                    if self.test_preview_inflight == Some((scope, signature)) {
                        self.test_preview_inflight = None;
                    }
                    if scope == self.current_preview_scope() {
                        self.test_preview_failed_signature = Some(signature);
                        self.source_error = Some(error);
                    }
                }
            }
        }
        if processed == PREVIEW_EVENTS_PER_FRAME {
            ctx.request_repaint();
        }
    }

    fn current_preview_scope(&self) -> u64 {
        self.preview_scope.load(Ordering::Acquire)
    }

    fn begin_preview_scope(&mut self) -> u64 {
        let scope = self.preview_scope.fetch_add(1, Ordering::AcqRel) + 1;
        self.preview_inflight.clear();
        self.test_preview_inflight = None;
        scope
    }

    fn request_source_discovery(&mut self, kind: &'static str) {
        if self.discovery_inflight.contains(kind) {
            return;
        }
        let generation = self
            .discovery_generation
            .get(kind)
            .copied()
            .unwrap_or_default()
            + 1;
        match self.source_request_tx.try_send(SourceRequest::Discover {
            generation,
            kind,
            display_non_window_elements: self.display_non_window_elements,
        }) {
            Ok(()) => {
                self.discovery_generation.insert(kind, generation);
                self.discovery_inflight.insert(kind);
            }
            Err(mpsc::TrySendError::Full(_)) => {}
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.source_error = Some("source discovery worker stopped".to_owned());
            }
        }
    }

    fn refresh_sources_if_due(&mut self) {
        let now = Instant::now();
        #[cfg(windows)]
        {
            let _ = self.source_watcher.as_ref();
            if SOURCE_CHANGE_PENDING.swap(false, Ordering::AcqRel) {
                self.next_source_refresh = now + SOURCE_EVENT_DEBOUNCE;
            }
        }
        if now < self.next_source_refresh {
            return;
        }
        self.next_source_refresh = now + SOURCE_REFRESH_INTERVAL;
        self.request_source_discovery("monitor");
        self.request_source_discovery("window");
    }

    fn ensure_source_preview(&mut self, source: CaptureSourceInfo) {
        let key = PreviewKey::for_source(&source);
        let selected_live_source = self.status.stream_active()
            && self.source_selected
            && source_matches_selection(
                &source,
                &self.config.source.kind,
                self.config.source.index,
                self.config.source.native_id,
            );
        if selected_live_source && self.status != HostStatus::Running {
            // Start/stop reconfiguration owns the capture graph. Wait for its
            // acknowledgement before asking for a matching latest-frame view.
            return;
        }
        let scope = self.current_preview_scope();
        let captured_epoch = self.source_previews.get(&key).and_then(|cached| {
            source_signature_matches(&cached.source_name, cached.source_size, &source)
                .then_some(cached.captured_epoch)
        });
        let failed = self
            .preview_failed_epoch
            .get(&key)
            .is_some_and(|failed_epoch| *failed_epoch >= self.preview_epoch);
        let retrying = self
            .preview_retry_after
            .get(&key)
            .is_some_and(|retry_after| *retry_after > Instant::now());
        if !preview_request_needed(
            captured_epoch,
            self.preview_epoch,
            self.preview_inflight.contains(&(scope, key.clone())),
            failed || retrying,
        ) {
            return;
        }
        if selected_live_source {
            let (snapshot_tx, snapshot_rx) = mpsc::sync_channel(1);
            let request = PreviewRequest::LiveSource {
                scope,
                epoch: self.preview_epoch,
                key: key.clone(),
                source,
                snapshot_rx,
            };
            match self.preview_request_tx.try_send(request) {
                Ok(()) => {
                    self.preview_inflight.insert((scope, key));
                    let control = self
                        .control_slot
                        .lock()
                        .ok()
                        .and_then(|slot| slot.as_ref().cloned());
                    if let Some(control) = control {
                        let _ = control.send(server::ServerCommand::PreviewSnapshot {
                            result: snapshot_tx,
                        });
                    }
                }
                Err(mpsc::TrySendError::Full(_)) => {}
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    self.source_error = Some("preview capture worker stopped".to_owned());
                }
            }
            return;
        }
        let request = PreviewRequest::Source {
            scope,
            epoch: self.preview_epoch,
            key: key.clone(),
            source,
        };
        match self.preview_request_tx.try_send(request) {
            Ok(()) => {
                self.preview_inflight.insert((scope, key));
            }
            Err(mpsc::TrySendError::Full(_)) => {}
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.source_error = Some("preview capture worker stopped".to_owned());
            }
        }
    }

    fn ensure_test_pattern_preview(&mut self) {
        let settings = CaptureSettings::from_config(&self.config);
        let (width, height) = settings.test_pattern_dimensions();
        let signature = TestPreviewSignature {
            width,
            height,
            fps: settings.output_fps.unwrap_or(settings.fps),
        };
        let mut scope = self.current_preview_scope();
        if self
            .test_preview_inflight
            .is_some_and(|(pending_scope, pending)| pending_scope == scope && pending != signature)
        {
            scope = self.begin_preview_scope();
        }
        if self.test_preview_inflight.is_none()
            && self.test_preview_signature.is_some()
            && self.test_preview_signature != Some(signature)
        {
            self.test_preview = None;
            self.test_preview_signature = None;
        }
        if !test_preview_request_needed(
            self.test_preview_signature,
            self.test_preview_inflight,
            self.test_preview_failed_signature,
            scope,
            signature,
        ) {
            return;
        }
        match self
            .preview_request_tx
            .try_send(PreviewRequest::TestPattern { scope, signature })
        {
            Ok(()) => self.test_preview_inflight = Some((scope, signature)),
            Err(mpsc::TrySendError::Full(_)) => {}
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.source_error = Some("preview capture worker stopped".to_owned());
            }
        }
    }

    fn has_valid_source_selection(&self) -> bool {
        source_selection_is_valid(
            self.source_selected,
            &self.config.source.kind,
            self.config.source.index,
            self.config.source.native_id,
            &self.sources,
        )
    }

    fn selected_window_is_minimized(&self) -> bool {
        self.config.source.kind == "window"
            && self
                .config
                .source
                .native_id
                .is_some_and(capture::native_window_is_minimized)
    }

    fn draw_source_picker(&mut self, ui: &mut egui::Ui) {
        self.poll_preview_events(ui.ctx());
        self.visible_preview_keys.clear();
        ui.heading("Sources");
        ui.horizontal(|ui| {
            let displays_response = ui
                .selectable_label(self.source_tab == SourceTab::Displays, "Displays")
                .on_hover_text("Choose a monitor to capture.");
            if displays_response.clicked() && self.source_tab != SourceTab::Displays {
                self.source_selected = false;
                self.source_tab = SourceTab::Displays;
                self.begin_preview_scope();
                self.leave_test_pattern("monitor");
                if !self.sources.iter().any(|source| source.kind == "monitor") {
                    self.request_source_discovery("monitor");
                }
            }
            let windows_response = ui
                .selectable_label(self.source_tab == SourceTab::Windows, "Windows")
                .on_hover_text("Choose an application window to capture.");
            if windows_response.clicked() && self.source_tab != SourceTab::Windows {
                self.source_selected = false;
                self.source_tab = SourceTab::Windows;
                self.begin_preview_scope();
                self.leave_test_pattern("window");
                if !self.sources.iter().any(|source| source.kind == "window") {
                    self.request_source_discovery("window");
                }
            }
            let test_response = ui
                .selectable_label(self.source_tab == SourceTab::TestPattern, "Test pattern")
                .on_hover_text("Use a deterministic animated test source.");
            if test_response.clicked() && self.source_tab != SourceTab::TestPattern {
                self.source_tab = SourceTab::TestPattern;
                self.begin_preview_scope();
                self.test_preview = None;
                self.test_preview_signature = None;
                self.test_preview_failed_signature = None;
                self.config.source.kind = "test".to_owned();
                self.config.source.index = 0;
                self.config.source.native_id = None;
                self.source_selected = true;
                self.config.audio_mode = "off".to_owned();
                if self.config.quality == "source" {
                    self.config.quality = "1080p".to_owned();
                }
                if self.config.fps_preset == "source" {
                    self.config.fps_preset = "60".to_owned();
                }
                if self.config.adaptive_quality_ceiling == "source" {
                    self.config.adaptive_quality_ceiling = "1080p".to_owned();
                }
                if self.config.adaptive_fps_ceiling == "source" {
                    self.config.adaptive_fps_ceiling = "60".to_owned();
                }
            }
        });
        if self.source_tab == SourceTab::Windows {
            let display_response = ui
                .checkbox(
                    &mut self.display_non_window_elements,
                    "Display Non-window Elements",
                )
                .on_hover_text(
                    "Also list system UI surfaces such as notification popups and shell overlays. These may not behave like normal application windows when captured.",
                );
            if display_response.changed() {
                self.begin_preview_scope();
                self.request_source_discovery("window");
            }
        }
        if self.source_tab == SourceTab::TestPattern {
            self.ensure_test_pattern_preview();
            self.draw_test_pattern_preview(ui);
            if let Some(error) = &self.source_error {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("Source error: {error}"),
                );
            }
            return;
        }

        if self.source_selected && self.selected_window_is_minimized() {
            ui.label("Restore the selected window before starting its capture.");
        }

        let kind = match self.source_tab {
            SourceTab::Displays => "monitor",
            SourceTab::Windows => "window",
            SourceTab::TestPattern => return,
        };
        let sources = self
            .sources
            .iter()
            .filter(|source| source.kind == kind)
            .cloned()
            .collect::<Vec<_>>();
        if sources.is_empty() {
            ui.add_space(12.0);
            if self.source_error.is_some() {
                ui.label("Source scan failed. Retrying automatically…");
            } else if self.discovery_inflight.contains(kind) {
                ui.label("Looking for capture sources…");
            } else {
                ui.label(if kind == "monitor" {
                    "No displays were found."
                } else {
                    "No capturable windows were found."
                });
            }
        } else {
            // Keep the source cards in one row so the picker behaves like a
            // carousel. The horizontal scroll area owns overflow while the
            // surrounding settings panel continues to handle vertical scroll.
            let mut visible_sources = Vec::new();
            ui.scope(|ui| {
                // A horizontal ScrollArea normally only consumes horizontal
                // wheel deltas. Let a regular mouse wheel move this carousel
                // horizontally while keeping the setting local to it, so
                // the surrounding page can still scroll vertically.
                ui.style_mut().always_scroll_the_only_direction = true;
                egui::ScrollArea::horizontal()
                    .id_salt("source-carousel")
                    .auto_shrink([false, true])
                    // Reserve the scrollbar track. Toggling it only when the
                    // last card crosses the viewport by a subpixel changes
                    // the available height and can oscillate every frame.
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                    .scroll_source(egui::scroll_area::ScrollSource::ALL)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            // The panel's edge padding is approximately 8 px;
                            // use that same explicit gap between cards.
                            ui.spacing_mut().item_spacing.x = 0.0;
                            for (source_position, source) in sources.into_iter().enumerate() {
                                if source_position > 0 {
                                    ui.add_space(SOURCE_CAROUSEL_GAP);
                                }
                                let key = PreviewKey::for_source(&source);
                                let selected = self.source_selected
                                    && source_matches_selection(
                                        &source,
                                        &self.config.source.kind,
                                        self.config.source.index,
                                        self.config.source.native_id,
                                    );
                                let texture = self.source_previews.get(&key).map(|cached| {
                                    (cached.texture.id(), cached.texture.size_vec2())
                                });
                                let source_kind = source.kind.clone();
                                let source_index = source.index;
                                let source_native_id = source.native_id;
                                let frame_response = egui::Frame::new()
                                    .fill(ui.visuals().faint_bg_color)
                                    .stroke(egui::Stroke::new(
                                        if selected { 2.0 } else { 1.0 },
                                        if selected {
                                            ui.visuals().selection.bg_fill
                                        } else {
                                            ui.visuals().widgets.noninteractive.bg_stroke.color
                                        },
                                    ))
                                    .inner_margin(10.0)
                                    .show(ui, |ui| {
                                        ui.with_layout(
                                            egui::Layout::top_down(egui::Align::Min),
                                            |ui| {
                                                let card_size = egui::vec2(
                                                    SOURCE_CARD_WIDTH,
                                                    SOURCE_CARD_HEIGHT,
                                                );
                                                ui.set_min_size(card_size);
                                                ui.set_max_size(card_size);
                                                ui.add_sized(
                                                    [SOURCE_CARD_WIDTH, 18.0],
                                                    egui::Label::new(
                                                        egui::RichText::new(&source.name)
                                                            .strong()
                                                            .monospace(),
                                                    )
                                                    .halign(egui::Align::Center)
                                                    .truncate(),
                                                );
                                                ui.add_space(6.0);
                                                let preview_size = egui::vec2(
                                                    SOURCE_CARD_WIDTH,
                                                    SOURCE_PREVIEW_HEIGHT,
                                                );
                                                ui.allocate_ui_with_layout(
                                                    preview_size,
                                                    egui::Layout::centered_and_justified(
                                                        egui::Direction::LeftToRight,
                                                    ),
                                                    |ui| {
                                                        if let Some(texture) = texture {
                                                            ui.add(
                                                                egui::Image::from_texture(texture)
                                                                    .fit_to_exact_size(preview_size)
                                                                    .maintain_aspect_ratio(true),
                                                            );
                                                        } else {
                                                            let (rect, _) = ui.allocate_exact_size(
                                                                preview_size,
                                                                egui::Sense::hover(),
                                                            );
                                                            ui.painter().rect_filled(
                                                                rect,
                                                                4.0,
                                                                ui.visuals().extreme_bg_color,
                                                            );
                                                        }
                                                    },
                                                );
                                                ui.add_space(6.0);
                                                ui.label(format!(
                                                    "{} × {}{}",
                                                    source.width,
                                                    source.height,
                                                    source
                                                        .fps
                                                        .map(|fps| format!(" · {fps} FPS"))
                                                        .unwrap_or_default()
                                                ));
                                            },
                                        );
                                    });
                                let card_response = ui.interact(
                                    frame_response.response.rect,
                                    ui.id().with(("source-card", source_position, &key)),
                                    egui::Sense::click(),
                                );
                                if ui.is_rect_visible(frame_response.response.rect) {
                                    visible_sources.push((selected, source.clone()));
                                    if let Some(cached) = self.source_previews.get_mut(&key) {
                                        cached.last_used = Instant::now();
                                    }
                                }
                                card_response.widget_info(|| {
                                    egui::WidgetInfo::selected(
                                        egui::WidgetType::Button,
                                        true,
                                        selected,
                                        format!("Capture source: {}", source.name),
                                    )
                                });
                                let keyboard_activated = card_response.has_focus()
                                    && ui.input(|input| {
                                        input.key_pressed(egui::Key::Enter)
                                            || input.key_pressed(egui::Key::Space)
                                    });
                                if card_response.clicked() || keyboard_activated {
                                    card_response.request_focus();
                                    if self.status.stream_active() {
                                        self.begin_preview_scope();
                                    }
                                    let update_legacy_index = source_index_should_update(
                                        &self.config.source.kind,
                                        self.config.source.native_id,
                                        &source_kind,
                                        source_native_id,
                                    );
                                    self.config.source.kind = source_kind;
                                    if update_legacy_index {
                                        self.config.source.index = source_index;
                                    }
                                    self.config.source.native_id = source_native_id;
                                    self.source_selected = true;
                                    if self.config.source.kind == "monitor"
                                        && self.config.audio_mode == "window"
                                    {
                                        self.config.audio_mode = "system".to_owned();
                                    } else if self.config.source.kind == "window"
                                        && self.config.audio_mode == "system"
                                    {
                                        self.config.audio_mode = "window".to_owned();
                                    }
                                    if self.status == HostStatus::Running
                                        && let Ok(slot) = self.control_slot.lock()
                                        && let Some(sender) = slot.as_ref()
                                    {
                                        // Put a live source switch ahead of any preview
                                        // snapshot request queued after this layout pass.
                                        let _ = sender.send(server::ServerCommand::Update(
                                            CaptureSettings::from_config(&self.config),
                                        ));
                                    }
                                }
                            }
                        });
                    });
            });
            // The selected card wins the serial capture slot, followed by the
            // other cards intersecting the current scroll viewport. A bounded
            // non-blocking queue prevents fast scrolling from creating a large
            // backlog of stale OS capture work.
            visible_sources.sort_by_key(|(selected, _)| !*selected);
            self.visible_preview_keys = visible_sources
                .iter()
                .take(MAX_CACHED_PREVIEWS)
                .map(|(_, source)| PreviewKey::for_source(source))
                .collect();
            for (_, source) in visible_sources.into_iter().take(MAX_CACHED_PREVIEWS) {
                self.ensure_source_preview(source);
            }
        }
        if let Some(error) = &self.source_error {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Source error: {error}"),
            );
        }
    }

    fn leave_test_pattern(&mut self, kind: &str) {
        if self.config.source.kind != "test" {
            return;
        }
        if let Some(source) = self.sources.iter().find(|source| source.kind == kind) {
            self.config.source.kind = source.kind.clone();
            self.config.source.index = source.index;
            self.config.source.native_id = source.native_id;
        } else {
            self.config.source.kind = kind.to_owned();
            self.config.source.index = 0;
            self.config.source.native_id = None;
        }
    }

    fn available_audio_processes(&self) -> Vec<String> {
        let mut processes = HashSet::new();
        for source in &self.sources {
            if source.kind != "window" {
                continue;
            }
            let process = source
                .name
                .split_once(':')
                .map(|(name, _)| name.trim())
                .unwrap_or(source.name.as_str())
                .to_owned();
            if !process.is_empty() {
                processes.insert(process);
            }
        }
        let mut processes = processes.into_iter().collect::<Vec<_>>();
        processes.sort_unstable_by_key(|name| name.to_ascii_lowercase());
        processes
    }

    fn draw_audio_settings(
        &mut self,
        ui: &mut egui::Ui,
        stream_active: bool,
        available_audio_processes: &[String],
    ) {
        let audio_mode = if self.source_tab == SourceTab::Windows {
            "window"
        } else {
            "system"
        };
        let audio_label = if audio_mode == "window" {
            "Capture window audio"
        } else {
            "Capture system audio"
        };
        let mut capture_audio = self.config.audio_mode == audio_mode;
        ui.horizontal(|ui| {
            let audio_response = ui
                .add_enabled(
                    !stream_active,
                    egui::Checkbox::new(&mut capture_audio, audio_label),
                )
                .on_hover_text(if stream_active {
                    "Stop the stream before changing audio capture."
                } else {
                    "Enable audio for the selected source."
                });
            if audio_response.changed() {
                self.config.audio_mode = if capture_audio {
                    audio_mode.to_owned()
                } else {
                    "off".to_owned()
                };
            }
        });

        if audio_mode != "system" || self.config.audio_mode != "system" {
            return;
        }
        ui.horizontal(|ui| {
            ui.label("Exclude audio");
            let picker_width = (ui.available_width() - 290.0).max(180.0);
            ui.add_enabled_ui(!stream_active, |ui| {
                egui::ComboBox::from_id_salt("audio-process-picker")
                    .selected_text("Add process to ignore list")
                    .width(picker_width)
                    .show_ui(ui, |ui| {
                        for process in available_audio_processes {
                            if self
                                .config
                                .excluded_audio_processes
                                .iter()
                                .any(|excluded| excluded.eq_ignore_ascii_case(process))
                            {
                                continue;
                            }
                            if ui.selectable_label(false, process).clicked() {
                                self.config.excluded_audio_processes.push(process.clone());
                                ui.close();
                            }
                        }
                    });
            });
            if ui
                .add_enabled(!stream_active, egui::Button::new("Add default ignore list"))
                .on_hover_text("Add currently running matches such as Discord or Telegram.")
                .clicked()
            {
                for default_process in DEFAULT_AUDIO_EXCLUSIONS {
                    if available_audio_processes
                        .iter()
                        .any(|process| process.eq_ignore_ascii_case(default_process))
                        && !self
                            .config
                            .excluded_audio_processes
                            .iter()
                            .any(|process| process.eq_ignore_ascii_case(default_process))
                    {
                        self.config
                            .excluded_audio_processes
                            .push((*default_process).to_owned());
                    }
                }
            }
            if ui
                .add_enabled(!stream_active, egui::Button::new("Clear"))
                .on_hover_text("Remove every process from the ignored-audio list.")
                .clicked()
            {
                self.config.excluded_audio_processes.clear();
            }
        });

        if self.config.excluded_audio_processes.is_empty() {
            return;
        }
        ui.label("Ignored");
        let ignored = self.config.excluded_audio_processes.clone();
        let column_count = ((ui.available_width() / 180.0).floor() as usize).clamp(1, 4);
        egui::Grid::new("ignored-audio-processes")
            .num_columns(column_count)
            .spacing(egui::vec2(12.0, 4.0))
            .show(ui, |ui| {
                for (index, process) in ignored.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add_sized([130.0, 20.0], egui::Label::new(process).truncate())
                            .on_hover_text(process);
                        if ui
                            .add_enabled(!stream_active, egui::Button::new("Remove"))
                            .clicked()
                        {
                            self.config
                                .excluded_audio_processes
                                .retain(|value| value != process);
                        }
                    });
                    if (index + 1) % column_count == 0 {
                        ui.end_row();
                    }
                }
                if !ignored.len().is_multiple_of(column_count) {
                    ui.end_row();
                }
            });
    }

    fn draw_test_pattern_preview(&mut self, ui: &mut egui::Ui) {
        if self.test_preview.is_none() {
            self.test_preview = Some(ui.ctx().load_texture(
                "test-pattern-thumbnail",
                Self::baked_test_pattern_thumbnail(),
                egui::TextureOptions::LINEAR,
            ));
        }
        if let Some(texture) = &self.test_preview {
            ui.add_space(10.0);
            let tile_size = egui::vec2(280.0, 158.0);
            ui.allocate_ui_with_layout(
                tile_size,
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.add(
                        egui::Image::from_texture(texture)
                            .fit_to_exact_size(tile_size)
                            .maintain_aspect_ratio(true),
                    );
                },
            );
            let settings = CaptureSettings::from_config(&self.config);
            let (pattern_width, pattern_height) = settings.test_pattern_dimensions();
            let pattern_fps = settings.output_fps.unwrap_or(settings.fps);
            ui.label(format!(
                "Test pattern · {pattern_width} × {pattern_height} · {pattern_fps} FPS"
            ));
            return;
        }
    }

    fn baked_test_pattern_thumbnail() -> egui::ColorImage {
        let decoder = png::Decoder::new(Cursor::new(include_bytes!(
            "../assets/test-pattern-thumbnail.png"
        )));
        let mut reader = decoder
            .read_info()
            .expect("embedded test-pattern thumbnail should decode");
        let mut rgb = vec![
            0_u8;
            reader
                .output_buffer_size()
                .expect("embedded test-pattern thumbnail size should fit in memory")
        ];
        let info = reader
            .next_frame(&mut rgb)
            .expect("embedded test-pattern thumbnail should contain a frame");
        assert_eq!(info.color_type, png::ColorType::Rgb);
        let rgb = &rgb[..info.buffer_size()];
        let mut rgba = Vec::with_capacity(rgb.len() / 3 * 4);
        for pixel in rgb.chunks_exact(3) {
            rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
        }
        egui::ColorImage::from_rgba_unmultiplied([info.width as usize, info.height as usize], &rgba)
    }

    fn poll_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                UiEvent::ServerReady => {
                    self.status = HostStatus::Ready;
                }
                UiEvent::StreamStarted => {
                    self.status = HostStatus::Running;
                }
                UiEvent::StreamStopped => {
                    self.status = HostStatus::Ready;
                }
                UiEvent::StreamFailed(error) => {
                    self.status = HostStatus::StreamFailed(error);
                }
                UiEvent::Failed(error) => {
                    self.status = HostStatus::Failed(error);
                }
                UiEvent::PublicIp(address) => {
                    self.public_ipv4 = Some(address);
                    self.public_ip_error = None;
                }
                UiEvent::PublicIpFailed(_error) => {
                    self.public_ip_error = Some("Public IPv4 lookup failed".to_owned());
                }
                UiEvent::UploadSpeedTestProgress(progress) => {
                    self.network_test_progress = Some(progress);
                    self.config.max_viewers =
                        self.recommended_max_viewers_for_upload(progress.upload_bps);
                }
                UiEvent::UploadSpeedTestFinished(result) => {
                    self.network_test_inflight = false;
                    match result {
                        Ok(upload_bps) => {
                            self.host_network_test = Some(HostNetworkTestResult {
                                upload_bps,
                                tested_at_unix_secs: unix_timestamp(),
                            });
                            self.config.max_viewers =
                                self.recommended_max_viewers_for_upload(upload_bps);
                            self.network_test_error = None;
                        }
                        Err(error) => {
                            self.network_test_error = Some(error);
                        }
                    }
                }
            }
        }
    }

    fn start_upload_speed_test(&mut self) {
        if self.network_test_inflight || self.status.stream_active() {
            return;
        }
        self.network_test_inflight = true;
        self.network_test_error = None;
        self.network_test_progress = None;
        if self.command_tx.send(UiCommand::TestUploadSpeed).is_err() {
            self.network_test_inflight = false;
            self.network_test_error = Some("network test worker stopped".to_owned());
        }
    }

    fn recommended_max_viewers_for_upload(&self, upload_bps: u64) -> usize {
        crate::network::recommended_max_viewers(upload_bps, self.config.effective_bitrate())
    }

    fn upload_speed_label(&self) -> Option<String> {
        self.host_network_test
            .as_ref()
            .map(|result| bitrate_label(result.upload_bps))
    }

    fn selected_viewer_host(&self) -> Option<String> {
        match self.viewer_url_mode {
            ViewerUrlMode::Local => Some("127.0.0.1".to_owned()),
            ViewerUrlMode::Lan => local_ipv4().map(|address| address.to_string()),
            ViewerUrlMode::Public => self.public_ipv4.clone(),
            ViewerUrlMode::Custom => custom_viewer_host(&self.custom_viewer_host),
        }
    }

    fn viewer_url(&self) -> String {
        let host = self
            .selected_viewer_host()
            .unwrap_or_else(|| match self.viewer_url_mode {
                ViewerUrlMode::Lan => "<lan-ip>".to_owned(),
                ViewerUrlMode::Public => "<public-ip>".to_owned(),
                ViewerUrlMode::Custom => "<custom-domain>".to_owned(),
                ViewerUrlMode::Local => "127.0.0.1".to_owned(),
            });
        self.config.viewer_url_for_host(&host)
    }

    fn port_change_pending(&self) -> bool {
        self.port_input
            .parse::<u16>()
            .ok()
            .is_some_and(|port| port != self.config.http_port)
    }

    fn port_input_value(&self) -> Option<u16> {
        self.port_input.parse::<u16>().ok().filter(|port| *port > 0)
    }

    fn draw_primary_action(&mut self, ui: &mut egui::Ui) {
        let stream_running = self.status.stream_active();
        let server_ready = matches!(self.status, HostStatus::Ready | HostStatus::StreamFailed(_));
        let server_alive = self.status.server_alive();
        let source_selected = self.has_valid_source_selection();
        let source_minimized = self.selected_window_is_minimized();
        let source_ready = source_selected && !source_minimized;
        let action = if !server_alive {
            ui.add(egui::Button::new("Retry viewer server"))
                .on_hover_text(
                    "Try binding the viewer server again with the saved endpoint settings.",
                )
        } else if stream_running {
            ui.add_enabled(
                self.status != HostStatus::StoppingStream,
                egui::Button::new(
                    egui::RichText::new("Stop Stream")
                        .strong()
                        .color(egui::Color32::from_rgb(13, 16, 20)),
                )
                .min_size(egui::vec2(160.0, 38.0))
                .fill(egui::Color32::from_rgb(226, 92, 92)),
            )
            .on_hover_text("Stop media delivery while keeping the viewer server online.")
        } else {
            let start_button = ui.add_enabled(
                server_ready && source_ready,
                egui::Button::new(
                    egui::RichText::new("Start Stream")
                        .strong()
                        .color(egui::Color32::from_rgb(13, 16, 20)),
                )
                .min_size(egui::vec2(160.0, 38.0))
                .fill(egui::Color32::from_rgb(201, 243, 107)),
            );
            if !source_selected {
                start_button.on_disabled_hover_text(
                    "Select a monitor, window, or test-pattern source before starting.",
                )
            } else if source_minimized {
                start_button.on_disabled_hover_text(
                    "Restore the selected window before starting its capture.",
                )
            } else if server_ready {
                start_button.on_hover_text("Start media delivery for connected viewers.")
            } else {
                start_button.on_disabled_hover_text("Waiting for the viewer server to start.")
            }
        };
        if !action.clicked() {
            return;
        }
        if !server_alive {
            let _ =
                self.command_tx
                    .send(UiCommand::StartServer(Box::new(server_bootstrap_config(
                        &self.config,
                    ))));
            self.status = HostStatus::StartingServer;
        } else if stream_running {
            let _ = self.command_tx.send(UiCommand::StopStream);
            self.status = HostStatus::StoppingStream;
        } else {
            match self.config.validate() {
                Ok(()) => {
                    self.begin_preview_scope();
                    let _ = self.command_tx.send(UiCommand::StartStream(Box::new(
                        CaptureSettings::from_config(&self.config),
                    )));
                    self.status = HostStatus::StartingStream;
                }
                Err(error) => {
                    self.status =
                        HostStatus::StreamFailed(format!("invalid stream settings: {error}"));
                }
            }
        }
    }

    fn draw_viewer_url_controls(&mut self, ui: &mut egui::Ui) {
        let viewer_url = self.viewer_url();
        let port_change_pending = self.port_change_pending();
        ui.add_space(12.0);
        ui.add_sized(
            [420.0, 38.0],
            egui::Label::new(egui::RichText::new(&viewer_url).monospace())
                .selectable(true)
                .truncate(),
        )
        .on_hover_text("Select the URL or use Copy.");
        let copy_response = ui
            .add_enabled(
                self.selected_viewer_host().is_some() && !port_change_pending,
                egui::Button::new(egui::RichText::new("Copy").strong())
                    .min_size(egui::vec2(92.0, 38.0)),
            )
            .on_hover_text("Copy the current viewer URL.");
        paint_copy_icon(ui, &copy_response);
        if copy_response.clicked() {
            ui.ctx().copy_text(viewer_url);
            self.copied_until = Some(Instant::now() + Duration::from_secs(2));
        }
        let reset_response = ui
            .add_sized([92.0, 38.0], egui::Button::new("Reset Token"))
            .on_hover_text("Generate a new token and invalidate the previous viewer URL.");
        if reset_response.clicked() {
            let token = generate_token();
            self.config.token = token.clone();
            if let Ok(slot) = self.control_slot.lock()
                && let Some(sender) = slot.as_ref()
            {
                let _ = sender.send(server::ServerCommand::ResetToken(token));
            }
        }
        if self
            .copied_until
            .is_some_and(|until| until > Instant::now())
        {
            ui.colored_label(egui::Color32::from_rgb(125, 210, 160), "Copied");
        }
    }
}

#[cfg(windows)]
struct SourceChangeWatcher {
    stop: Arc<AtomicBool>,
    thread_id: u32,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl Drop for SourceChangeWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                self.thread_id,
                windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(windows)]
fn start_source_change_watcher() -> Option<SourceChangeWatcher> {
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent};
    use windows::Win32::UI::WindowsAndMessaging::{
        EVENT_OBJECT_CREATE, EVENT_OBJECT_NAMECHANGE, GetMessageW, MSG, PM_NOREMOVE, PeekMessageW,
        WINEVENT_OUTOFCONTEXT,
    };

    SOURCE_CHANGE_PENDING.store(false, Ordering::Release);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = thread::spawn(move || {
        let thread_id = unsafe { GetCurrentThreadId() };
        let hook = unsafe {
            SetWinEventHook(
                EVENT_OBJECT_CREATE,
                EVENT_OBJECT_NAMECHANGE,
                None,
                Some(source_change_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        if hook.is_invalid() {
            let _ = ready_tx.send(None);
            return;
        }
        let mut message = MSG::default();
        unsafe {
            // Ensure the thread owns a message queue before the UI can try to
            // wake it during shutdown.
            let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
        }
        let _ = ready_tx.send(Some(thread_id));
        loop {
            let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
            if result.0 <= 0 || thread_stop.load(Ordering::Acquire) {
                break;
            }
        }
        unsafe {
            let _ = UnhookWinEvent(hook);
        }
    });
    let Ok(Some(thread_id)) = ready_rx.recv() else {
        stop.store(true, Ordering::Release);
        let _ = thread.join();
        return None;
    };
    Some(SourceChangeWatcher {
        stop,
        thread_id,
        thread: Some(thread),
    })
}

#[cfg(windows)]
unsafe extern "system" fn source_change_event_proc(
    _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    event: u32,
    hwnd: windows::Win32::Foundation::HWND,
    id_object: i32,
    id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    use windows::Win32::UI::WindowsAndMessaging::{
        CHILDID_SELF, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
        EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_SHOW, OBJID_WINDOW,
    };

    if hwnd.0.is_null()
        || id_object != OBJID_WINDOW.0
        || id_child != CHILDID_SELF as i32
        || !matches!(
            event,
            EVENT_OBJECT_CREATE
                | EVENT_OBJECT_DESTROY
                | EVENT_OBJECT_SHOW
                | EVENT_OBJECT_HIDE
                | EVENT_OBJECT_LOCATIONCHANGE
                | EVENT_OBJECT_NAMECHANGE
        )
    {
        return;
    }
    SOURCE_CHANGE_PENDING.store(true, Ordering::Release);
}

fn initial_viewer_url_state(config: &AppConfig) -> (ViewerUrlMode, String, Option<String>) {
    let configured_host = config.advertise_host.as_deref();
    let local_host = local_ipv4().map(|address| address.to_string());
    let mode = match configured_host {
        Some(host) if host == "127.0.0.1" => ViewerUrlMode::Local,
        Some(host) if local_host.as_deref() == Some(host) => ViewerUrlMode::Lan,
        Some(host) if config.bind == "public" && host.parse::<std::net::Ipv4Addr>().is_ok() => {
            ViewerUrlMode::Public
        }
        Some(_) => ViewerUrlMode::Custom,
        None => match config.bind.as_str() {
            "localhost" | "loopback" => ViewerUrlMode::Local,
            "lan" | "all" | "public" => ViewerUrlMode::Public,
            _ => ViewerUrlMode::Custom,
        },
    };
    let custom_host = if mode == ViewerUrlMode::Custom {
        configured_host
            .or_else(|| match config.bind.as_str() {
                "localhost" | "loopback" | "lan" | "public" | "all" => None,
                _ => Some(config.bind.as_str()),
            })
            .unwrap_or_default()
            .to_owned()
    } else {
        String::new()
    };
    let public_ipv4 = (mode == ViewerUrlMode::Public)
        .then(|| configured_host)
        .flatten()
        .filter(|host| host.parse::<std::net::Ipv4Addr>().is_ok())
        .map(str::to_owned);
    (mode, custom_host, public_ipv4)
}

fn custom_viewer_host(value: &str) -> Option<String> {
    let host = value.trim();
    if host.is_empty() || host != value || host.contains("://") {
        return None;
    }
    if host.chars().any(|character| {
        character.is_whitespace() || matches!(character, '/' | '\\' | '?' | '#' | '@' | ':')
    }) {
        return None;
    }
    Some(host.to_owned())
}

fn server_bootstrap_config(config: &AppConfig) -> AppConfig {
    let mut bootstrap = config.clone();
    bootstrap.source.kind = "test".to_owned();
    bootstrap.source.index = 0;
    bootstrap.source.native_id = None;
    bootstrap.audio_mode = "off".to_owned();
    bootstrap
}

fn replace_sources_for_kind(
    sources: &mut Vec<CaptureSourceInfo>,
    kind: &str,
    replacements: Vec<CaptureSourceInfo>,
) {
    sources.retain(|source| source.kind != kind);
    sources.extend(replacements);
}

fn preview_request_needed(
    captured_epoch: Option<u64>,
    requested_epoch: u64,
    in_flight: bool,
    failed: bool,
) -> bool {
    captured_epoch.is_none_or(|captured| captured < requested_epoch) && !in_flight && !failed
}

fn source_signature_matches(
    cached_name: &str,
    cached_size: (u32, u32),
    source: &CaptureSourceInfo,
) -> bool {
    cached_name == source.name && cached_size == (source.width, source.height)
}

fn test_preview_request_needed(
    loaded: Option<TestPreviewSignature>,
    in_flight: Option<(u64, TestPreviewSignature)>,
    failed: Option<TestPreviewSignature>,
    scope: u64,
    requested: TestPreviewSignature,
) -> bool {
    loaded != Some(requested) && in_flight != Some((scope, requested)) && failed != Some(requested)
}

fn prune_preview_cache(
    cache: &mut HashMap<PreviewKey, CachedPreview>,
    maximum: usize,
    protected: &HashSet<PreviewKey>,
) {
    while cache.len() > maximum {
        let Some(oldest) = cache
            .iter()
            .filter(|(key, _)| !protected.contains(*key))
            .min_by_key(|(_, preview)| preview.last_used)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        cache.remove(&oldest);
    }
}

fn source_selection_is_valid(
    source_selected: bool,
    source_kind: &str,
    source_index: usize,
    source_native_id: Option<u64>,
    sources: &[CaptureSourceInfo],
) -> bool {
    source_selected
        && (source_kind == "test"
            || sources.iter().any(|source| {
                source_matches_selection(source, source_kind, source_index, source_native_id)
            }))
}

fn source_matches_selection(
    source: &CaptureSourceInfo,
    source_kind: &str,
    source_index: usize,
    source_native_id: Option<u64>,
) -> bool {
    source.kind == source_kind
        && match source_native_id {
            Some(native_id) => source.native_id == Some(native_id),
            None => source.index == source_index,
        }
}

fn source_index_should_update(
    current_kind: &str,
    current_native_id: Option<u64>,
    next_kind: &str,
    next_native_id: Option<u64>,
) -> bool {
    !(current_kind == "window"
        && next_kind == "window"
        && next_native_id.is_some()
        && current_native_id == next_native_id)
}

fn retain_selected_native_source(
    sources: &mut Vec<CaptureSourceInfo>,
    previous_sources: &[CaptureSourceInfo],
    source_selected: bool,
    source_kind: &str,
    source_native_id: Option<u64>,
    native_window_exists: bool,
    display_non_window_elements: bool,
) {
    let Some(native_id) = source_native_id else {
        return;
    };
    if !source_selected
        || source_kind != "window"
        || !native_window_exists
        || sources
            .iter()
            .any(|source| source.native_id == Some(native_id))
    {
        return;
    }
    if !display_non_window_elements && capture::native_window_is_non_window_element(native_id) {
        return;
    }
    if let Some(previous) = previous_sources
        .iter()
        .find(|source| source.native_id == Some(native_id))
    {
        sources.push(previous.clone());
    }
}

fn retain_temporarily_unenumerated_windows(
    sources: &mut Vec<CaptureSourceInfo>,
    previous_sources: &[CaptureSourceInfo],
    display_non_window_elements: bool,
    mut native_window_is_available: impl FnMut(u64) -> bool,
) {
    let mut present = sources
        .iter()
        .filter_map(|source| source.native_id)
        .collect::<HashSet<_>>();
    for previous in previous_sources {
        if previous.kind != "window" {
            continue;
        }
        let Some(native_id) = previous.native_id else {
            continue;
        };
        if present.contains(&native_id)
            || !native_window_is_available(native_id)
            || (!display_non_window_elements
                && capture::native_window_is_non_window_element(native_id))
        {
            continue;
        }
        sources.push(previous.clone());
        present.insert(native_id);
    }
}

fn source_worker_loop(
    request_rx: mpsc::Receiver<SourceRequest>,
    event_tx: mpsc::SyncSender<PreviewEvent>,
    repaint_context: egui::Context,
) {
    while let Ok(request) = request_rx.recv() {
        let SourceRequest::Discover {
            generation,
            kind,
            display_non_window_elements,
        } = request
        else {
            break;
        };
        let event =
            match capture::list_sources_for_kind_with_options(kind, display_non_window_elements) {
                Ok(sources) => PreviewEvent::SourcesDiscovered {
                    generation,
                    kind,
                    sources,
                },
                Err(error) => PreviewEvent::SourceDiscoveryFailed {
                    generation,
                    kind,
                    error: error.to_string(),
                },
            };
        if !send_preview_event(&event_tx, &repaint_context, event) {
            break;
        }
    }
}

fn preview_worker_loop(
    request_rx: mpsc::Receiver<PreviewRequest>,
    event_tx: mpsc::SyncSender<PreviewEvent>,
    active_scope: Arc<AtomicU64>,
    repaint_context: egui::Context,
) {
    while let Ok(request) = request_rx.recv() {
        match request {
            PreviewRequest::Source {
                scope,
                epoch,
                key,
                source,
            } => {
                if scope != active_scope.load(Ordering::Acquire) {
                    continue;
                }
                let result = capture::capture_preview(&source, 320, 180).map(preview_color_image);
                if scope != active_scope.load(Ordering::Acquire) {
                    continue;
                }
                let event = match result {
                    Ok(image) => PreviewEvent::SourceReady {
                        scope,
                        epoch,
                        key,
                        source_size: (source.width, source.height),
                        source_name: source.name.clone(),
                        image,
                    },
                    Err(error) => {
                        tracing::debug!(
                            source = %source.name,
                            %error,
                            "source preview capture unavailable"
                        );
                        PreviewEvent::SourceFailed {
                            scope,
                            epoch,
                            key,
                            sticky: true,
                        }
                    }
                };
                if !send_preview_event(&event_tx, &repaint_context, event) {
                    break;
                }
            }
            PreviewRequest::LiveSource {
                scope,
                epoch,
                key,
                source,
                snapshot_rx,
            } => {
                if scope != active_scope.load(Ordering::Acquire) {
                    continue;
                }
                let snapshot = snapshot_rx
                    .recv_timeout(Duration::from_millis(5_500))
                    .map_err(|error| anyhow::anyhow!("live preview snapshot unavailable: {error}"))
                    .and_then(|snapshot| snapshot.context("live capture has no current frame"));
                if scope != active_scope.load(Ordering::Acquire) {
                    continue;
                }
                let result = snapshot
                    .and_then(|snapshot| {
                        anyhow::ensure!(
                            snapshot_matches_source(&snapshot, &source),
                            "live capture source changed before preview delivery"
                        );
                        capture::capture_frame_preview(&snapshot.frame, 320, 180)
                    })
                    .map(preview_color_image);
                let event = match result {
                    Ok(image) => PreviewEvent::SourceReady {
                        scope,
                        epoch,
                        key,
                        source_size: (source.width, source.height),
                        source_name: source.name.clone(),
                        image,
                    },
                    Err(error) => {
                        tracing::debug!(
                            source = %source.name,
                            %error,
                            "live source preview unavailable"
                        );
                        PreviewEvent::SourceFailed {
                            scope,
                            epoch,
                            key,
                            sticky: false,
                        }
                    }
                };
                if !send_preview_event(&event_tx, &repaint_context, event) {
                    break;
                }
            }
            PreviewRequest::TestPattern { scope, signature } => {
                if scope != active_scope.load(Ordering::Acquire) {
                    continue;
                }
                let result = crate::packaging::prepare_ffmpeg().and_then(|ffmpeg| {
                    capture::capture_test_pattern_preview(
                        &ffmpeg.command,
                        signature.width,
                        signature.height,
                        signature.fps,
                        320,
                        180,
                    )
                });
                if scope != active_scope.load(Ordering::Acquire) {
                    continue;
                }
                let event = match result {
                    Ok(preview) => PreviewEvent::TestPatternReady {
                        scope,
                        signature,
                        image: preview_color_image(preview),
                    },
                    Err(error) => PreviewEvent::TestPatternFailed {
                        scope,
                        signature,
                        error: error.to_string(),
                    },
                };
                if !send_preview_event(&event_tx, &repaint_context, event) {
                    break;
                }
            }
            PreviewRequest::Shutdown => break,
        }
    }
}

fn preview_color_image(preview: CapturePreview) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied([preview.width, preview.height], &preview.rgba)
}

fn snapshot_matches_source(
    snapshot: &server::CapturePreviewSnapshot,
    source: &CaptureSourceInfo,
) -> bool {
    snapshot.settings.source_kind == source.kind
        && match (snapshot.settings.source_native_id, source.native_id) {
            (Some(active), Some(requested)) => active == requested,
            (None, None) => snapshot.settings.source_index == source.index,
            _ => false,
        }
}

fn send_preview_event(
    event_tx: &mpsc::SyncSender<PreviewEvent>,
    repaint_context: &egui::Context,
    event: PreviewEvent,
) -> bool {
    if event_tx.send(event).is_err() {
        return false;
    }
    repaint_context.request_repaint();
    true
}

fn quality_label(value: &str) -> String {
    match value {
        "source" => "Source".to_owned(),
        "720p" | "1080p" | "1440p" => format!("{value} HD"),
        "2160p" => "2160p 4K".to_owned(),
        "4320p" => "4320p 8K".to_owned(),
        other => other.to_owned(),
    }
}

fn selected_quality_label(value: &str, source: Option<(u32, u32)>) -> String {
    if value == "source"
        && let Some((width, height)) = source
    {
        return format!("Source · {width} × {height}");
    }
    quality_label(value)
}

fn quality_option_label(value: &str, source: Option<(u32, u32)>) -> String {
    if value == "source" {
        selected_quality_label(value, source)
    } else {
        quality_label(value)
    }
}

fn fps_label(value: &str) -> String {
    if value == "source" {
        "Source".to_owned()
    } else {
        format!("{value} FPS")
    }
}

fn selected_fps_label(value: &str, source_fps: Option<u32>) -> String {
    if value == "source"
        && let Some(fps) = source_fps
    {
        return format!("Source · {fps} FPS");
    }
    fps_label(value)
}

fn fps_option_label(value: &str, source_fps: Option<u32>) -> String {
    if value == "source" {
        selected_fps_label(value, source_fps)
    } else {
        fps_label(value)
    }
}

fn quality_mode_label(value: &str) -> &'static str {
    if value == "adaptive" {
        "Auto"
    } else {
        "Manual"
    }
}

fn bitrate_mode_label(value: &str) -> &'static str {
    if value == "automatic" {
        "Auto"
    } else {
        "Manual"
    }
}

fn codec_label(value: &str) -> &'static str {
    match value.to_ascii_lowercase().as_str() {
        "auto" => "Auto",
        "vp8" => "VP8",
        "vp9" => "VP9",
        "h264" => "H.264",
        _ => "Auto",
    }
}

fn latency_preference_label(value: &str) -> &'static str {
    match value {
        "low" => "Low",
        "balanced" => "Balanced",
        "quality" => "Quality",
        _ => "Balanced",
    }
}

fn bitrate_label(bits_per_second: u64) -> String {
    if bits_per_second >= 1_000_000 {
        format!("{:.1} Mbps", bits_per_second as f64 / 1_000_000.0)
    } else {
        format!("{} Kbps", bits_per_second / 1_000)
    }
}

fn paint_copy_icon(ui: &egui::Ui, response: &egui::Response) {
    let color = ui.style().interact(response).text_color();
    let icon_origin = response.rect.left_center() + egui::vec2(12.0, 0.0);
    let rear =
        egui::Rect::from_center_size(icon_origin + egui::vec2(2.0, -2.0), egui::vec2(8.0, 10.0));
    let front =
        egui::Rect::from_center_size(icon_origin + egui::vec2(0.0, 2.0), egui::vec2(8.0, 10.0));
    let stroke = egui::Stroke::new(1.2, color);
    ui.painter()
        .rect_stroke(rear, 1.0, stroke, egui::StrokeKind::Inside);
    ui.painter()
        .rect_stroke(front, 1.0, stroke, egui::StrokeKind::Inside);
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

impl eframe::App for HostUi {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.refresh_sources_if_due();
        let previous_settings = CaptureSettings::from_config(&self.config);
        let source_details = if self.source_tab == SourceTab::TestPattern {
            let settings = CaptureSettings::from_config(&self.config);
            let (width, height) = settings.test_pattern_dimensions();
            Some((width, height, settings.output_fps.or(Some(settings.fps))))
        } else {
            self.sources
                .iter()
                .find(|source| {
                    source_matches_selection(
                        source,
                        &self.config.source.kind,
                        self.config.source.index,
                        self.config.source.native_id,
                    )
                })
                .map(|source| (source.width, source.height, source.fps))
        };
        let source_height_limit = (self.source_tab != SourceTab::TestPattern)
            .then(|| source_details.map(|(_, height, _)| height))
            .flatten();
        let source_fps_limit = (self.source_tab != SourceTab::TestPattern)
            .then(|| source_details.and_then(|(_, _, fps)| fps))
            .flatten();
        let available_audio_processes = self.available_audio_processes();
        self.poll_events();
        let stream_active = self.status.stream_active();
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));

        egui::Panel::bottom("operation-share-bar").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                self.draw_primary_action(ui);
                self.draw_viewer_url_controls(ui);
            });
            let status_label = self.status.label();
            if !status_label.is_empty() {
                ui.add(egui::Label::new(egui::RichText::new(status_label).strong()).wrap());
            }
        });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("host-content")
                .auto_shrink([false, false])
                .show(ui, |ui| {
            self.draw_source_picker(ui);
            ui.separator();

            ui.columns(2, |columns| {
            let (capture_columns, host_columns) = columns.split_at_mut(1);
            let ui = &mut capture_columns[0];
            ui.heading("Capture settings");
            egui::Grid::new("settings").num_columns(2).show(ui, |ui| {
                ui.label("Cursor");
                ui.checkbox(&mut self.config.draw_mouse, "Capture pointer");
                ui.end_row();

                ui.label("Quality mode");
                egui::ComboBox::from_id_salt("quality-mode")
                    .selected_text(quality_mode_label(&self.config.quality_mode))
                    .show_ui(ui, |ui| {
                        for (value, label) in [("manual", "Manual"), ("adaptive", "Auto")] {
                            ui.selectable_value(
                                &mut self.config.quality_mode,
                                value.to_owned(),
                                label,
                            );
                        }
                    });
                ui.end_row();

                if self.config.quality_mode == "adaptive" {
                    ui.label("Viewer quality groups");
                    egui::ComboBox::from_id_salt("quality-groups")
                        .selected_text(&self.config.max_quality_groups)
                        .show_ui(ui, |ui| {
                            for groups in 1..=4 {
                                let value = groups.to_string();
                                ui.selectable_value(
                                    &mut self.config.max_quality_groups,
                                    value.clone(),
                                    value,
                                );
                            }
                        });
                    ui.end_row();

                }

                ui.label("Codec");
                egui::ComboBox::from_id_salt("codec")
                    .selected_text(codec_label(&self.config.codec))
                    .show_ui(ui, |ui| {
                        for value in ["auto", "vp8", "vp9", "h264"] {
                            ui.selectable_value(
                                &mut self.config.codec,
                                value.to_owned(),
                                codec_label(value),
                            );
                        }
                    });
                ui.end_row();

                let adaptive_mode = self.config.quality_mode == "adaptive";
                let mut selected_quality = if adaptive_mode {
                    self.config.adaptive_quality_ceiling.clone()
                } else {
                    self.config.quality.clone()
                };
                ui.label(if adaptive_mode {
                    "Quality ceiling"
                } else {
                    "Resolution"
                });
                egui::ComboBox::from_id_salt("quality")
                    .selected_text(selected_quality_label(
                        &selected_quality,
                        source_details.map(|(width, height, _)| (width, height)),
                    ))
                    .show_ui(ui, |ui| {
                        for value in QUALITY_PRESETS {
                            if self.source_tab == SourceTab::TestPattern && *value == "source" {
                                continue;
                            }
                            if source_height_limit.is_some_and(|limit| {
                                quality_height(value).is_some_and(|height| height > limit)
                            }) {
                                continue;
                            }
                            ui.selectable_value(
                                &mut selected_quality,
                                (*value).to_owned(),
                                quality_option_label(
                                    value,
                                    source_details.map(|(width, height, _)| (width, height)),
                                ),
                            );
                        }
                    });
                if adaptive_mode {
                    self.config.adaptive_quality_ceiling = selected_quality;
                } else {
                    self.config.quality = selected_quality;
                }
                ui.end_row();

                let mut selected_fps = if adaptive_mode {
                    self.config.adaptive_fps_ceiling.clone()
                } else {
                    self.config.fps_preset.clone()
                };
                ui.label(if adaptive_mode {
                    "Max FPS"
                } else {
                    "Frame rate"
                });
                egui::ComboBox::from_id_salt("fps")
                    .selected_text(selected_fps_label(
                        &selected_fps,
                        source_details.and_then(|(_, _, fps)| fps),
                    ))
                    .show_ui(ui, |ui| {
                        for value in FPS_PRESETS {
                            if self.source_tab == SourceTab::TestPattern && *value == "source" {
                                continue;
                            }
                            if source_fps_limit.is_some_and(|limit| {
                                fps_value(value).is_some_and(|fps| fps > limit)
                            }) {
                                continue;
                            }
                            ui.selectable_value(
                                &mut selected_fps,
                                (*value).to_owned(),
                                fps_option_label(value, source_details.and_then(|(_, _, fps)| fps)),
                            );
                        }
                    });
                if adaptive_mode {
                    self.config.adaptive_fps_ceiling = selected_fps;
                } else {
                    self.config.fps_preset = selected_fps;
                }
                ui.end_row();

                if self.config.bitrate_mode == "automatic" {
                    ui.label("Latency preference").on_hover_text(
                        "Controls the automatic bitrate target: Low favors responsiveness and bandwidth efficiency; Quality favors image detail.",
                    );
                    egui::ComboBox::from_id_salt("latency-preference")
                        .selected_text(latency_preference_label(&self.config.latency_preference))
                        .show_ui(ui, |ui| {
                            for value in ["low", "balanced", "quality"] {
                                ui.selectable_value(
                                    &mut self.config.latency_preference,
                                    value.to_owned(),
                                    latency_preference_label(value),
                                );
                            }
                    });
                    ui.end_row();
                }

                ui.label("Bitrate mode");
                egui::ComboBox::from_id_salt("bitrate-mode")
                    .selected_text(bitrate_mode_label(&self.config.bitrate_mode))
                    .show_ui(ui, |ui| {
                        for (value, label) in [("fixed", "Manual"), ("automatic", "Auto")] {
                            ui.selectable_value(
                                &mut self.config.bitrate_mode,
                                value.to_owned(),
                                label,
                            );
                        }
                    });
                ui.end_row();

                ui.label("Bitrate");
                if self.config.bitrate_mode == "fixed" {
                    ui.add(
                        egui::DragValue::new(&mut self.config.bitrate).range(250_000..=50_000_000),
                    );
                } else {
                    ui.label(format!(
                        "Auto · {}",
                        bitrate_label(self.config.effective_bitrate().into())
                    ));
                }
                ui.end_row();
            });
            if self.source_tab != SourceTab::TestPattern {
                self.draw_audio_settings(ui, stream_active, &available_audio_processes);
            } else {
                ui.label("Audio is unavailable for the test pattern.");
            }

            let ui = &mut host_columns[0];
            ui.heading("Host Settings");
            let upload_speed = self.upload_speed_label();
            egui::Grid::new("viewer-capacity-settings")
                .num_columns(2)
                .spacing([8.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Max Viewers");
                    ui.horizontal_wrapped(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.config.max_viewers)
                                .range(1..=256)
                                .speed(0.2),
                        );
                        if self.network_test_inflight {
                            ui.add(egui::Spinner::new());
                            ui.label("Testing...");
                            if let Some(progress) = self.network_test_progress {
                                ui.label(bitrate_label(progress.upload_bps));
                            }
                        } else {
                            let retest_response = ui
                                .add_enabled(
                                    !self.status.stream_active(),
                                    egui::Button::new("↻"),
                                )
                                .on_hover_text(
                                    "Retest upload speed and update the viewer limit automatically.",
                                );
                            if retest_response.clicked() {
                                self.start_upload_speed_test();
                            }
                            if let Some(upload) = upload_speed {
                                ui.label(format!("Measured: {upload}"));
                            } else {
                                ui.label("No upload result yet");
                            }
                        }
                    });
                    ui.end_row();
                    if let Some(error) = self.network_test_error.as_deref() {
                        ui.label("");
                        ui.colored_label(
                            egui::Color32::from_rgb(210, 170, 80),
                            format!("Upload test unavailable: {error}"),
                        );
                        ui.end_row();
                    }
                });
            ui.horizontal(|ui| {
                ui.label("Share URL host");
                for (mode, label, tooltip) in [
                    (
                        ViewerUrlMode::Local,
                        "Local",
                        "Use 127.0.0.1; only this computer can open the URL.",
                    ),
                    (
                        ViewerUrlMode::Lan,
                        "LAN",
                        "Use this computer's local IPv4 address for viewers on the same network.",
                    ),
                    (
                        ViewerUrlMode::Public,
                        "Public",
                        "Use the discovered public IPv4 address. Port forwarding may be required.",
                    ),
                    (
                        ViewerUrlMode::Custom,
                        "Custom",
                        "Use a custom HTTP hostname or IPv4 address.",
                    ),
                ] {
                    let response = ui
                        .selectable_label(self.viewer_url_mode == mode, label)
                        .on_hover_text(tooltip);
                    if response.clicked() {
                        self.viewer_url_mode = mode;
                    }
                }
            });
            if self.viewer_url_mode == ViewerUrlMode::Custom {
                ui.horizontal(|ui| {
                    ui.label("Custom domain");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.custom_viewer_host)
                            .desired_width(260.0)
                            .hint_text("example.com"),
                    );
                });
            } else if self.viewer_url_mode == ViewerUrlMode::Public && self.public_ipv4.is_none() {
                let message = self
                    .public_ip_error
                    .as_deref()
                    .unwrap_or("Looking up public IPv4…");
                ui.colored_label(egui::Color32::from_rgb(210, 170, 80), message);
            }
            egui::Grid::new("share-endpoint-settings")
                .num_columns(2)
                .spacing([8.0, 6.0])
                .show(ui, |ui| {
                    ui.label("HTTP port");
                    ui.horizontal(|ui| {
                        let port_editable = matches!(
                            self.status,
                            HostStatus::Ready
                                | HostStatus::StreamFailed(_)
                                | HostStatus::Failed(_)
                        );
                        let port_response = ui.add_enabled(
                            port_editable,
                            egui::TextEdit::singleline(&mut self.port_input)
                                .desired_width(90.0)
                                .char_limit(5),
                        );
                        if port_response.changed() {
                            self.port_input.retain(|character| character.is_ascii_digit());
                        }
                        let port_value = self.port_input_value();
                        if port_value.is_none() {
                            ui.colored_label(
                                egui::Color32::from_rgb(210, 170, 80),
                                "Enter a port from 1 to 65535.",
                            );
                        } else if port_value != Some(self.config.http_port) {
                            if self.status.stream_active() {
                                ui.colored_label(
                                    egui::Color32::from_rgb(210, 170, 80),
                                    "Stop the stream before applying the new port.",
                                );
                            } else if ui.button("Apply port").clicked() {
                                let port = port_value.expect("validated port input");
                                self.config.http_port = port;
                                self.port_input = port.to_string();
                                let _ = self.command_tx.send(UiCommand::StartServer(Box::new(
                                    server_bootstrap_config(&self.config),
                                )));
                                self.status = HostStatus::StartingServer;
                            }
                        }
                    });
                    ui.end_row();
                });
            });
                });
        });

        let preferences_snapshot =
            UserPreferences::from_config(&self.config, self.host_network_test.clone());
        if self.last_saved_preferences.as_ref() != Some(&preferences_snapshot) {
            if let Err(error) = preferences::save(&self.config, self.host_network_test.clone()) {
                tracing::debug!(%error, "could not persist UI preferences");
            }
            self.last_saved_preferences = Some(preferences_snapshot);
        }

        if self.status == HostStatus::Running {
            let current_settings = CaptureSettings::from_config(&self.config);
            if current_settings != previous_settings
                && let Ok(slot) = self.control_slot.lock()
                && let Some(sender) = slot.as_ref()
            {
                let _ = sender.send(server::ServerCommand::Update(current_settings));
            }
        }
        if self.status.server_alive()
            && self.last_sent_max_viewers != Some(self.config.max_viewers)
            && let Ok(slot) = self.control_slot.lock()
            && let Some(sender) = slot.as_ref()
        {
            if sender
                .send(server::ServerCommand::UpdateMaxViewers(
                    self.config.max_viewers,
                ))
                .is_ok()
            {
                self.last_sent_max_viewers = Some(self.config.max_viewers);
            }
        }
    }

    fn on_exit(&mut self) {
        self.preview_scope.fetch_add(1, Ordering::AcqRel);
        let _ = self.source_request_tx.try_send(SourceRequest::Shutdown);
        let _ = self.preview_request_tx.try_send(PreviewRequest::Shutdown);
        let _ = self.command_tx.send(UiCommand::Shutdown);
    }
}

fn worker_loop(
    command_rx: mpsc::Receiver<UiCommand>,
    event_tx: mpsc::Sender<UiEvent>,
    shutdown_slot: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    control_slot: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<server::ServerCommand>>>>,
) {
    while let Ok(command) = command_rx.recv() {
        match command {
            UiCommand::StartServer(config) => {
                let config = *config;
                let existing_shutdown = shutdown_slot.lock().ok().and_then(|mut slot| slot.take());
                if let Some(shutdown) = existing_shutdown {
                    let _ = shutdown.send(());
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let mut stop_timed_out = false;
                    loop {
                        let shutdown_finished = shutdown_slot
                            .lock()
                            .map(|slot| slot.is_none())
                            .unwrap_or(false);
                        let control_finished = control_slot
                            .lock()
                            .map(|slot| slot.is_none())
                            .unwrap_or(false);
                        if shutdown_finished && control_finished {
                            break;
                        }
                        if Instant::now() >= deadline {
                            let _ = event_tx.send(UiEvent::Failed(
                                "viewer server did not stop before the port change".to_owned(),
                            ));
                            stop_timed_out = true;
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    if stop_timed_out {
                        continue;
                    }
                }
                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
                if let Ok(mut slot) = shutdown_slot.lock() {
                    *slot = Some(shutdown_tx);
                }
                if let Ok(mut slot) = control_slot.lock() {
                    *slot = Some(control_tx);
                }
                let event_tx = event_tx.clone();
                let thread_shutdown = Arc::clone(&shutdown_slot);
                let thread_control = Arc::clone(&control_slot);
                thread::spawn(move || {
                    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                    let ready_events = event_tx.clone();
                    let ready_waiter = thread::spawn(move || {
                        if ready_rx.blocking_recv().is_ok() {
                            let _ = ready_events.send(UiEvent::ServerReady);
                        }
                    });
                    let stream_failure_events = event_tx.clone();
                    let stream_failure_callback: server::StreamFailureCallback =
                        Arc::new(move |error| {
                            let _ = stream_failure_events.send(UiEvent::StreamFailed(error));
                        });
                    let result = tokio::runtime::Runtime::new()
                        .context("create Tokio runtime")
                        .and_then(|runtime| {
                            runtime.block_on(server::run_with_control_readiness(
                                config,
                                shutdown_rx,
                                control_rx,
                                Some(ready_tx),
                                Some(stream_failure_callback),
                            ))
                        });
                    let _ = ready_waiter.join();
                    if let Ok(mut slot) = thread_shutdown.lock() {
                        *slot = None;
                    }
                    if let Ok(mut slot) = thread_control.lock() {
                        *slot = None;
                    }
                    match result {
                        Ok(()) => {
                            let _ = event_tx.send(UiEvent::StreamStopped);
                        }
                        Err(error) => {
                            let _ =
                                event_tx.send(UiEvent::Failed(format!("server failed: {error}")));
                        }
                    }
                });
            }
            UiCommand::StartStream(settings) => {
                let settings = *settings;
                if let Ok(slot) = control_slot.lock()
                    && let Some(sender) = slot.as_ref()
                {
                    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                    if sender
                        .send(server::ServerCommand::StartStream {
                            settings,
                            result: Some(result_tx),
                        })
                        .is_err()
                    {
                        let _ = event_tx.send(UiEvent::Failed(
                            "viewer server stopped unexpectedly".to_owned(),
                        ));
                        continue;
                    }
                    let mut result_rx = result_rx;
                    let deadline = Instant::now() + STREAM_START_ACK_TIMEOUT;
                    loop {
                        match result_rx.try_recv() {
                            Ok(Ok(())) => {
                                let _ = event_tx.send(UiEvent::StreamStarted);
                                break;
                            }
                            Ok(Err(error)) => {
                                let _ = event_tx.send(UiEvent::StreamFailed(format!(
                                    "could not start stream: {error}"
                                )));
                                break;
                            }
                            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                                let _ = event_tx.send(UiEvent::StreamFailed(
                                    "viewer server stopped before starting the stream".to_owned(),
                                ));
                                break;
                            }
                            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                                if Instant::now() < deadline =>
                            {
                                thread::sleep(Duration::from_millis(20));
                            }
                            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                                let _ = event_tx.send(UiEvent::StreamFailed(format!(
                                    "stream start exceeded {} seconds",
                                    STREAM_START_ACK_TIMEOUT.as_secs()
                                )));
                                break;
                            }
                        }
                    }
                } else {
                    let _ =
                        event_tx.send(UiEvent::Failed("viewer server is not ready yet".to_owned()));
                }
            }
            UiCommand::StopStream => {
                if let Ok(slot) = control_slot.lock()
                    && let Some(sender) = slot.as_ref()
                {
                    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                    if sender
                        .send(server::ServerCommand::StopStream {
                            result: Some(result_tx),
                        })
                        .is_err()
                    {
                        let _ = event_tx.send(UiEvent::Failed(
                            "viewer server stopped before stopping the stream".to_owned(),
                        ));
                    } else {
                        let mut result_rx = result_rx;
                        let deadline = Instant::now() + STREAM_STOP_ACK_TIMEOUT;
                        loop {
                            match result_rx.try_recv() {
                                Ok(()) => {
                                    let _ = event_tx.send(UiEvent::StreamStopped);
                                    break;
                                }
                                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                                    let _ = event_tx.send(UiEvent::StreamFailed(
                                        "viewer server stopped before acknowledging stream stop"
                                            .to_owned(),
                                    ));
                                    break;
                                }
                                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                                    if Instant::now() < deadline =>
                                {
                                    thread::sleep(Duration::from_millis(20));
                                }
                                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                                    let _ = event_tx.send(UiEvent::StreamFailed(format!(
                                        "stream stop exceeded {} seconds; capture termination was forced",
                                        STREAM_STOP_ACK_TIMEOUT.as_secs()
                                    )));
                                    break;
                                }
                            }
                        }
                    }
                } else {
                    let _ =
                        event_tx.send(UiEvent::Failed("viewer server is not running".to_owned()));
                }
            }
            UiCommand::Shutdown => {
                if let Ok(mut slot) = shutdown_slot.lock()
                    && let Some(sender) = slot.take()
                {
                    let _ = sender.send(());
                }
                if let Ok(mut slot) = control_slot.lock() {
                    slot.take();
                }
            }
            UiCommand::TestUploadSpeed => {
                let progress_events = event_tx.clone();
                let result =
                    crate::network::measure_cloudflare_upload_bps_with_progress(move |progress| {
                        let _ = progress_events.send(UiEvent::UploadSpeedTestProgress(progress));
                    })
                    .map_err(|error| error.to_string());
                let _ = event_tx.send(UiEvent::UploadSpeedTestFinished(result));
            }
            UiCommand::LookupPublicIp => match crate::network::lookup_public_ipv4() {
                Ok(address) => {
                    let _ = event_tx.send(UiEvent::PublicIp(address.to_string()));
                }
                Err(error) => {
                    let _ = event_tx.send(UiEvent::PublicIpFailed(error.to_string()));
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_preview_cache_key_ignores_z_order_and_size_but_validates_process() {
        let source = CaptureSourceInfo {
            kind: "window".to_owned(),
            index: 2,
            native_id: Some(42),
            width: 800,
            height: 600,
            fps: Some(60),
            pid: Some(100),
            name: "Editor: document".to_owned(),
        };
        let key = PreviewKey::for_source(&source);

        let mut reordered = source.clone();
        reordered.index = 99;
        assert_eq!(key, PreviewKey::for_source(&reordered));

        let mut reused_handle = source.clone();
        reused_handle.pid = Some(200);
        assert_ne!(key, PreviewKey::for_source(&reused_handle));

        let mut resized = source;
        resized.width = 1_024;
        assert_eq!(key, PreviewKey::for_source(&resized));
        assert!(!source_signature_matches(
            "Editor: document",
            (800, 600),
            &resized
        ));

        let renamed = CaptureSourceInfo {
            width: 800,
            name: "Editor: other document".to_owned(),
            ..resized
        };
        assert_eq!(key, PreviewKey::for_source(&renamed));
        assert!(!source_signature_matches(
            "Editor: document",
            (800, 600),
            &renamed
        ));
    }

    #[test]
    fn preview_request_is_cache_inflight_and_retry_aware() {
        assert!(preview_request_needed(None, 1, false, false));
        assert!(!preview_request_needed(Some(1), 1, false, false));
        assert!(preview_request_needed(Some(1), 2, false, false));
        assert!(!preview_request_needed(None, 1, true, false));
        assert!(!preview_request_needed(None, 1, false, true));
    }

    #[test]
    fn failed_test_preview_waits_for_settings_change_or_manual_reset() {
        let original = TestPreviewSignature {
            width: 1_920,
            height: 1_080,
            fps: 60,
        };
        let changed = TestPreviewSignature {
            fps: 30,
            ..original
        };

        assert!(!test_preview_request_needed(
            None,
            None,
            Some(original),
            1,
            original
        ));
        assert!(test_preview_request_needed(
            None,
            None,
            Some(original),
            1,
            changed
        ));
        assert!(test_preview_request_needed(None, None, None, 2, original));
    }

    #[test]
    fn preview_cache_has_a_hard_bound_and_keeps_protected_entries() {
        let context = egui::Context::default();
        let image = egui::ColorImage::from_rgba_unmultiplied([1, 1], &[1, 2, 3, 255]);
        let started = Instant::now();
        let mut cache = HashMap::new();
        let mut keys = Vec::new();
        for index in 0..(MAX_CACHED_PREVIEWS + 2) {
            let source = CaptureSourceInfo {
                kind: "monitor".to_owned(),
                index,
                native_id: None,
                width: 1_920,
                height: 1_080,
                fps: Some(60),
                pid: None,
                name: format!("Display {index}"),
            };
            let key = PreviewKey::for_source(&source);
            let texture = context.load_texture(
                key.texture_name(),
                image.clone(),
                egui::TextureOptions::LINEAR,
            );
            cache.insert(
                key.clone(),
                CachedPreview {
                    texture,
                    captured_epoch: 1,
                    source_size: (source.width, source.height),
                    source_name: source.name.clone(),
                    last_used: started + Duration::from_millis(index as u64),
                },
            );
            keys.push(key);
        }
        let protected = HashSet::from([keys[0].clone()]);

        prune_preview_cache(&mut cache, MAX_CACHED_PREVIEWS, &protected);

        assert_eq!(cache.len(), MAX_CACHED_PREVIEWS);
        assert!(cache.contains_key(&keys[0]));
        assert!(!cache.contains_key(&keys[1]));
        assert!(!cache.contains_key(&keys[2]));
    }

    #[test]
    fn reselecting_the_same_native_window_keeps_its_legacy_index_stable() {
        assert!(!source_index_should_update(
            "window",
            Some(42),
            "window",
            Some(42)
        ));
        assert!(source_index_should_update(
            "window",
            Some(42),
            "window",
            Some(99)
        ));
        assert!(source_index_should_update(
            "monitor",
            None,
            "window",
            Some(42)
        ));
    }

    #[test]
    fn live_snapshot_must_match_the_requested_native_source() {
        let mut config = AppConfig::default();
        config.source.kind = "window".to_owned();
        config.source.index = 7;
        config.source.native_id = Some(42);
        let snapshot = server::CapturePreviewSnapshot {
            settings: CaptureSettings::from_config(&config),
            frame: crate::shared_capture::SourceFrame {
                width: 2,
                height: 2,
                pixel_format: crate::shared_capture::SourcePixelFormat::Bgra,
                captured_at_unix_nanos: 0,
                data: Arc::from(vec![0_u8; 16]),
            },
        };
        let source = CaptureSourceInfo {
            kind: "window".to_owned(),
            index: 99,
            native_id: Some(42),
            width: 800,
            height: 600,
            fps: Some(60),
            pid: Some(100),
            name: "Selected".to_owned(),
        };

        assert!(snapshot_matches_source(&snapshot, &source));
        let other = CaptureSourceInfo {
            native_id: Some(99),
            ..source
        };
        assert!(!snapshot_matches_source(&snapshot, &other));
    }

    #[test]
    fn per_kind_discovery_preserves_the_other_source_class() {
        let monitor = CaptureSourceInfo {
            kind: "monitor".to_owned(),
            index: 0,
            native_id: None,
            width: 1_920,
            height: 1_080,
            fps: Some(60),
            pid: None,
            name: "Display".to_owned(),
        };
        let old_window = CaptureSourceInfo {
            kind: "window".to_owned(),
            index: 0,
            native_id: Some(42),
            width: 800,
            height: 600,
            fps: Some(60),
            pid: Some(100),
            name: "Old window".to_owned(),
        };
        let new_window = CaptureSourceInfo {
            name: "New window".to_owned(),
            ..old_window.clone()
        };
        let mut sources = vec![monitor.clone(), old_window];

        replace_sources_for_kind(&mut sources, "window", vec![new_window.clone()]);

        assert_eq!(sources.len(), 2);
        assert!(sources.iter().any(|source| source.name == monitor.name));
        assert!(sources.iter().any(|source| source.name == new_window.name));
        assert!(!sources.iter().any(|source| source.name == "Old window"));
    }

    #[test]
    fn stream_start_requires_an_explicit_valid_source_selection() {
        let monitor = CaptureSourceInfo {
            kind: "monitor".to_owned(),
            index: 0,
            native_id: None,
            width: 1_920,
            height: 1_080,
            fps: Some(60),
            pid: None,
            name: "Display".to_owned(),
        };
        assert!(!source_selection_is_valid(
            false,
            "monitor",
            0,
            None,
            &[monitor.clone()]
        ));
        assert!(!source_selection_is_valid(
            true,
            "monitor",
            1,
            None,
            &[monitor.clone()]
        ));
        assert!(source_selection_is_valid(
            true,
            "monitor",
            0,
            None,
            &[monitor]
        ));
        assert!(source_selection_is_valid(true, "test", 0, None, &[]));
    }

    #[test]
    fn window_selection_uses_native_identity_when_enumeration_order_changes() {
        let selected = CaptureSourceInfo {
            kind: "window".to_owned(),
            index: 7,
            native_id: Some(42),
            width: 800,
            height: 600,
            fps: Some(60),
            pid: Some(100),
            name: "Selected".to_owned(),
        };
        let unrelated = CaptureSourceInfo {
            kind: "window".to_owned(),
            index: 2,
            native_id: Some(99),
            width: 800,
            height: 600,
            fps: Some(60),
            pid: Some(200),
            name: "Unrelated".to_owned(),
        };

        assert!(source_selection_is_valid(
            true,
            "window",
            unrelated.index,
            selected.native_id,
            &[unrelated, selected]
        ));
    }

    #[test]
    fn minimized_selected_window_is_retained_when_enumeration_hides_it() {
        let selected = CaptureSourceInfo {
            kind: "window".to_owned(),
            index: 3,
            native_id: Some(42),
            width: 800,
            height: 600,
            fps: Some(60),
            pid: Some(100),
            name: "Selected".to_owned(),
        };
        let mut refreshed = Vec::new();

        retain_selected_native_source(
            &mut refreshed,
            &[selected],
            true,
            "window",
            Some(42),
            true,
            false,
        );

        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0].native_id, Some(42));
    }

    #[test]
    fn temporarily_unenumerated_windows_remain_in_the_carousel() {
        let retained = CaptureSourceInfo {
            kind: "window".to_owned(),
            index: 3,
            native_id: Some(42),
            width: 800,
            height: 600,
            fps: Some(60),
            pid: Some(100),
            name: "Background window".to_owned(),
        };
        let closed = CaptureSourceInfo {
            native_id: Some(99),
            name: "Closed window".to_owned(),
            ..retained.clone()
        };
        let mut refreshed = Vec::new();

        retain_temporarily_unenumerated_windows(
            &mut refreshed,
            &[retained, closed],
            true,
            |native_id| native_id == 42,
        );

        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0].native_id, Some(42));
    }

    #[test]
    fn ui_server_bootstrap_never_trusts_a_persisted_window_handle() {
        let mut config = AppConfig::default();
        config.source.kind = "window".to_owned();
        config.source.index = 7;
        config.source.native_id = Some(42);
        config.audio_mode = "window".to_owned();

        let bootstrap = server_bootstrap_config(&config);

        assert_eq!(bootstrap.source.kind, "test");
        assert_eq!(bootstrap.source.native_id, None);
        assert_eq!(bootstrap.audio_mode, "off");
        assert_eq!(bootstrap.http_port, config.http_port);
        assert_eq!(bootstrap.token, config.token);
    }

    #[test]
    fn custom_viewer_host_accepts_a_host_without_a_url_scheme() {
        assert_eq!(
            custom_viewer_host("stream.example.com"),
            Some("stream.example.com".to_owned())
        );
        assert!(custom_viewer_host("http://stream.example.com").is_none());
        assert!(custom_viewer_host("https://stream.example.com").is_none());
        assert!(custom_viewer_host("stream.example.com/viewer").is_none());
        assert!(custom_viewer_host(" stream.example.com").is_none());
        assert!(custom_viewer_host("stream .example.com").is_none());
    }

    #[test]
    fn settings_labels_standardize_automatic_as_auto() {
        assert_eq!(quality_mode_label("adaptive"), "Auto");
        assert_eq!(bitrate_mode_label("automatic"), "Auto");
        assert_eq!(bitrate_mode_label("fixed"), "Manual");
        assert_eq!(codec_label("auto"), "Auto");
        assert_eq!(latency_preference_label("low"), "Low");
    }

    #[test]
    fn default_share_settings_choose_public_mode() {
        let config = AppConfig::default();

        let (mode, custom_host, public_ipv4) = initial_viewer_url_state(&config);

        assert_eq!(mode, ViewerUrlMode::Public);
        assert!(custom_host.is_empty());
        assert!(public_ipv4.is_none());
    }

    #[test]
    fn stream_failure_keeps_the_server_available_for_retry() {
        let failed = HostStatus::StreamFailed("capture unavailable".to_owned());

        assert!(failed.server_alive());
        assert!(!failed.stream_active());
        assert!(failed.label().contains("capture unavailable"));

        assert!(!HostStatus::Failed("bind failed".to_owned()).server_alive());
    }

    #[test]
    fn bitrate_labels_are_human_readable() {
        assert_eq!(bitrate_label(14_000_000), "14.0 Mbps");
        assert_eq!(bitrate_label(750_000), "750 Kbps");
    }
}
