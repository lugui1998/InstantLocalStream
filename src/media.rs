use std::collections::{HashMap, VecDeque};
use std::io::{BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Condvar, Mutex, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use rtc::media::{
    Sample,
    io::{h26x_reader::H26xSampleReader, ivf_reader::IVFReader},
};
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::{
    MIME_TYPE_H264, MIME_TYPE_VP8, MIME_TYPE_VP9,
};
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use tokio::sync::Notify;
use uuid::Uuid;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;

use crate::{config::AppConfig, shared_capture::SourcePixelFormat};

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

const VP8_PAYLOAD_TYPE: u8 = 96;
const VP9_PAYLOAD_TYPE: u8 = 98;
// Chrome advertises constrained-baseline packetization-mode=1 as PT 108.
// Keeping this fixed makes the static sample writer match the answer SDP.
const H264_PAYLOAD_TYPE: u8 = 108;
// The shared encoder must fit the level browsers actually offer to receive.
// Mainstream WebRTC offers commonly use constrained-baseline level 3.1; higher
// output profiles are rejected instead of negotiating 3.1 and emitting a 4.2
// bitstream that the receiver never agreed to decode.
const H264_LEVEL: &str = "3.1";
const H264_SDP_FMTP_LINE: &str =
    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";
const H264_LEVEL_31_MAX_FRAME_SIZE_MACROBLOCKS: u64 = 3_600;
const H264_LEVEL_31_MAX_MACROBLOCKS_PER_SECOND: u64 = 108_000;
const H264_LEVEL_31_MAX_BITRATE_BPS: u32 = 14_000_000;
const MAX_ENCODED_FRAME_AGE: Duration = Duration::from_millis(250);
const ENCODER_BACKLOG_RESTART_AGE: Duration = Duration::from_millis(750);
const ENCODER_BACKLOG_RESTART_FRAMES: u32 = 8;
const ENCODER_TIMING_QUEUE_LIMIT: usize = 120;
const FRAME_TIMING_RETENTION: Duration = Duration::from_secs(65);
const FRAME_TIMING_ENTRY_LIMIT: usize = 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    Vp8,
    Vp9,
    H264,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSettings {
    pub source_kind: String,
    pub source_index: usize,
    pub source_native_id: Option<u64>,
    pub draw_mouse: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub output_height: Option<u32>,
    pub output_fps: Option<u32>,
    pub bitrate: u32,
    pub quality_mode: String,
    pub bitrate_mode: String,
    pub adaptive_quality_ceiling: String,
    pub adaptive_fps_ceiling: String,
    pub max_quality_groups: String,
    pub latency_preference: String,
    pub audio_mode: String,
    pub excluded_audio_processes: Vec<String>,
}

impl CaptureSettings {
    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            source_kind: config.source.kind.clone(),
            source_index: config.source.index,
            source_native_id: config.source.native_id,
            draw_mouse: config.draw_mouse,
            width: config.width,
            height: config.height,
            fps: config.fps,
            output_height: config.output_height(),
            output_fps: config.output_fps(),
            bitrate: config.effective_bitrate(),
            quality_mode: config.quality_mode.clone(),
            bitrate_mode: config.bitrate_mode.clone(),
            adaptive_quality_ceiling: config.adaptive_quality_ceiling.clone(),
            adaptive_fps_ceiling: config.adaptive_fps_ceiling.clone(),
            max_quality_groups: config.max_quality_groups.clone(),
            latency_preference: config.latency_preference.clone(),
            audio_mode: config.audio_mode.clone(),
            excluded_audio_processes: config.excluded_audio_processes.clone(),
        }
    }

    pub fn test_pattern_dimensions(&self) -> (u32, u32) {
        if let Some(height) = self.output_height {
            let source_width = self.width.max(2);
            let source_height = self.height.max(2);
            let width = ((source_width as u64 * height as u64) / source_height as u64) as u32;
            let width = width.max(2) & !1;
            (width, height)
        } else {
            (self.width, self.height)
        }
    }
}

impl VideoCodec {
    pub fn parse(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "vp8" => Ok(Self::Vp8),
            "vp9" => Ok(Self::Vp9),
            "h264" => Ok(Self::H264),
            other => anyhow::bail!("unsupported codec '{other}'"),
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Vp8 => "VP8",
            Self::Vp9 => "VP9",
            Self::H264 => "H.264",
        }
    }

    #[cfg(test)]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Vp8 => "vp8",
            Self::Vp9 => "vp9",
            Self::H264 => "h264",
        }
    }

    pub const fn mime_type(self) -> &'static str {
        match self {
            Self::Vp8 => MIME_TYPE_VP8,
            Self::Vp9 => MIME_TYPE_VP9,
            Self::H264 => MIME_TYPE_H264,
        }
    }

    pub const fn payload_type(self) -> u8 {
        match self {
            Self::Vp8 => VP8_PAYLOAD_TYPE,
            Self::Vp9 => VP9_PAYLOAD_TYPE,
            Self::H264 => H264_PAYLOAD_TYPE,
        }
    }

    pub const fn sdp_fmtp_line(self) -> &'static str {
        match self {
            Self::Vp8 => "",
            Self::Vp9 => "profile-id=0",
            Self::H264 => H264_SDP_FMTP_LINE,
        }
    }
}

#[derive(Clone)]
pub struct MediaTrack {
    connection_id: Uuid,
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: u8,
    preserve_nal_order: bool,
    queue: Arc<SubscriberQueue>,
    frame_capture_times: Arc<Mutex<HashMap<Uuid, VecDeque<FrameCaptureMapping>>>>,
}

#[derive(Clone, Copy)]
struct FrameCaptureMapping {
    rtp_timestamp: u32,
    capture_time_unix_nanos: u64,
    encoder_delay_ms: u64,
    recorded_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameCaptureTiming {
    pub capture_time_unix_nanos: u64,
    pub encoder_delay_ms: u64,
}

struct QueuedSample {
    data: Bytes,
    duration: Duration,
    capture_time_unix_nanos: Option<u64>,
    encoder_delay_ms: Option<u64>,
}

struct SubscriberQueue {
    samples: Mutex<VecDeque<QueuedSample>>,
    notify: Notify,
    closed: AtomicBool,
}

#[derive(Clone)]
struct EncoderTiming {
    submitted_frames: Arc<Mutex<VecDeque<SubmittedFrameTiming>>>,
    encoder_delay_ms: Arc<AtomicU64>,
    stale_encoded_frames: Arc<AtomicU64>,
}

#[derive(Clone, Copy)]
struct SubmittedFrameTiming {
    captured_at: Instant,
    capture_time_unix_nanos: u64,
}

#[derive(Clone, Copy, Default)]
struct EncodedFrameTiming {
    stale: bool,
    age: Option<Duration>,
    capture_time_unix_nanos: Option<u64>,
    encoder_delay_ms: Option<u64>,
}

impl EncoderTiming {
    fn record_input(&self, captured_at: Instant, captured_at_unix_nanos: u64) {
        if let Ok(mut frames) = self.submitted_frames.lock() {
            while frames.len() >= ENCODER_TIMING_QUEUE_LIMIT {
                frames.pop_front();
            }
            frames.push_back(SubmittedFrameTiming {
                captured_at,
                capture_time_unix_nanos: captured_at_unix_nanos,
            });
        }
    }

    fn discard_input(&self, captured_at_unix_nanos: u64) {
        if let Ok(mut frames) = self.submitted_frames.lock()
            && let Some(index) = frames
                .iter()
                .rposition(|frame| frame.capture_time_unix_nanos == captured_at_unix_nanos)
        {
            frames.remove(index);
        }
    }

    /// Associates an encoded access unit with its source capture timestamp and
    /// reports whether it is already too old to send.
    fn encoded_frame_timing(&self) -> EncodedFrameTiming {
        let submitted = self
            .submitted_frames
            .lock()
            .ok()
            .and_then(|mut frames| frames.pop_front());
        let Some(submitted) = submitted else {
            return EncodedFrameTiming::default();
        };
        let age = submitted.captured_at.elapsed();
        let encoder_delay_ms = age.as_millis().min(u64::MAX as u128) as u64;
        self.encoder_delay_ms
            .store(encoder_delay_ms, Ordering::Release);
        if age > MAX_ENCODED_FRAME_AGE {
            self.stale_encoded_frames.fetch_add(1, Ordering::AcqRel);
        }
        EncodedFrameTiming {
            stale: age > MAX_ENCODED_FRAME_AGE,
            age: Some(age),
            capture_time_unix_nanos: Some(submitted.capture_time_unix_nanos),
            encoder_delay_ms: Some(encoder_delay_ms),
        }
    }
}

impl MediaTrack {
    pub fn track(&self) -> Arc<TrackLocalStaticSample> {
        Arc::clone(&self.track)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vp8_encoder_forces_opaque_yuv420p_output() {
        let pipeline = MediaPipeline::with_codec("vp8").unwrap();
        let args = pipeline.video_encode_args(None, None, "1_000_000".to_owned());
        assert!(args.windows(2).any(|pair| pair == ["-pix_fmt", "yuv420p"]));
    }

    #[test]
    fn vp8_encoder_disables_lookahead_and_flushes_low_latency_output() {
        let pipeline = MediaPipeline::with_codec("vp8").unwrap();
        let args = pipeline.video_encode_args(None, Some("30".to_owned()), "1_000_000".to_owned());
        assert!(args.windows(2).any(|pair| pair == ["-lag-in-frames", "0"]));
        assert!(args.windows(2).any(|pair| pair == ["-auto-alt-ref", "0"]));
        assert!(args.windows(2).any(|pair| pair == ["-flush_packets", "1"]));
    }

    #[test]
    fn encoder_readiness_reports_first_frame_and_startup_failure() {
        let ready = MediaPipeline::with_codec("vp8").unwrap();
        ready.mark_ready();
        ready.wait_until_ready(Duration::from_millis(1)).unwrap();

        let failed = MediaPipeline::with_codec("vp8").unwrap();
        failed.mark_failed("test failure");
        let error = failed
            .wait_until_ready(Duration::from_millis(1))
            .unwrap_err();
        assert!(error.to_string().contains("test failure"));
    }

    #[test]
    fn per_viewer_rtp_timestamp_resolves_the_source_capture_time() {
        let pipeline = MediaPipeline::with_codec("vp8").unwrap();
        let connection_id = Uuid::new_v4();
        pipeline.frame_capture_times.lock().unwrap().insert(
            connection_id,
            VecDeque::from([FrameCaptureMapping {
                rtp_timestamp: 123,
                capture_time_unix_nanos: 456_000_000,
                encoder_delay_ms: 17,
                recorded_at: Instant::now(),
            }]),
        );

        assert_eq!(
            pipeline.frame_capture_timing(connection_id, 123),
            Some(FrameCaptureTiming {
                capture_time_unix_nanos: 456_000_000,
                encoder_delay_ms: 17,
            })
        );
        assert_eq!(pipeline.frame_capture_timing(connection_id, 124), None);
    }

    #[test]
    fn encoder_timing_is_recorded_before_output_and_can_be_rolled_back() {
        let timing = EncoderTiming {
            submitted_frames: Arc::new(Mutex::new(VecDeque::new())),
            encoder_delay_ms: Arc::new(AtomicU64::new(0)),
            stale_encoded_frames: Arc::new(AtomicU64::new(0)),
        };
        timing.record_input(Instant::now(), 123);
        assert_eq!(
            timing.encoded_frame_timing().capture_time_unix_nanos,
            Some(123)
        );

        timing.record_input(Instant::now(), 456);
        timing.discard_input(456);
        assert_eq!(timing.encoded_frame_timing().capture_time_unix_nanos, None);
    }

    #[test]
    fn sustained_encoder_backlog_triggers_a_live_edge_restart() {
        let pipeline = MediaPipeline::with_codec("vp8").unwrap();
        let timing = EncodedFrameTiming {
            stale: true,
            age: Some(ENCODER_BACKLOG_RESTART_AGE),
            capture_time_unix_nanos: Some(1),
            encoder_delay_ms: Some(ENCODER_BACKLOG_RESTART_AGE.as_millis() as u64),
        };
        let mut consecutive = 0;
        for _ in 1..ENCODER_BACKLOG_RESTART_FRAMES {
            assert!(!pipeline.encoder_backlog_requires_restart(&timing, &mut consecutive));
        }
        assert!(pipeline.encoder_backlog_requires_restart(&timing, &mut consecutive));
        assert_eq!(pipeline.encoder_backlog_restarts(), 1);
        assert_eq!(pipeline.capture_revision.load(Ordering::Acquire), 1);
    }

    #[test]
    fn explicit_rtp_timestamp_supports_zero_wraparound_and_multi_packet_frames() {
        let mut packets = vec![rtc::rtp::Packet::default(), rtc::rtp::Packet::default()];
        packets[0].header.timestamp = 10;
        packets[1].header.timestamp = 11;
        webrtc::media_stream::track_local::static_sample::apply_rtp_timestamp_override(
            &mut packets,
            None,
        );
        assert_eq!(packets[0].header.timestamp, 10);
        assert_eq!(packets[1].header.timestamp, 11);

        webrtc::media_stream::track_local::static_sample::apply_rtp_timestamp_override(
            &mut packets,
            Some(0),
        );
        assert!(packets.iter().all(|packet| packet.header.timestamp == 0));

        webrtc::media_stream::track_local::static_sample::apply_rtp_timestamp_override(
            &mut packets,
            Some(u32::MAX),
        );
        assert!(
            packets
                .iter()
                .all(|packet| packet.header.timestamp == u32::MAX)
        );
    }

    #[test]
    fn stale_encoder_frame_threshold_is_sub_second() {
        assert!(MAX_ENCODED_FRAME_AGE <= Duration::from_millis(250));
    }

    #[test]
    fn h264_encoder_uses_single_slice_frames() {
        let pipeline = MediaPipeline::with_codec("h264").unwrap();
        let args = pipeline.video_encode_args(None, None, "1_000_000".to_owned());
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-x264-params", "repeat-headers=1:sliced-threads=0"])
        );
    }

    #[test]
    fn h264_encoder_and_sdp_advertise_the_same_conservative_level() {
        let pipeline = MediaPipeline::with_codec("h264").unwrap();
        let low =
            pipeline.video_encode_args(Some(720), Some("30".to_owned()), "1_000_000".to_owned());
        let high =
            pipeline.video_encode_args(Some(1080), Some("60".to_owned()), "1_000_000".to_owned());
        assert!(low.windows(2).any(|pair| pair == ["-level:v", H264_LEVEL]));
        assert!(high.windows(2).any(|pair| pair == ["-level:v", H264_LEVEL]));
        assert!(pipeline.sdp_fmtp_line().contains("profile-level-id=42e01f"));
    }

    #[test]
    fn h264_level_31_limits_cover_720p30_but_not_larger_profiles() {
        assert!(h264_level_31_compatible(1280, 720, 30));
        assert!(!h264_level_31_compatible(1280, 720, 60));
        assert!(!h264_level_31_compatible(1920, 1080, 30));
        assert!(h264_level_31_bitrate_compatible(14_000_000));
        assert!(!h264_level_31_bitrate_compatible(14_000_001));
    }

    #[test]
    fn output_fps_never_upsamples_capture_frames() {
        let settings = CaptureSettings::from_config(&AppConfig::default());
        assert_eq!(configured_output_fps(60, &settings).unwrap(), 60);

        let mut faster_output = settings;
        faster_output.output_fps = Some(60);
        assert!(configured_output_fps(30, &faster_output).is_err());
    }

    #[test]
    fn yuv420p_frame_size_is_one_and_a_half_bytes_per_pixel() {
        assert_eq!(yuv420p_frame_size(4, 2).unwrap(), 12);
    }

    #[test]
    fn h264_filler_nals_are_not_queued_as_video_frames() {
        assert_eq!(
            h264_nal_kind(&Bytes::from_static(&[0x65, 0x88])),
            H264NalKind::Picture
        );
        assert_eq!(
            h264_nal_kind(&Bytes::from_static(&[0x67, 0x42])),
            H264NalKind::ParameterOrPrefix
        );
        assert_eq!(
            h264_nal_kind(&Bytes::from_static(&[0x0c, 0x00])),
            H264NalKind::Discard
        );
    }

    #[test]
    fn test_pattern_preserves_a_custom_source_aspect_ratio() {
        let mut settings = CaptureSettings::from_config(&AppConfig::default());
        settings.width = 3440;
        settings.height = 1440;
        settings.output_height = Some(720);
        assert_eq!(settings.test_pattern_dimensions(), (1720, 720));
    }
}

pub struct MediaPipeline {
    subscribers: Arc<Mutex<HashMap<Uuid, MediaTrack>>>,
    frame_capture_times: Arc<Mutex<HashMap<Uuid, VecDeque<FrameCaptureMapping>>>>,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    ended: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    capture_settings: Arc<Mutex<Option<CaptureSettings>>>,
    capture_revision: Arc<std::sync::atomic::AtomicU64>,
    encoder_delay_ms: Arc<AtomicU64>,
    stale_encoded_frames: Arc<AtomicU64>,
    encoder_backlog_restarts: Arc<AtomicU64>,
    readiness: Arc<(Mutex<EncoderReadiness>, Condvar)>,
    encoder_children: Arc<Mutex<Vec<Weak<Mutex<Child>>>>>,
    codec: VideoCodec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EncoderReadiness {
    Pending,
    Ready,
    Failed(String),
}

impl MediaPipeline {
    pub fn with_codec(codec: &str) -> Result<Self> {
        Ok(Self {
            subscribers: Arc::new(Mutex::new(HashMap::new())),
            frame_capture_times: Arc::new(Mutex::new(HashMap::new())),
            stop: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicBool::new(false)),
            ended: Arc::new(AtomicBool::new(false)),
            failure: Arc::new(Mutex::new(None)),
            capture_settings: Arc::new(Mutex::new(None)),
            capture_revision: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            encoder_delay_ms: Arc::new(AtomicU64::new(0)),
            stale_encoded_frames: Arc::new(AtomicU64::new(0)),
            encoder_backlog_restarts: Arc::new(AtomicU64::new(0)),
            readiness: Arc::new((Mutex::new(EncoderReadiness::Pending), Condvar::new())),
            encoder_children: Arc::new(Mutex::new(Vec::new())),
            codec: VideoCodec::parse(codec)?,
        })
    }

    pub fn subscribe(&self, connection_id: Uuid) -> Result<MediaTrack> {
        let ssrc = rand::random::<u32>();
        let track = TrackLocalStaticSample::new(MediaStreamTrack::new(
            "instant-local-stream".to_owned(),
            format!("screen-video-{connection_id}"),
            "InstantLocalStream screen".to_owned(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: RTCRtpCodec {
                    mime_type: self.codec.mime_type().to_owned(),
                    clock_rate: 90_000,
                    channels: 0,
                    sdp_fmtp_line: self.codec.sdp_fmtp_line().to_owned(),
                    rtcp_feedback: vec![],
                },
                ..Default::default()
            }],
        ))?;
        let media_track = MediaTrack {
            connection_id,
            track: Arc::new(track),
            ssrc,
            payload_type: self.codec.payload_type(),
            preserve_nal_order: matches!(self.codec, VideoCodec::H264),
            queue: Arc::new(SubscriberQueue {
                samples: Mutex::new(VecDeque::with_capacity(1)),
                notify: Notify::new(),
                closed: AtomicBool::new(false),
            }),
            frame_capture_times: Arc::clone(&self.frame_capture_times),
        };
        let writer_track = media_track.clone();
        tokio::spawn(async move {
            writer_track.run_writer().await;
        });
        self.subscribers
            .lock()
            .map_err(|_| anyhow::anyhow!("media subscriber lock poisoned"))?
            .insert(connection_id, media_track.clone());
        if let Ok(mut timings) = self.frame_capture_times.lock() {
            timings.insert(connection_id, VecDeque::with_capacity(120));
        }
        Ok(media_track)
    }

    pub fn unsubscribe(&self, connection_id: Uuid) {
        if let Ok(mut subscribers) = self.subscribers.lock()
            && let Some(track) = subscribers.remove(&connection_id)
        {
            track.close();
        }
        if let Ok(mut timings) = self.frame_capture_times.lock() {
            timings.remove(&connection_id);
        }
    }

    pub fn frame_capture_timing(
        &self,
        connection_id: Uuid,
        rtp_timestamp: u32,
    ) -> Option<FrameCaptureTiming> {
        self.frame_capture_times
            .lock()
            .ok()
            .and_then(|timings| timings.get(&connection_id).cloned())
            .and_then(|timings| {
                timings
                    .iter()
                    .rev()
                    .find(|timing| timing.rtp_timestamp == rtp_timestamp)
                    .map(|timing| FrameCaptureTiming {
                        capture_time_unix_nanos: timing.capture_time_unix_nanos,
                        encoder_delay_ms: timing.encoder_delay_ms,
                    })
            })
    }
    pub fn codec_name(&self) -> &'static str {
        self.codec.name()
    }
    #[cfg(test)]
    pub fn codec_id(&self) -> &'static str {
        self.codec.id()
    }
    pub fn mime_type(&self) -> &'static str {
        self.codec.mime_type()
    }
    pub fn sdp_fmtp_line(&self) -> &'static str {
        self.codec.sdp_fmtp_line()
    }
    pub fn payload_type(&self) -> u8 {
        self.codec.payload_type()
    }
    pub fn stop(&self) {
        self.request_stop();
        if let Ok(subscribers) = self.subscribers.lock() {
            for track in subscribers.values() {
                track.close();
            }
        }
    }

    /// Signals encoder threads and their FFmpeg killer without taking any
    /// subscriber locks. Used by the urgent Stop path before bounded cleanup.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(mut children) = self.encoder_children.lock() {
            children.retain(|child| {
                let Some(child) = child.upgrade() else {
                    return false;
                };
                if let Ok(mut child) = child.try_lock() {
                    let _ = child.kill();
                }
                true
            });
        }
    }
    pub fn activate(&self) {
        self.active.store(true, Ordering::Release);
    }

    pub fn reconfigure(&self, settings: CaptureSettings) {
        if let Ok(mut current) = self.capture_settings.lock() {
            if current.as_ref() == Some(&settings) {
                return;
            }
            *current = Some(settings);
            self.capture_revision.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub fn status(&self) -> &'static str {
        if self
            .failure
            .lock()
            .map(|failure| failure.is_some())
            .unwrap_or(true)
        {
            "error"
        } else if self.ended.load(Ordering::Acquire) {
            "stopped"
        } else if self.active.load(Ordering::Acquire) {
            "streaming"
        } else {
            "waiting"
        }
    }

    pub fn failure(&self) -> Option<String> {
        self.failure.lock().ok().and_then(|failure| failure.clone())
    }

    pub fn encoder_delay_ms(&self) -> Option<u64> {
        let delay = self.encoder_delay_ms.load(Ordering::Acquire);
        (delay > 0).then_some(delay)
    }

    pub fn stale_encoded_frames(&self) -> u64 {
        self.stale_encoded_frames.load(Ordering::Acquire)
    }

    pub fn encoder_backlog_restarts(&self) -> u64 {
        self.encoder_backlog_restarts.load(Ordering::Acquire)
    }

    pub fn wait_until_ready(&self, timeout: Duration) -> Result<()> {
        let (state, changed) = &*self.readiness;
        let state = state
            .lock()
            .map_err(|_| anyhow::anyhow!("encoder readiness lock poisoned"))?;
        let (state, _) = changed
            .wait_timeout_while(state, timeout, |state| {
                matches!(state, EncoderReadiness::Pending)
            })
            .map_err(|_| anyhow::anyhow!("encoder readiness lock poisoned"))?;
        match &*state {
            EncoderReadiness::Ready => Ok(()),
            EncoderReadiness::Failed(error) => anyhow::bail!("encoder startup failed: {error}"),
            EncoderReadiness::Pending => {
                anyhow::bail!("encoder did not produce its first frame within {timeout:?}")
            }
        }
    }

    fn mark_ready(&self) {
        let (state, changed) = &*self.readiness;
        if let Ok(mut state) = state.lock()
            && matches!(*state, EncoderReadiness::Pending)
        {
            *state = EncoderReadiness::Ready;
            changed.notify_all();
        }
    }

    fn mark_failed(&self, error: &str) {
        let (state, changed) = &*self.readiness;
        if let Ok(mut state) = state.lock()
            && matches!(*state, EncoderReadiness::Pending)
        {
            *state = EncoderReadiness::Failed(error.to_owned());
            changed.notify_all();
        }
    }

    fn record_result(&self, result: Result<()>) {
        if let Err(error) = result {
            self.mark_failed(&error.to_string());
            if let Ok(mut failure) = self.failure.lock() {
                *failure = Some(error.to_string());
            }
            tracing::error!(%error, "media pipeline stopped");
        }
        self.ended.store(true, Ordering::Release);
    }

    /// Starts an encoder that consumes the newest frames from a shared raw-video source.
    pub fn spawn_from_shared_source(
        self: Arc<Self>,
        ffmpeg: String,
        source: crate::shared_capture::SourceSubscription,
        source_dimensions: (u32, u32),
        source_pixel_format: SourcePixelFormat,
        source_fps: u32,
        settings: CaptureSettings,
    ) -> tokio::task::JoinHandle<()> {
        self.reconfigure(settings);
        tokio::task::spawn_blocking(move || {
            let result = self.run_shared_source_loop(
                ffmpeg,
                source,
                source_dimensions,
                source_pixel_format,
                source_fps,
            );
            self.record_result(result);
        })
    }

    fn run_shared_source_loop(
        &self,
        ffmpeg: String,
        mut source: crate::shared_capture::SourceSubscription,
        source_dimensions: (u32, u32),
        source_pixel_format: SourcePixelFormat,
        source_fps: u32,
    ) -> Result<()> {
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            let revision = self.capture_revision.load(Ordering::Acquire);
            let settings = self
                .capture_settings
                .lock()
                .map_err(|_| anyhow::anyhow!("capture settings lock poisoned"))?
                .clone()
                .context("capture settings were not initialized")?;
            let result = self.run_shared_source_encoder(
                &ffmpeg,
                &mut source,
                source_dimensions,
                source_pixel_format,
                source_fps,
                &settings,
                revision,
            );
            if self.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            if self.capture_revision.load(Ordering::Acquire) != revision {
                continue;
            }
            return result;
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "The encoder needs the source stream metadata, active settings, and revision independently to restart safely."
    )]
    fn run_shared_source_encoder(
        &self,
        ffmpeg: &str,
        source: &mut crate::shared_capture::SourceSubscription,
        source_dimensions: (u32, u32),
        source_pixel_format: SourcePixelFormat,
        source_fps: u32,
        settings: &CaptureSettings,
        revision: u64,
    ) -> Result<()> {
        let (width, height) = source_dimensions;
        let frame_size = source_pixel_format_frame_size(source_pixel_format, width, height)?;
        let output_fps = configured_output_fps(source_fps, settings)?;
        if matches!(self.codec, VideoCodec::H264) {
            let (output_width, output_height) =
                output_dimensions(width, height, settings.output_height)?;
            if !h264_level_31_compatible(output_width, output_height, output_fps) {
                anyhow::bail!(
                    "requested H.264 output {output_width}x{output_height} at {output_fps} FPS exceeds constrained-baseline level {H264_LEVEL}"
                );
            }
            if !h264_level_31_bitrate_compatible(settings.bitrate) {
                anyhow::bail!(
                    "requested H.264 bitrate {} exceeds constrained-baseline level {H264_LEVEL}'s {} bps limit",
                    settings.bitrate,
                    H264_LEVEL_31_MAX_BITRATE_BPS
                );
            }
        }
        let mut args = vec![
            "-hide_banner".to_owned(),
            "-loglevel".to_owned(),
            "error".to_owned(),
            // `nobuffer` makes FFmpeg's rawvideo demuxer hold back the first
            // packet until a second frame arrives. Windows Graphics Capture
            // is event-driven and may publish only one frame for a completely
            // static window, so that behavior can deadlock encoder startup.
            // A raw pipe has no network/demux jitter to buffer here anyway.
            "-f".to_owned(),
            "rawvideo".to_owned(),
            "-pix_fmt".to_owned(),
            source_pixel_format.ffmpeg_name().to_owned(),
            "-video_size".to_owned(),
            format!("{width}x{height}"),
            "-framerate".to_owned(),
            output_fps.to_string(),
            "-i".to_owned(),
            "pipe:0".to_owned(),
            "-an".to_owned(),
        ];
        args.extend(self.video_encode_args(
            settings.output_height,
            Some(output_fps.to_string()),
            settings.bitrate.to_string(),
        ));
        let mut command = Command::new(ffmpeg);
        hide_console(&mut command);
        let mut child = command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("start FFmpeg from '{ffmpeg}'"))?;
        let stdin = child.stdin.take().context("FFmpeg did not expose stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("FFmpeg did not expose stdout")?;
        let child_control = Arc::new(Mutex::new(child));
        if let Ok(mut children) = self.encoder_children.lock() {
            children.retain(|child| child.strong_count() > 0);
            children.push(Arc::downgrade(&child_control));
        }
        let (killer, done) = self.spawn_child_killer(Arc::clone(&child_control), revision);
        let stop = Arc::clone(&self.stop);
        let capture_revision = Arc::clone(&self.capture_revision);
        self.encoder_delay_ms.store(0, Ordering::Release);
        self.stale_encoded_frames.store(0, Ordering::Release);
        let timing = EncoderTiming {
            submitted_frames: Arc::new(Mutex::new(VecDeque::new())),
            encoder_delay_ms: Arc::clone(&self.encoder_delay_ms),
            stale_encoded_frames: Arc::clone(&self.stale_encoded_frames),
        };
        thread::scope(|scope| {
            let writer_timing = timing.clone();
            let writer = scope.spawn(move || -> Result<()> {
                let mut stdin = stdin;
                let input_interval = Duration::from_secs_f64(1.0 / output_fps as f64);
                let mut next_input_at = Instant::now();
                while !stop.load(Ordering::Acquire)
                    && capture_revision.load(Ordering::Acquire) == revision
                {
                    let Some(frame) = source.recv()? else {
                        return Ok(());
                    };
                    let now = Instant::now();
                    if now < next_input_at {
                        continue;
                    }
                    if frame.width != width || frame.height != height {
                        anyhow::bail!(
                            "shared source frame dimensions changed from {width}x{height} to {}x{}",
                            frame.width,
                            frame.height
                        );
                    }
                    if frame.pixel_format != source_pixel_format {
                        anyhow::bail!(
                            "shared source pixel format changed from {} to {}",
                            source_pixel_format.ffmpeg_name(),
                            frame.pixel_format.ffmpeg_name(),
                        );
                    }
                    if frame.data.len() != frame_size {
                        anyhow::bail!(
                            "shared source frame has {} bytes; expected {frame_size} for {width}x{height} {}",
                            frame.data.len(),
                            source_pixel_format.ffmpeg_name(),
                        );
                    }
                    writer_timing.record_input(Instant::now(), frame.captured_at_unix_nanos);
                    if let Err(error) = stdin.write_all(&frame.data) {
                        writer_timing.discard_input(frame.captured_at_unix_nanos);
                        return Err(error.into());
                    }
                    next_input_at = now + input_interval;
                }
                Ok(())
            });
            let stream_result = self.stream_encoded(stdout, Some(output_fps), &timing);
            if let Ok(mut child) = child_control.lock() {
                let _ = child.kill();
                let _ = child.wait();
            }
            done.store(true, Ordering::Release);
            let _ = killer.join();
            let writer_result = writer
                .join()
                .map_err(|_| anyhow::anyhow!("shared source writer thread panicked"))?;
            stream_result.and(writer_result)
        })
    }

    fn spawn_child_killer(
        &self,
        child: Arc<Mutex<Child>>,
        revision: u64,
    ) -> (thread::JoinHandle<()>, Arc<AtomicBool>) {
        let stop = Arc::clone(&self.stop);
        let capture_revision = Arc::clone(&self.capture_revision);
        let done = Arc::new(AtomicBool::new(false));
        let done_thread = Arc::clone(&done);
        let handle = thread::spawn(move || {
            while !stop.load(Ordering::Acquire)
                && !done_thread.load(Ordering::Acquire)
                && (revision == 0 || capture_revision.load(Ordering::Acquire) == revision)
            {
                thread::sleep(Duration::from_millis(25));
            }
            if !done_thread.load(Ordering::Acquire)
                && let Ok(mut child) = child.lock()
            {
                let _ = child.kill();
            }
        });
        (handle, done)
    }

    fn video_encode_args(
        &self,
        output_height: Option<u32>,
        group: Option<String>,
        bitrate: String,
    ) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(height) = output_height {
            args.extend(["-vf".to_owned(), format!("scale=-2:{height}")]);
        }
        let mut keyframe_args = Vec::new();
        if let Some(group) = group {
            keyframe_args.extend(["-g".to_owned(), group]);
        }
        match self.codec {
            VideoCodec::Vp8 => args.extend([
                "-c:v".to_owned(),
                "libvpx".to_owned(),
                "-deadline".to_owned(),
                "realtime".to_owned(),
                "-cpu-used".to_owned(),
                "8".to_owned(),
                "-lag-in-frames".to_owned(),
                "0".to_owned(),
                "-auto-alt-ref".to_owned(),
                "0".to_owned(),
                "-error-resilient".to_owned(),
                "1".to_owned(),
                "-flags".to_owned(),
                "low_delay".to_owned(),
                "-max_delay".to_owned(),
                "0".to_owned(),
                "-b:v".to_owned(),
                bitrate.clone(),
                "-pix_fmt".to_owned(),
                "yuv420p".to_owned(),
                "-f".to_owned(),
                "ivf".to_owned(),
                "-flush_packets".to_owned(),
                "1".to_owned(),
                "pipe:1".to_owned(),
            ]),
            VideoCodec::Vp9 => args.extend([
                "-c:v".to_owned(),
                "libvpx-vp9".to_owned(),
                "-deadline".to_owned(),
                "realtime".to_owned(),
                "-cpu-used".to_owned(),
                "8".to_owned(),
                "-row-mt".to_owned(),
                "1".to_owned(),
                "-lag-in-frames".to_owned(),
                "0".to_owned(),
                "-auto-alt-ref".to_owned(),
                "0".to_owned(),
                "-error-resilient".to_owned(),
                "1".to_owned(),
                "-flags".to_owned(),
                "low_delay".to_owned(),
                "-max_delay".to_owned(),
                "0".to_owned(),
                "-b:v".to_owned(),
                bitrate.clone(),
                "-f".to_owned(),
                "ivf".to_owned(),
                "-flush_packets".to_owned(),
                "1".to_owned(),
                "pipe:1".to_owned(),
            ]),
            VideoCodec::H264 => args.extend([
                "-c:v".to_owned(),
                "libx264".to_owned(),
                "-preset".to_owned(),
                "ultrafast".to_owned(),
                "-tune".to_owned(),
                "zerolatency".to_owned(),
                "-profile:v".to_owned(),
                "baseline".to_owned(),
                "-level:v".to_owned(),
                H264_LEVEL.to_owned(),
                "-pix_fmt".to_owned(),
                "yuv420p".to_owned(),
                "-keyint_min".to_owned(),
                "1".to_owned(),
                "-sc_threshold".to_owned(),
                "0".to_owned(),
                "-x264-params".to_owned(),
                "repeat-headers=1:sliced-threads=0".to_owned(),
                "-flags".to_owned(),
                "low_delay".to_owned(),
                "-max_delay".to_owned(),
                "0".to_owned(),
                "-b:v".to_owned(),
                bitrate,
                "-f".to_owned(),
                "h264".to_owned(),
                "-flush_packets".to_owned(),
                "1".to_owned(),
                "pipe:1".to_owned(),
            ]),
        }
        args.splice(0..0, keyframe_args);
        args
    }

    fn stream_encoded(
        &self,
        stdout: impl std::io::Read,
        fps: Option<u32>,
        timing: &EncoderTiming,
    ) -> Result<()> {
        match self.codec {
            VideoCodec::Vp8 | VideoCodec::Vp9 => self.stream_ivf(stdout, timing),
            VideoCodec::H264 => self.stream_h264(stdout, fps.unwrap_or(60), timing),
        }
    }

    fn stream_ivf(&self, stdout: impl std::io::Read, timing: &EncoderTiming) -> Result<()> {
        let (mut reader, header) = IVFReader::new(BufReader::new(stdout))?;
        let frame_duration = Duration::from_secs_f64(
            header.timebase_numerator as f64 / header.timebase_denominator.max(1) as f64,
        )
        .max(Duration::from_millis(1));
        let mut pacer = FramePacer::new(frame_duration);
        let mut backlog_frames = 0_u32;
        while !self.stop.load(Ordering::Acquire) {
            let (frame, _) = match reader.parse_next_frame() {
                Ok(frame) => frame,
                Err(_error) if self.stop.load(Ordering::Acquire) => break,
                Err(error) => return Err(anyhow::anyhow!("FFmpeg IVF stream ended: {error}")),
            };
            let frame_timing = timing.encoded_frame_timing();
            self.mark_ready();
            if self.encoder_backlog_requires_restart(&frame_timing, &mut backlog_frames) {
                return Ok(());
            }
            if !self.active.load(Ordering::Acquire) || frame_timing.stale {
                // Never pace stale output: drain buffered encoder frames as
                // fast as possible until a current source frame emerges.
                pacer.reset_to_now();
                continue;
            }
            self.write_frame(
                frame.freeze(),
                frame_duration,
                frame_timing.capture_time_unix_nanos,
                frame_timing.encoder_delay_ms,
            )?;
            pacer.wait_after_frame();
        }
        Ok(())
    }

    fn stream_h264(
        &self,
        stdout: impl std::io::Read,
        fps: u32,
        timing: &EncoderTiming,
    ) -> Result<()> {
        let mut reader = H26xSampleReader::new(BufReader::new(stdout), 1_048_576, false);
        let frame_duration = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
        let mut pacer = FramePacer::new(frame_duration);
        let mut timed_samples = 0_u64;
        let mut backlog_frames = 0_u32;
        while !self.stop.load(Ordering::Acquire) {
            let sample = match reader.next_sample() {
                Ok(sample) => sample,
                Err(_error) if self.stop.load(Ordering::Acquire) => break,
                Err(error) => {
                    tracing::error!(%error, "H.264 sample reader stopped");
                    return Err(anyhow::anyhow!("FFmpeg H.264 stream ended: {error}"));
                }
            };
            match h264_nal_kind(&sample.data) {
                H264NalKind::ParameterOrPrefix => {
                    if self.active.load(Ordering::Acquire) {
                        self.write_frame(sample.data, Duration::ZERO, None, None)?;
                    }
                    continue;
                }
                H264NalKind::Picture => {}
                H264NalKind::Discard => continue,
            }
            timed_samples += 1;
            self.mark_ready();
            if timed_samples <= 3 {
                tracing::info!(
                    timed_samples,
                    bytes = sample.data.len(),
                    "H.264 sample received"
                );
            } else if timed_samples.is_multiple_of(30) {
                tracing::debug!(
                    timed_samples,
                    bytes = sample.data.len(),
                    "H.264 encoder is producing access units"
                );
            }
            let frame_timing = timing.encoded_frame_timing();
            if self.encoder_backlog_requires_restart(&frame_timing, &mut backlog_frames) {
                return Ok(());
            }
            if !self.active.load(Ordering::Acquire) || frame_timing.stale {
                pacer.reset_to_now();
                continue;
            }
            self.write_frame(
                sample.data,
                frame_duration,
                frame_timing.capture_time_unix_nanos,
                frame_timing.encoder_delay_ms,
            )?;
            pacer.wait_after_frame();
        }
        Ok(())
    }

    fn write_frame(
        &self,
        data: Bytes,
        duration: Duration,
        capture_time_unix_nanos: Option<u64>,
        encoder_delay_ms: Option<u64>,
    ) -> Result<()> {
        let subscribers = self
            .subscribers
            .lock()
            .map_err(|_| anyhow::anyhow!("media subscriber lock poisoned"))?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for subscriber in subscribers {
            subscriber.enqueue(QueuedSample {
                data: data.clone(),
                duration,
                capture_time_unix_nanos,
                encoder_delay_ms,
            });
        }
        Ok(())
    }

    fn encoder_backlog_requires_restart(
        &self,
        timing: &EncodedFrameTiming,
        consecutive_backlog_frames: &mut u32,
    ) -> bool {
        if timing
            .age
            .is_some_and(|age| age >= ENCODER_BACKLOG_RESTART_AGE)
        {
            *consecutive_backlog_frames = consecutive_backlog_frames.saturating_add(1);
        } else {
            *consecutive_backlog_frames = 0;
        }
        if *consecutive_backlog_frames < ENCODER_BACKLOG_RESTART_FRAMES {
            return false;
        }
        self.encoder_backlog_restarts.fetch_add(1, Ordering::AcqRel);
        self.capture_revision.fetch_add(1, Ordering::AcqRel);
        tracing::warn!(
            backlog_ms = timing.age.map(|age| age.as_millis()).unwrap_or_default(),
            "encoder backlog remained stale; restarting this encoder at the live edge"
        );
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum H264NalKind {
    ParameterOrPrefix,
    Picture,
    Discard,
}

struct FramePacer {
    frame_duration: Duration,
    next_deadline: Instant,
}

impl FramePacer {
    fn new(frame_duration: Duration) -> Self {
        Self {
            frame_duration,
            next_deadline: Instant::now(),
        }
    }

    fn wait_after_frame(&mut self) {
        self.next_deadline += self.frame_duration;
        let now = Instant::now();
        if self.next_deadline > now {
            thread::sleep(self.next_deadline - now);
        } else if now.duration_since(self.next_deadline) > self.frame_duration {
            // A late encoder must not retain an ever-growing timing debt.
            self.next_deadline = now;
        }
    }

    fn reset_to_now(&mut self) {
        self.next_deadline = Instant::now();
    }
}

fn h264_nal_kind(nal: &Bytes) -> H264NalKind {
    match nal.first().map(|byte| byte & 0x1f) {
        Some(1..=5 | 19) => H264NalKind::Picture,
        Some(6..=9 | 13) => H264NalKind::ParameterOrPrefix,
        _ => H264NalKind::Discard,
    }
}

/// Returns the configured output rate only when each encoded access unit can
/// correspond to one submitted capture frame.  Allowing FFmpeg to upsample a
/// rawvideo pipe duplicates output access units, which makes the capture-time
/// FIFO attribute duplicates to later source frames.
fn configured_output_fps(source_fps: u32, settings: &CaptureSettings) -> Result<u32> {
    if source_fps == 0 {
        anyhow::bail!("source FPS must be greater than zero");
    }
    let output_fps = settings.output_fps.unwrap_or(source_fps).max(1);
    if output_fps > source_fps {
        anyhow::bail!(
            "output FPS ({output_fps}) cannot exceed source FPS ({source_fps}); frame duplication would invalidate capture timing"
        );
    }
    Ok(output_fps)
}

/// Mirrors `scale=-2:<height>` closely enough for conservative H.264 level
/// validation: preserve aspect ratio and round the calculated width up to an
/// even value required by yuv420p.
fn output_dimensions(
    source_width: u32,
    source_height: u32,
    configured_output_height: Option<u32>,
) -> Result<(u32, u32)> {
    if source_width == 0 || source_height == 0 {
        anyhow::bail!("source dimensions must be greater than zero");
    }
    let Some(output_height) = configured_output_height else {
        return Ok((source_width, source_height));
    };
    if output_height == 0 {
        anyhow::bail!("output height must be greater than zero");
    }
    let scaled_width = (u64::from(source_width) * u64::from(output_height))
        .div_ceil(u64::from(source_height))
        .max(2);
    let even_width = scaled_width.div_ceil(2) * 2;
    let width = u32::try_from(even_width).context("scaled output width exceeds u32")?;
    Ok((width, output_height))
}

fn h264_level_31_compatible(width: u32, height: u32, fps: u32) -> bool {
    if width == 0 || height == 0 || fps == 0 {
        return false;
    }
    let macroblocks_per_frame = u64::from(width).div_ceil(16) * u64::from(height).div_ceil(16);
    macroblocks_per_frame <= H264_LEVEL_31_MAX_FRAME_SIZE_MACROBLOCKS
        && macroblocks_per_frame.saturating_mul(u64::from(fps))
            <= H264_LEVEL_31_MAX_MACROBLOCKS_PER_SECOND
}

fn h264_level_31_bitrate_compatible(bitrate_bps: u32) -> bool {
    bitrate_bps <= H264_LEVEL_31_MAX_BITRATE_BPS
}

fn yuv420p_frame_size(width: u32, height: u32) -> Result<usize> {
    if width < 2 || height < 2 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        anyhow::bail!("shared source dimensions must be at least 2x2 and even for yuv420p");
    }
    let pixels = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .context("shared source dimensions overflow frame size")?;
    pixels
        .checked_mul(3)
        .and_then(|value| value.checked_div(2))
        .context("shared source dimensions overflow YUV420P frame size")
}

fn source_pixel_format_frame_size(
    pixel_format: SourcePixelFormat,
    width: u32,
    height: u32,
) -> Result<usize> {
    match pixel_format {
        SourcePixelFormat::Yuv420p => yuv420p_frame_size(width, height),
        SourcePixelFormat::Bgra => {
            let pixels = usize::try_from(width)
                .ok()
                .and_then(|width| {
                    usize::try_from(height)
                        .ok()
                        .and_then(|height| width.checked_mul(height))
                })
                .context("shared source dimensions overflow BGRA frame size")?;
            pixels
                .checked_mul(4)
                .context("shared source dimensions overflow BGRA frame size")
        }
    }
}

impl MediaTrack {
    fn enqueue(&self, sample: QueuedSample) {
        if self.queue.closed.load(Ordering::Acquire) {
            return;
        }
        if let Ok(mut samples) = self.queue.samples.lock() {
            if self.preserve_nal_order {
                // The RTP H.264 packetizer retains SPS/PPS until it sees the next
                // picture. Keep that short NAL sequence intact; replacing it with a
                // later picture would make the decoder lose its parameter sets.
                const MAX_QUEUED_H264_NALS: usize = 16;
                if samples.len() >= MAX_QUEUED_H264_NALS {
                    samples.clear();
                }
            } else {
                // This is a live stream, not a recording: preserving an older access
                // unit only adds latency for a viewer that is already behind.
                samples.clear();
            }
            samples.push_back(sample);
        }
        self.queue.notify.notify_one();
    }

    fn close(&self) {
        self.queue.closed.store(true, Ordering::Release);
        if let Ok(mut samples) = self.queue.samples.lock() {
            samples.clear();
        }
        self.queue.notify.notify_one();
    }

    async fn run_writer(self) {
        let mut next_rtp_timestamp = rand::random::<u32>().max(1);
        loop {
            let sample = loop {
                if self.queue.closed.load(Ordering::Acquire) {
                    return;
                }
                if let Ok(mut samples) = self.queue.samples.lock()
                    && let Some(sample) = samples.pop_front()
                {
                    break sample;
                }
                self.queue.notify.notified().await;
            };
            let packet_timestamp = next_rtp_timestamp;
            let timestamp_step = (sample.duration.as_secs_f64() * 90_000.0).round().max(0.0) as u32;
            next_rtp_timestamp = next_rtp_timestamp.wrapping_add(timestamp_step);
            if let (Some(capture_time), Some(encoder_delay_ms)) =
                (sample.capture_time_unix_nanos, sample.encoder_delay_ms)
                && let Ok(mut timings) = self.frame_capture_times.lock()
                && let Some(timings) = timings.get_mut(&self.connection_id)
            {
                while timings
                    .front()
                    .is_some_and(|timing| timing.recorded_at.elapsed() > FRAME_TIMING_RETENTION)
                    || timings.len() >= FRAME_TIMING_ENTRY_LIMIT
                {
                    timings.pop_front();
                }
                timings.push_back(FrameCaptureMapping {
                    rtp_timestamp: packet_timestamp,
                    capture_time_unix_nanos: capture_time,
                    encoder_delay_ms,
                    recorded_at: Instant::now(),
                });
            }
            let writer = self
                .track
                .sample_writer(self.ssrc, self.payload_type)
                .with_rtp_timestamp(packet_timestamp);
            if let Err(error) = writer
                .write_sample(&Sample {
                    data: sample.data,
                    duration: sample.duration,
                    ..Default::default()
                })
                .await
            {
                // A track can receive its first sample before ICE/DTLS has completed.
                // Keep the writer alive so a transient pre-connection send failure does
                // not permanently black-hole that viewer's later video frames.
                tracing::debug!(%error, "viewer media sample was not sent yet");
                if self.queue.closed.load(Ordering::Acquire) {
                    return;
                }
            }
        }
    }
}
