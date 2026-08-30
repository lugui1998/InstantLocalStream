use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Command;
use std::sync::{
    Arc, Mutex as StdMutex, OnceLock, Weak,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use rtc::ice::mdns::MulticastDnsMode;
use rtc::interceptor::Registry;
use rtc::peer_connection::configuration::{
    RTCConfigurationBuilder,
    interceptor_registry::register_default_interceptors,
    media_engine::{MIME_TYPE_OPUS, MediaEngine},
    setting_engine::SettingEngine,
};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::json;
use socketioxide::{
    SocketIo,
    extract::{AckSender, Data, Extension, SocketRef, State as SocketState},
    socket::DisconnectReason,
};
use tokio::sync::{Mutex, RwLock, oneshot};
use tracing::{info, warn};
use uuid::Uuid;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};

use crate::audio::AudioPipeline;
use crate::config::AppConfig;
use crate::media::{CaptureSettings, MediaPipeline};
#[cfg(windows)]
use crate::shared_capture::{SharedCapture, SourceFrame};
use crate::udp_mux::UdpMux;
#[cfg(windows)]
use crate::window_capture::{WindowResizeEvent, WindowResizeWatcher};

pub type StreamFailureCallback = Arc<dyn Fn(String) + Send + Sync>;

pub enum ServerCommand {
    StartStream {
        settings: CaptureSettings,
        result: Option<oneshot::Sender<std::result::Result<(), String>>>,
    },
    StopStream {
        result: Option<oneshot::Sender<()>>,
    },
    FinalizeStop,
    Update(CaptureSettings),
    UpdateMaxViewers(usize),
    UpdateMediaCandidateHost(String),
    PreviewSnapshot {
        result: std::sync::mpsc::SyncSender<Option<CapturePreviewSnapshot>>,
    },
    ResetToken(String),
    RecoverStream {
        pending: Arc<AtomicBool>,
    },
    #[cfg(windows)]
    RefreshWindowCapture {
        source_index: usize,
        source_native_id: Option<u64>,
        dimensions: (u32, u32),
    },
    #[cfg(windows)]
    RetryWindowCapture {
        previous_dimensions: (u32, u32),
        stop_request_revision: u64,
        attempt: usize,
    },
}

#[cfg(windows)]
const WINDOW_RESIZE_SAFETY_POLL_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(windows)]
const WINDOW_RESIZE_SAFETY_SETTLE_TIME: Duration = Duration::from_millis(200);
#[cfg(windows)]
const WINDOW_RESIZE_EVENT_QUIET_TIME: Duration = Duration::from_millis(120);
#[cfg(windows)]
const WINDOW_RESIZE_FRAME_GRACE: Duration = Duration::from_millis(40);
#[cfg(windows)]
const WINDOW_RESIZE_RESTART_ATTEMPTS: usize = 3;
#[cfg(windows)]
const WINDOW_RESIZE_RETRY_DELAY: Duration = Duration::from_millis(150);
#[cfg(windows)]
const WINDOW_RESIZE_RECOVERY_RETRY_DELAY: Duration = Duration::from_secs(2);
const STREAM_STALL_TIMEOUT: Duration = Duration::from_secs(5);
const STREAM_CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

struct RecoveryPendingGuard(Arc<AtomicBool>);

impl Drop for RecoveryPendingGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowSourceIdentity {
    index: usize,
    native_id: Option<u64>,
}

#[cfg(windows)]
#[derive(Default)]
struct WindowResizeDebouncer {
    source: Option<WindowSourceIdentity>,
    candidate: Option<(u32, u32)>,
    candidate_since: Option<Instant>,
    emitted: Option<(u32, u32)>,
}

#[cfg(windows)]
impl WindowResizeDebouncer {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn observe(
        &mut self,
        source: WindowSourceIdentity,
        current: (u32, u32),
        observed: (u32, u32),
        now: Instant,
    ) -> Option<(u32, u32)> {
        if observed.0 < 2 || observed.1 < 2 || observed == current {
            self.source = Some(source);
            self.candidate = None;
            self.candidate_since = None;
            self.emitted = None;
            return None;
        }
        if self.source != Some(source) || self.candidate != Some(observed) {
            self.source = Some(source);
            self.candidate = Some(observed);
            self.candidate_since = Some(now);
            self.emitted = None;
            return None;
        }
        if now.duration_since(self.candidate_since.unwrap_or(now))
            < WINDOW_RESIZE_SAFETY_SETTLE_TIME
        {
            return None;
        }
        if self.emitted == Some(observed) {
            return None;
        }
        self.emitted = Some(observed);
        Some(observed)
    }

    fn observe_settled(
        &mut self,
        source: WindowSourceIdentity,
        current: (u32, u32),
        observed: (u32, u32),
    ) -> Option<(u32, u32)> {
        if observed.0 < 2 || observed.1 < 2 || observed == current {
            self.source = Some(source);
            self.candidate = None;
            self.candidate_since = None;
            self.emitted = None;
            return None;
        }
        if self.source != Some(source) || self.candidate != Some(observed) {
            self.source = Some(source);
            self.candidate = Some(observed);
            self.candidate_since = None;
            self.emitted = None;
        }
        if self.emitted == Some(observed) {
            return None;
        }
        self.emitted = Some(observed);
        Some(observed)
    }
}

#[derive(Clone, Debug)]
pub struct CapturePreviewSnapshot {
    pub settings: CaptureSettings,
    pub frame: SourceFrame,
}

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct WebAssets;

#[derive(Clone)]
pub struct ServerState {
    pub config: Arc<AppConfig>,
    active_token: Arc<StdMutex<String>>,
    settings_revision: Arc<AtomicU64>,
    media_session_revision: Arc<AtomicU64>,
    stop_request_revision: Arc<AtomicU64>,
    pub audio: Option<Arc<AudioPipeline>>,
    stream_enabled: Arc<AtomicBool>,
    stream_resetting: Arc<AtomicBool>,
    max_viewers: Arc<AtomicUsize>,
    settings: Arc<StdMutex<CaptureSettings>>,
    viewer_metrics: Arc<StdMutex<HashMap<String, ViewerMetrics>>>,
    groups: Arc<TranscodeGroups>,
    shared_capture: Arc<CaptureSlot>,
    pub connections: Arc<Mutex<std::collections::HashMap<Uuid, Arc<dyn PeerConnection>>>>,
    pub udp_mux: Arc<UdpMux>,
    pending_connections: Arc<StdMutex<HashSet<Uuid>>>,
    connected_connections: Arc<StdMutex<HashSet<Uuid>>>,
    client_connections: Arc<StdMutex<std::collections::HashMap<String, Uuid>>>,
    connection_bindings: Arc<StdMutex<HashMap<Uuid, ConnectionMediaBinding>>>,
    client_sockets: Arc<StdMutex<std::collections::HashMap<String, SocketRef>>>,
    /// Serializes SDP negotiation for a viewer. Without this, two overlapping
    /// offers can both replace the same client mapping and orphan one peer.
    offer_locks: Arc<StdMutex<HashMap<String, Weak<Mutex<()>>>>>,
    /// Invalidates offers that were already being negotiated when a sharing
    /// token was rotated.
    session_generation: Arc<AtomicU64>,
    /// A reset takes the exclusive side of this gate. Offers retain a shared
    /// guard until their peer is committed, so a reset cannot miss an offer
    /// that is between its final authorization check and map insertion.
    session_gate: Arc<RwLock<()>>,
    stream_failure_callback: Option<StreamFailureCallback>,
}

impl ServerState {
    fn token_matches(&self, token: &str) -> bool {
        self.active_token
            .lock()
            .map(|active_token| active_token.as_str() == token)
            .unwrap_or(false)
    }

    fn reset_token(&self, token: String) {
        if let Ok(mut active_token) = self.active_token.lock() {
            *active_token = token;
        }
        self.session_generation.fetch_add(1, Ordering::AcqRel);
    }
}

/// Owns the currently running raw capture.  Keeping this indirection lets the
/// server remain completely cold until Start is requested and makes each run
/// use a fresh capture reader.
#[derive(Default)]
struct CaptureSlot(StdMutex<Option<Arc<SharedCapture>>>);

impl CaptureSlot {
    fn current(&self) -> Option<Arc<SharedCapture>> {
        self.0.lock().ok().and_then(|capture| capture.clone())
    }

    fn install(&self, capture: Arc<SharedCapture>) -> Result<()> {
        let mut slot = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("capture slot lock poisoned"))?;
        if slot.is_some() {
            anyhow::bail!("shared capture is already running");
        }
        *slot = Some(capture);
        Ok(())
    }

    fn take(&self) -> Option<Arc<SharedCapture>> {
        self.0.lock().ok().and_then(|mut capture| capture.take())
    }
}

/// The exact media graph used by a negotiated peer.
///
/// Capture reconfiguration replaces a group's current encoder pipeline.  A
/// reconnecting peer can still be attached to the old pipeline for a short
/// time, so cleanup must use this binding rather than looking the group up
/// again by id.
#[derive(Clone)]
struct ConnectionMediaBinding {
    media: Arc<MediaPipeline>,
    audio: Option<Arc<AudioPipeline>>,
}

#[derive(Debug, Clone)]
struct ViewerMetrics {
    rtt_ms: f64,
    jitter_ms: f64,
    loss_rate: f64,
    bitrate_bps: f64,
    reported_frames_dropped: u64,
    reported_freeze_count: u64,
    updated_at: std::time::Instant,
    samples: VecDeque<MetricSample>,
}

#[derive(Debug, Clone)]
struct MetricSample {
    captured_at: Instant,
    rtt_ms: f64,
    jitter_ms: f64,
    loss_rate: f64,
    bitrate_bps: f64,
    available_incoming_bitrate_bps: Option<f64>,
    frames_dropped: u64,
    freeze_count: u64,
    visibility_state: String,
}

const MAX_METRIC_SAMPLES_PER_VIEWER: usize = 32;
const MIN_VIEWER_METRIC_INTERVAL: Duration = Duration::from_millis(250);

// Browser `droppedVideoFrames` often includes intentional latest-frame and
// catch-up drops.  Treat it as a downgrade signal only when it is extreme over
// the rolling 15-second window; freezes are the meaningful playback failure.
const FREEZE_DOWNGRADE_THRESHOLD: u64 = 3;
const EXTREME_DROPPED_FRAME_THRESHOLD: u64 = 300;
const DROPPED_FRAME_UPGRADE_HOLD_THRESHOLD: u64 = 120;
const FREEZE_UPGRADE_HOLD_THRESHOLD: u64 = 1;
const ENCODER_STARTUP_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone)]
struct TranscodeGroup {
    id: usize,
    media: Arc<StdMutex<Option<Arc<MediaPipeline>>>>,
    codec: Arc<StdMutex<String>>,
    settings: Arc<StdMutex<CaptureSettings>>,
    ceiling: Arc<StdMutex<CaptureSettings>>,
    last_tuned: Arc<StdMutex<Instant>>,
    lifecycle: Arc<StdMutex<GroupLifecycle>>,
    drain_started_at: Arc<StdMutex<Option<Instant>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupLifecycle {
    Stopped,
    Active,
    Draining,
}

impl GroupLifecycle {
    fn is_visible(self) -> bool {
        matches!(self, Self::Active)
    }

    fn client_state(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Active => "stable",
            Self::Draining => "draining",
        }
    }
}

impl TranscodeGroup {
    #[cfg(test)]
    fn active(id: usize, media: Arc<MediaPipeline>, settings: CaptureSettings) -> Self {
        let codec = media.codec_id().to_owned();
        Self {
            id,
            media: Arc::new(StdMutex::new(Some(media))),
            codec: Arc::new(StdMutex::new(codec)),
            settings: Arc::new(StdMutex::new(settings.clone())),
            ceiling: Arc::new(StdMutex::new(settings)),
            last_tuned: Arc::new(StdMutex::new(Instant::now() - Duration::from_secs(30))),
            lifecycle: Arc::new(StdMutex::new(GroupLifecycle::Active)),
            drain_started_at: Arc::new(StdMutex::new(None)),
        }
    }

    fn stopped(id: usize, settings: CaptureSettings, codec: impl Into<String>) -> Self {
        Self {
            id,
            media: Arc::new(StdMutex::new(None)),
            codec: Arc::new(StdMutex::new(codec.into())),
            settings: Arc::new(StdMutex::new(settings.clone())),
            ceiling: Arc::new(StdMutex::new(settings)),
            last_tuned: Arc::new(StdMutex::new(Instant::now() - Duration::from_secs(30))),
            lifecycle: Arc::new(StdMutex::new(GroupLifecycle::Stopped)),
            drain_started_at: Arc::new(StdMutex::new(None)),
        }
    }

    fn media(&self) -> Option<Arc<MediaPipeline>> {
        self.media.lock().ok().and_then(|media| media.clone())
    }

    fn lifecycle(&self) -> GroupLifecycle {
        self.lifecycle
            .lock()
            .map(|lifecycle| *lifecycle)
            .unwrap_or(GroupLifecycle::Stopped)
    }

    fn codec(&self) -> String {
        self.codec
            .lock()
            .map(|codec| codec.clone())
            .unwrap_or_else(|_| "vp8".to_owned())
    }
}

#[derive(Clone)]
struct GroupFactory {
    ffmpeg: String,
    capture_slot: Arc<CaptureSlot>,
    codec_policy: String,
    host_codecs: Arc<HashSet<String>>,
    stream_enabled: Arc<AtomicBool>,
    tasks: Arc<StdMutex<Vec<tokio::task::JoinHandle<()>>>>,
    pipelines: Arc<StdMutex<Vec<Weak<MediaPipeline>>>>,
}

impl GroupFactory {
    fn start(&self, codec: &str, settings: CaptureSettings) -> Result<Arc<MediaPipeline>> {
        if !self.stream_enabled.load(Ordering::Acquire) {
            anyhow::bail!("cannot start a transcode group while the stream is stopped");
        }
        self.start_with_capture(codec, settings, false)
    }

    /// The primary group is intentionally brought up before `stream_enabled`
    /// flips so startup can fail atomically without publishing a live stream.
    fn start_primary(&self, codec: &str, settings: CaptureSettings) -> Result<Arc<MediaPipeline>> {
        self.start_with_capture(codec, settings, true)
    }

    fn start_with_capture(
        &self,
        codec: &str,
        settings: CaptureSettings,
        include_latest_frame: bool,
    ) -> Result<Arc<MediaPipeline>> {
        let shared_capture = self
            .capture_slot
            .current()
            .context("shared capture is not running")?;
        let media = Arc::new(MediaPipeline::with_codec(codec)?);
        if let Ok(mut pipelines) = self.pipelines.lock() {
            pipelines.retain(|pipeline| pipeline.strong_count() > 0);
            pipelines.push(Arc::downgrade(&media));
        }
        let source_dimensions = shared_capture.source_dimensions();
        let source_pixel_format = shared_capture.source_pixel_format();
        let source_fps = shared_capture.source_fps();
        let source = if include_latest_frame {
            shared_capture.subscribe_including_latest()
        } else {
            shared_capture.subscribe()
        };
        let task = media.clone().spawn_from_shared_source(
            self.ffmpeg.clone(),
            source,
            source_dimensions,
            source_pixel_format,
            source_fps,
            settings,
        );
        self.register_task(task);
        if self.stream_enabled.load(Ordering::Acquire) {
            media.activate();
        }
        if let Err(error) = media.wait_until_ready(ENCODER_STARTUP_TIMEOUT) {
            media.stop();
            return Err(error);
        }
        Ok(media)
    }

    fn register_task(&self, task: tokio::task::JoinHandle<()>) {
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.retain(|task| !task.is_finished());
            tasks.push(task);
        }
    }

    fn take_tasks(&self) -> Vec<tokio::task::JoinHandle<()>> {
        self.tasks
            .lock()
            .map(|mut tasks| std::mem::take(&mut *tasks))
            .unwrap_or_default()
    }

    fn request_stop_all(&self) {
        if let Ok(mut pipelines) = self.pipelines.lock() {
            pipelines.retain(|pipeline| {
                let Some(pipeline) = pipeline.upgrade() else {
                    return false;
                };
                pipeline.request_stop();
                true
            });
        }
    }
}

#[derive(Clone)]
struct TranscodeGroups {
    groups: Arc<Vec<TranscodeGroup>>,
    assignments: Arc<StdMutex<HashMap<String, ClientGroupState>>>,
    factory: Option<Arc<GroupFactory>>,
    lifecycle_lock: Arc<StdMutex<()>>,
    /// The currently configured upper budget.  Slots are allocated up front so
    /// switching between Manual and Auto mode can take effect while streaming.
    active_budget: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct ClientGroupState {
    group_id: usize,
    last_change: Instant,
    reason: String,
    supported_codecs: HashSet<String>,
    rejected_codecs: HashSet<String>,
}

#[derive(Clone)]
struct GroupAssignment {
    group_id: usize,
    reason: String,
    restart: bool,
}

#[derive(Clone)]
struct GroupMigration {
    client_id: String,
    assignment: GroupAssignment,
}

struct GroupMaintenance {
    changed: bool,
    migrations: Vec<GroupMigration>,
}

#[derive(Default)]
struct GroupReconfiguration {
    topology_changed: bool,
}

fn playback_totals(samples: &[&MetricSample]) -> (u64, u64) {
    let dropped = samples
        .iter()
        .map(|sample| sample.frames_dropped)
        .sum::<u64>();
    let freezes = samples
        .iter()
        .map(|sample| sample.freeze_count)
        .sum::<u64>();
    (dropped, freezes)
}

fn playback_requires_downgrade(dropped: u64, freezes: u64) -> bool {
    freezes >= FREEZE_DOWNGRADE_THRESHOLD || dropped >= EXTREME_DROPPED_FRAME_THRESHOLD
}

fn playback_holds_upgrade(dropped: u64, freezes: u64) -> bool {
    freezes >= FREEZE_UPGRADE_HOLD_THRESHOLD || dropped >= DROPPED_FRAME_UPGRADE_HOLD_THRESHOLD
}

impl TranscodeGroups {
    #[cfg(test)]
    fn new(groups: Vec<TranscodeGroup>) -> Self {
        Self::with_factory(groups, None)
    }

    fn with_factory(groups: Vec<TranscodeGroup>, factory: Option<Arc<GroupFactory>>) -> Self {
        let capacity = groups.len().max(1);
        Self {
            groups: Arc::new(groups),
            assignments: Arc::new(StdMutex::new(HashMap::new())),
            factory,
            lifecycle_lock: Arc::new(StdMutex::new(())),
            active_budget: Arc::new(AtomicUsize::new(capacity)),
        }
    }

    fn count(&self) -> usize {
        self.active_group_ids().len()
    }

    fn capacity(&self) -> usize {
        self.active_budget
            .load(Ordering::Acquire)
            .clamp(1, self.groups.len().max(1))
    }

    fn resource_count(&self) -> usize {
        self.groups
            .iter()
            .filter(|group| group.id < self.capacity() && group.media().is_some())
            .count()
    }

    fn active_group_ids(&self) -> Vec<usize> {
        self.groups
            .iter()
            .filter(|group| {
                group.id < self.capacity()
                    && group.lifecycle().is_visible()
                    && group.media().is_some()
            })
            .map(|group| group.id)
            .collect()
    }

    fn group(&self, group_id: usize) -> &TranscodeGroup {
        &self.groups[group_id.min(self.groups.len().saturating_sub(1))]
    }

    fn primary_group_id(&self) -> usize {
        self.active_group_ids().into_iter().next().unwrap_or(0)
    }

    fn activate_group(&self, group_id: usize) -> Result<Arc<MediaPipeline>> {
        let group = self.group(group_id);
        if let Some(media) = group.media() {
            return Ok(media);
        }
        let _lifecycle = self
            .lifecycle_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("group lifecycle lock poisoned"))?;
        if let Some(media) = group.media() {
            return Ok(media);
        }
        let factory = self
            .factory
            .as_ref()
            .context("dynamic transcode group factory is unavailable")?;
        let settings = self.group_settings(group_id);
        let codec = group.codec();
        let media = factory.start(&codec, settings)?;
        if let Ok(mut current) = group.media.lock() {
            *current = Some(Arc::clone(&media));
        }
        if let Ok(mut lifecycle) = group.lifecycle.lock() {
            *lifecycle = GroupLifecycle::Active;
        }
        if let Ok(mut drain_started_at) = group.drain_started_at.lock() {
            *drain_started_at = None;
        }
        Ok(media)
    }

    fn start_primary(&self) -> Result<Arc<MediaPipeline>> {
        let group = self.group(0);
        if let Some(media) = group.media() {
            return Ok(media);
        }
        let _lifecycle = self
            .lifecycle_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("group lifecycle lock poisoned"))?;
        if let Some(media) = group.media() {
            return Ok(media);
        }
        let factory = self
            .factory
            .as_ref()
            .context("dynamic transcode group factory is unavailable")?;
        let media = factory.start_primary(&group.codec(), self.group_settings(0))?;
        *group
            .media
            .lock()
            .map_err(|_| anyhow::anyhow!("group media lock poisoned"))? = Some(Arc::clone(&media));
        *group
            .lifecycle
            .lock()
            .map_err(|_| anyhow::anyhow!("group lifecycle lock poisoned"))? =
            GroupLifecycle::Active;
        Ok(media)
    }

    fn ensure_client(&self, client_id: &str) -> GroupAssignment {
        let fallback_group_id = self.primary_group_id();
        let mut assignments = self.assignments.lock().expect("group assignments lock");
        let assignment =
            assignments
                .entry(client_id.to_owned())
                .or_insert_with(|| ClientGroupState {
                    group_id: fallback_group_id,
                    last_change: Instant::now() - Duration::from_secs(31),
                    reason: "initial assignment".to_owned(),
                    supported_codecs: HashSet::new(),
                    rejected_codecs: HashSet::new(),
                });
        if !self.active_group_ids().contains(&assignment.group_id) {
            assignment.group_id = fallback_group_id;
            assignment.last_change = Instant::now();
            assignment.reason = "active group fallback".to_owned();
        }
        GroupAssignment {
            group_id: assignment.group_id,
            reason: assignment.reason.clone(),
            restart: false,
        }
    }

    fn assignment_for(&self, client_id: &str) -> GroupAssignment {
        self.ensure_client(client_id)
    }

    fn media_for(&self, client_id: &str) -> Result<Arc<MediaPipeline>> {
        let assignment = self.assignment_for(client_id);
        self.activate_group(assignment.group_id)
    }

    fn codec_for_client(&self, client_id: &str) -> Option<String> {
        let group_id = self
            .assignments
            .lock()
            .ok()?
            .get(client_id)
            .map(|assignment| assignment.group_id)?;
        Some(self.group(group_id).codec())
    }

    fn media_by_id(&self, group_id: usize) -> Option<Arc<MediaPipeline>> {
        self.group(group_id).media()
    }

    fn group_settings(&self, group_id: usize) -> CaptureSettings {
        self.group(group_id)
            .settings
            .lock()
            .map(|settings| settings.clone())
            .unwrap_or_else(|_| CaptureSettings {
                source_kind: "test".to_owned(),
                source_index: 0,
                source_native_id: None,
                draw_mouse: true,
                width: 1280,
                height: 720,
                fps: 30,
                output_height: Some(720),
                output_fps: Some(30),
                bitrate: 1_000_000,
                quality_mode: "adaptive".to_owned(),
                bitrate_mode: "automatic".to_owned(),
                adaptive_quality_ceiling: "720p".to_owned(),
                adaptive_fps_ceiling: "30".to_owned(),
                max_quality_groups: "1".to_owned(),
                latency_preference: "low".to_owned(),
                audio_mode: "off".to_owned(),
                excluded_audio_processes: Vec::new(),
            })
    }

    /// Applies a new desired group budget and profile ceiling.
    ///
    /// `replace_active_media` is used when the raw capture format is about to
    /// change.  In that case the caller replaces the encoder instances as a
    /// unit, rather than briefly restarting each old encoder against a source
    /// whose dimensions no longer match it.
    fn reconfigure(
        &self,
        settings: CaptureSettings,
        replace_active_media: bool,
    ) -> GroupReconfiguration {
        let Ok(_lifecycle) = self.lifecycle_lock.lock() else {
            return GroupReconfiguration::default();
        };
        self.reconfigure_locked(settings, replace_active_media)
    }

    /// Applies a capture/profile update as one topology transaction.  This
    /// prevents the adaptive maintenance loop from installing or removing an
    /// encoder between a new profile being recorded and the replacement track
    /// being created.
    fn reconfigure_and_restart(&self, settings: CaptureSettings) -> Result<GroupReconfiguration> {
        let _lifecycle = self
            .lifecycle_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("group lifecycle lock poisoned"))?;
        let requested_budget = if settings.quality_mode == "adaptive" {
            configured_group_count(&settings.max_quality_groups)
        } else {
            1
        }
        .min(self.groups.len().max(1));
        let factory = self
            .factory
            .as_ref()
            .context("dynamic transcode group factory is unavailable")?;
        let mut replacements = Vec::new();
        for group in self.groups.iter().filter(|group| {
            group.id < requested_budget && group.lifecycle().is_visible() && group.media().is_some()
        }) {
            let profile = group_settings(&settings, group.id);
            let codec = group.codec();
            match factory.start(&codec, profile) {
                Ok(media) => replacements.push((group.id, media)),
                Err(error) => {
                    for (_, replacement) in replacements {
                        replacement.stop();
                    }
                    return Err(error);
                }
            }
        }

        // Commit topology/profile state only after every required replacement
        // encoder has started successfully.
        let result = self.reconfigure_locked(settings, true);
        for (group_id, replacement) in replacements {
            let previous = self
                .group(group_id)
                .media
                .lock()
                .map_err(|_| anyhow::anyhow!("group media lock poisoned"))?
                .replace(replacement);
            if let Some(previous) = previous {
                previous.stop();
            }
        }
        Ok(result)
    }

    fn reconfigure_locked(
        &self,
        settings: CaptureSettings,
        replace_active_media: bool,
    ) -> GroupReconfiguration {
        let requested_budget = if settings.quality_mode == "adaptive" {
            configured_group_count(&settings.max_quality_groups)
        } else {
            1
        }
        .min(self.groups.len().max(1));
        let previous_budget = self.capacity();
        self.active_budget
            .store(requested_budget, Ordering::Release);
        let result = GroupReconfiguration {
            topology_changed: previous_budget != requested_budget,
        };
        for group in self.groups.iter() {
            let profile = group_settings(&settings, group.id);
            if let Ok(mut current) = group.settings.lock() {
                *current = profile.clone();
            }
            if let Ok(mut ceiling) = group.ceiling.lock() {
                *ceiling = profile.clone();
            }
            if !replace_active_media
                && group.id < requested_budget
                && let Some(media) = group.media()
            {
                media.reconfigure(profile);
            }
        }

        if requested_budget < previous_budget {
            let fallback_group_id = self.primary_group_id();
            let now = Instant::now();
            if let Ok(mut assignments) = self.assignments.lock() {
                for (_, assignment) in assignments.iter_mut() {
                    if assignment.group_id < requested_budget {
                        continue;
                    }
                    assignment.group_id = fallback_group_id;
                    assignment.last_change = now;
                    assignment.reason = "group budget reduced".to_owned();
                }
            }
            for group in self
                .groups
                .iter()
                .filter(|group| group.id >= requested_budget)
            {
                if let Ok(mut media) = group.media.lock()
                    && let Some(media) = media.take()
                {
                    media.stop();
                }
                if let Ok(mut lifecycle) = group.lifecycle.lock() {
                    *lifecycle = GroupLifecycle::Stopped;
                }
                if let Ok(mut drain_started_at) = group.drain_started_at.lock() {
                    *drain_started_at = None;
                }
            }
        }
        result
    }

    fn set_group_profile(&self, group_id: usize, settings: CaptureSettings) {
        let group = self.group(group_id);
        if let Ok(mut current) = group.settings.lock() {
            *current = settings.clone();
        }
        if let Some(media) = group.media() {
            media.reconfigure(settings);
        }
    }

    fn group_codec(&self, group_id: usize) -> String {
        self.group(group_id).codec()
    }

    fn configure_stopped_group(
        &self,
        group_id: usize,
        codec: &str,
        settings: CaptureSettings,
        profile_ceiling: CaptureSettings,
    ) -> bool {
        let group = self.group(group_id);
        if group.media().is_some() {
            return false;
        }
        if let Ok(mut current) = group.settings.lock() {
            *current = settings.clone();
        }
        if let Ok(mut current_ceiling) = group.ceiling.lock() {
            *current_ceiling = profile_ceiling;
        }
        if let Ok(mut current_codec) = group.codec.lock() {
            *current_codec = codec.to_owned();
        }
        true
    }

    fn group_for_codec_profile(
        &self,
        codec: &str,
        preferred_group_id: usize,
        profile: CaptureSettings,
        ceiling: CaptureSettings,
    ) -> Option<usize> {
        let preferred = self.group(preferred_group_id);
        if preferred.codec().eq_ignore_ascii_case(codec) {
            if preferred.media().is_none() {
                self.configure_stopped_group(preferred_group_id, codec, profile, ceiling);
            }
            return Some(preferred_group_id);
        }
        if let Some(group_id) = self
            .active_group_ids()
            .into_iter()
            .find(|group_id| self.group_codec(*group_id).eq_ignore_ascii_case(codec))
        {
            return Some(group_id);
        }
        let stopped_group_id = self
            .groups
            .iter()
            .filter(|group| group.id < self.capacity())
            .find(|group| group.media().is_none())
            .map(|group| group.id)?;
        self.configure_stopped_group(stopped_group_id, codec, profile, ceiling)
            .then_some(stopped_group_id)
    }

    fn select_codec(
        &self,
        supported_codecs: &HashSet<String>,
        rejected_codecs: &HashSet<String>,
        probe: &ViewerBootstrap,
    ) -> String {
        let Some(factory) = &self.factory else {
            return "vp8".to_owned();
        };
        choose_codec(
            &factory.codec_policy,
            &factory.host_codecs,
            supported_codecs,
            rejected_codecs,
            probe.download_bps,
            probe.latency_ms,
        )
    }

    fn activate(&self) {
        for group in self.groups.iter() {
            if group.id < self.capacity()
                && group.lifecycle().is_visible()
                && let Some(media) = group.media()
            {
                media.activate();
            }
        }
    }

    /// Removes every encoder from the live group slots as one short topology
    /// transaction. The returned pipelines are no longer reachable through
    /// this group set, so slow process teardown can safely happen elsewhere
    /// without stopping media installed by a later restart attempt.
    fn detach_all_media(&self) -> Vec<Arc<MediaPipeline>> {
        let Ok(_lifecycle) = self.lifecycle_lock.lock() else {
            return Vec::new();
        };
        let mut stopped_media = Vec::new();
        for group in self.groups.iter() {
            if let Ok(mut media) = group.media.lock()
                && let Some(media) = media.take()
            {
                stopped_media.push(media);
            }
            if let Ok(mut lifecycle) = group.lifecycle.lock() {
                *lifecycle = GroupLifecycle::Stopped;
            }
        }
        stopped_media
    }

    fn request_stop(&self) {
        if let Some(factory) = &self.factory {
            factory.request_stop_all();
        }
    }

    fn take_tasks(&self) -> Vec<tokio::task::JoinHandle<()>> {
        self.factory
            .as_ref()
            .map(|factory| factory.take_tasks())
            .unwrap_or_default()
    }

    fn tune(&self, metrics: &HashMap<String, ViewerMetrics>) -> bool {
        let Ok(_lifecycle) = self.lifecycle_lock.lock() else {
            return false;
        };
        let assignments = self
            .assignments
            .lock()
            .map(|assignments| assignments.clone())
            .unwrap_or_default();
        let now = Instant::now();
        let mut changed = false;
        for group in self.groups.iter().filter(|group| {
            group.id < self.capacity() && group.lifecycle().is_visible() && group.media().is_some()
        }) {
            let member_metrics = assignments
                .iter()
                .filter(|(_, assignment)| {
                    assignment.group_id == group.id
                        && now.duration_since(assignment.last_change) >= Duration::from_secs(30)
                })
                .filter_map(|(client_id, _)| metrics.get(client_id))
                .collect::<Vec<_>>();
            let samples = member_metrics
                .iter()
                .flat_map(|metric| metric.samples.iter())
                .filter(|sample| {
                    sample.visibility_state == "visible"
                        && now.duration_since(sample.captured_at) <= Duration::from_secs(15)
                })
                .collect::<Vec<_>>();
            if samples.len() < 5 {
                continue;
            }
            let last_tuned = group.last_tuned.lock().map(|last| *last).unwrap_or(now);
            if now.duration_since(last_tuned) < Duration::from_secs(10) {
                continue;
            }
            let current = match group.settings.lock() {
                Ok(settings) => settings.clone(),
                Err(_) => continue,
            };
            let ceiling = match group.ceiling.lock() {
                Ok(ceiling) => ceiling.clone(),
                Err(_) => continue,
            };
            let low_delivery = samples
                .iter()
                .filter(|sample| {
                    sample
                        .available_incoming_bitrate_bps
                        .is_some_and(|capacity| {
                            capacity < current.bitrate as f64 * 0.70
                                && sample.bitrate_bps < current.bitrate as f64 * 0.75
                        })
                })
                .count();
            let lossy = samples
                .iter()
                .filter(|sample| {
                    sample.loss_rate > 0.05 || sample.jitter_ms > 120.0 || sample.rtt_ms > 500.0
                })
                .count();
            let (dropped, freezes) = playback_totals(&samples);
            let playback_busy = playback_holds_upgrade(dropped, freezes);
            let playback_failures = member_metrics
                .iter()
                .filter(|metric| {
                    let client_samples = metric
                        .samples
                        .iter()
                        .filter(|sample| {
                            sample.visibility_state == "visible"
                                && now.duration_since(sample.captured_at) <= Duration::from_secs(15)
                        })
                        .collect::<Vec<_>>();
                    let (dropped, freezes) = playback_totals(&client_samples);
                    playback_requires_downgrade(dropped, freezes)
                })
                .count();
            // A local renderer may drop stale frames while catching up.  Only
            // freezes or an extreme number of drops count as playback failure,
            // and a shared group changes only when a majority of its members
            // show that failure (one client can be migrated separately).
            let playback_unstable = !member_metrics.is_empty()
                && playback_failures.saturating_mul(2) >= member_metrics.len();
            let unstable = low_delivery as f64 / samples.len() as f64 >= 0.80
                || lossy as f64 / samples.len() as f64 >= 0.60
                || playback_unstable;
            let stable = low_delivery == 0 && lossy == 0 && !playback_busy;
            let mut next = current.clone();
            if unstable {
                next.bitrate = (current.bitrate.saturating_mul(90) / 100).max(250_000);
                if next.bitrate == current.bitrate && current.output_fps.unwrap_or(current.fps) > 5
                {
                    next.output_fps = Some(lower_fps(current.output_fps.unwrap_or(current.fps)));
                } else if next.bitrate == current.bitrate {
                    next.output_height = Some(lower_height(
                        current.output_height.unwrap_or(current.height),
                    ));
                }
            } else if stable {
                next.bitrate = (current.bitrate.saturating_mul(110) / 100).min(ceiling.bitrate);
                if next.bitrate == current.bitrate {
                    let ceiling_fps = ceiling.output_fps.unwrap_or(ceiling.fps);
                    let current_fps = current.output_fps.unwrap_or(current.fps);
                    if current_fps < ceiling_fps {
                        next.output_fps = Some(higher_fps(current_fps, ceiling_fps));
                    } else {
                        let ceiling_height = ceiling.output_height.unwrap_or(ceiling.height);
                        let current_height = current.output_height.unwrap_or(current.height);
                        if current_height < ceiling_height {
                            next.output_height =
                                Some(higher_height(current_height, ceiling_height));
                        }
                    }
                }
            }
            if next != current {
                if let Ok(mut settings) = group.settings.lock() {
                    *settings = next.clone();
                }
                if let Some(media) = group.media() {
                    media.reconfigure(next);
                }
                if let Ok(mut last) = group.last_tuned.lock() {
                    *last = now;
                }
                changed = true;
            }
        }
        changed
    }

    fn assignment_json(&self, client_id: &str) -> serde_json::Value {
        let assignment = self.assignment_for(client_id);
        self.assignment_json_for(assignment.group_id, &assignment.reason, assignment.restart)
    }

    fn assignment_json_for(
        &self,
        group_id: usize,
        reason: &str,
        restart: bool,
    ) -> serde_json::Value {
        let settings = self.group_settings(group_id);
        let codec = self
            .media_by_id(group_id)
            .map(|media| media.codec_name().to_owned())
            .or_else(|| Some(display_codec_name(&self.group(group_id).codec()).to_owned()))
            .unwrap_or_else(|| "Unknown".to_owned());
        let display_index = self
            .active_group_ids()
            .iter()
            .position(|id| *id == group_id)
            .map(|index| index + 1)
            .unwrap_or(group_id + 1);
        let lifecycle = self.group(group_id).lifecycle();
        let height = settings.output_height.unwrap_or(settings.height);
        let fps = settings.output_fps.unwrap_or(settings.fps);
        json!({
            "id": format!("group-{display_index}"),
            "index": display_index,
            "label": format!("{height}p · {fps} FPS"),
            "quality": format!("{height}p"),
            "fps": fps,
            "bitrate_bps": settings.bitrate,
            "codec": codec,
            "state": lifecycle.client_state(),
            "reason": reason,
            "restart": restart
            ,"sync_mode": "latest-frame"
        })
    }

    fn groups_json(&self) -> Vec<serde_json::Value> {
        self.active_group_ids()
            .into_iter()
            .map(|group_id| self.assignment_json_for(group_id, "available", false))
            .collect()
    }

    fn observe(&self, client_id: &str, metrics: &ViewerMetrics) -> Option<GroupAssignment> {
        let now = Instant::now();
        let window = metrics
            .samples
            .iter()
            .filter(|sample| {
                sample.visibility_state == "visible"
                    && now.duration_since(sample.captured_at) <= Duration::from_secs(15)
            })
            .collect::<Vec<_>>();
        if window.len() < 5 {
            return None;
        }
        let mut assignments = self.assignments.lock().ok()?;
        let assignment =
            assignments
                .entry(client_id.to_owned())
                .or_insert_with(|| ClientGroupState {
                    group_id: self.primary_group_id(),
                    last_change: now - Duration::from_secs(31),
                    reason: "initial assignment".to_owned(),
                    supported_codecs: HashSet::new(),
                    rejected_codecs: HashSet::new(),
                });
        let current = self.group_settings(assignment.group_id);
        let low_delivery = window
            .iter()
            .filter(|sample| {
                sample
                    .available_incoming_bitrate_bps
                    .is_some_and(|capacity| {
                        capacity < current.bitrate as f64 * 0.65
                            && sample.bitrate_bps < current.bitrate as f64 * 0.70
                    })
            })
            .count();
        let lossy_samples = window
            .iter()
            .filter(|sample| {
                sample.loss_rate > 0.05 || sample.jitter_ms > 120.0 || sample.rtt_ms > 500.0
            })
            .count();
        let (dropped, freezes) = playback_totals(&window);
        let playback_busy = playback_holds_upgrade(dropped, freezes);
        let playback_unstable = playback_requires_downgrade(dropped, freezes);
        let low_delivery_ratio = low_delivery as f64 / window.len() as f64;
        let degraded = low_delivery_ratio >= 0.8
            || lossy_samples as f64 / window.len() as f64 >= 0.60
            || playback_unstable;
        let current_group_id = assignment.group_id;
        let current_codec = self.group_codec(current_group_id);
        if degraded && now.duration_since(assignment.last_change) >= Duration::from_secs(30) {
            let lower_group_id = if self.resource_count() < self.capacity() {
                ((current_group_id + 1)..self.capacity())
                    .find(|group_id| self.group(*group_id).media().is_none())
                    .and_then(|group_id| {
                        let profile = self.group_settings(group_id);
                        self.configure_stopped_group(
                            group_id,
                            &current_codec,
                            profile.clone(),
                            profile,
                        )
                        .then(|| self.activate_group(group_id).ok().map(|_| group_id))
                        .flatten()
                    })
                    .or_else(|| {
                        self.active_group_ids().into_iter().find(|group_id| {
                            *group_id > current_group_id
                                && self
                                    .group_codec(*group_id)
                                    .eq_ignore_ascii_case(&current_codec)
                        })
                    })
            } else {
                self.active_group_ids().into_iter().find(|group_id| {
                    *group_id > current_group_id
                        && self
                            .group_codec(*group_id)
                            .eq_ignore_ascii_case(&current_codec)
                })
            };
            if let Some(lower_group_id) = lower_group_id {
                assignment.group_id = lower_group_id;
                assignment.last_change = now;
                assignment.reason = if playback_unstable {
                    "15-second playback failure window".to_owned()
                } else {
                    "15-second media-path congestion window".to_owned()
                };
                return Some(GroupAssignment {
                    group_id: assignment.group_id,
                    reason: assignment.reason.clone(),
                    restart: true,
                });
            }
        }

        let next_higher_id = self.active_group_ids().into_iter().rfind(|group_id| {
            *group_id < assignment.group_id
                && self
                    .group_codec(*group_id)
                    .eq_ignore_ascii_case(&current_codec)
        })?;
        let next_higher = self.group_settings(next_higher_id);
        let headroom_samples = window
            .iter()
            .filter(|sample| {
                sample
                    .available_incoming_bitrate_bps
                    .is_some_and(|value| value >= next_higher.bitrate as f64 * 1.25)
                    && sample.loss_rate < 0.01
                    && sample.jitter_ms < 30.0
            })
            .count();
        let can_upgrade = headroom_samples as f64 / window.len() as f64 >= 0.8
            && lossy_samples == 0
            && !playback_busy;
        if assignment.group_id != next_higher_id
            && can_upgrade
            && now.duration_since(assignment.last_change) >= Duration::from_secs(30)
        {
            assignment.group_id = next_higher_id;
            assignment.last_change = now;
            assignment.reason = "15-second media-path headroom window".to_owned();
            return Some(GroupAssignment {
                group_id: assignment.group_id,
                reason: assignment.reason.clone(),
                restart: true,
            });
        }
        None
    }

    fn bootstrap(&self, client_id: &str, probe: &ViewerBootstrap) -> GroupAssignment {
        let supported_codecs = viewer_supported_codecs(probe);
        let rejected_codecs = self
            .assignments
            .lock()
            .ok()
            .and_then(|assignments| {
                assignments
                    .get(client_id)
                    .map(|state| state.rejected_codecs.clone())
            })
            .unwrap_or_default();
        let codec = self.select_codec(&supported_codecs, &rejected_codecs, probe);
        let mut preferred_group_id = self.capacity().saturating_sub(1);
        if !probe.timed_out && probe.download_bps > 0.0 {
            for group in self
                .groups
                .iter()
                .filter(|group| group.id < self.capacity())
            {
                let settings = self.group_settings(group.id);
                if probe.download_bps >= settings.bitrate as f64 * 1.25
                    && probe.latency_ms <= 1_500.0
                {
                    preferred_group_id = group.id;
                    break;
                }
            }
        }
        let ceiling = self.group_settings(preferred_group_id);
        let mut profile = ceiling.clone();
        let delivery_budget = if probe.timed_out || probe.download_bps <= 0.0 {
            250_000
        } else {
            (probe.download_bps * 0.65).round() as u32
        }
        .max(250_000);
        while profile.bitrate > delivery_budget {
            let previous = profile.clone();
            profile.bitrate = (profile.bitrate.saturating_mul(3) / 5).max(250_000);
            profile.output_fps = Some(lower_fps(profile.output_fps.unwrap_or(profile.fps)));
            profile.output_height = Some(lower_height(
                profile.output_height.unwrap_or(profile.height),
            ));
            if profile == previous {
                break;
            }
        }
        let mut group_id = self
            .group_for_codec_profile(&codec, preferred_group_id, profile.clone(), ceiling)
            .unwrap_or_else(|| self.primary_group_id());
        if self.group(group_id).media().is_none() || group_id == preferred_group_id {
            self.set_group_profile(group_id, profile);
        }
        if self.activate_group(group_id).is_err() {
            group_id = self.primary_group_id();
        }
        let mut assignments = self.assignments.lock().expect("group assignments lock");
        let assignment =
            assignments
                .entry(client_id.to_owned())
                .or_insert_with(|| ClientGroupState {
                    group_id,
                    last_change: Instant::now() - Duration::from_secs(30),
                    reason: "bootstrap probe".to_owned(),
                    supported_codecs: HashSet::new(),
                    rejected_codecs: HashSet::new(),
                });
        assignment.group_id = group_id;
        assignment.supported_codecs = supported_codecs;
        assignment
            .rejected_codecs
            .retain(|codec| assignment.supported_codecs.contains(codec));
        assignment.last_change = Instant::now();
        assignment.reason = if probe.timed_out {
            format!(
                "bootstrap probe timed out; selected safe {} group",
                display_codec_name(&codec)
            )
        } else {
            format!(
                "initial TCP throughput probe; selected {}",
                display_codec_name(&codec)
            )
        };
        GroupAssignment {
            group_id,
            reason: assignment.reason.clone(),
            restart: false,
        }
    }

    fn codec_failure(&self, client_id: &str, failed_codec: &str) -> Option<GroupAssignment> {
        let failed_codec = failed_codec.to_ascii_lowercase();
        let (current_group_id, supported_codecs, rejected_codecs) = {
            let mut assignments = self.assignments.lock().ok()?;
            let assignment = assignments.get_mut(client_id)?;
            assignment.rejected_codecs.insert(failed_codec.clone());
            (
                assignment.group_id,
                assignment.supported_codecs.clone(),
                assignment.rejected_codecs.clone(),
            )
        };
        let probe = ViewerBootstrap {
            download_bps: 0.0,
            latency_ms: 0.0,
            timed_out: false,
            video_capabilities: Vec::new(),
        };
        let codec = self.select_codec(&supported_codecs, &rejected_codecs, &probe);
        if codec == failed_codec || rejected_codecs.contains(&codec) {
            return None;
        }
        let profile = self.group_settings(current_group_id);
        let group_id =
            self.group_for_codec_profile(&codec, current_group_id, profile.clone(), profile)?;
        self.activate_group(group_id).ok()?;
        let mut assignments = self.assignments.lock().ok()?;
        let assignment = assignments.get_mut(client_id)?;
        assignment.group_id = group_id;
        assignment.last_change = Instant::now();
        assignment.reason = format!(
            "{} decode failed; fell back to {}",
            display_codec_name(&failed_codec),
            display_codec_name(&codec)
        );
        Some(GroupAssignment {
            group_id,
            reason: assignment.reason.clone(),
            restart: true,
        })
    }

    fn maintain(&self) -> GroupMaintenance {
        const DRAIN_GRACE: Duration = Duration::from_secs(12);

        let Ok(_lifecycle) = self.lifecycle_lock.lock() else {
            return GroupMaintenance {
                changed: false,
                migrations: Vec::new(),
            };
        };

        let now = Instant::now();
        let mut changed = false;
        let mut migrations = Vec::new();
        let active_group_ids = self.active_group_ids();
        let assignments_snapshot = self
            .assignments
            .lock()
            .map(|assignments| assignments.clone())
            .unwrap_or_default();
        let mut member_counts = HashMap::<usize, usize>::new();
        for assignment in assignments_snapshot.values() {
            *member_counts.entry(assignment.group_id).or_default() += 1;
        }

        for source_id in active_group_ids.iter().copied().filter(|id| *id != 0) {
            let source_settings = self.group_settings(source_id);
            let source_codec = self.group_codec(source_id);
            let source_members = member_counts.get(&source_id).copied().unwrap_or_default();
            if source_members == 0 {
                continue;
            }
            let target_id = active_group_ids
                .iter()
                .copied()
                .filter(|target_id| *target_id != source_id)
                .filter(|target_id| {
                    self.group_codec(*target_id)
                        .eq_ignore_ascii_case(&source_codec)
                        && profiles_are_similar(&source_settings, &self.group_settings(*target_id))
                })
                .max_by_key(|target_id| member_counts.get(target_id).copied().unwrap_or_default());
            let Some(target_id) = target_id else {
                continue;
            };
            if member_counts.get(&target_id).copied().unwrap_or_default() < source_members {
                continue;
            }
            if let Ok(mut assignments) = self.assignments.lock() {
                for (client_id, assignment) in assignments.iter_mut() {
                    if assignment.group_id != source_id {
                        continue;
                    }
                    assignment.group_id = target_id;
                    assignment.last_change = now;
                    assignment.reason = "merged with a near-identical group".to_owned();
                    migrations.push(GroupMigration {
                        client_id: client_id.clone(),
                        assignment: GroupAssignment {
                            group_id: target_id,
                            reason: assignment.reason.clone(),
                            restart: true,
                        },
                    });
                }
            }
            if let Ok(mut lifecycle) = self.group(source_id).lifecycle.lock() {
                *lifecycle = GroupLifecycle::Draining;
            }
            if let Ok(mut drain_started_at) = self.group(source_id).drain_started_at.lock() {
                *drain_started_at = Some(now);
            }
            changed = true;
        }

        let assignments = self
            .assignments
            .lock()
            .map(|assignments| assignments.clone())
            .unwrap_or_default();
        for group in self
            .groups
            .iter()
            .filter(|group| group.id != 0 && group.id < self.capacity())
        {
            let has_members = assignments
                .values()
                .any(|assignment| assignment.group_id == group.id);
            if has_members {
                if group.lifecycle() == GroupLifecycle::Draining {
                    if let Ok(mut lifecycle) = group.lifecycle.lock() {
                        *lifecycle = GroupLifecycle::Active;
                    }
                    changed = true;
                }
                if let Ok(mut drain_started_at) = group.drain_started_at.lock() {
                    *drain_started_at = None;
                }
                continue;
            }
            if group.media().is_none() {
                continue;
            }
            let started_at = group
                .drain_started_at
                .lock()
                .ok()
                .and_then(|started_at| *started_at);
            if started_at.is_none() {
                if let Ok(mut lifecycle) = group.lifecycle.lock() {
                    *lifecycle = GroupLifecycle::Draining;
                }
                if let Ok(mut drain_started_at) = group.drain_started_at.lock() {
                    *drain_started_at = Some(now);
                }
                changed = true;
                continue;
            }
            if now.duration_since(started_at.expect("checked above")) < DRAIN_GRACE {
                continue;
            }
            if let Ok(mut media) = group.media.lock()
                && let Some(media) = media.take()
            {
                media.stop();
            }
            if let Ok(mut lifecycle) = group.lifecycle.lock() {
                *lifecycle = GroupLifecycle::Stopped;
            }
            if let Ok(mut drain_started_at) = group.drain_started_at.lock() {
                *drain_started_at = None;
            }
            changed = true;
        }

        GroupMaintenance {
            changed,
            migrations,
        }
    }

    fn remove_client(&self, client_id: &str) {
        if let Ok(mut assignments) = self.assignments.lock() {
            assignments.remove(client_id);
        }
    }
}

fn configured_group_count(value: &str) -> usize {
    value.parse::<usize>().unwrap_or(2).clamp(1, 4)
}

fn lower_height(height: u32) -> u32 {
    match height {
        value if value > 2160 => 2160,
        value if value > 1440 => 1440,
        value if value > 1080 => 1080,
        value if value > 720 => 720,
        value if value > 480 => 480,
        value if value > 360 => 360,
        value if value > 240 => 240,
        _ => 144,
    }
}

fn lower_fps(fps: u32) -> u32 {
    match fps {
        value if value > 60 => 60,
        value if value > 30 => 30,
        value if value > 24 => 24,
        value if value > 15 => 15,
        value if value > 10 => 10,
        _ => 5,
    }
}

fn higher_fps(fps: u32, ceiling: u32) -> u32 {
    [5, 10, 15, 24, 30, 60, 75, 120]
        .into_iter()
        .find(|value| *value > fps && *value <= ceiling)
        .unwrap_or(ceiling)
}

fn higher_height(height: u32, ceiling: u32) -> u32 {
    [144, 240, 360, 480, 720, 1080, 1440, 2160, 4320]
        .into_iter()
        .find(|value| *value > height && *value <= ceiling)
        .unwrap_or(ceiling)
}

fn group_settings(base: &CaptureSettings, group_id: usize) -> CaptureSettings {
    let mut settings = base.clone();
    let mut height = base.output_height.unwrap_or(base.height);
    let mut fps = base.output_fps.unwrap_or(base.fps);
    let mut bitrate = base.bitrate;
    for _ in 0..group_id {
        height = lower_height(height);
        fps = match fps {
            value if value > 30 => 30,
            value if value > 24 => 24,
            value if value > 15 => 15,
            value if value > 10 => 10,
            _ => 5,
        };
        bitrate = (bitrate.saturating_mul(3) / 5).max(250_000);
    }
    settings.output_height = Some(height);
    settings.output_fps = Some(fps);
    settings.bitrate = bitrate;
    settings
}

fn profiles_are_similar(left: &CaptureSettings, right: &CaptureSettings) -> bool {
    fn close(left: u32, right: u32, tolerance: f64) -> bool {
        let maximum = left.max(right).max(1) as f64;
        (left as f64 - right as f64).abs() / maximum <= tolerance
    }

    close(
        left.output_height.unwrap_or(left.height),
        right.output_height.unwrap_or(right.height),
        0.15,
    ) && close(
        left.output_fps.unwrap_or(left.fps),
        right.output_fps.unwrap_or(right.fps),
        0.15,
    ) && close(left.bitrate, right.bitrate, 0.20)
}

fn initial_codec_for_policy(policy: &str) -> &str {
    match policy.to_ascii_lowercase().as_str() {
        "vp8" => "vp8",
        "vp9" => "vp9",
        "h264" => "h264",
        _ => "vp8",
    }
}

fn discover_host_codecs(ffmpeg: &str, initial_codec: &str) -> HashSet<String> {
    let mut codecs = HashSet::from(["vp8".to_owned(), initial_codec.to_ascii_lowercase()]);
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output();
    let Ok(output) = output else {
        return codecs;
    };
    let listing = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if listing.contains("libvpx-vp9") {
        codecs.insert("vp9".to_owned());
    }
    if listing.contains("libx264") || listing.contains("h264_nvenc") {
        codecs.insert("h264".to_owned());
    }
    codecs
}

fn viewer_supported_codecs(probe: &ViewerBootstrap) -> HashSet<String> {
    probe
        .video_capabilities
        .iter()
        .filter_map(|capability| {
            capability
                .mime_type
                .trim()
                .rsplit('/')
                .next()
                .map(|codec| codec.to_ascii_lowercase())
        })
        .filter(|codec| matches!(codec.as_str(), "vp8" | "vp9" | "h264" | "av1"))
        .collect()
}

fn display_codec_name(codec: &str) -> &'static str {
    match codec.to_ascii_lowercase().as_str() {
        "vp8" => "VP8",
        "vp9" => "VP9",
        "h264" => "H.264",
        "av1" => "AV1",
        _ => "Unknown",
    }
}

fn choose_codec(
    policy: &str,
    host_codecs: &HashSet<String>,
    supported_codecs: &HashSet<String>,
    rejected_codecs: &HashSet<String>,
    download_bps: f64,
    latency_ms: f64,
) -> String {
    let normalized_policy = policy.to_ascii_lowercase();
    let candidates: Vec<&str> = if normalized_policy != "auto" {
        vec![normalized_policy.as_str(), "vp8", "vp9", "h264"]
    } else {
        // VP8 is the validated browser baseline. Other codecs remain
        // available explicitly and as fallbacks, but must not outrank a codec
        // that is known to produce decodable first frames across browsers.
        let _ = (download_bps, latency_ms);
        vec!["vp8", "vp9", "h264"]
    };
    candidates
        .into_iter()
        .find(|codec| {
            host_codecs.contains::<str>(codec)
                && !rejected_codecs.contains::<str>(codec)
                && (supported_codecs.contains::<str>(codec)
                    || (supported_codecs.is_empty() && *codec == "vp8"))
        })
        .unwrap_or("vp8")
        .to_owned()
}

#[derive(Debug, Clone, Deserialize)]
struct SocketAuth {
    token: String,
    #[serde(rename = "clientId")]
    client_id: String,
}

#[derive(Debug, Clone)]
struct ClientIdentity {
    client_id: String,
}

#[derive(Debug, Deserialize)]
struct ViewerStats {
    #[serde(rename = "rttMs", default)]
    rtt_ms: f64,
    #[serde(rename = "jitterMs", default)]
    jitter_ms: f64,
    #[serde(rename = "lossRate", default)]
    loss_rate: f64,
    #[serde(rename = "bitrateBps", default)]
    bitrate_bps: f64,
    #[serde(rename = "availableIncomingBitrateBps")]
    available_incoming_bitrate_bps: Option<f64>,
    #[serde(rename = "framesDropped", default)]
    frames_dropped: u64,
    #[serde(rename = "freezeCount", default)]
    freeze_count: u64,
    #[serde(rename = "visibilityState", default = "visible_document")]
    visibility_state: String,
}

#[derive(Debug, Deserialize)]
struct ViewerBootstrap {
    #[serde(rename = "downloadBps", default)]
    download_bps: f64,
    #[serde(rename = "latencyMs", default)]
    latency_ms: f64,
    #[serde(rename = "timedOut", default)]
    timed_out: bool,
    #[serde(rename = "videoCapabilities", default)]
    video_capabilities: Vec<ViewerVideoCapability>,
}

#[derive(Debug, Deserialize)]
struct ViewerVideoCapability {
    #[serde(rename = "mimeType")]
    mime_type: String,
}

#[derive(Debug, Deserialize)]
struct ViewerCodecFailure {
    codec: String,
    #[serde(default)]
    reason: String,
}

fn visible_document() -> String {
    "visible".to_owned()
}

#[derive(Debug, Deserialize)]
struct ControlPing {
    #[serde(rename = "sentAt")]
    sent_at: f64,
}

#[derive(Debug, Deserialize)]
struct ViewerFrameTimingRequest {
    #[serde(rename = "rtpTimestamp")]
    rtp_timestamp: u32,
}

#[derive(Clone)]
struct PeerHandler {
    gather_complete: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    media: Arc<MediaPipeline>,
    audio: Option<Arc<AudioPipeline>>,
    stream_enabled: Arc<AtomicBool>,
    connection_id: Uuid,
    connections: Arc<Mutex<std::collections::HashMap<Uuid, Arc<dyn PeerConnection>>>>,
    udp_mux: Arc<UdpMux>,
    connected_connections: Arc<StdMutex<HashSet<Uuid>>>,
    client_connections: Arc<StdMutex<std::collections::HashMap<String, Uuid>>>,
    connection_bindings: Arc<StdMutex<HashMap<Uuid, ConnectionMediaBinding>>>,
    client_id: String,
    state: ServerState,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for PeerHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete
            && let Some(sender) = self.gather_complete.lock().await.take()
        {
            let _ = sender.send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if state == RTCPeerConnectionState::Connected {
            if let Ok(mut connected) = self.connected_connections.lock() {
                connected.insert(self.connection_id);
            }
            broadcast_status(&self.state);
            if self.stream_enabled.load(Ordering::Acquire) {
                let media = Arc::clone(&self.media);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    media.activate();
                });
                if let Some(audio) = &self.audio {
                    audio.activate();
                }
            }
        }
        let disconnected = if state == RTCPeerConnectionState::Disconnected {
            self.connected_connections
                .lock()
                .map(|mut connected| connected.remove(&self.connection_id))
                .unwrap_or(false)
        } else {
            false
        };
        if disconnected {
            broadcast_status(&self.state);
        }
        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        ) {
            self.connections.lock().await.remove(&self.connection_id);
            self.media.unsubscribe(self.connection_id);
            if let Some(audio) = &self.audio {
                audio.unsubscribe(self.connection_id);
            }
            self.udp_mux.unregister(self.connection_id);
            if let Ok(mut connected) = self.connected_connections.lock() {
                connected.remove(&self.connection_id);
            }
            if let Ok(mut clients) = self.client_connections.lock()
                && clients.get(&self.client_id) == Some(&self.connection_id)
            {
                clients.remove(&self.client_id);
            }
            if let Ok(mut bindings) = self.connection_bindings.lock() {
                bindings.remove(&self.connection_id);
            }
            broadcast_status(&self.state);
        }
        info!(state = %state, "WebRTC peer state changed");
    }
}

pub async fn run(config: AppConfig, shutdown: oneshot::Receiver<()>) -> Result<()> {
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = control_tx.send(ServerCommand::StartStream {
        settings: CaptureSettings::from_config(&config),
        result: None,
    });
    run_with_control(config, shutdown, control_rx).await
}

pub async fn run_with_control(
    config: AppConfig,
    shutdown: oneshot::Receiver<()>,
    control_rx: tokio::sync::mpsc::UnboundedReceiver<ServerCommand>,
) -> Result<()> {
    run_with_control_readiness(config, shutdown, control_rx, None, None).await
}

/// Runs the host and reports readiness only once its TCP listener owns the
/// configured HTTP address.  Startup failures are returned normally, which
/// lets native callers keep a failed start retryable.
pub async fn run_with_control_readiness(
    config: AppConfig,
    mut shutdown: oneshot::Receiver<()>,
    mut control_rx: tokio::sync::mpsc::UnboundedReceiver<ServerCommand>,
    ready: Option<oneshot::Sender<()>>,
    stream_failure_callback: Option<StreamFailureCallback>,
) -> Result<()> {
    let addr = config.http_addr()?;
    // Bind the public listener before any background work is started. This is
    // the most common fallible startup step; doing it first prevents a port
    // collision from leaving audio or control tasks alive.
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let listener_addr = listener.local_addr()?;
    #[cfg(windows)]
    if config.source.kind == "window" {
        crate::window_capture::WindowCapture::dimensions_for(
            config.source.index,
            config.source.native_id,
        )
        .context("validate selected Windows Graphics Capture window")?;
    } else if config.source.kind != "test" {
        crate::capture::ffmpeg_input_args(
            &config.source.kind,
            config.source.index,
            config.source.native_id,
            config.output_fps(),
            config.draw_mouse,
        )
        .context("validate selected capture source")?;
    }
    #[cfg(not(windows))]
    if config.source.kind != "test" {
        crate::capture::ffmpeg_input_args(
            &config.source.kind,
            config.source.index,
            config.source.native_id,
            config.output_fps(),
            config.draw_mouse,
        )
        .context("validate selected capture source")?;
    }
    let initial_codec = initial_codec_for_policy(&config.codec);
    // Keep one stable audio track graph even while audio is disabled. This
    // lets live source/mode changes reopen the native loopback input without
    // invalidating already-negotiated WebRTC tracks.
    let audio = Some(Arc::new(AudioPipeline::new()));
    let stream_enabled = Arc::new(AtomicBool::new(false));
    let media_bind_addr = format!("{}:{}", config.media_bind_host(), config.media_ports.first)
        .parse()
        .with_context(|| "invalid media bind address")?;
    let media_candidate_addr = format!(
        "{}:{}",
        config.advertised_host_for_media(),
        config.media_ports.first
    )
    .parse()
    .with_context(|| "invalid media candidate address")?;
    let udp_mux = UdpMux::bind(media_bind_addr, media_candidate_addr)?;
    if config.media_ports.first != config.media_ports.last {
        warn!(
            configured_first = config.media_ports.first,
            configured_last = config.media_ports.last,
            "media port range is deprecated; only the first port is used by the shared UDP mux"
        );
    }
    let ffmpeg = crate::packaging::prepare_ffmpeg()?;
    let initial_settings = CaptureSettings::from_config(&config);
    let settings_slot = Arc::new(StdMutex::new(initial_settings.clone()));
    let capture_slot = Arc::new(CaptureSlot::default());
    let group_budget = if config.quality_mode == "adaptive" {
        configured_group_count(&config.max_quality_groups)
    } else {
        1
    };
    let group_factory = Arc::new(GroupFactory {
        ffmpeg: ffmpeg.command.clone(),
        capture_slot: Arc::clone(&capture_slot),
        codec_policy: config.codec.clone(),
        host_codecs: Arc::new(discover_host_codecs(&ffmpeg.command, initial_codec)),
        stream_enabled: Arc::clone(&stream_enabled),
        tasks: Arc::new(StdMutex::new(Vec::new())),
        pipelines: Arc::new(StdMutex::new(Vec::new())),
    });
    // Always allocate the four lightweight group slots.  The configured group
    // count is a live budget, not a startup-only topology decision.
    let mut groups = Vec::with_capacity(4);
    for group_id in 0..4 {
        let settings = group_settings(&initial_settings, group_id);
        groups.push(TranscodeGroup::stopped(group_id, settings, initial_codec));
    }
    let groups = Arc::new(TranscodeGroups::with_factory(
        groups,
        Some(Arc::clone(&group_factory)),
    ));
    groups.active_budget.store(group_budget, Ordering::Release);
    let viewer_metrics = Arc::new(StdMutex::new(HashMap::new()));
    let state = ServerState {
        config: Arc::new(config.clone()),
        active_token: Arc::new(StdMutex::new(config.token.clone())),
        settings_revision: Arc::new(AtomicU64::new(0)),
        media_session_revision: Arc::new(AtomicU64::new(0)),
        stop_request_revision: Arc::new(AtomicU64::new(0)),
        audio: audio.clone(),
        stream_enabled: Arc::clone(&stream_enabled),
        stream_resetting: Arc::new(AtomicBool::new(false)),
        max_viewers: Arc::new(AtomicUsize::new(config.max_viewers.max(1))),
        settings: Arc::clone(&settings_slot),
        viewer_metrics: Arc::clone(&viewer_metrics),
        groups: Arc::clone(&groups),
        shared_capture: Arc::clone(&capture_slot),
        connections: Arc::new(Mutex::new(std::collections::HashMap::new())),
        udp_mux,
        pending_connections: Arc::new(StdMutex::new(HashSet::new())),
        connected_connections: Arc::new(StdMutex::new(HashSet::new())),
        client_connections: Arc::new(StdMutex::new(std::collections::HashMap::new())),
        connection_bindings: Arc::new(StdMutex::new(HashMap::new())),
        client_sockets: Arc::new(StdMutex::new(std::collections::HashMap::new())),
        offer_locks: Arc::new(StdMutex::new(HashMap::new())),
        session_generation: Arc::new(AtomicU64::new(0)),
        session_gate: Arc::new(RwLock::new(())),
        stream_failure_callback,
    };
    // Persist readiness before spawning background tasks so a write failure
    // cannot orphan any of them. The listener and UDP mux clean up on drop.
    crate::runtime::write(&crate::runtime::RuntimeStatus {
        pid: std::process::id(),
        http_port: config.http_port,
        media_port: config.media_ports.first,
        // The runtime record is discoverable process metadata, not a secret
        // store. Keep the URL shape useful without persisting the bearer token.
        viewer_url: redacted_viewer_url(&config),
        source: config
            .source
            .native_id
            .map(|native_id| format!("{}:{native_id}", config.source.kind))
            .unwrap_or_else(|| format!("{}:{}", config.source.kind, config.source.index)),
        codec: display_codec_name(initial_codec).to_owned(),
    })?;
    let audio_task = audio
        .as_ref()
        .map(|audio| Arc::clone(audio).spawn(initial_settings.clone()));
    // Both controllers remain available for live Manual <-> Auto changes.
    // Each loop reads the current settings before it acts, so inactive modes
    // are idle rather than retaining the behavior selected at server startup.
    let adaptive_tasks = vec![
        tokio::spawn(adaptive_loop(state.clone())),
        tokio::spawn(group_adaptive_loop(state.clone())),
    ];
    // Feed both UI commands and internal resize refreshes through one queue so
    // capture replacement cannot race Stop/Start or a settings update.
    let (serialized_control_tx, mut serialized_control_rx) = tokio::sync::mpsc::unbounded_channel();
    let (urgent_stop_tx, mut urgent_stop_rx) = tokio::sync::mpsc::unbounded_channel();
    let forwarded_control_tx = serialized_control_tx.clone();
    let control_forward_task = tokio::spawn(async move {
        while let Some(command) = control_rx.recv().await {
            let sent = match command {
                ServerCommand::StopStream { result } => urgent_stop_tx.send(result).is_ok(),
                command => forwarded_control_tx.send(command).is_ok(),
            };
            if !sent {
                break;
            }
        }
    });
    let stop_state = state.clone();
    let stop_finalize_tx = serialized_control_tx.clone();
    let urgent_stop_task = tokio::spawn(async move {
        while let Some(result) = urgent_stop_rx.recv().await {
            stop_state.stream_resetting.store(false, Ordering::Release);
            stop_state
                .stop_request_revision
                .fetch_add(1, Ordering::AcqRel);
            request_stream_stop(&stop_state);
            let _ = stop_finalize_tx.send(ServerCommand::FinalizeStop);
            if let Some(result) = result {
                let _ = result.send(());
            }
        }
    });
    #[cfg(windows)]
    let window_resize_task = tokio::spawn(window_resize_monitor(
        state.clone(),
        serialized_control_tx.clone(),
    ));
    let stream_recovery_task = tokio::spawn(stream_health_monitor(
        state.clone(),
        serialized_control_tx.clone(),
    ));
    #[cfg(windows)]
    let window_retry_tx = serialized_control_tx.clone();
    drop(serialized_control_tx);
    let control_state = state.clone();
    let control_task = tokio::spawn(async move {
        while let Some(command) = serialized_control_rx.recv().await {
            match command {
                ServerCommand::StartStream { settings, result } => {
                    control_state
                        .stream_resetting
                        .store(false, Ordering::Release);
                    // A user Start supersedes any delayed automatic window
                    // resize retry just as decisively as an explicit Stop.
                    // Reuse the host-command revision so stale retry commands
                    // cannot tear down the newly requested stream.
                    control_state
                        .stop_request_revision
                        .fetch_add(1, Ordering::AcqRel);
                    if let Err(error) = apply_capture_settings(&control_state, settings.clone()) {
                        warn!(%error, "could not apply stream settings");
                        if let Some(result) = result {
                            let _ = result.send(Err(error.to_string()));
                        }
                        continue;
                    }
                    if !control_state.stream_enabled.load(Ordering::Acquire) {
                        let startup = (|| -> Result<()> {
                            let capture =
                                Arc::new(SharedCapture::start(&ffmpeg.command, settings.clone())?);
                            control_state.shared_capture.install(capture)?;
                            control_state.groups.start_primary()?;
                            Ok(())
                        })();
                        if let Err(error) = startup {
                            stop_stream_resources(&control_state).await;
                            warn!(%error, "could not start stream capture");
                            if let Some(result) = result {
                                let _ = result.send(Err(error.to_string()));
                            }
                            continue;
                        }
                    }
                    control_state.stream_enabled.store(true, Ordering::Release);
                    // A new host start is a fresh media session, even when its
                    // capture settings match the previous run.  Existing
                    // browser peers must re-offer to receive the new graph.
                    control_state
                        .media_session_revision
                        .fetch_add(1, Ordering::AcqRel);
                    control_state.groups.activate();
                    if let Some(audio) = &control_state.audio {
                        audio.activate();
                    }
                    broadcast_status(&control_state);
                    if let Some(result) = result {
                        let _ = result.send(Ok(()));
                    }
                }
                ServerCommand::StopStream { result } => {
                    control_state
                        .stream_resetting
                        .store(false, Ordering::Release);
                    control_state
                        .stop_request_revision
                        .fetch_add(1, Ordering::AcqRel);
                    request_stream_stop(&control_state);
                    stop_stream_resources(&control_state).await;
                    broadcast_status(&control_state);
                    if let Some(result) = result {
                        let _ = result.send(());
                    }
                }
                ServerCommand::FinalizeStop => {
                    stop_stream_resources(&control_state).await;
                    broadcast_status(&control_state);
                }
                ServerCommand::Update(settings) => {
                    if let Err(error) = apply_capture_settings(&control_state, settings) {
                        warn!(%error, "could not update capture settings");
                        continue;
                    }
                    broadcast_status(&control_state);
                }
                ServerCommand::UpdateMaxViewers(max_viewers) => {
                    control_state
                        .max_viewers
                        .store(max_viewers.max(1), Ordering::Release);
                    broadcast_status(&control_state);
                }
                ServerCommand::UpdateMediaCandidateHost(host) => {
                    match update_media_candidate(&control_state, &host).await {
                        Ok(candidate_addr) => {
                            info!(%candidate_addr, "updated WebRTC media candidate");
                            broadcast_status(&control_state);
                        }
                        Err(error) => {
                            warn!(%error, %host, "could not resolve WebRTC media candidate host");
                        }
                    }
                }
                ServerCommand::PreviewSnapshot { result } => {
                    let settings = control_state
                        .settings
                        .lock()
                        .ok()
                        .map(|settings| settings.clone());
                    let frame = control_state
                        .shared_capture
                        .current()
                        .and_then(|capture| capture.latest_frame_snapshot());
                    let snapshot = settings
                        .zip(frame)
                        .map(|(settings, frame)| CapturePreviewSnapshot { settings, frame });
                    let _ = result.try_send(snapshot);
                }
                ServerCommand::ResetToken(token) => {
                    reset_token_and_revoke(&control_state, token).await;
                }
                ServerCommand::RecoverStream { pending } => {
                    let _pending_guard = RecoveryPendingGuard(pending);
                    if !control_state.stream_enabled.load(Ordering::Acquire) {
                        continue;
                    }
                    let stop_request_revision =
                        control_state.stop_request_revision.load(Ordering::Acquire);
                    let Some(reason) = stream_failure(&control_state) else {
                        // A resize replacement may have completed before this
                        // queued health event reached the serialized worker.
                        continue;
                    };
                    warn!(%reason, "stream pipeline failed; restarting media graph");
                    let settings = match control_state.settings.lock() {
                        Ok(settings) => settings.clone(),
                        Err(_) => {
                            warn!("could not recover stream: capture settings lock poisoned");
                            continue;
                        }
                    };
                    // Recovery is an internal Stop -> Start. Publish the cold
                    // state first so viewers tear down the failed peer before
                    // the replacement graph can accept a new offer.
                    control_state
                        .stream_resetting
                        .store(true, Ordering::Release);
                    request_stream_stop(&control_state);
                    broadcast_status(&control_state);
                    stop_stream_resources(&control_state).await;
                    if control_state.stop_request_revision.load(Ordering::Acquire)
                        != stop_request_revision
                    {
                        broadcast_status(&control_state);
                        continue;
                    }
                    let recovery = (|| -> Result<()> {
                        let capture =
                            Arc::new(SharedCapture::start(&ffmpeg.command, settings.clone())?);
                        control_state.shared_capture.install(capture)?;
                        control_state.groups.start_primary()?;
                        Ok(())
                    })();
                    if control_state.stop_request_revision.load(Ordering::Acquire)
                        != stop_request_revision
                    {
                        stop_stream_resources(&control_state).await;
                        broadcast_status(&control_state);
                        continue;
                    }
                    match recovery {
                        Ok(()) => {
                            control_state
                                .stream_resetting
                                .store(false, Ordering::Release);
                            control_state.stream_enabled.store(true, Ordering::Release);
                            if control_state.stop_request_revision.load(Ordering::Acquire)
                                != stop_request_revision
                            {
                                stop_stream_resources(&control_state).await;
                                broadcast_status(&control_state);
                                continue;
                            }
                            control_state
                                .media_session_revision
                                .fetch_add(1, Ordering::AcqRel);
                            control_state.groups.activate();
                            if let Some(audio) = &control_state.audio {
                                audio.activate();
                            }
                            if control_state.stop_request_revision.load(Ordering::Acquire)
                                != stop_request_revision
                            {
                                stop_stream_resources(&control_state).await;
                                broadcast_status(&control_state);
                                continue;
                            }
                            // Match manual Start: the new authoritative media
                            // session revision causes one clean re-offer.
                            broadcast_status(&control_state);
                            info!(%reason, "stream pipeline recovered");
                        }
                        Err(error) => {
                            stop_stream_resources(&control_state).await;
                            if control_state.stop_request_revision.load(Ordering::Acquire)
                                != stop_request_revision
                            {
                                broadcast_status(&control_state);
                                continue;
                            }
                            control_state
                                .stream_resetting
                                .store(false, Ordering::Release);
                            control_state.stream_enabled.store(false, Ordering::Release);
                            control_state
                                .media_session_revision
                                .fetch_add(1, Ordering::AcqRel);
                            broadcast_status(&control_state);
                            warn!(%error, %reason, "automatic stream recovery failed");
                            if let Some(callback) = &control_state.stream_failure_callback {
                                callback(format!(
                                    "stream stopped after automatic recovery failed: {error}"
                                ));
                            }
                        }
                    }
                }
                #[cfg(windows)]
                ServerCommand::RefreshWindowCapture {
                    source_index,
                    source_native_id,
                    dimensions,
                } => {
                    match prepare_window_capture_restart(
                        &control_state,
                        source_index,
                        source_native_id,
                        dimensions,
                    ) {
                        Ok(Some((settings, previous_dimensions, stop_request_revision))) => {
                            execute_window_capture_restart_attempt(
                                &control_state,
                                &ffmpeg.command,
                                &window_retry_tx,
                                settings,
                                previous_dimensions,
                                stop_request_revision,
                                1,
                            )
                            .await;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            warn!(%error, "could not prepare resized window capture restart");
                        }
                    }
                }
                #[cfg(windows)]
                ServerCommand::RetryWindowCapture {
                    previous_dimensions,
                    stop_request_revision,
                    attempt,
                } => {
                    let settings = match control_state.settings.lock() {
                        Ok(settings) => settings.clone(),
                        Err(_) => {
                            warn!(
                                "could not retry resized capture: capture settings lock poisoned"
                            );
                            continue;
                        }
                    };
                    execute_window_capture_restart_attempt(
                        &control_state,
                        &ffmpeg.command,
                        &window_retry_tx,
                        settings,
                        previous_dimensions,
                        stop_request_revision,
                        attempt,
                    )
                    .await;
                }
            }
        }
    });
    let (socket_layer, socket_io) = SocketIo::builder()
        .req_path("/ws")
        .with_state(state.clone())
        .max_payload(128 * 1024)
        .max_buffer_size(128)
        .build_layer();
    socket_io.ns("/", control_connect);
    let app = router(state.clone()).layer(socket_layer);
    info!(address = %listener_addr, viewer_url = %redacted_viewer_url(&config), "viewer server listening");
    if let Some(ready) = ready {
        let _ = ready.send(());
    }
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = (&mut shutdown).await;
            info!("viewer server shutting down");
        })
        .await;

    let _ = socket_io.close().await;
    control_forward_task.abort();
    urgent_stop_task.abort();
    control_task.abort();
    stream_recovery_task.abort();
    #[cfg(windows)]
    window_resize_task.abort();
    for adaptive_task in adaptive_tasks {
        adaptive_task.abort();
    }
    stop_stream_resources(&state).await;
    if let Some(audio) = &audio {
        audio.stop();
    }
    if let Some(audio_task) = audio_task {
        let _ = audio_task.await;
    }
    let connections = state.connections.lock().await.clone();
    for connection in connections.into_values() {
        let _ = connection.close().await;
    }
    state.connections.lock().await.clear();
    tokio::time::sleep(Duration::from_millis(100)).await;
    crate::runtime::remove(config.http_port);
    serve_result?;
    drop(state);
    Ok(())
}

fn redacted_viewer_url(config: &AppConfig) -> String {
    let url = config.viewer_url();
    url.rsplit_once('/')
        .map(|(origin, _)| format!("{origin}/<token>"))
        .unwrap_or_else(|| "<redacted>".to_owned())
}

fn request_stream_stop(state: &ServerState) {
    if state.stream_enabled.swap(false, Ordering::AcqRel) {
        state.media_session_revision.fetch_add(1, Ordering::AcqRel);
    }
    state.groups.request_stop();
    if let Some(capture) = state.shared_capture.current() {
        capture.request_stop();
    }
    if let Some(audio) = &state.audio {
        audio.deactivate();
    }
}

async fn stop_stream_resources(state: &ServerState) {
    request_stream_stop(state);

    // Cleanup APIs can enter driver or pipe waits. Run them off the async
    // control worker and bound the whole teardown; the urgent phase above has
    // already invalidated capture generations and signalled FFmpeg killers.
    // Detach group slots synchronously first. A cleanup task that outlives the
    // deadline then owns only old pipelines and cannot stop a newly installed
    // encoder graph.
    let detached_media = state.groups.detach_all_media();
    let groups_cleanup = tokio::task::spawn_blocking(move || {
        for media in detached_media {
            media.stop();
        }
    });
    let capture_cleanup = state.shared_capture.take().map(|capture| {
        capture.request_stop();
        tokio::task::spawn_blocking(move || {
            let _ = capture.stop();
        })
    });
    let mut media_tasks = state.groups.take_tasks();
    let cleanup = async {
        let _ = groups_cleanup.await;
        if let Some(capture_cleanup) = capture_cleanup {
            let _ = capture_cleanup.await;
        }
        for task in &mut media_tasks {
            let _ = task.await;
        }
    };
    if tokio::time::timeout(STREAM_CLEANUP_TIMEOUT, cleanup)
        .await
        .is_err()
    {
        warn!(
            timeout_ms = STREAM_CLEANUP_TIMEOUT.as_millis(),
            "stream cleanup exceeded its deadline; abandoning stuck teardown tasks"
        );
        for task in media_tasks {
            task.abort();
        }
    }
}

#[cfg(windows)]
async fn window_resize_monitor(
    state: ServerState,
    control_tx: tokio::sync::mpsc::UnboundedSender<ServerCommand>,
) {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut event_watcher: Option<WindowResizeWatcher> = None;
    let mut watched_source: Option<WindowSourceIdentity> = None;
    let mut safety_interval = tokio::time::interval(WINDOW_RESIZE_SAFETY_POLL_INTERVAL);
    safety_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut debouncer = WindowResizeDebouncer::default();
    loop {
        tokio::select! {
            signal = event_rx.recv() => {
                let Some(signal) = signal else { break };
                let mut quiet_time = match signal {
                    WindowResizeEvent::LocationChange => WINDOW_RESIZE_EVENT_QUIET_TIME,
                    WindowResizeEvent::MoveSizeEnd => WINDOW_RESIZE_FRAME_GRACE,
                };
                // Location-change events arrive repeatedly during an
                // interactive resize. Wait for the event stream to go quiet,
                // while treating the native move/size-end event as the
                // stronger completion signal.
                loop {
                    match tokio::time::timeout(quiet_time, event_rx.recv()).await {
                        Ok(Some(next)) => {
                            quiet_time = match next {
                                WindowResizeEvent::LocationChange => {
                                    WINDOW_RESIZE_EVENT_QUIET_TIME
                                }
                                WindowResizeEvent::MoveSizeEnd => WINDOW_RESIZE_FRAME_GRACE,
                            };
                        }
                        Ok(None) => return,
                        Err(_) => break,
                    }
                }
                sync_window_resize_watcher(
                    &state,
                    &mut event_watcher,
                    &mut watched_source,
                    &event_tx,
                );
                if let Some((source, dimensions)) =
                    observe_window_resize(&state, &mut debouncer, true)
                    && control_tx
                        .send(ServerCommand::RefreshWindowCapture {
                            source_index: source.index,
                            source_native_id: source.native_id,
                            dimensions,
                        })
                        .is_err()
                {
                    break;
                }
            }
            _ = safety_interval.tick() => {
                sync_window_resize_watcher(
                    &state,
                    &mut event_watcher,
                    &mut watched_source,
                    &event_tx,
                );
                if let Some((source, dimensions)) =
                    observe_window_resize(&state, &mut debouncer, false)
                    && control_tx
                        .send(ServerCommand::RefreshWindowCapture {
                            source_index: source.index,
                            source_native_id: source.native_id,
                            dimensions,
                        })
                        .is_err()
                {
                    break;
                }
            }
        }
    }
}

#[cfg(windows)]
fn sync_window_resize_watcher(
    state: &ServerState,
    event_watcher: &mut Option<WindowResizeWatcher>,
    watched_source: &mut Option<WindowSourceIdentity>,
    event_tx: &tokio::sync::mpsc::UnboundedSender<WindowResizeEvent>,
) {
    let Some(settings) = state.settings.lock().ok().map(|settings| settings.clone()) else {
        event_watcher.take();
        *watched_source = None;
        return;
    };
    if settings.source_kind != "window" {
        event_watcher.take();
        *watched_source = None;
        return;
    }
    let source = WindowSourceIdentity {
        index: settings.source_index,
        native_id: settings.source_native_id,
    };
    if *watched_source != Some(source) || event_watcher.is_none() {
        event_watcher.take();
        *event_watcher =
            WindowResizeWatcher::start(source.index, source.native_id, event_tx.clone());
        *watched_source = Some(source);
    }
}

#[cfg(windows)]
fn observe_window_resize(
    state: &ServerState,
    debouncer: &mut WindowResizeDebouncer,
    event_settled: bool,
) -> Option<(WindowSourceIdentity, (u32, u32))> {
    if !state.stream_enabled.load(Ordering::Acquire) {
        debouncer.reset();
        return None;
    }
    let Some(settings) = state.settings.lock().ok().map(|settings| settings.clone()) else {
        debouncer.reset();
        return None;
    };
    if settings.source_kind != "window" {
        debouncer.reset();
        return None;
    }
    if settings
        .source_native_id
        .is_some_and(crate::capture::native_window_is_minimized)
    {
        // A queued resize is intentionally ignored while minimized. Clear
        // its emitted marker so restoring at the same size can settle and
        // request a fresh media graph.
        debouncer.reset();
        return None;
    }
    let Some(capture) = state.shared_capture.current() else {
        debouncer.reset();
        return None;
    };
    let source = WindowSourceIdentity {
        index: settings.source_index,
        native_id: settings.source_native_id,
    };
    // A maximize can invalidate the old WGC frame pool before its callback
    // publishes the new content dimensions. Querying the capture item itself
    // keeps resize recovery independent from that potentially stalled pool.
    let observed_dimensions = crate::window_capture::WindowCapture::dimensions_for(
        settings.source_index,
        settings.source_native_id,
    )
    .unwrap_or_else(|_| capture.observed_source_dimensions());
    let dimensions = if event_settled {
        debouncer.observe_settled(source, capture.source_dimensions(), observed_dimensions)
    } else {
        debouncer.observe(
            source,
            capture.source_dimensions(),
            observed_dimensions,
            Instant::now(),
        )
    }?;
    Some((source, dimensions))
}

#[cfg(windows)]
type WindowCaptureRestart = (CaptureSettings, (u32, u32), u64);

#[cfg(windows)]
fn prepare_window_capture_restart(
    state: &ServerState,
    source_index: usize,
    source_native_id: Option<u64>,
    requested_dimensions: (u32, u32),
) -> Result<Option<WindowCaptureRestart>> {
    if !state.stream_enabled.load(Ordering::Acquire) {
        return Ok(None);
    }
    let settings = state
        .settings
        .lock()
        .map_err(|_| anyhow::anyhow!("capture settings lock poisoned"))?
        .clone();
    if settings.source_kind != "window"
        || settings.source_index != source_index
        || settings.source_native_id != source_native_id
    {
        return Ok(None);
    }
    if source_native_id.is_some_and(crate::capture::native_window_is_minimized) {
        return Ok(None);
    }
    let current_capture = state
        .shared_capture
        .current()
        .context("shared capture is unavailable during window resize")?;
    let previous_dimensions = current_capture.source_dimensions();
    if previous_dimensions == requested_dimensions {
        return Ok(None);
    }
    // Reject stale queued resize commands, but do not require the old capture
    // callback to have observed the new size: that callback may have stopped
    // precisely because its frame pool no longer matches the maximized window.
    match crate::window_capture::WindowCapture::dimensions_for(
        settings.source_index,
        settings.source_native_id,
    ) {
        Ok(live_dimensions) if live_dimensions != requested_dimensions => return Ok(None),
        Err(_) if current_capture.observed_source_dimensions() != requested_dimensions => {
            return Ok(None);
        }
        _ => {}
    }

    Ok(Some((
        settings,
        previous_dimensions,
        state.stop_request_revision.load(Ordering::Acquire),
    )))
}

#[cfg(windows)]
async fn execute_window_capture_restart_attempt(
    state: &ServerState,
    ffmpeg_command: &str,
    retry_tx: &tokio::sync::mpsc::UnboundedSender<ServerCommand>,
    settings: CaptureSettings,
    previous_dimensions: (u32, u32),
    stop_request_revision: u64,
    attempt: usize,
) {
    let result = restart_window_capture_graph_once(
        state,
        ffmpeg_command,
        &settings,
        previous_dimensions,
        stop_request_revision,
        attempt,
    )
    .await;
    let Err(error) = result else {
        return;
    };
    if state.stop_request_revision.load(Ordering::Acquire) != stop_request_revision {
        return;
    }
    // The first few attempts cover the normal compositor transition.  Do not
    // turn their exhaustion into a permanently stopped stream, though: both
    // the resize observer and the health observer intentionally stand down
    // while this internal Stop -> Start is in progress.  A delayed explicit
    // retry is therefore the only component that can recover after Windows
    // makes the capture item available again.
    let delay = window_resize_retry_delay(attempt);
    if attempt <= WINDOW_RESIZE_RESTART_ATTEMPTS || attempt.is_multiple_of(10) {
        warn!(
            attempt,
            quick_attempts = WINDOW_RESIZE_RESTART_ATTEMPTS,
            retry_delay_ms = delay.as_millis(),
            %error,
            "fresh window capture did not start after resize; keeping the reset alive"
        );
    } else {
        info!(
            attempt,
            retry_delay_ms = delay.as_millis(),
            %error,
            "resized window capture is still unavailable; retrying"
        );
    }
    let retry_tx = retry_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = retry_tx.send(ServerCommand::RetryWindowCapture {
            previous_dimensions,
            stop_request_revision,
            attempt: attempt.saturating_add(1),
        });
    });
}

#[cfg(windows)]
fn window_resize_retry_delay(attempt: usize) -> Duration {
    if attempt < WINDOW_RESIZE_RESTART_ATTEMPTS {
        WINDOW_RESIZE_RETRY_DELAY * attempt as u32
    } else {
        WINDOW_RESIZE_RECOVERY_RETRY_DELAY
    }
}

#[cfg(windows)]
async fn restart_window_capture_graph_once(
    state: &ServerState,
    ffmpeg_command: &str,
    settings: &CaptureSettings,
    previous_dimensions: (u32, u32),
    stop_request_revision: u64,
    attempt: usize,
) -> Result<()> {
    if state.stop_request_revision.load(Ordering::Acquire) != stop_request_revision {
        return Ok(());
    }

    // Match the known-good manual Stop/Start behavior. Reusing a capture bus
    // after a large compositor resize leaves some WGC drivers permanently
    // wedged even when their frame pool is replaced. Tear down the complete
    // media graph and create a new SharedCapture with fresh synchronization,
    // WGC, and encoder state instead.
    //
    // Publish the stopped generation before teardown. This makes every
    // viewer discard its old peer and wait, just as it does after a manual
    // Stop. Without this transition the client can receive a group restart
    // and a session revision together, race two offers, and remain attached
    // to the retired encoder graph.
    state.stream_resetting.store(true, Ordering::Release);
    request_stream_stop(state);
    broadcast_status(state);
    stop_stream_resources(state).await;
    if state.stop_request_revision.load(Ordering::Acquire) != stop_request_revision {
        broadcast_status(state);
        return Ok(());
    }

    let startup = (|| -> Result<(u32, u32)> {
        let capture = Arc::new(SharedCapture::start(ffmpeg_command, settings.clone())?);
        let dimensions = capture.source_dimensions();
        state.shared_capture.install(capture)?;
        state.groups.start_primary()?;
        Ok(dimensions)
    })();
    let refreshed_dimensions = match startup {
        Ok(dimensions) => dimensions,
        Err(error) => {
            // If the host pressed Stop, its queued FinalizeStop owns cleanup.
            // Returning now keeps the control queue available for that Stop
            // and the subsequent user-requested Start.
            if state.stop_request_revision.load(Ordering::Acquire) != stop_request_revision {
                return Ok(());
            }
            stop_stream_resources(state).await;
            return Err(error.context("fresh capture attempt after window resize failed"));
        }
    };
    if state.stop_request_revision.load(Ordering::Acquire) != stop_request_revision {
        return Ok(());
    }
    state.stream_resetting.store(false, Ordering::Release);
    state.stream_enabled.store(true, Ordering::Release);
    state.groups.activate();
    if let Some(audio) = &state.audio {
        audio.activate();
    }
    if state.stop_request_revision.load(Ordering::Acquire) != stop_request_revision {
        return Ok(());
    }
    info!(
        previous_width = previous_dimensions.0,
        previous_height = previous_dimensions.1,
        width = refreshed_dimensions.0,
        height = refreshed_dimensions.1,
        attempt,
        "window resize rebuilt the complete live media graph"
    );
    publish_window_resize_restart(state, "window resized");
    Ok(())
}

#[cfg(windows)]
fn publish_window_resize_restart(state: &ServerState, reason: &str) {
    // Mirror a manual Start after the stopped state published above. The
    // authoritative session revision is sufficient to make current clients
    // negotiate once; a simultaneous group.restart event would request a
    // second offer against the same newly-created graph.
    state.media_session_revision.fetch_add(1, Ordering::AcqRel);
    info!(%reason, "publishing restarted window stream session");
    broadcast_status(state);
}

fn apply_capture_settings(state: &ServerState, settings: CaptureSettings) -> Result<()> {
    let previous = state
        .settings
        .lock()
        .map_err(|_| anyhow::anyhow!("capture settings lock poisoned"))?
        .clone();
    let raw_capture_changed = capture_format_changed(&previous, &settings);
    let encoder_profile_changed = encoder_profile_changed(&previous, &settings);
    let audio_topology_changed = audio_enabled(&previous) != audio_enabled(&settings);
    if raw_capture_changed {
        // When cold, settings are only desired metadata.  The next Start
        // creates a fresh capture directly with them.
        let Some(shared_capture) = state.shared_capture.current() else {
            if let Ok(mut current) = state.settings.lock() {
                *current = settings.clone();
            }
            state.groups.reconfigure(settings.clone(), false);
            if let Some(audio) = &state.audio {
                audio.reconfigure(settings);
            }
            return Ok(());
        };
        if let Err(error) = shared_capture.restart(&settings) {
            // A monitor/window can disappear between UI discovery and capture
            // startup.  Put the last working source back and rebuild its
            // encoders rather than leaving viewers attached to a failed raw
            // source bus.
            match shared_capture.restart(&previous) {
                Ok(()) => {
                    warn!(%error, "capture refresh failed; restored the previous source");
                    if let Err(restart_error) =
                        state.groups.reconfigure_and_restart(previous.clone())
                    {
                        return Err(error.context(format!(
                            "capture refresh failed; the previous source returned, but its encoder restart failed: {restart_error}"
                        )));
                    }
                    restart_viewer_sessions(
                        state,
                        "source switch failed; previous source restored",
                    );
                    broadcast_status(state);
                }
                Err(restore_error) => {
                    return Err(error.context(format!(
                        "capture refresh failed and restoring the previous source also failed: {restore_error}"
                    )));
                }
            }
            return Err(error);
        }
        let (width, height) = shared_capture.source_dimensions();
        info!(
            source = %format!("{}:{}", settings.source_kind, settings.source_index),
            width,
            height,
            "shared capture format refreshed"
        );
    }
    // Publish the mode before releasing the group transaction so the two
    // adaptive loops cannot take one more action using the old mode while a
    // Manual <-> Auto switch is in progress.
    if let Ok(mut current) = state.settings.lock() {
        *current = settings.clone();
    }
    let reconfiguration = if raw_capture_changed || encoder_profile_changed {
        match state.groups.reconfigure_and_restart(settings.clone()) {
            Ok(reconfiguration) => reconfiguration,
            Err(error) => {
                if let Ok(mut current) = state.settings.lock() {
                    *current = previous.clone();
                }
                if raw_capture_changed {
                    let shared_capture = state
                        .shared_capture
                        .current()
                        .context("shared capture is unavailable during rollback")?;
                    if let Err(restore_error) = shared_capture.restart(&previous) {
                        return Err(error.context(format!(
                            "encoder replacement failed and restoring the previous capture also failed: {restore_error}"
                        )));
                    }
                    if let Err(restore_error) =
                        state.groups.reconfigure_and_restart(previous.clone())
                    {
                        return Err(error.context(format!(
                            "encoder replacement failed; the previous capture returned, but rebuilding its encoders also failed: {restore_error}"
                        )));
                    }
                    restart_viewer_sessions(
                        state,
                        "encoder replacement failed; previous source restored",
                    );
                    broadcast_status(state);
                }
                return Err(error);
            }
        }
    } else {
        state.groups.reconfigure(settings.clone(), false)
    };
    if raw_capture_changed || encoder_profile_changed || reconfiguration.topology_changed {
        let reason = if raw_capture_changed {
            "capture format changed"
        } else if encoder_profile_changed {
            "stream profile changed"
        } else {
            "transcode group budget changed"
        };
        restart_viewer_sessions(state, reason);
    }
    if let Some(audio) = &state.audio {
        audio.reconfigure(settings);
    }
    if audio_topology_changed {
        // WebRTC cannot add or remove an audio m-line on an already-negotiated
        // peer.  Publish a new session generation so viewers reconnect with
        // the authoritative audio topology.
        state.media_session_revision.fetch_add(1, Ordering::AcqRel);
    }
    Ok(())
}

fn audio_enabled(settings: &CaptureSettings) -> bool {
    settings.audio_mode != "off"
}

fn capture_input_changed(previous: &CaptureSettings, next: &CaptureSettings) -> bool {
    let source_identity_changed = previous.source_kind != next.source_kind
        || match (previous.source_native_id, next.source_native_id) {
            (Some(previous_id), Some(next_id)) => previous_id != next_id,
            (None, None) => previous.source_index != next.source_index,
            _ => true,
        };
    source_identity_changed || previous.draw_mouse != next.draw_mouse
}

/// Changes that alter the dimensions or cadence of the shared raw frame bus.
/// They require rebuilding the capture process and every active encoder.
fn capture_format_changed(previous: &CaptureSettings, next: &CaptureSettings) -> bool {
    capture_input_changed(previous, next)
        || previous.width != next.width
        || previous.height != next.height
        || previous.fps != next.fps
        || previous.output_height != next.output_height
        || previous.output_fps != next.output_fps
}

/// Changes that can alter a viewer's negotiated video profile even when the
/// raw capture format happens to stay the same.
fn encoder_profile_changed(previous: &CaptureSettings, next: &CaptureSettings) -> bool {
    previous.quality_mode != next.quality_mode
        || previous.adaptive_quality_ceiling != next.adaptive_quality_ceiling
        || previous.adaptive_fps_ceiling != next.adaptive_fps_ceiling
        || previous.max_quality_groups != next.max_quality_groups
}

fn uses_group_adaptation(settings: &CaptureSettings) -> bool {
    settings.quality_mode == "adaptive"
}

fn uses_single_group_bitrate_adaptation(settings: &CaptureSettings) -> bool {
    settings.quality_mode != "adaptive" && settings.bitrate_mode == "automatic"
}

/// A changed capture size, rate, or group topology cannot be made reliable by
/// mutating an already-negotiated static RTP track.  Ask each Socket.IO client
/// to create a new offer; `close_client_connection` uses its exact old media
/// binding so the retired encoder is cleaned up safely.
fn restart_viewer_sessions(state: &ServerState, reason: &str) {
    let sockets = state
        .client_sockets
        .lock()
        .map(|sockets| {
            sockets
                .iter()
                .map(|(client_id, socket)| (client_id.clone(), socket.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for (client_id, socket) in sockets {
        let assignment = state.groups.assignment_for(&client_id);
        let payload = authoritative_assignment_payload(state, assignment.group_id, reason, true);
        socket.emit("group.assignment", &payload).ok();
    }
}

fn stream_failure(state: &ServerState) -> Option<String> {
    let capture = state.shared_capture.current();
    let capture_failure = match capture.as_ref() {
        Some(capture) => capture.failure().or_else(|| {
            let frame = capture.latest_frame_snapshot()?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_nanos();
            let age = now.saturating_sub(u128::from(frame.captured_at_unix_nanos));
            (age > STREAM_STALL_TIMEOUT.as_nanos()).then(|| {
                format!(
                    "capture produced no frame for more than {} seconds",
                    STREAM_STALL_TIMEOUT.as_secs()
                )
            })
        }),
        None => Some("shared capture is unavailable".to_owned()),
    };
    capture_failure.or_else(|| {
        state
            .groups
            .active_group_ids()
            .into_iter()
            .find_map(|group_id| {
                let media = state.groups.media_by_id(group_id)?;
                media.failure().or_else(|| {
                    (media.status() == "stopped")
                        .then(|| format!("encoder group {group_id} stopped producing video"))
                })
            })
    })
}

async fn stream_health_monitor(
    state: ServerState,
    control_tx: tokio::sync::mpsc::UnboundedSender<ServerCommand>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let recovery_pending = Arc::new(AtomicBool::new(false));
    loop {
        interval.tick().await;
        if !state.stream_enabled.load(Ordering::Acquire) {
            continue;
        }
        if stream_failure(&state).is_some()
            && recovery_pending
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            && control_tx
                .send(ServerCommand::RecoverStream {
                    pending: Arc::clone(&recovery_pending),
                })
                .is_err()
        {
            recovery_pending.store(false, Ordering::Release);
            break;
        }
    }
}

async fn adaptive_loop(state: ServerState) {
    let mut interval = tokio::time::interval(Duration::from_secs(3));
    let mut stable_since = std::time::Instant::now();
    let mut last_change = std::time::Instant::now() - Duration::from_secs(10);
    loop {
        interval.tick().await;
        if !state.stream_enabled.load(Ordering::Acquire) {
            stable_since = Instant::now();
            continue;
        }
        let should_adapt = state
            .settings
            .lock()
            .map(|settings| uses_single_group_bitrate_adaptation(&settings))
            .unwrap_or(false);
        if !should_adapt {
            stable_since = Instant::now();
            continue;
        }
        let now = std::time::Instant::now();
        let metrics = state
            .viewer_metrics
            .lock()
            .map(|metrics| {
                metrics
                    .values()
                    .filter(|metric| {
                        now.duration_since(metric.updated_at) <= Duration::from_secs(10)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if metrics.is_empty() {
            continue;
        }
        let worst_loss = metrics
            .iter()
            .map(|metric| metric.loss_rate)
            .fold(0.0, f64::max);
        let worst_rtt = metrics
            .iter()
            .map(|metric| metric.rtt_ms)
            .fold(0.0, f64::max);
        let worst_jitter = metrics
            .iter()
            .map(|metric| metric.jitter_ms)
            .fold(0.0, f64::max);
        let average_bitrate = metrics
            .iter()
            .map(|metric| metric.bitrate_bps)
            .filter(|bitrate| *bitrate > 0.0)
            .sum::<f64>()
            / metrics.len() as f64;
        let bad = worst_loss > 0.05
            || worst_rtt > 250.0
            || worst_jitter > 50.0
            || (average_bitrate > 0.0 && average_bitrate < 250_000.0);
        let good = worst_loss < 0.01 && worst_rtt < 100.0 && worst_jitter < 20.0;
        if !good {
            stable_since = now;
        }
        if now.duration_since(last_change) < Duration::from_secs(5) {
            continue;
        }
        let current = state
            .settings
            .lock()
            .ok()
            .map(|settings| settings.bitrate)
            .unwrap_or(state.config.effective_bitrate());
        let ceiling = state.config.effective_bitrate().max(250_000);
        let target = if bad {
            current.saturating_mul(3) / 4
        } else if good && now.duration_since(stable_since) >= Duration::from_secs(10) {
            current.saturating_mul(11) / 10
        } else {
            current
        }
        .clamp(250_000, ceiling);
        if target == current || (target as f64 - current as f64).abs() < current as f64 * 0.05 {
            continue;
        }
        let changed = if let Ok(mut settings) = state.settings.lock() {
            settings.bitrate = target;
            let _ = state.groups.reconfigure(settings.clone(), false);
            last_change = now;
            true
        } else {
            false
        };
        if changed {
            broadcast_status(&state);
        }
    }
}

async fn group_adaptive_loop(state: ServerState) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;
        if !state.stream_enabled.load(Ordering::Acquire) {
            continue;
        }
        let adaptive_quality_enabled = state
            .settings
            .lock()
            .map(|settings| uses_group_adaptation(&settings))
            .unwrap_or(false);
        if !adaptive_quality_enabled {
            continue;
        }
        let metrics = state
            .viewer_metrics
            .lock()
            .map(|metrics| metrics.clone())
            .unwrap_or_default();
        let tuned = state.groups.tune(&metrics);
        let maintenance = state.groups.maintain();
        for migration in &maintenance.migrations {
            let socket = state
                .client_sockets
                .lock()
                .ok()
                .and_then(|sockets| sockets.get(&migration.client_id).cloned());
            if let Some(socket) = socket {
                let payload = authoritative_assignment_payload(
                    &state,
                    migration.assignment.group_id,
                    &migration.assignment.reason,
                    migration.assignment.restart,
                );
                socket.emit("group.assignment", &payload).ok();
            }
        }
        if tuned || maintenance.changed {
            broadcast_status(&state);
        }
    }
}

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/", get(index_without_token))
        .route("/healthz", get(healthz))
        .route("/api/status", get(status))
        .route("/api/session", post(session_without_token))
        .route("/assets/{*path}", get(web_asset))
        .route("/favicon.ico", get(favicon))
        .route("/{token}", get(index))
        .route("/{token}/api/status", get(token_status))
        .route("/{token}/api/probe", get(connection_probe))
        .route("/{token}/api/session", post(session))
        .route("/{token}/api/offer", post(offer))
        .layer(DefaultBodyLimit::max(256 * 1024))
        .with_state(state)
}

async fn index_without_token() -> (StatusCode, &'static str) {
    (
        StatusCode::NOT_FOUND,
        "A stream token is required in the URL",
    )
}

async fn index(
    Path(token): Path<String>,
    State(state): State<ServerState>,
) -> Result<Response, StatusCode> {
    if state.token_matches(&token) {
        let file = WebAssets::get("index.html").ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
        let html = String::from_utf8_lossy(file.data.as_ref()).into_owned();
        Ok(Html(html).into_response())
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn web_asset(Path(path): Path<String>) -> Result<Response, StatusCode> {
    let requested = path.trim_start_matches('/');
    let prefixed = format!("assets/{requested}");
    let file = WebAssets::get(requested)
        .or_else(|| WebAssets::get(&prefixed))
        .ok_or(StatusCode::NOT_FOUND)?;
    let content_type = mime_guess::from_path(&path).first_or_octet_stream();
    Response::builder()
        .header(header::CONTENT_TYPE, content_type.as_ref())
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(Body::from(file.data.into_owned()))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn status(State(state): State<ServerState>) -> Result<Json<serde_json::Value>, StatusCode> {
    if !matches!(state.config.bind.as_str(), "localhost" | "loopback") {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(status_snapshot(&state)))
}

async fn token_status(
    Path(token): Path<String>,
    State(state): State<ServerState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !state.token_matches(&token) {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(status_snapshot(&state)))
}

async fn connection_probe(
    Path(token): Path<String>,
    State(state): State<ServerState>,
) -> Result<Response, StatusCode> {
    if !state.token_matches(&token) {
        return Err(StatusCode::NOT_FOUND);
    }
    let payload = network_probe_payload();
    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_ENCODING, "identity")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_LENGTH, payload.len().to_string())
        .body(Body::from(payload))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn resolve_media_candidate(host: &str, port: u16) -> Result<std::net::SocketAddr> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(std::net::SocketAddr::new(ip, port));
    }
    tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolve media candidate host '{host}'"))?
        .find(std::net::SocketAddr::is_ipv4)
        .with_context(|| format!("media candidate host '{host}' has no IPv4 address"))
}

async fn update_media_candidate(state: &ServerState, host: &str) -> Result<std::net::SocketAddr> {
    let candidate_addr = resolve_media_candidate(host, state.config.media_ports.first).await?;
    state.udp_mux.set_candidate_addr(candidate_addr)?;
    Ok(candidate_addr)
}

fn network_probe_payload() -> Bytes {
    const PROBE_BYTES: usize = 1024 * 1024;
    static PAYLOAD: OnceLock<Bytes> = OnceLock::new();
    PAYLOAD
        .get_or_init(|| {
            let mut state = 0xC0FF_EE12_3456_789Au64;
            let mut bytes = vec![0_u8; PROBE_BYTES];
            for byte in &mut bytes {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state as u8;
            }
            Bytes::from(bytes)
        })
        .clone()
}

fn status_snapshot(state: &ServerState) -> serde_json::Value {
    let stream_enabled = state.stream_enabled.load(Ordering::Acquire);
    let stream_resetting = state.stream_resetting.load(Ordering::Acquire);
    let viewers = state
        .connected_connections
        .lock()
        .map(|connected| connected.len())
        .unwrap_or_default();
    let settings = state.settings.lock().ok().map(|settings| settings.clone());
    let audio_enabled = settings.as_ref().is_some_and(audio_enabled);
    let primary_media = state.groups.media_by_id(state.groups.primary_group_id());
    let capture = state.shared_capture.current();
    let capture_error = capture.as_ref().and_then(|capture| capture.failure());
    let codec = primary_media
        .as_ref()
        .map(|media| media.codec_name().to_owned())
        .unwrap_or_else(|| state.groups.group(0).codec());
    json!({
        "status": if stream_resetting {
            "resetting"
        } else if stream_enabled {
            primary_media.as_ref().map(|media| media.status()).unwrap_or("stopped")
        } else {
            "stopped"
        },
        "stream_enabled": stream_enabled,
        "stream_resetting": stream_resetting,
        "media_session_revision": state.media_session_revision.load(Ordering::Acquire),
        "viewers": viewers,
        "bind": state.config.bind,
        "http_port": state.config.http_port,
        "media_port": state.config.media_ports.first,
        "media_candidate": state.udp_mux.candidate_addr().ok().map(|address| address.to_string()),
        "codec": codec,
        "media_error": capture_error.clone().or_else(|| primary_media.as_ref().and_then(|media| media.failure())),
        "capture_error": capture_error,
        "encoder_delay_ms": primary_media.as_ref().and_then(|media| media.encoder_delay_ms()),
        "stale_encoded_frames": primary_media.as_ref().map(|media| media.stale_encoded_frames()).unwrap_or_default(),
        "encoder_backlog_restarts": primary_media.as_ref().map(|media| media.encoder_backlog_restarts()).unwrap_or_default(),
        "audio_status": audio_enabled.then(|| state.audio.as_ref().map(|audio| audio.status())).flatten(),
        "audio_error": audio_enabled.then(|| state.audio.as_ref().and_then(|audio| audio.failure())).flatten(),
        "audio_enabled": audio_enabled,
        "quality": settings.as_ref().and_then(|settings| settings.output_height).map(|height| format!("{height}p")).unwrap_or_else(|| "Source".to_owned()),
        "fps": settings.as_ref().and_then(|settings| settings.output_fps).map(|fps| fps.to_string()).unwrap_or_else(|| "Source".to_owned()),
        "bitrate_bps": settings.as_ref().map(|settings| settings.bitrate),
        "max_viewers": state.max_viewers.load(Ordering::Acquire),
        "active_group_count": state.groups.count(),
        "max_group_count": settings
            .as_ref()
            .map(|settings| {
                if settings.quality_mode == "adaptive" {
                    configured_group_count(&settings.max_quality_groups)
                } else {
                    1
                }
            })
            .unwrap_or(1),
        "groups": state.groups.groups_json(),
        "latency_mode": "latest-frame",
        "sync_mode": "latest-frame",
        "media_transport": "webrtc",
        "capture_backend": capture.as_ref().map(|capture| capture.backend_name())
    })
}

fn status_for_client(state: &ServerState, client_id: &str) -> serde_json::Value {
    let revision = state.settings_revision.load(Ordering::Acquire);
    status_for_client_revision(state, client_id, revision)
}

fn status_for_client_revision(
    state: &ServerState,
    client_id: &str,
    revision: u64,
) -> serde_json::Value {
    let mut status = status_snapshot(state);
    let group = state.groups.assignment_json(client_id);
    status["quality"] = group["quality"].clone();
    status["fps"] = group["fps"].clone();
    status["bitrate_bps"] = group["bitrate_bps"].clone();
    status["codec"] = group["codec"].clone();
    status["group"] = group;
    status["settings_revision"] = serde_json::json!(revision);
    status
}

fn authoritative_assignment_payload(
    state: &ServerState,
    group_id: usize,
    reason: &str,
    restart: bool,
) -> serde_json::Value {
    let mut assignment = state.groups.assignment_json_for(group_id, reason, restart);
    assignment["settings_revision"] =
        serde_json::json!(state.settings_revision.load(Ordering::Acquire));
    assignment
}

fn authoritative_settings_event(
    state: &ServerState,
    client_id: &str,
    revision: u64,
) -> serde_json::Value {
    serde_json::json!({
        "revision": revision,
        "status": status_for_client_revision(state, client_id, revision),
    })
}

async fn session_without_token() -> Json<serde_json::Value> {
    Json(json!({ "ok": false, "error": "use the token route" }))
}

async fn session(
    Path(token): Path<String>,
    State(state): State<ServerState>,
) -> Json<serde_json::Value> {
    if !state.token_matches(&token) {
        return Json(json!({ "ok": false, "error": "invalid token" }));
    }
    Json(json!({ "ok": true, "message": "signaling session endpoint is ready" }))
}

async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

fn offer_supports_h264_level_31(sdp: &str) -> bool {
    let h264_payloads = sdp
        .lines()
        .filter_map(|line| line.trim().strip_prefix("a=rtpmap:"))
        .filter_map(|mapping| mapping.split_once(' '))
        .filter(|(_, encoding)| {
            encoding
                .split('/')
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("H264"))
        })
        .map(|(payload, _)| payload.trim())
        .collect::<HashSet<_>>();

    sdp.lines()
        .filter_map(|line| line.trim().strip_prefix("a=fmtp:"))
        .filter_map(|format| format.split_once(' '))
        .filter(|(payload, _)| h264_payloads.contains(payload.trim()))
        .flat_map(|(_, parameters)| parameters.split(';'))
        .filter_map(|parameter| parameter.trim().split_once('='))
        .filter(|(name, _)| name.trim().eq_ignore_ascii_case("profile-level-id"))
        .map(|(_, value)| value.trim().to_ascii_lowercase())
        .any(|profile_level_id| {
            profile_level_id.len() == 6
                && profile_level_id.starts_with("42e0")
                && u8::from_str_radix(&profile_level_id[4..], 16).is_ok_and(|level| level >= 0x1f)
        })
}

async fn offer(
    Path(token): Path<String>,
    State(state): State<ServerState>,
    Json(request): Json<serde_json::Value>,
) -> Result<Json<RTCSessionDescription>, (StatusCode, String)> {
    if !state.token_matches(&token) {
        return Err((StatusCode::NOT_FOUND, "invalid token".to_owned()));
    }
    if !state.stream_enabled.load(Ordering::Acquire) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "stream is stopped".to_owned(),
        ));
    }
    let client_id = request
        .get("clientId")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or_else(|| internal_error("offer is missing a valid clientId"))?
        .to_owned();
    let _offer_guard = lock_offer(&state, &client_id).await;
    let _session_guard = Arc::clone(&state.session_gate).read_owned().await;
    // An offer may wait behind a previous negotiation while its sharing token
    // is rotated. Revalidate after acquiring the per-client gate.
    if !state.token_matches(&token) {
        return Err((StatusCode::NOT_FOUND, "invalid token".to_owned()));
    }
    if !state.stream_enabled.load(Ordering::Acquire) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "stream is stopped".to_owned(),
        ));
    }
    let session_generation = state.session_generation.load(Ordering::Acquire);
    let offer: RTCSessionDescription = serde_json::from_value(request).map_err(internal_error)?;
    let remote_ufrag = crate::udp_mux::ice_ufrag(&offer.sdp)
        .ok_or_else(|| internal_error("offer does not contain an ICE username fragment"))?;
    let has_control_socket = state
        .client_sockets
        .lock()
        .map(|sockets| sockets.contains_key(&client_id))
        .unwrap_or(false);
    if !has_control_socket {
        return Err((
            StatusCode::UNAUTHORIZED,
            "offer requires an authenticated control session".to_owned(),
        ));
    }
    let assigned_codec = state
        .groups
        .codec_for_client(&client_id)
        .ok_or_else(|| internal_error("control session has no media assignment"))?;
    if assigned_codec == "h264" && !offer_supports_h264_level_31(&offer.sdp) {
        return Err((
            StatusCode::BAD_REQUEST,
            "offer does not support constrained-baseline H.264 level 3.1".to_owned(),
        ));
    }

    // Validate stateless inputs before replacing a working connection, then
    // reserve bounded capacity before activating or subscribing to media.
    close_client_connection(&state, &client_id).await;
    let connection_id = Uuid::new_v4();
    {
        let connections = state.connections.lock().await;
        let mut pending = state
            .pending_connections
            .lock()
            .map_err(|_| internal_error("pending connection lock poisoned"))?;
        if connections.len() + pending.len() >= state.max_viewers.load(Ordering::Acquire).max(1) {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "viewer limit reached".to_owned(),
            ));
        }
        pending.insert(connection_id);
    }

    let media = match state.groups.media_for(&client_id) {
        Ok(media) => media,
        Err(error) => {
            remove_pending(&state, connection_id);
            return Err(internal_error(error));
        }
    };
    if media.codec_name() == "H.264" && !offer_supports_h264_level_31(&offer.sdp) {
        remove_pending(&state, connection_id);
        return Err((
            StatusCode::BAD_REQUEST,
            "offer does not support constrained-baseline H.264 level 3.1".to_owned(),
        ));
    }

    let media_track = match media.subscribe(connection_id) {
        Ok(track) => track,
        Err(error) => {
            remove_pending(&state, connection_id);
            return Err(internal_error(error));
        }
    };
    let audio_track = if let Some(audio) = &state.audio {
        match audio.subscribe(connection_id) {
            Ok(track) => Some(track),
            Err(error) => {
                media.unsubscribe(connection_id);
                remove_pending(&state, connection_id);
                return Err(internal_error(error));
            }
        }
    } else {
        None
    };
    let udp_socket = state.udp_mux.endpoint(connection_id, remote_ufrag);
    let reservation = OfferReservation {
        pending_connections: Arc::clone(&state.pending_connections),
        media: Arc::clone(&media),
        audio: state.audio.clone(),
        udp_mux: Arc::clone(&state.udp_mux),
        client_connections: Arc::clone(&state.client_connections),
        connection_bindings: Arc::clone(&state.connection_bindings),
        client_id: client_id.clone(),
        connection_id,
        committed: false,
    };

    let mut media_engine = MediaEngine::default();
    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: media.mime_type().to_owned(),
                    clock_rate: 90_000,
                    channels: 0,
                    sdp_fmtp_line: media.sdp_fmtp_line().to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: media.payload_type(),
            },
            RtpCodecKind::Video,
        )
        .map_err(internal_error)?;
    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48_000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: crate::audio::OPUS_PAYLOAD_TYPE,
            },
            RtpCodecKind::Audio,
        )
        .map_err(internal_error)?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)
        .map_err(internal_error)?;
    // Browsers hide host ICE addresses behind mDNS names. The host only needs to
    // resolve those remote names; it does not advertise an mDNS candidate itself.
    let mut setting_engine = SettingEngine::default();
    setting_engine.set_multicast_dns_mode(MulticastDnsMode::QueryOnly);
    setting_engine.set_multicast_dns_timeout(Some(Duration::from_secs(10)));
    let (gather_tx, gather_rx) = oneshot::channel();
    let handler = Arc::new(PeerHandler {
        gather_complete: Arc::new(Mutex::new(Some(gather_tx))),
        media: Arc::clone(&media),
        audio: state.audio.clone(),
        stream_enabled: Arc::clone(&state.stream_enabled),
        connection_id,
        connections: Arc::clone(&state.connections),
        udp_mux: Arc::clone(&state.udp_mux),
        connected_connections: Arc::clone(&state.connected_connections),
        client_connections: Arc::clone(&state.client_connections),
        connection_bindings: Arc::clone(&state.connection_bindings),
        client_id: client_id.clone(),
        state: state.clone(),
    });
    let config = RTCConfigurationBuilder::new().build();
    let peer_connection = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting_engine)
        .with_handler(handler)
        .with_udp_addrs(Vec::<String>::new())
        .with_udp_socket(udp_socket)
        .build()
        .await
        .map_err(internal_error)?;

    let peer_connection: Arc<dyn PeerConnection> = Arc::new(peer_connection);
    peer_connection
        .add_track(media_track.track() as Arc<dyn TrackLocal>)
        .await
        .map_err(internal_error)?;
    if let Some(audio_track) = audio_track {
        peer_connection
            .add_track(audio_track.track() as Arc<dyn TrackLocal>)
            .await
            .map_err(internal_error)?;
    }
    peer_connection
        .set_remote_description(offer)
        .await
        .map_err(internal_error)?;
    let answer = peer_connection
        .create_answer(None)
        .await
        .map_err(internal_error)?;
    peer_connection
        .set_local_description(answer)
        .await
        .map_err(internal_error)?;
    tokio::time::timeout(Duration::from_secs(8), gather_rx)
        .await
        .map_err(|_| internal_error("ICE gathering timed out"))?
        .map_err(|_| internal_error("ICE gathering was cancelled"))?;
    let local_description = peer_connection
        .local_description()
        .await
        .context("WebRTC did not produce a local description")
        .map_err(internal_error)?;
    if state.session_generation.load(Ordering::Acquire) != session_generation
        || !state.token_matches(&token)
    {
        let _ = peer_connection.close().await;
        return Err((StatusCode::UNAUTHORIZED, "session was revoked".to_owned()));
    }
    state
        .connections
        .lock()
        .await
        .insert(connection_id, peer_connection);
    reservation.commit();
    Ok(Json(local_description))
}

async fn lock_offer(state: &ServerState, client_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let gate = state
        .offer_locks
        .lock()
        .map(|mut locks| {
            // A client identifier must not become permanent map state. Retain
            // only gates that are currently held or awaited, then reuse the
            // requested live gate or install a weak reference to a new one.
            locks.retain(|_, gate| gate.strong_count() > 0);
            if let Some(gate) = locks.get(client_id).and_then(Weak::upgrade) {
                gate
            } else {
                let gate = Arc::new(Mutex::new(()));
                locks.insert(client_id.to_owned(), Arc::downgrade(&gate));
                gate
            }
        })
        // A poisoned lock only follows a panic in server code. Preserve offer
        // availability rather than turning it into a permanent outage.
        .unwrap_or_else(|_| Arc::new(Mutex::new(())));
    gate.lock_owned().await
}

fn remove_pending(state: &ServerState, connection_id: Uuid) {
    if let Ok(mut pending) = state.pending_connections.lock() {
        pending.remove(&connection_id);
    }
}

struct OfferReservation {
    pending_connections: Arc<StdMutex<HashSet<Uuid>>>,
    media: Arc<MediaPipeline>,
    audio: Option<Arc<AudioPipeline>>,
    udp_mux: Arc<UdpMux>,
    client_connections: Arc<StdMutex<std::collections::HashMap<String, Uuid>>>,
    connection_bindings: Arc<StdMutex<HashMap<Uuid, ConnectionMediaBinding>>>,
    client_id: String,
    connection_id: Uuid,
    committed: bool,
}

impl OfferReservation {
    fn commit(mut self) {
        self.committed = true;
        if let Ok(mut pending) = self.pending_connections.lock() {
            pending.remove(&self.connection_id);
        }
        if let Ok(mut clients) = self.client_connections.lock() {
            clients.insert(self.client_id.clone(), self.connection_id);
        }
        if let Ok(mut bindings) = self.connection_bindings.lock() {
            bindings.insert(
                self.connection_id,
                ConnectionMediaBinding {
                    media: Arc::clone(&self.media),
                    audio: self.audio.clone(),
                },
            );
        }
    }
}

async fn close_client_connection(state: &ServerState, client_id: &str) {
    let connection_id = state
        .client_connections
        .lock()
        .ok()
        .and_then(|mut clients| clients.remove(client_id));
    let Some(connection_id) = connection_id else {
        return;
    };
    let binding = state
        .connection_bindings
        .lock()
        .ok()
        .and_then(|mut bindings| bindings.remove(&connection_id));
    let connection = state.connections.lock().await.remove(&connection_id);
    if let Some(connection) = connection {
        let _ = connection.close().await;
    }
    remove_pending(state, connection_id);
    if let Some(binding) = binding {
        binding.media.unsubscribe(connection_id);
        if let Some(audio) = binding.audio {
            audio.unsubscribe(connection_id);
        }
    }
    state.udp_mux.unregister(connection_id);
    if let Ok(mut connected) = state.connected_connections.lock() {
        connected.remove(&connection_id);
    }
    if let Ok(mut metrics) = state.viewer_metrics.lock() {
        metrics.remove(client_id);
    }
    broadcast_status(state);
}

/// Revokes every stateful capability associated with the previous sharing
/// token. In-flight offers observe `session_generation` and clean their own
/// reservations before they can commit.
async fn reset_token_and_revoke(state: &ServerState, token: String) {
    let _session_guard = Arc::clone(&state.session_gate).write_owned().await;
    state.reset_token(token);
    revoke_all_sessions_locked(state).await;
}

async fn revoke_all_sessions_locked(state: &ServerState) {
    let sockets = state
        .client_sockets
        .lock()
        .map(|mut sockets| sockets.drain().collect::<Vec<_>>())
        .unwrap_or_default();
    let mut client_ids = sockets
        .iter()
        .map(|(client_id, _)| client_id.clone())
        .collect::<HashSet<_>>();
    for (_, socket) in sockets {
        let _ = socket.disconnect();
    }

    let client_connections = state
        .client_connections
        .lock()
        .map(|mut clients| clients.drain().collect::<Vec<_>>())
        .unwrap_or_default();
    client_ids.extend(
        client_connections
            .iter()
            .map(|(client_id, _)| client_id.clone()),
    );
    let connection_ids = client_connections
        .into_iter()
        .map(|(_, connection_id)| connection_id)
        .collect::<HashSet<_>>();
    let bindings = state
        .connection_bindings
        .lock()
        .map(|mut bindings| bindings.drain().collect::<HashMap<_, _>>())
        .unwrap_or_default();
    let pending = state
        .pending_connections
        .lock()
        .map(|mut pending| pending.drain().collect::<Vec<_>>())
        .unwrap_or_default();
    let connections = {
        let mut connections = state.connections.lock().await;
        connections.drain().collect::<HashMap<_, _>>()
    };

    for (connection_id, connection) in connections {
        let _ = connection.close().await;
        state.udp_mux.unregister(connection_id);
    }
    for (connection_id, binding) in bindings {
        binding.media.unsubscribe(connection_id);
        if let Some(audio) = binding.audio {
            audio.unsubscribe(connection_id);
        }
        state.udp_mux.unregister(connection_id);
    }
    for connection_id in connection_ids.into_iter().chain(pending) {
        state.udp_mux.unregister(connection_id);
    }
    if let Ok(mut connected) = state.connected_connections.lock() {
        connected.clear();
    }
    if let Ok(mut metrics) = state.viewer_metrics.lock() {
        metrics.clear();
    }
    for client_id in client_ids {
        state.groups.remove_client(&client_id);
    }
    broadcast_status(state);
}

impl Drop for OfferReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Ok(mut pending) = self.pending_connections.lock() {
            pending.remove(&self.connection_id);
        }
        self.media.unsubscribe(self.connection_id);
        if let Some(audio) = &self.audio {
            audio.unsubscribe(self.connection_id);
        }
        self.udp_mux.unregister(self.connection_id);
    }
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    warn!(%error, "WebRTC offer failed");
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

async fn control_connect(
    socket: SocketRef,
    Data(auth): Data<SocketAuth>,
    SocketState(state): SocketState<ServerState>,
) {
    // Serialize authentication and registration with token revocation. Without
    // this read guard, a socket that validated the old token immediately before
    // reset could insert itself after the revocation cleanup had drained maps.
    let _session_guard = Arc::clone(&state.session_gate).read_owned().await;
    let client_id = auth.client_id.trim();
    if !state.token_matches(&auth.token) || client_id.is_empty() || client_id.len() > 128 {
        warn!("rejecting invalid control socket authentication");
        let _ = socket.disconnect();
        return;
    }
    let client_id = client_id.to_owned();
    let previous = match state.client_sockets.lock() {
        Ok(mut sockets) => {
            if !sockets.contains_key(&client_id)
                && sockets.len() >= state.max_viewers.load(Ordering::Acquire).max(1)
            {
                warn!(%client_id, "rejecting control socket above viewer limit");
                drop(sockets);
                let _ = socket.disconnect();
                return;
            }
            sockets.insert(client_id.clone(), socket.clone())
        }
        Err(_) => {
            warn!(%client_id, "rejecting control socket because session state is unavailable");
            let _ = socket.disconnect();
            return;
        }
    };
    state.groups.ensure_client(&client_id);
    if let Some(previous) = previous {
        let _ = previous.disconnect();
    }
    socket.extensions.insert(ClientIdentity {
        client_id: client_id.clone(),
    });
    socket.on("status.request", handle_status_request);
    socket.on("viewer.bootstrap", handle_viewer_bootstrap);
    socket.on("control.ping", handle_control_ping);
    socket.on("viewer.frameTiming", handle_viewer_frame_timing);
    socket.on("viewer.stats", handle_viewer_stats);
    socket.on("viewer.codecFailure", handle_viewer_codec_failure);
    socket.on_disconnect(handle_control_disconnect);

    let snapshot = status_for_client(&state, &client_id);
    let ready = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "media": "webrtc",
        "transport": "socket.io",
        "status": snapshot
    });
    socket.emit("session.ready", &ready).ok();
    socket.emit("status.snapshot", &snapshot).ok();
    let authoritative = authoritative_settings_event(
        &state,
        &client_id,
        state.settings_revision.load(Ordering::Acquire),
    );
    socket.emit("stream.settings", &authoritative).ok();
    info!(%client_id, "control socket connected");
}

async fn handle_status_request(
    socket: SocketRef,
    Extension(identity): Extension<ClientIdentity>,
    SocketState(state): SocketState<ServerState>,
    ack: AckSender,
) {
    if !is_current_socket(&state, &identity.client_id, &socket) {
        return;
    }
    let snapshot = status_for_client(&state, &identity.client_id);
    socket.emit("status.snapshot", &snapshot).ok();
    let authoritative = authoritative_settings_event(
        &state,
        &identity.client_id,
        state.settings_revision.load(Ordering::Acquire),
    );
    socket.emit("stream.settings", &authoritative).ok();
    ack.send(&snapshot).ok();
}

async fn handle_control_ping(
    Data(ping): Data<ControlPing>,
    Extension(identity): Extension<ClientIdentity>,
    SocketState(state): SocketState<ServerState>,
    ack: AckSender,
) {
    let assignment = state.groups.assignment_for(&identity.client_id);
    let media = state.groups.media_by_id(assignment.group_id);
    let media_status = media
        .as_ref()
        .map(|media| media.status())
        .unwrap_or("stopped");
    let media_error = media.as_ref().and_then(|media| media.failure());
    let response = json!({
        "sentAt": ping.sent_at,
        "serverTime": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default(),
        "encoderDelayMs": media.as_ref().and_then(|media| media.encoder_delay_ms()),
        "staleEncodedFrames": media.as_ref().map(|media| media.stale_encoded_frames()).unwrap_or_default(),
        "encoderBacklogRestarts": media.as_ref().map(|media| media.encoder_backlog_restarts()).unwrap_or_default(),
        "mediaStatus": media_status,
        "mediaError": media_error,
    });
    ack.send(&response).ok();
}

async fn handle_viewer_frame_timing(
    socket: SocketRef,
    Data(request): Data<ViewerFrameTimingRequest>,
    Extension(identity): Extension<ClientIdentity>,
    SocketState(state): SocketState<ServerState>,
    ack: AckSender,
) {
    if !is_current_socket(&state, &identity.client_id, &socket) {
        return;
    }
    let connection_id = state
        .client_connections
        .lock()
        .ok()
        .and_then(|connections| connections.get(&identity.client_id).copied());
    let frame_timing = connection_id.and_then(|connection_id| {
        state
            .connection_bindings
            .lock()
            .ok()
            .and_then(|bindings| bindings.get(&connection_id).cloned())
            .and_then(|binding| {
                binding
                    .media
                    .frame_capture_timing(connection_id, request.rtp_timestamp)
            })
    });
    let response = json!({
        "rtpTimestamp": request.rtp_timestamp,
        "captureTimeUnixMs": frame_timing.map(|timing| timing.capture_time_unix_nanos as f64 / 1_000_000.0),
        "encoderDelayMs": frame_timing.map(|timing| timing.encoder_delay_ms),
        "serverTime": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs_f64() * 1_000.0)
            .unwrap_or_default(),
    });
    ack.send(&response).ok();
}

async fn handle_viewer_bootstrap(
    socket: SocketRef,
    Data(probe): Data<ViewerBootstrap>,
    Extension(identity): Extension<ClientIdentity>,
    SocketState(state): SocketState<ServerState>,
) {
    if !is_current_socket(&state, &identity.client_id, &socket) {
        return;
    }
    let assignment = state.groups.bootstrap(&identity.client_id, &probe);
    let payload = authoritative_assignment_payload(
        &state,
        assignment.group_id,
        &assignment.reason,
        assignment.restart,
    );
    socket.emit("group.bootstrap", &payload).ok();
    let snapshot = status_for_client(&state, &identity.client_id);
    socket.emit("status.snapshot", &snapshot).ok();
    let authoritative = authoritative_settings_event(
        &state,
        &identity.client_id,
        state.settings_revision.load(Ordering::Acquire),
    );
    socket.emit("stream.settings", &authoritative).ok();
}

async fn handle_viewer_stats(
    socket: SocketRef,
    Data(stats): Data<ViewerStats>,
    Extension(identity): Extension<ClientIdentity>,
    SocketState(state): SocketState<ServerState>,
) {
    if is_current_socket(&state, &identity.client_id, &socket) {
        let Some(metrics) = update_viewer_metrics(&state, &identity.client_id, &stats) else {
            return;
        };
        if let Some(assignment) = state.groups.observe(&identity.client_id, &metrics) {
            let payload = authoritative_assignment_payload(
                &state,
                assignment.group_id,
                &assignment.reason,
                assignment.restart,
            );
            socket.emit("group.assignment", &payload).ok();
            broadcast_status(&state);
        }
    }
}

async fn handle_viewer_codec_failure(
    socket: SocketRef,
    Data(failure): Data<ViewerCodecFailure>,
    Extension(identity): Extension<ClientIdentity>,
    SocketState(state): SocketState<ServerState>,
) {
    if !is_current_socket(&state, &identity.client_id, &socket) {
        return;
    }
    let Some(assignment) = state
        .groups
        .codec_failure(&identity.client_id, &failure.codec)
    else {
        return;
    };
    warn!(
        client_id = %identity.client_id,
        codec = %failure.codec,
        reason = %failure.reason,
        "viewer reported a codec decode failure"
    );
    let payload = authoritative_assignment_payload(
        &state,
        assignment.group_id,
        &assignment.reason,
        assignment.restart,
    );
    socket.emit("group.assignment", &payload).ok();
    broadcast_status(&state);
}

async fn handle_control_disconnect(
    socket: SocketRef,
    Extension(identity): Extension<ClientIdentity>,
    SocketState(state): SocketState<ServerState>,
    _reason: DisconnectReason,
) {
    if take_current_socket(&state, &identity.client_id, &socket) {
        // Removing the socket first makes a queued offer fail authentication;
        // the per-client gate then waits for any offer already in progress so
        // its peer and assignment are cleaned up before disconnect returns.
        let _offer_guard = lock_offer(&state, &identity.client_id).await;
        close_client_connection(&state, &identity.client_id).await;
        state.groups.remove_client(&identity.client_id);
        if let Ok(mut metrics) = state.viewer_metrics.lock() {
            metrics.remove(&identity.client_id);
        }
        info!(client_id = %identity.client_id, "control socket disconnected");
    }
}

fn broadcast_status(state: &ServerState) {
    let revision = state.settings_revision.fetch_add(1, Ordering::AcqRel) + 1;
    let sockets = state
        .client_sockets
        .lock()
        .map(|sockets| {
            sockets
                .iter()
                .map(|(client_id, socket)| (client_id.clone(), socket.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for (client_id, socket) in sockets {
        let authoritative = authoritative_settings_event(state, &client_id, revision);
        if let Err(error) = socket.emit("stream.settings", &authoritative) {
            warn!(%client_id, %error, "authoritative settings delivery failed");
        }
        if let Err(error) = socket.emit("status.changed", &authoritative["status"]) {
            warn!(%client_id, %error, "client status delivery failed");
        }
    }
}

fn update_viewer_metrics(
    state: &ServerState,
    client_id: &str,
    value: &ViewerStats,
) -> Option<ViewerMetrics> {
    update_viewer_metrics_at(state, client_id, value, Instant::now())
}

fn update_viewer_metrics_at(
    state: &ServerState,
    client_id: &str,
    value: &ViewerStats,
    now: Instant,
) -> Option<ViewerMetrics> {
    let rtt_ms = finite_metric(value.rtt_ms, 0.0, 60_000.0);
    let jitter_ms = finite_metric(value.jitter_ms, 0.0, 60_000.0);
    let loss_rate = finite_metric(value.loss_rate, 0.0, 1.0);
    let bitrate_bps = finite_metric(value.bitrate_bps, 0.0, 10_000_000_000.0);
    let available_incoming_bitrate_bps = value
        .available_incoming_bitrate_bps
        .map(|value| finite_metric(value, 0.0, 10_000_000_000.0));
    let visibility_state = match value.visibility_state.as_str() {
        "visible" | "hidden" => value.visibility_state.clone(),
        _ => "unknown".to_owned(),
    };
    let previous = state
        .viewer_metrics
        .lock()
        .ok()
        .and_then(|metrics| metrics.get(client_id).cloned());
    if previous
        .as_ref()
        .is_some_and(|metrics| now.duration_since(metrics.updated_at) < MIN_VIEWER_METRIC_INTERVAL)
    {
        return None;
    }
    let frames_dropped = previous
        .as_ref()
        .map(|metrics| {
            value
                .frames_dropped
                .saturating_sub(metrics.reported_frames_dropped)
        })
        .unwrap_or_default();
    let freeze_count = previous
        .as_ref()
        .map(|metrics| {
            value
                .freeze_count
                .saturating_sub(metrics.reported_freeze_count)
        })
        .unwrap_or_default();
    let mut samples = previous
        .as_ref()
        .map(|metrics| metrics.samples.clone())
        .unwrap_or_default();
    samples.retain(|sample| now.duration_since(sample.captured_at) <= Duration::from_secs(15));
    samples.push_back(MetricSample {
        captured_at: now,
        rtt_ms,
        jitter_ms,
        loss_rate,
        bitrate_bps,
        available_incoming_bitrate_bps,
        frames_dropped,
        freeze_count,
        visibility_state,
    });
    while samples.len() > MAX_METRIC_SAMPLES_PER_VIEWER {
        samples.pop_front();
    }
    let metric = ViewerMetrics {
        rtt_ms,
        jitter_ms,
        loss_rate,
        bitrate_bps,
        reported_frames_dropped: value.frames_dropped,
        reported_freeze_count: value.freeze_count,
        updated_at: now,
        samples,
    };
    if let Ok(mut metrics) = state.viewer_metrics.lock() {
        metrics.insert(client_id.to_owned(), metric.clone());
    }
    Some(metric)
}

fn finite_metric(value: f64, minimum: f64, maximum: f64) -> f64 {
    if value.is_finite() {
        value.clamp(minimum, maximum)
    } else {
        minimum
    }
}

fn is_current_socket(state: &ServerState, client_id: &str, socket: &SocketRef) -> bool {
    if client_id.is_empty() {
        return false;
    }
    state
        .client_sockets
        .lock()
        .ok()
        .and_then(|sockets| sockets.get(client_id).map(|current| current == socket))
        .unwrap_or(false)
}

fn take_current_socket(state: &ServerState, client_id: &str, socket: &SocketRef) -> bool {
    if client_id.is_empty() {
        return false;
    }
    state
        .client_sockets
        .lock()
        .ok()
        .and_then(|mut sockets| {
            sockets
                .get(client_id)
                .is_some_and(|current| current == socket)
                .then(|| sockets.remove(client_id))
        })
        .flatten()
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> ServerState {
        let config = AppConfig::default();
        let active_token = Arc::new(StdMutex::new(config.token.clone()));
        let settings = CaptureSettings::from_config(&config);
        let groups = Arc::new(TranscodeGroups::new(vec![TranscodeGroup::stopped(
            0,
            settings.clone(),
            "vp8",
        )]));
        let mux = UdpMux::bind(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        ServerState {
            config: Arc::new(config),
            active_token,
            settings_revision: Arc::new(AtomicU64::new(0)),
            media_session_revision: Arc::new(AtomicU64::new(0)),
            stop_request_revision: Arc::new(AtomicU64::new(0)),
            audio: None,
            stream_enabled: Arc::new(AtomicBool::new(false)),
            stream_resetting: Arc::new(AtomicBool::new(false)),
            max_viewers: Arc::new(AtomicUsize::new(8)),
            settings: Arc::new(StdMutex::new(settings)),
            viewer_metrics: Arc::new(StdMutex::new(HashMap::new())),
            groups,
            shared_capture: Arc::new(CaptureSlot::default()),
            connections: Arc::new(Mutex::new(std::collections::HashMap::new())),
            udp_mux: mux,
            pending_connections: Arc::new(StdMutex::new(HashSet::new())),
            connected_connections: Arc::new(StdMutex::new(HashSet::new())),
            client_connections: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            connection_bindings: Arc::new(StdMutex::new(HashMap::new())),
            client_sockets: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            offer_locks: Arc::new(StdMutex::new(HashMap::new())),
            session_generation: Arc::new(AtomicU64::new(0)),
            session_gate: Arc::new(RwLock::new(())),
            stream_failure_callback: None,
        }
    }

    #[tokio::test]
    async fn health_endpoint_works() {
        let app = router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "ok"
        );
    }

    #[tokio::test]
    async fn client_status_carries_the_authoritative_settings_revision() {
        let state = test_state();
        state.settings_revision.store(7, Ordering::Release);
        state.media_session_revision.store(3, Ordering::Release);

        let status = status_for_client(&state, "viewer");

        assert_eq!(status["settings_revision"], 7);
        assert_eq!(status["media_session_revision"], 3);
        assert_eq!(status["group"]["bitrate_bps"], status["bitrate_bps"]);
    }

    #[tokio::test]
    async fn automatic_restart_has_a_distinct_resetting_status() {
        let state = test_state();
        state.stream_resetting.store(true, Ordering::Release);

        let resetting = status_snapshot(&state);

        assert_eq!(resetting["status"], "resetting");
        assert_eq!(resetting["stream_enabled"], false);
        assert_eq!(resetting["stream_resetting"], true);

        state.stream_resetting.store(false, Ordering::Release);
        let stopped = status_snapshot(&state);

        assert_eq!(stopped["status"], "stopped");
        assert_eq!(stopped["stream_resetting"], false);
    }

    #[tokio::test]
    async fn token_reset_revokes_session_state() {
        let state = test_state();
        let connection_id = Uuid::new_v4();
        let pending_id = Uuid::new_v4();
        state
            .client_connections
            .lock()
            .unwrap()
            .insert("viewer".to_owned(), connection_id);
        state
            .connected_connections
            .lock()
            .unwrap()
            .insert(connection_id);
        state.pending_connections.lock().unwrap().insert(pending_id);

        reset_token_and_revoke(&state, "replacement-token".to_owned()).await;

        assert!(state.token_matches("replacement-token"));
        assert!(!state.token_matches("test-token"));
        assert!(state.client_connections.lock().unwrap().is_empty());
        assert!(state.connected_connections.lock().unwrap().is_empty());
        assert!(state.pending_connections.lock().unwrap().is_empty());
        assert_eq!(state.session_generation.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn concurrent_offers_for_one_client_share_a_gate() {
        let state = test_state();
        let first = lock_offer(&state, "viewer").await;
        let waiting_state = state.clone();
        let mut waiting = tokio::spawn(async move {
            let _second = lock_offer(&waiting_state, "viewer").await;
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut waiting)
                .await
                .is_err()
        );
        drop(first);
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("second offer did not acquire the gate")
            .expect("second offer task failed");

        let other = lock_offer(&state, "other-viewer").await;
        let locks = state.offer_locks.lock().unwrap();
        assert_eq!(locks.len(), 1);
        assert!(locks.contains_key("other-viewer"));
        drop(locks);
        drop(other);
    }

    #[tokio::test]
    async fn explicit_public_ip_becomes_the_media_candidate() {
        let state = test_state();
        let candidate = update_media_candidate(&state, "203.0.113.7").await.unwrap();

        assert_eq!(
            candidate,
            "203.0.113.7:40000".parse::<std::net::SocketAddr>().unwrap()
        );
        assert_eq!(state.udp_mux.candidate_addr().unwrap(), candidate);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ui_bootstrap_can_switch_to_an_explicit_test_pattern() {
        let mut bootstrap = AppConfig {
            bind: "127.0.0.1".to_owned(),
            http_port: 0,
            ..Default::default()
        };
        bootstrap.media_ports.first = 0;
        bootstrap.media_ports.last = 0;
        bootstrap.source.kind = "test".to_owned();
        bootstrap.source.index = 0;
        bootstrap.source.native_id = None;
        bootstrap.audio_mode = "off".to_owned();

        let mut selected = CaptureSettings::from_config(&bootstrap);
        selected.output_height = Some(1080);
        selected.output_fps = Some(60);
        selected.adaptive_quality_ceiling = "1080p".to_owned();
        selected.adaptive_fps_ceiling = "60".to_owned();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(run_with_control(bootstrap, shutdown_rx, control_rx));
        let (initial_snapshot_tx, initial_snapshot_rx) = std::sync::mpsc::sync_channel(1);
        control_tx
            .send(ServerCommand::PreviewSnapshot {
                result: initial_snapshot_tx,
            })
            .unwrap();
        let initial_snapshot = tokio::task::spawn_blocking(move || {
            initial_snapshot_rx.recv_timeout(Duration::from_secs(2))
        })
        .await
        .unwrap()
        .expect("initial cold preview snapshot timed out");
        assert!(initial_snapshot.is_none());

        let (result_tx, result_rx) = oneshot::channel();
        control_tx
            .send(ServerCommand::StartStream {
                settings: selected.clone(),
                result: Some(result_tx),
            })
            .unwrap();

        let start_result = tokio::time::timeout(Duration::from_secs(15), result_rx)
            .await
            .expect("UI-style stream start timed out")
            .expect("server dropped the UI stream-start result");
        assert!(start_result.is_ok(), "{start_result:?}");

        let (snapshot_tx, snapshot_rx) = std::sync::mpsc::sync_channel(1);
        control_tx
            .send(ServerCommand::PreviewSnapshot {
                result: snapshot_tx,
            })
            .unwrap();
        let running_snapshot =
            tokio::task::spawn_blocking(move || snapshot_rx.recv_timeout(Duration::from_secs(2)))
                .await
                .unwrap()
                .expect("running preview snapshot timed out");
        assert!(running_snapshot.is_some());

        let (stop_tx, stop_rx) = oneshot::channel();
        control_tx
            .send(ServerCommand::StopStream {
                result: Some(stop_tx),
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), stop_rx)
            .await
            .expect("stream stop timed out")
            .expect("server dropped the stream-stop result");

        let (cold_snapshot_tx, cold_snapshot_rx) = std::sync::mpsc::sync_channel(1);
        control_tx
            .send(ServerCommand::PreviewSnapshot {
                result: cold_snapshot_tx,
            })
            .unwrap();
        let cold_snapshot = tokio::task::spawn_blocking(move || {
            cold_snapshot_rx.recv_timeout(Duration::from_secs(2))
        })
        .await
        .unwrap()
        .expect("cold preview snapshot timed out");
        assert!(cold_snapshot.is_none());

        let (restart_tx, restart_rx) = oneshot::channel();
        control_tx
            .send(ServerCommand::StartStream {
                settings: selected,
                result: Some(restart_tx),
            })
            .unwrap();
        let restart_result = tokio::time::timeout(Duration::from_secs(15), restart_rx)
            .await
            .expect("second stream start timed out")
            .expect("server dropped the second stream-start result");
        assert!(restart_result.is_ok(), "{restart_result:?}");

        let (second_stop_tx, second_stop_rx) = oneshot::channel();
        control_tx
            .send(ServerCommand::StopStream {
                result: Some(second_stop_tx),
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), second_stop_rx)
            .await
            .expect("second stream stop timed out")
            .expect("server dropped the second stream-stop result");

        let (second_cold_tx, second_cold_rx) = std::sync::mpsc::sync_channel(1);
        control_tx
            .send(ServerCommand::PreviewSnapshot {
                result: second_cold_tx,
            })
            .unwrap();
        let second_cold = tokio::task::spawn_blocking(move || {
            second_cold_rx.recv_timeout(Duration::from_secs(2))
        })
        .await
        .unwrap()
        .expect("second cold preview snapshot timed out");
        assert!(second_cold.is_none());

        let _ = shutdown_tx.send(());
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("server shutdown timed out")
            .expect("server task panicked")
            .expect("server failed");
    }

    #[tokio::test]
    async fn token_route_serves_viewer() {
        let state = test_state();
        let token = state.config.token.clone();
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/{token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn sustained_degradation_moves_a_viewer_to_the_lower_group() {
        let settings = CaptureSettings::from_config(&AppConfig::default());
        let primary = Arc::new(MediaPipeline::with_codec("vp8").unwrap());
        let secondary = Arc::new(MediaPipeline::with_codec("vp8").unwrap());
        let groups = TranscodeGroups::new(vec![
            TranscodeGroup::active(0, primary, settings.clone()),
            TranscodeGroup::active(1, secondary, group_settings(&settings, 1)),
        ]);
        let metrics = ViewerMetrics {
            rtt_ms: 20.0,
            jitter_ms: 5.0,
            loss_rate: 0.0,
            bitrate_bps: 100_000.0,
            reported_frames_dropped: 0,
            reported_freeze_count: 0,
            updated_at: Instant::now(),
            samples: (0..5)
                .map(|_| MetricSample {
                    captured_at: Instant::now(),
                    rtt_ms: 20.0,
                    jitter_ms: 5.0,
                    loss_rate: 0.0,
                    bitrate_bps: 100_000.0,
                    available_incoming_bitrate_bps: Some(100_000.0),
                    frames_dropped: 0,
                    freeze_count: 0,
                    visibility_state: "visible".to_owned(),
                })
                .collect(),
        };

        let assignment = groups.observe("slow-viewer", &metrics).unwrap();
        assert_eq!(assignment.group_id, 1);
        assert!(assignment.restart);
    }

    #[test]
    fn playback_artifacts_without_transport_pressure_do_not_lower_quality() {
        let settings = CaptureSettings::from_config(&AppConfig::default());
        let primary = Arc::new(MediaPipeline::with_codec("vp8").unwrap());
        let secondary = Arc::new(MediaPipeline::with_codec("vp8").unwrap());
        let groups = TranscodeGroups::new(vec![
            TranscodeGroup::active(0, primary, settings.clone()),
            TranscodeGroup::active(1, secondary, group_settings(&settings, 1)),
        ]);
        let metrics = ViewerMetrics {
            rtt_ms: 20.0,
            jitter_ms: 5.0,
            loss_rate: 0.0,
            bitrate_bps: settings.bitrate as f64,
            reported_frames_dropped: 100,
            reported_freeze_count: 0,
            updated_at: Instant::now(),
            samples: (0..5)
                .map(|_| MetricSample {
                    captured_at: Instant::now(),
                    rtt_ms: 20.0,
                    jitter_ms: 5.0,
                    loss_rate: 0.0,
                    bitrate_bps: settings.bitrate as f64,
                    available_incoming_bitrate_bps: None,
                    // Normal catch-up/renderer drops are below the extreme
                    // threshold and must not reduce quality.
                    frames_dropped: 20,
                    freeze_count: 0,
                    visibility_state: "visible".to_owned(),
                })
                .collect(),
        };

        assert!(groups.observe("healthy-viewer", &metrics).is_none());
        let mut all_metrics = HashMap::new();
        all_metrics.insert("healthy-viewer".to_owned(), metrics);
        assert!(!groups.tune(&all_metrics));
        assert_eq!(groups.group_settings(0).bitrate, settings.bitrate);
    }

    #[test]
    fn freezes_and_only_extreme_frame_drops_trigger_playback_downgrade() {
        assert!(!playback_requires_downgrade(1, 0));
        assert!(!playback_requires_downgrade(
            EXTREME_DROPPED_FRAME_THRESHOLD - 1,
            FREEZE_DOWNGRADE_THRESHOLD - 1,
        ));
        assert!(playback_requires_downgrade(0, FREEZE_DOWNGRADE_THRESHOLD));
        assert!(playback_requires_downgrade(
            EXTREME_DROPPED_FRAME_THRESHOLD,
            0,
        ));
    }

    #[test]
    fn repeated_freezes_move_a_single_viewer_to_the_lower_group() {
        let settings = CaptureSettings::from_config(&AppConfig::default());
        let primary = Arc::new(MediaPipeline::with_codec("vp8").unwrap());
        let secondary = Arc::new(MediaPipeline::with_codec("vp8").unwrap());
        let groups = TranscodeGroups::new(vec![
            TranscodeGroup::active(0, primary, settings.clone()),
            TranscodeGroup::active(1, secondary, group_settings(&settings, 1)),
        ]);
        let metrics = ViewerMetrics {
            rtt_ms: 20.0,
            jitter_ms: 5.0,
            loss_rate: 0.0,
            bitrate_bps: settings.bitrate as f64,
            reported_frames_dropped: 0,
            reported_freeze_count: FREEZE_DOWNGRADE_THRESHOLD,
            updated_at: Instant::now(),
            samples: (0..5)
                .map(|_| MetricSample {
                    captured_at: Instant::now(),
                    rtt_ms: 20.0,
                    jitter_ms: 5.0,
                    loss_rate: 0.0,
                    bitrate_bps: settings.bitrate as f64,
                    available_incoming_bitrate_bps: None,
                    frames_dropped: 0,
                    freeze_count: 1,
                    visibility_state: "visible".to_owned(),
                })
                .collect(),
        };

        let assignment = groups.observe("freezing-viewer", &metrics).unwrap();
        assert_eq!(assignment.group_id, 1);
        assert_eq!(assignment.reason, "15-second playback failure window");
    }

    #[test]
    fn stopped_group_slots_are_hidden_from_client_status() {
        let settings = CaptureSettings::from_config(&AppConfig::default());
        let primary = Arc::new(MediaPipeline::with_codec("vp8").unwrap());
        let groups = TranscodeGroups::new(vec![
            TranscodeGroup::active(0, primary, settings.clone()),
            TranscodeGroup::stopped(1, group_settings(&settings, 1), "vp8"),
        ]);

        assert_eq!(groups.count(), 1);
        let visible_groups = groups.groups_json();
        assert_eq!(visible_groups.len(), 1);
        assert_eq!(visible_groups[0]["id"], "group-1");
    }

    #[test]
    fn empty_secondary_group_is_drained_after_the_grace_period() {
        let settings = CaptureSettings::from_config(&AppConfig::default());
        let primary = Arc::new(MediaPipeline::with_codec("vp8").unwrap());
        let secondary = Arc::new(MediaPipeline::with_codec("vp8").unwrap());
        let groups = TranscodeGroups::new(vec![
            TranscodeGroup::active(0, primary, settings.clone()),
            TranscodeGroup::active(1, secondary, group_settings(&settings, 1)),
        ]);

        assert!(groups.maintain().changed);
        assert_eq!(groups.group(1).lifecycle(), GroupLifecycle::Draining);
        *groups.group(1).drain_started_at.lock().unwrap() =
            Some(Instant::now() - Duration::from_secs(13));

        assert!(groups.maintain().changed);
        assert_eq!(groups.group(1).lifecycle(), GroupLifecycle::Stopped);
        assert!(groups.group(1).media().is_none());
        assert_eq!(groups.count(), 1);
    }

    #[test]
    fn network_probe_is_a_one_mebibyte_non_repeating_body() {
        let payload = network_probe_payload();
        assert_eq!(payload.len(), 1024 * 1024);
        assert_ne!(payload[..64], payload[64..128]);
    }

    #[test]
    fn automatic_codec_uses_vp8_when_firefox_does_not_advertise_h264() {
        let host = HashSet::from(["vp8".to_owned(), "vp9".to_owned(), "h264".to_owned()]);
        let firefox = HashSet::from(["vp8".to_owned(), "vp9".to_owned()]);
        assert_eq!(
            choose_codec("auto", &host, &firefox, &HashSet::new(), 8_000_000.0, 20.0),
            "vp8"
        );
    }

    #[test]
    fn automatic_codec_prefers_the_validated_vp8_baseline() {
        let host = HashSet::from(["vp8".to_owned(), "h264".to_owned()]);
        let chromium = HashSet::from(["vp8".to_owned(), "h264".to_owned()]);
        assert_eq!(
            choose_codec("auto", &host, &chromium, &HashSet::new(), 8_000_000.0, 20.0),
            "vp8"
        );
    }

    #[test]
    fn explicit_h264_policy_still_selects_h264() {
        let host = HashSet::from(["vp8".to_owned(), "h264".to_owned()]);
        let chromium = HashSet::from(["vp8".to_owned(), "h264".to_owned()]);
        assert_eq!(
            choose_codec("h264", &host, &chromium, &HashSet::new(), 8_000_000.0, 20.0),
            "h264"
        );
    }

    #[test]
    fn h264_offer_must_receive_constrained_baseline_level_31() {
        let offer = |profile_level_id: &str| {
            format!(
                "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 108\r\na=rtpmap:108 H264/90000\r\na=fmtp:108 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile_level_id}\r\n"
            )
        };

        assert!(offer_supports_h264_level_31(&offer("42e01f")));
        assert!(offer_supports_h264_level_31(&offer("42e02a")));
        assert!(!offer_supports_h264_level_31(&offer("42e01e")));
        assert!(!offer_supports_h264_level_31(&offer("4d001f")));
        assert!(!offer_supports_h264_level_31(
            "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 VP8/90000\r\n"
        ));
    }

    #[test]
    fn changing_monitor_or_cursor_restarts_shared_capture_but_bitrate_does_not() {
        let original = CaptureSettings::from_config(&AppConfig::default());
        let mut different_monitor = original.clone();
        different_monitor.source_index = 1;
        let mut different_cursor = original.clone();
        different_cursor.draw_mouse = !original.draw_mouse;
        let mut different_native_window = original.clone();
        different_native_window.source_kind = "window".to_owned();
        different_native_window.source_native_id = Some(42);
        let mut reordered_same_window = different_native_window.clone();
        reordered_same_window.source_index = 99;
        let mut different_quality = original.clone();
        different_quality.output_height = Some(720);
        let mut different_fps = original.clone();
        different_fps.output_fps = Some(30);
        let mut bitrate_only = original.clone();
        bitrate_only.bitrate = bitrate_only.bitrate.saturating_add(1);

        assert!(capture_input_changed(&original, &different_monitor));
        assert!(capture_input_changed(&original, &different_cursor));
        assert!(capture_input_changed(&original, &different_native_window));
        assert!(!capture_input_changed(
            &different_native_window,
            &reordered_same_window
        ));
        assert!(!capture_input_changed(&original, &bitrate_only));
        assert!(capture_format_changed(&original, &different_quality));
        assert!(capture_format_changed(&original, &different_fps));
        assert!(!capture_format_changed(&original, &bitrate_only));
    }

    #[cfg(windows)]
    #[test]
    fn window_resize_debounce_emits_only_the_last_settled_dimensions() {
        let source = WindowSourceIdentity {
            index: 3,
            native_id: Some(42),
        };
        let start = Instant::now();
        let mut debounce = WindowResizeDebouncer::default();

        assert_eq!(
            debounce.observe(source, (800, 600), (900, 600), start),
            None
        );
        assert_eq!(
            debounce.observe(
                source,
                (800, 600),
                (1000, 700),
                start + Duration::from_millis(200)
            ),
            None
        );
        assert_eq!(
            debounce.observe(
                source,
                (800, 600),
                (1000, 700),
                start + WINDOW_RESIZE_SAFETY_SETTLE_TIME + Duration::from_millis(199)
            ),
            None
        );
        assert_eq!(
            debounce.observe(
                source,
                (800, 600),
                (1000, 700),
                start + WINDOW_RESIZE_SAFETY_SETTLE_TIME + Duration::from_millis(200)
            ),
            Some((1000, 700))
        );
    }

    #[cfg(windows)]
    #[test]
    fn window_resize_retries_switch_from_quick_attempts_to_persistent_recovery() {
        assert_eq!(window_resize_retry_delay(1), WINDOW_RESIZE_RETRY_DELAY);
        assert_eq!(window_resize_retry_delay(2), WINDOW_RESIZE_RETRY_DELAY * 2);
        assert_eq!(
            window_resize_retry_delay(WINDOW_RESIZE_RESTART_ATTEMPTS),
            WINDOW_RESIZE_RECOVERY_RETRY_DELAY
        );
        assert_eq!(
            window_resize_retry_delay(WINDOW_RESIZE_RESTART_ATTEMPTS + 100),
            WINDOW_RESIZE_RECOVERY_RETRY_DELAY
        );
    }

    #[cfg(windows)]
    #[test]
    fn window_resize_event_signal_emits_the_observed_dimensions_immediately() {
        let source = WindowSourceIdentity {
            index: 2,
            native_id: Some(42),
        };
        let mut debounce = WindowResizeDebouncer::default();

        assert_eq!(
            debounce.observe_settled(source, (800, 600), (1200, 700)),
            Some((1200, 700))
        );
        assert_eq!(
            debounce.observe_settled(source, (800, 600), (1200, 700)),
            None
        );
        assert_eq!(
            debounce.observe_settled(source, (800, 600), (1280, 720)),
            Some((1280, 720))
        );
    }

    #[cfg(windows)]
    #[test]
    fn window_resize_debounce_resets_after_refresh_and_suppresses_failed_dimensions() {
        let source = WindowSourceIdentity {
            index: 1,
            native_id: Some(7),
        };
        let start = Instant::now();
        let mut debounce = WindowResizeDebouncer::default();
        assert_eq!(
            debounce.observe(source, (800, 600), (1200, 700), start),
            None
        );
        assert_eq!(
            debounce.observe(
                source,
                (800, 600),
                (1200, 700),
                start + WINDOW_RESIZE_SAFETY_SETTLE_TIME
            ),
            Some((1200, 700))
        );
        assert_eq!(
            debounce.observe(
                source,
                (800, 600),
                (1200, 700),
                start + WINDOW_RESIZE_SAFETY_SETTLE_TIME + Duration::from_millis(100)
            ),
            None
        );
        assert_eq!(
            debounce.observe(
                source,
                (800, 600),
                (1200, 700),
                start + Duration::from_secs(30)
            ),
            None
        );
        debounce.reset();
        assert_eq!(
            debounce.observe(
                source,
                (800, 600),
                (1200, 700),
                start + Duration::from_secs(31)
            ),
            None
        );
        assert_eq!(
            debounce.observe(
                source,
                (800, 600),
                (1200, 700),
                start + Duration::from_secs(31) + WINDOW_RESIZE_SAFETY_SETTLE_TIME
            ),
            Some((1200, 700))
        );
        assert_eq!(
            debounce.observe(
                source,
                (1200, 700),
                (1200, 700),
                start + Duration::from_secs(3)
            ),
            None
        );
        assert!(debounce.candidate.is_none());
    }

    #[test]
    fn capture_slot_is_empty_until_a_capture_is_installed() {
        let slot = CaptureSlot::default();
        assert!(slot.current().is_none());
        assert!(slot.take().is_none());
        assert!(slot.current().is_none());
    }

    #[test]
    fn detached_group_cleanup_cannot_remove_replacement_media() {
        let settings = CaptureSettings::from_config(&AppConfig::default());
        let old_media = Arc::new(MediaPipeline::with_codec("vp8").unwrap());
        let groups = TranscodeGroups::new(vec![TranscodeGroup::active(
            0,
            Arc::clone(&old_media),
            settings,
        )]);

        let detached = groups.detach_all_media();
        assert_eq!(detached.len(), 1);
        assert!(groups.media_by_id(0).is_none());

        let replacement = Arc::new(MediaPipeline::with_codec("vp8").unwrap());
        *groups.group(0).media.lock().unwrap() = Some(Arc::clone(&replacement));
        *groups.group(0).lifecycle.lock().unwrap() = GroupLifecycle::Active;
        for old in detached {
            old.stop();
        }

        assert!(Arc::ptr_eq(
            &groups
                .media_by_id(0)
                .expect("replacement remains installed"),
            &replacement
        ));
    }

    #[test]
    fn cold_groups_have_no_media_or_visible_primary() {
        let settings = CaptureSettings::from_config(&AppConfig::default());
        let groups = TranscodeGroups::new(vec![TranscodeGroup::stopped(0, settings, "vp8")]);
        assert_eq!(groups.resource_count(), 0);
        assert!(groups.active_group_ids().is_empty());
        assert!(groups.media_by_id(0).is_none());
    }

    #[test]
    fn switching_to_manual_collapses_viewers_to_the_primary_group() {
        let settings = CaptureSettings::from_config(&AppConfig::default());
        let primary = Arc::new(MediaPipeline::with_codec("vp8").unwrap());
        let secondary = Arc::new(MediaPipeline::with_codec("vp8").unwrap());
        let groups = TranscodeGroups::new(vec![
            TranscodeGroup::active(0, primary, settings.clone()),
            TranscodeGroup::active(1, secondary, group_settings(&settings, 1)),
        ]);
        groups.assignments.lock().unwrap().insert(
            "viewer".to_owned(),
            ClientGroupState {
                group_id: 1,
                last_change: Instant::now(),
                reason: "adaptive assignment".to_owned(),
                supported_codecs: HashSet::new(),
                rejected_codecs: HashSet::new(),
            },
        );
        let mut manual = settings;
        manual.quality_mode = "manual".to_owned();
        let result = groups.reconfigure(manual, true);

        assert!(result.topology_changed);
        assert_eq!(groups.capacity(), 1);
        assert_eq!(groups.assignment_for("viewer").group_id, 0);
        assert_eq!(groups.group(1).lifecycle(), GroupLifecycle::Stopped);
        assert!(groups.group(1).media().is_none());
    }

    #[test]
    fn live_quality_mode_selects_the_matching_adaptive_controller() {
        let mut settings = CaptureSettings::from_config(&AppConfig::default());
        assert!(uses_group_adaptation(&settings));
        assert!(!uses_single_group_bitrate_adaptation(&settings));

        settings.quality_mode = "manual".to_owned();
        settings.bitrate_mode = "automatic".to_owned();
        assert!(!uses_group_adaptation(&settings));
        assert!(uses_single_group_bitrate_adaptation(&settings));

        settings.bitrate_mode = "fixed".to_owned();
        assert!(!uses_group_adaptation(&settings));
        assert!(!uses_single_group_bitrate_adaptation(&settings));
    }

    #[tokio::test]
    async fn viewer_metrics_are_normalized_and_count_bounded() {
        let state = test_state();
        let started_at = Instant::now();
        for index in 0..(MAX_METRIC_SAMPLES_PER_VIEWER + 5) {
            let _ = update_viewer_metrics_at(
                &state,
                "viewer",
                &ViewerStats {
                    rtt_ms: f64::INFINITY,
                    jitter_ms: -10.0,
                    loss_rate: 2.0,
                    bitrate_bps: 1_000_000.0,
                    available_incoming_bitrate_bps: Some(f64::NAN),
                    frames_dropped: index as u64,
                    freeze_count: 0,
                    visibility_state: "unexpected".to_owned(),
                },
                started_at + MIN_VIEWER_METRIC_INTERVAL * index as u32,
            );
        }

        let metrics = state.viewer_metrics.lock().unwrap();
        let metric = metrics.get("viewer").unwrap();
        assert_eq!(metric.samples.len(), MAX_METRIC_SAMPLES_PER_VIEWER);
        assert_eq!(metric.rtt_ms, 0.0);
        assert_eq!(metric.jitter_ms, 0.0);
        assert_eq!(metric.loss_rate, 1.0);
        assert_eq!(metric.samples.back().unwrap().visibility_state, "unknown");
    }
}
