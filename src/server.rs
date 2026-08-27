use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Command;
use std::sync::{
    Arc, Mutex as StdMutex, OnceLock,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
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
use tokio::sync::{Mutex, oneshot};
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
use crate::shared_capture::SharedCapture;
use crate::udp_mux::UdpMux;

pub enum ServerCommand {
    StartStream {
        settings: CaptureSettings,
        result: Option<oneshot::Sender<std::result::Result<(), String>>>,
    },
    StopStream,
    Update(CaptureSettings),
}

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct WebAssets;

#[derive(Clone)]
pub struct ServerState {
    pub config: Arc<AppConfig>,
    settings_revision: Arc<AtomicU64>,
    media_session_revision: Arc<AtomicU64>,
    pub audio: Option<Arc<AudioPipeline>>,
    stream_enabled: Arc<AtomicBool>,
    settings: Arc<StdMutex<CaptureSettings>>,
    viewer_metrics: Arc<StdMutex<HashMap<String, ViewerMetrics>>>,
    groups: Arc<TranscodeGroups>,
    shared_capture: Option<Arc<SharedCapture>>,
    pub connections: Arc<Mutex<std::collections::HashMap<Uuid, Arc<dyn PeerConnection>>>>,
    pub udp_mux: Arc<UdpMux>,
    pending_connections: Arc<StdMutex<HashSet<Uuid>>>,
    connected_connections: Arc<StdMutex<HashSet<Uuid>>>,
    client_connections: Arc<StdMutex<std::collections::HashMap<String, Uuid>>>,
    connection_bindings: Arc<StdMutex<HashMap<Uuid, ConnectionMediaBinding>>>,
    client_sockets: Arc<StdMutex<std::collections::HashMap<String, SocketRef>>>,
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
    shared_capture: Arc<SharedCapture>,
    codec_policy: String,
    host_codecs: Arc<HashSet<String>>,
    stream_enabled: Arc<AtomicBool>,
    tasks: Arc<StdMutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl GroupFactory {
    fn start(&self, codec: &str, settings: CaptureSettings) -> Result<Arc<MediaPipeline>> {
        let media = Arc::new(MediaPipeline::with_codec(codec)?);
        let source_dimensions = self.shared_capture.source_dimensions();
        let source_pixel_format = self.shared_capture.source_pixel_format();
        let source_fps = self.shared_capture.source_fps();
        let task = media.clone().spawn_from_shared_source(
            self.ffmpeg.clone(),
            self.shared_capture.subscribe(),
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
            tasks.push(task);
        }
    }

    fn take_tasks(&self) -> Vec<tokio::task::JoinHandle<()>> {
        self.tasks
            .lock()
            .map(|mut tasks| std::mem::take(&mut *tasks))
            .unwrap_or_default()
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
            ..GroupReconfiguration::default()
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

    fn deactivate(&self) {
        for group in self.groups.iter() {
            if let Some(media) = group.media() {
                media.deactivate();
            }
        }
    }

    fn stop(&self) {
        for group in self.groups.iter() {
            if let Ok(mut media) = group.media.lock()
                && let Some(media) = media.take()
            {
                media.stop();
            }
            if let Ok(mut lifecycle) = group.lifecycle.lock() {
                *lifecycle = GroupLifecycle::Stopped;
            }
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

        let next_higher_id = self
            .active_group_ids()
            .into_iter()
            .filter(|group_id| {
                *group_id < assignment.group_id
                    && self
                        .group_codec(*group_id)
                        .eq_ignore_ascii_case(&current_codec)
            })
            .next_back()?;
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
        if state == RTCPeerConnectionState::Disconnected
            && let Ok(mut connected) = self.connected_connections.lock()
        {
            connected.remove(&self.connection_id);
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
    mut shutdown: oneshot::Receiver<()>,
    mut control_rx: tokio::sync::mpsc::UnboundedReceiver<ServerCommand>,
) -> Result<()> {
    let addr = config.http_addr()?;
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
    let media = Arc::new(MediaPipeline::with_codec(initial_codec)?);
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
    let shared_capture = Arc::new(SharedCapture::start(
        &ffmpeg.command,
        initial_settings.clone(),
    )?);
    let source_dimensions = shared_capture.source_dimensions();
    let source_pixel_format = shared_capture.source_pixel_format();
    let source_fps = shared_capture.source_fps();
    let group_budget = if config.quality_mode == "adaptive" {
        configured_group_count(&config.max_quality_groups)
    } else {
        1
    };
    let group_factory = Arc::new(GroupFactory {
        ffmpeg: ffmpeg.command.clone(),
        shared_capture: Arc::clone(&shared_capture),
        codec_policy: config.codec.clone(),
        host_codecs: Arc::new(discover_host_codecs(&ffmpeg.command, initial_codec)),
        stream_enabled: Arc::clone(&stream_enabled),
        tasks: Arc::new(StdMutex::new(Vec::new())),
    });
    // Always allocate the four lightweight group slots.  The configured group
    // count is a live budget, not a startup-only topology decision.
    let mut groups = Vec::with_capacity(4);
    for group_id in 0..4 {
        let settings = group_settings(&initial_settings, group_id);
        if group_id == 0 {
            let task = media.clone().spawn_from_shared_source(
                ffmpeg.command.clone(),
                shared_capture.subscribe(),
                source_dimensions,
                source_pixel_format,
                source_fps,
                settings.clone(),
            );
            group_factory.register_task(task);
            groups.push(TranscodeGroup::active(
                group_id,
                Arc::clone(&media),
                settings,
            ));
        } else {
            groups.push(TranscodeGroup::stopped(group_id, settings, initial_codec));
        }
    }
    let groups = Arc::new(TranscodeGroups::with_factory(
        groups,
        Some(Arc::clone(&group_factory)),
    ));
    groups.active_budget.store(group_budget, Ordering::Release);
    let audio_task = audio
        .as_ref()
        .map(|audio| Arc::clone(audio).spawn(initial_settings.clone()));
    let viewer_metrics = Arc::new(StdMutex::new(HashMap::new()));
        let state = ServerState {
            config: Arc::new(config.clone()),
            settings_revision: Arc::new(AtomicU64::new(0)),
            media_session_revision: Arc::new(AtomicU64::new(0)),
        audio: audio.clone(),
        stream_enabled: Arc::clone(&stream_enabled),
        settings: Arc::clone(&settings_slot),
        viewer_metrics: Arc::clone(&viewer_metrics),
        groups: Arc::clone(&groups),
        shared_capture: Some(Arc::clone(&shared_capture)),
        connections: Arc::new(Mutex::new(std::collections::HashMap::new())),
        udp_mux,
        pending_connections: Arc::new(StdMutex::new(HashSet::new())),
        connected_connections: Arc::new(StdMutex::new(HashSet::new())),
        client_connections: Arc::new(StdMutex::new(std::collections::HashMap::new())),
        connection_bindings: Arc::new(StdMutex::new(HashMap::new())),
        client_sockets: Arc::new(StdMutex::new(std::collections::HashMap::new())),
    };
    // Both controllers remain available for live Manual <-> Auto changes.
    // Each loop reads the current settings before it acts, so inactive modes
    // are idle rather than retaining the behavior selected at server startup.
    let adaptive_tasks = vec![
        tokio::spawn(adaptive_loop(state.clone())),
        tokio::spawn(group_adaptive_loop(state.clone())),
    ];
    let control_state = state.clone();
    let control_task = tokio::spawn(async move {
        while let Some(command) = control_rx.recv().await {
            match command {
                ServerCommand::StartStream { settings, result } => {
                    if let Err(error) = apply_capture_settings(&control_state, settings.clone()) {
                        warn!(%error, "could not apply stream settings");
                        if let Some(result) = result {
                            let _ = result.send(Err(error.to_string()));
                        }
                        continue;
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
                ServerCommand::StopStream => {
                    control_state.stream_enabled.store(false, Ordering::Release);
                    control_state
                        .media_session_revision
                        .fetch_add(1, Ordering::AcqRel);
                    control_state.groups.deactivate();
                    if let Some(audio) = &control_state.audio {
                        audio.deactivate();
                    }
                    broadcast_status(&control_state);
                }
                ServerCommand::Update(settings) => {
                    if let Err(error) = apply_capture_settings(&control_state, settings) {
                        warn!(%error, "could not update capture settings");
                        continue;
                    }
                    broadcast_status(&control_state);
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
    let listener = tokio::net::TcpListener::bind(addr).await?;
    crate::runtime::write(&crate::runtime::RuntimeStatus {
        pid: std::process::id(),
        http_port: config.http_port,
        media_port: config.media_ports.first,
        viewer_url: config.viewer_url(),
        source: config
            .source
            .native_id
            .map(|native_id| format!("{}:{native_id}", config.source.kind))
            .unwrap_or_else(|| format!("{}:{}", config.source.kind, config.source.index)),
        codec: media.codec_name().to_owned(),
    })?;
    info!(address = %listener.local_addr()?, viewer_url = %config.viewer_url(), "viewer server listening");
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = (&mut shutdown).await;
            info!("viewer server shutting down");
        })
        .await;

    let _ = socket_io.close().await;
    control_task.abort();
    for adaptive_task in adaptive_tasks {
        adaptive_task.abort();
    }
    state.groups.stop();
    if let Some(shared_capture) = &state.shared_capture {
        let _ = shared_capture.stop();
    }
    if let Some(audio) = &audio {
        audio.stop();
    }
    for media_task in state.groups.take_tasks() {
        let _ = media_task.await;
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
        let shared_capture = state
            .shared_capture
            .as_ref()
            .context("shared capture is unavailable")?;
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
                        .as_ref()
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
        state
            .media_session_revision
            .fetch_add(1, Ordering::AcqRel);
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

async fn adaptive_loop(state: ServerState) {
    let mut interval = tokio::time::interval(Duration::from_secs(3));
    let mut stable_since = std::time::Instant::now();
    let mut last_change = std::time::Instant::now() - Duration::from_secs(10);
    loop {
        interval.tick().await;
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
        if let Ok(mut settings) = state.settings.lock() {
            settings.bitrate = target;
            let _ = state.groups.reconfigure(settings.clone(), false);
            last_change = now;
            broadcast_status(&state);
        }
    }
}

async fn group_adaptive_loop(state: ServerState) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;
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
    if token == state.config.token {
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
    if token != state.config.token.as_str() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(status_snapshot(&state)))
}

async fn connection_probe(
    Path(token): Path<String>,
    State(state): State<ServerState>,
) -> Result<Response, StatusCode> {
    if token != state.config.token.as_str() {
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
    let viewers = state
        .connected_connections
        .lock()
        .map(|connected| connected.len())
        .unwrap_or_default();
    let settings = state.settings.lock().ok().map(|settings| settings.clone());
    let audio_enabled = settings.as_ref().is_some_and(audio_enabled);
    let primary_media = state.groups.media_by_id(state.groups.primary_group_id());
    let capture_error = state
        .shared_capture
        .as_ref()
        .and_then(|capture| capture.failure());
    json!({
        "status": primary_media.as_ref().map(|media| media.status()).unwrap_or("stopped"),
        "stream_enabled": state.stream_enabled.load(Ordering::Acquire),
        "media_session_revision": state.media_session_revision.load(Ordering::Acquire),
        "viewers": viewers,
        "bind": state.config.bind,
        "http_port": state.config.http_port,
        "media_port": state.config.media_ports.first,
        "codec": primary_media.as_ref().map(|media| media.codec_name()).unwrap_or("Unknown"),
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
        "max_viewers": state.config.max_viewers,
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
        "capture_backend": state.shared_capture.as_ref().map(|capture| capture.backend_name())
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
    if token != state.config.token.as_str() {
        return Json(json!({ "ok": false, "error": "invalid token" }));
    }
    Json(json!({ "ok": true, "message": "signaling session endpoint is ready" }))
}

async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn offer(
    Path(token): Path<String>,
    State(state): State<ServerState>,
    Json(request): Json<serde_json::Value>,
) -> Result<Json<RTCSessionDescription>, (StatusCode, String)> {
    if token != state.config.token.as_str() {
        return Err((StatusCode::NOT_FOUND, "invalid token".to_owned()));
    }
    let client_id = request
        .get("clientId")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or_else(|| internal_error("offer is missing a valid clientId"))?
        .to_owned();
    let offer: RTCSessionDescription = serde_json::from_value(request).map_err(internal_error)?;
    close_client_connection(&state, &client_id).await;
    let media = state.groups.media_for(&client_id).map_err(internal_error)?;
    let remote_ufrag = crate::udp_mux::ice_ufrag(&offer.sdp)
        .ok_or_else(|| internal_error("offer does not contain an ICE username fragment"))?;
    let connection_id = Uuid::new_v4();
    {
        let connections = state.connections.lock().await;
        let mut pending = state
            .pending_connections
            .lock()
            .map_err(|_| internal_error("pending connection lock poisoned"))?;
        if connections.len() + pending.len() >= state.config.max_viewers {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "viewer limit reached".to_owned(),
            ));
        }
        pending.insert(connection_id);
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
    state
        .connections
        .lock()
        .await
        .insert(connection_id, peer_connection);
    reservation.commit();
    Ok(Json(local_description))
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
    let client_id = auth.client_id.trim();
    if auth.token != state.config.token.as_str() || client_id.is_empty() || client_id.len() > 128 {
        warn!("rejecting invalid control socket authentication");
        let _ = socket.disconnect();
        return;
    }
    let client_id = client_id.to_owned();
    state.groups.ensure_client(&client_id);
    let previous = state
        .client_sockets
        .lock()
        .ok()
        .and_then(|mut sockets| sockets.insert(client_id.clone(), socket.clone()));
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
        let metrics = update_viewer_metrics(&state, &identity.client_id, &stats);
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
) -> ViewerMetrics {
    let previous = state
        .viewer_metrics
        .lock()
        .ok()
        .and_then(|metrics| metrics.get(client_id).cloned());
    let now = Instant::now();
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
        rtt_ms: value.rtt_ms,
        jitter_ms: value.jitter_ms,
        loss_rate: value.loss_rate,
        bitrate_bps: value.bitrate_bps,
        available_incoming_bitrate_bps: value.available_incoming_bitrate_bps,
        frames_dropped,
        freeze_count,
        visibility_state: value.visibility_state.clone(),
    });
    let metric = ViewerMetrics {
        rtt_ms: value.rtt_ms,
        jitter_ms: value.jitter_ms,
        loss_rate: value.loss_rate,
        bitrate_bps: value.bitrate_bps,
        reported_frames_dropped: value.frames_dropped,
        reported_freeze_count: value.freeze_count,
        updated_at: now,
        samples,
    };
    if let Ok(mut metrics) = state.viewer_metrics.lock() {
        metrics.insert(client_id.to_owned(), metric.clone());
    }
    metric
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
        let settings = CaptureSettings::from_config(&config);
        let media = Arc::new(MediaPipeline::with_codec("vp8").unwrap());
        let groups = Arc::new(TranscodeGroups::new(vec![TranscodeGroup::active(
            0,
            Arc::clone(&media),
            settings.clone(),
        )]));
        let mux = UdpMux::bind(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        ServerState {
            config: Arc::new(config),
            settings_revision: Arc::new(AtomicU64::new(0)),
            media_session_revision: Arc::new(AtomicU64::new(0)),
            audio: None,
            stream_enabled: Arc::new(AtomicBool::new(false)),
            settings: Arc::new(StdMutex::new(settings)),
            viewer_metrics: Arc::new(StdMutex::new(HashMap::new())),
            groups,
            shared_capture: None,
            connections: Arc::new(Mutex::new(std::collections::HashMap::new())),
            udp_mux: mux,
            pending_connections: Arc::new(StdMutex::new(HashSet::new())),
            connected_connections: Arc::new(StdMutex::new(HashSet::new())),
            client_connections: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            connection_bindings: Arc::new(StdMutex::new(HashMap::new())),
            client_sockets: Arc::new(StdMutex::new(std::collections::HashMap::new())),
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ui_bootstrap_can_switch_to_an_explicit_test_pattern() {
        let mut bootstrap = AppConfig::default();
        bootstrap.bind = "127.0.0.1".to_owned();
        bootstrap.http_port = 0;
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
        let (result_tx, result_rx) = oneshot::channel();
        control_tx
            .send(ServerCommand::StartStream {
                settings: selected,
                result: Some(result_tx),
            })
            .unwrap();

        let start_result = tokio::time::timeout(Duration::from_secs(15), result_rx)
            .await
            .expect("UI-style stream start timed out")
            .expect("server dropped the UI stream-start result");
        assert!(start_result.is_ok(), "{start_result:?}");

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
}
