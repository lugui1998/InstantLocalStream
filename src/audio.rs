use std::collections::{HashMap, HashSet, VecDeque};
use std::f32::consts::TAU;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bytes::Bytes;
use flexaudio::core::backend::RawSink;
use flexaudio::{
    AudioChunk, CaptureBackend, ChunkFlags, Event as FlexAudioEvent, OutputFormat, ProcessMode,
    SourceKind, Stream, StreamConfig as FlexAudioConfig,
};
use opus_rs::{Application, OpusEncoder};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_OPUS;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use tokio::sync::Notify;
use uuid::Uuid;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;

use crate::capture;
use crate::media::CaptureSettings;

pub const OPUS_PAYLOAD_TYPE: u8 = 111;
pub const OPUS_FMTP_LINE: &str = "minptime=10;stereo=1;sprop-stereo=1;maxaveragebitrate=256000";
const OPUS_FRAME_SAMPLES: usize = 960;
const STEREO_SAMPLES_PER_FRAME: usize = OPUS_FRAME_SAMPLES * 2;
const OPUS_BITRATE_BPS: i32 = 256_000;
const OPUS_COMPLEXITY: i32 = 10;
pub const TEST_TONE_FREQUENCY_HZ: f32 = 1_000.0;
pub const TEST_TONE_LEVEL_DBFS: f32 = -12.0;
pub const TEST_TONE_ON_DURATION: Duration = Duration::from_secs(1);
pub const TEST_TONE_CYCLE_DURATION: Duration = Duration::from_secs(2);
const TEST_TONE_AMPLITUDE: f32 = 0.251_188_64;
const TEST_TONE_FADE_FRAMES: u64 = 240;
const TEST_TONE_BLOCK_FRAMES: u64 = 480;
const AUDIO_QUEUE_CAPACITY: usize = 10;
const AUDIO_CAPTURE_BACKLOG_CAPACITY: usize = 10;
const AUDIO_STALL_LIVE_EDGE_FRAMES: usize = 2;
const AUDIO_FRAME_DURATION: Duration = Duration::from_millis(20);
const RETRY_INITIAL_DELAY: Duration = Duration::from_millis(250);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct AudioTrack {
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    queue: Arc<AudioQueue>,
    diagnostics: Arc<AudioDiagnosticsCounters>,
}

struct AudioSample {
    data: Bytes,
    duration: Duration,
    prev_dropped_packets: u16,
}

struct AudioQueue {
    samples: Mutex<std::collections::VecDeque<AudioSample>>,
    notify: Notify,
    closed: AtomicBool,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct AudioDiagnostics {
    pub capture_raw_sample_drops: u64,
    pub capture_chunk_drops: u64,
    pub capture_discontinuities: u64,
    pub capture_recovery_gap_packets: u64,
    pub pacing_backlog_drops: u64,
    pub subscriber_queue_drops: u64,
    pub malformed_chunks: u64,
    pub encode_failures: u64,
    pub write_failures: u64,
    pub native_thread_priority_promotions: u64,
}

#[derive(Default)]
struct AudioDiagnosticsCounters {
    capture_raw_sample_drops: AtomicU64,
    capture_chunk_drops: AtomicU64,
    capture_discontinuities: AtomicU64,
    capture_recovery_gap_packets: AtomicU64,
    pacing_backlog_drops: AtomicU64,
    subscriber_queue_drops: AtomicU64,
    malformed_chunks: AtomicU64,
    encode_failures: AtomicU64,
    write_failures: AtomicU64,
    native_thread_priority_promotions: AtomicU64,
}

struct PacedAudioChunk {
    chunk: AudioChunk,
    prev_dropped_packets: u32,
}

struct DiagnosticToneBackend {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl DiagnosticToneBackend {
    fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

impl CaptureBackend for DiagnosticToneBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }

    fn start(&mut self, mut sink: RawSink) -> flexaudio::Result<()> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let running = Arc::clone(&self.running);
        let handle = thread::Builder::new()
            .name("flexaudio-test-tone".to_owned())
            .spawn(move || {
                let mut frame_index = diagnostic_cycle_phase_frame();
                let mut samples = Vec::with_capacity(TEST_TONE_BLOCK_FRAMES as usize * 2);
                let block_duration = Duration::from_millis(10);
                let mut next_deadline = Instant::now();
                while running.load(Ordering::SeqCst) {
                    samples.clear();
                    for offset in 0..TEST_TONE_BLOCK_FRAMES {
                        let sample = diagnostic_tone_sample(frame_index + offset);
                        samples.extend_from_slice(&[sample, sample]);
                    }
                    let pts_ns = frame_index.saturating_mul(1_000_000_000) / 48_000;
                    sink.push(&samples, pts_ns.min(i64::MAX as u64) as i64);
                    frame_index = frame_index.wrapping_add(TEST_TONE_BLOCK_FRAMES);
                    next_deadline += block_duration;
                    if let Some(wait) = next_deadline.checked_duration_since(Instant::now()) {
                        thread::sleep(wait);
                    }
                }
            })
            .map_err(|error| {
                self.running.store(false, Ordering::SeqCst);
                flexaudio::Error::Backend(format!("spawn diagnostic tone: {error}"))
            })?;
        self.handle = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for DiagnosticToneBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

fn diagnostic_cycle_phase_seconds() -> f64 {
    let cycle_ns = TEST_TONE_CYCLE_DURATION.as_nanos();
    let elapsed_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    (elapsed_ns % cycle_ns) as f64 / 1_000_000_000.0
}

fn diagnostic_cycle_phase_frame() -> u64 {
    (diagnostic_cycle_phase_seconds() * 48_000.0).round() as u64
}

fn diagnostic_tone_sample(frame_index: u64) -> f32 {
    const SAMPLE_RATE: u64 = 48_000;
    let cycle_frames = TEST_TONE_CYCLE_DURATION.as_secs() * SAMPLE_RATE;
    let on_frames = TEST_TONE_ON_DURATION.as_secs() * SAMPLE_RATE;
    let cycle_frame = frame_index % cycle_frames;
    if cycle_frame >= on_frames {
        return 0.0;
    }
    let fade_in = (cycle_frame as f32 / TEST_TONE_FADE_FRAMES as f32).min(1.0);
    let frames_until_silence = on_frames.saturating_sub(cycle_frame);
    let fade_out = (frames_until_silence as f32 / TEST_TONE_FADE_FRAMES as f32).min(1.0);
    let envelope = fade_in.min(fade_out);
    let phase_frame = frame_index % SAMPLE_RATE;
    let phase = TAU * TEST_TONE_FREQUENCY_HZ * phase_frame as f32 / SAMPLE_RATE as f32;
    phase.sin() * TEST_TONE_AMPLITUDE * envelope
}

pub struct AudioPipeline {
    subscribers: Arc<Mutex<HashMap<Uuid, AudioTrack>>>,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    ended: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    settings: Arc<Mutex<Option<CaptureSettings>>>,
    revision: Arc<AtomicU64>,
    diagnostics: Arc<AudioDiagnosticsCounters>,
}

impl AudioPipeline {
    pub fn new() -> Self {
        Self {
            subscribers: Arc::new(Mutex::new(HashMap::new())),
            stop: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicBool::new(false)),
            ended: Arc::new(AtomicBool::new(false)),
            failure: Arc::new(Mutex::new(None)),
            settings: Arc::new(Mutex::new(None)),
            revision: Arc::new(AtomicU64::new(0)),
            diagnostics: Arc::new(AudioDiagnosticsCounters::default()),
        }
    }

    pub fn subscribe(&self, connection_id: Uuid) -> Result<AudioTrack> {
        let ssrc = rand::random::<u32>();
        let track = TrackLocalStaticSample::new(MediaStreamTrack::new(
            "instant-local-stream".to_owned(),
            format!("screen-audio-{connection_id}"),
            "Instant Local Stream audio".to_owned(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48_000,
                    channels: 2,
                    sdp_fmtp_line: OPUS_FMTP_LINE.to_owned(),
                    rtcp_feedback: vec![],
                },
                ..Default::default()
            }],
        ))?;
        let audio_track = AudioTrack {
            track: Arc::new(track),
            ssrc,
            queue: Arc::new(AudioQueue {
                samples: Mutex::new(std::collections::VecDeque::with_capacity(
                    AUDIO_QUEUE_CAPACITY,
                )),
                notify: Notify::new(),
                closed: AtomicBool::new(false),
            }),
            diagnostics: Arc::clone(&self.diagnostics),
        };
        let writer = audio_track.clone();
        tokio::spawn(async move { writer.run_writer().await });
        self.subscribers
            .lock()
            .map_err(|_| anyhow::anyhow!("audio subscriber lock poisoned"))?
            .insert(connection_id, audio_track.clone());
        Ok(audio_track)
    }

    pub fn unsubscribe(&self, connection_id: Uuid) {
        if let Ok(mut subscribers) = self.subscribers.lock()
            && let Some(track) = subscribers.remove(&connection_id)
        {
            track.close();
        }
    }

    pub fn spawn(self: Arc<Self>, settings: CaptureSettings) -> tokio::task::JoinHandle<()> {
        self.reconfigure(settings);
        tokio::task::spawn_blocking(move || {
            let result = self.run();
            if let Err(error) = result
                && let Ok(mut failure) = self.failure.lock()
            {
                *failure = Some(error.to_string());
            }
            self.ended.store(true, Ordering::Release);
        })
    }

    /// Reopens the loopback source when audio routing or the selected window
    /// changes while preserving the WebRTC audio tracks already negotiated by
    /// connected viewers.
    pub fn reconfigure(&self, settings: CaptureSettings) {
        let changed = self
            .settings
            .lock()
            .map(|mut current| {
                let changed = current
                    .as_ref()
                    .is_none_or(|previous| audio_input_changed(previous, &settings));
                *current = Some(settings);
                changed
            })
            .unwrap_or(false);
        if changed {
            if let Ok(mut failure) = self.failure.lock() {
                *failure = None;
            }
            self.ended.store(false, Ordering::Release);
            self.revision.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(subscribers) = self.subscribers.lock() {
            for subscriber in subscribers.values() {
                subscriber.close();
            }
        }
    }

    pub fn activate(&self) {
        if !self.active.swap(true, Ordering::AcqRel) {
            self.revision.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub fn deactivate(&self) {
        if self.active.swap(false, Ordering::AcqRel) {
            // Break the inner poll loop so its native streams are stopped.
            // The outer loop remains parked until the next activation.
            self.revision.fetch_add(1, Ordering::AcqRel);
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

    pub fn diagnostics(&self) -> AudioDiagnostics {
        AudioDiagnostics {
            capture_raw_sample_drops: self
                .diagnostics
                .capture_raw_sample_drops
                .load(Ordering::Relaxed),
            capture_chunk_drops: self.diagnostics.capture_chunk_drops.load(Ordering::Relaxed),
            capture_discontinuities: self
                .diagnostics
                .capture_discontinuities
                .load(Ordering::Relaxed),
            capture_recovery_gap_packets: self
                .diagnostics
                .capture_recovery_gap_packets
                .load(Ordering::Relaxed),
            pacing_backlog_drops: self
                .diagnostics
                .pacing_backlog_drops
                .load(Ordering::Relaxed),
            subscriber_queue_drops: self
                .diagnostics
                .subscriber_queue_drops
                .load(Ordering::Relaxed),
            malformed_chunks: self.diagnostics.malformed_chunks.load(Ordering::Relaxed),
            encode_failures: self.diagnostics.encode_failures.load(Ordering::Relaxed),
            write_failures: self.diagnostics.write_failures.load(Ordering::Relaxed),
            native_thread_priority_promotions: self
                .diagnostics
                .native_thread_priority_promotions
                .load(Ordering::Relaxed),
        }
    }

    fn run(&self) -> Result<()> {
        let _audio_priority = prioritize_audio_thread();
        let mut encoder = high_quality_opus_encoder()?;
        let mut encoded = vec![0_u8; 1_500];

        let mut retry_attempt = 0;
        let mut retry_revision = None;
        let mut last_broadcast_at = None;
        while !self.stop.load(Ordering::Acquire) {
            if !self.active.load(Ordering::Acquire) {
                last_broadcast_at = None;
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            let revision = self.revision.load(Ordering::Acquire);
            if retry_revision != Some(revision) {
                retry_attempt = 0;
                retry_revision = Some(revision);
            }
            let Some(settings) = self.settings.lock().ok().and_then(|value| value.clone()) else {
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            if settings.audio_mode == "off" {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            let mut stream = match open_stream(&settings) {
                Ok(stream) => stream,
                Err(error) => {
                    self.record_failure(error.to_string());
                    self.wait_for_retry(revision, retry_attempt);
                    retry_attempt = retry_attempt.saturating_add(1);
                    continue;
                }
            };
            if let Err(error) = stream.start() {
                stream.stop();
                self.record_failure(error.to_string());
                self.wait_for_retry(revision, retry_attempt);
                retry_attempt = retry_attempt.saturating_add(1);
                continue;
            }
            self.prioritize_native_threads();
            retry_attempt = 0;
            if let Ok(mut failure) = self.failure.lock() {
                *failure = None;
            }
            let mut capture_backlog = VecDeque::with_capacity(AUDIO_CAPTURE_BACKLOG_CAPACITY);
            let mut previous_capture_seq = None;
            let mut previous_capture_pts_ns = None;
            let mut previous_raw_drops = 0_u64;
            let mut pending_dropped_packets = 0_u32;
            let mut next_send_at = None;

            while !self.stop.load(Ordering::Acquire)
                && self.revision.load(Ordering::Acquire) == revision
            {
                let raw_drops = stream.dropped_raw_samples();
                let newly_dropped_raw_samples = raw_drops.saturating_sub(previous_raw_drops);
                previous_raw_drops = raw_drops;
                if newly_dropped_raw_samples > 0 {
                    self.diagnostics
                        .capture_raw_sample_drops
                        .fetch_add(newly_dropped_raw_samples, Ordering::Relaxed);
                }
                while let Some(event) = stream.poll_event() {
                    match event {
                        FlexAudioEvent::StreamRecovered => {
                            self.prioritize_native_threads();
                        }
                        FlexAudioEvent::StreamStalled => {
                            tracing::warn!("native audio capture stalled");
                        }
                        FlexAudioEvent::Error(error) => {
                            tracing::warn!(%error, "native audio capture reported an error");
                        }
                        _ => {}
                    }
                }
                while let Some(chunk) = stream.poll_chunk() {
                    if !self.active.load(Ordering::Acquire) {
                        // Starting playback establishes a fresh RTP timeline;
                        // capture loss while nobody is listening is irrelevant.
                        capture_backlog.clear();
                        pending_dropped_packets = 0;
                        next_send_at = None;
                        last_broadcast_at = None;
                        continue;
                    }
                    let sequence_gap = capture_sequence_gap(&mut previous_capture_seq, chunk.seq);
                    if sequence_gap > 0 {
                        self.diagnostics
                            .capture_chunk_drops
                            .fetch_add(sequence_gap as u64, Ordering::Relaxed);
                    }
                    let discontinuity = chunk.flags.contains(ChunkFlags::DISCONTINUITY);
                    let recovery_gap = capture_discontinuity_gap(
                        &mut previous_capture_pts_ns,
                        chunk.pts_ns,
                        discontinuity,
                    );
                    if discontinuity {
                        self.diagnostics
                            .capture_discontinuities
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    if requires_discontinuity_concealment(discontinuity, sequence_gap, recovery_gap)
                    {
                        // A sub-frame raw-ring overflow is still a hard PCM
                        // splice. Do not encode that boundary as crackle;
                        // omit the affected normalized frame and let Opus PLC
                        // conceal one explicitly skipped RTP packet instead.
                        self.diagnostics
                            .capture_recovery_gap_packets
                            .fetch_add(1, Ordering::Relaxed);
                        pending_dropped_packets = pending_dropped_packets.saturating_add(1);
                        continue;
                    }
                    if recovery_gap > 0 {
                        self.diagnostics
                            .capture_recovery_gap_packets
                            .fetch_add(recovery_gap as u64, Ordering::Relaxed);
                    }
                    let capture_gap = sequence_gap.max(recovery_gap);
                    let dropped = enqueue_bounded_capture_chunk(
                        &mut capture_backlog,
                        PacedAudioChunk {
                            chunk,
                            prev_dropped_packets: capture_gap,
                        },
                        &mut pending_dropped_packets,
                    );
                    self.diagnostics
                        .pacing_backlog_drops
                        .fetch_add(dropped, Ordering::Relaxed);
                }

                let now = Instant::now();
                if !capture_backlog.is_empty()
                    && next_send_at.is_none_or(|deadline| now >= deadline)
                {
                    if next_send_at.is_some_and(|deadline| {
                        now.saturating_duration_since(deadline) >= AUDIO_FRAME_DURATION
                    }) {
                        let dropped = trim_capture_backlog_to_live_edge(
                            &mut capture_backlog,
                            &mut pending_dropped_packets,
                        );
                        self.diagnostics
                            .pacing_backlog_drops
                            .fetch_add(dropped, Ordering::Relaxed);
                    }
                    let paced = capture_backlog
                        .pop_front()
                        .expect("backlog checked non-empty");
                    pending_dropped_packets =
                        pending_dropped_packets.saturating_add(paced.prev_dropped_packets);
                    next_send_at = Some(advance_audio_deadline(next_send_at.unwrap_or(now), now));
                    if paced.chunk.frames != OPUS_FRAME_SAMPLES
                        || paced.chunk.data.len() != STEREO_SAMPLES_PER_FRAME
                    {
                        self.diagnostics
                            .malformed_chunks
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            frames = paced.chunk.frames,
                            samples = paced.chunk.data.len(),
                            "discarding malformed normalized audio chunk"
                        );
                        pending_dropped_packets = pending_dropped_packets.saturating_add(1);
                        continue;
                    }
                    let size =
                        match encoder.encode(&paced.chunk.data, OPUS_FRAME_SAMPLES, &mut encoded) {
                            Ok(size) => size,
                            Err(error) => {
                                self.diagnostics
                                    .encode_failures
                                    .fetch_add(1, Ordering::Relaxed);
                                return Err(anyhow::anyhow!(error));
                            }
                        };
                    let elapsed_gap = elapsed_audio_gap(last_broadcast_at, now);
                    let rtp_gap = pending_dropped_packets
                        .max(elapsed_gap)
                        .min(u16::MAX as u32) as u16;
                    pending_dropped_packets = 0;
                    self.broadcast(Bytes::copy_from_slice(&encoded[..size]), rtp_gap);
                    last_broadcast_at = Some(now);
                    continue;
                }

                let sleep_for = next_send_at
                    .and_then(|deadline| deadline.checked_duration_since(now))
                    .unwrap_or(Duration::from_millis(2))
                    .min(Duration::from_millis(2));
                thread::sleep(sleep_for);
            }
            stream.stop();
        }
        Ok(())
    }

    fn record_failure(&self, error: String) {
        if let Ok(mut failure) = self.failure.lock() {
            *failure = Some(error);
        }
    }

    fn prioritize_native_threads(&self) {
        let promoted = prioritize_audio_capture_threads();
        self.diagnostics
            .native_thread_priority_promotions
            .fetch_add(promoted, Ordering::Relaxed);
        if cfg!(windows) && promoted < 2 {
            tracing::warn!(
                promoted,
                "not every native audio worker received high priority"
            );
        }
    }

    fn wait_for_retry(&self, revision: u64, attempt: u32) {
        let deadline = Instant::now() + retry_delay(attempt);
        while !self.stop.load(Ordering::Acquire)
            && self.active.load(Ordering::Acquire)
            && self.revision.load(Ordering::Acquire) == revision
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn broadcast(&self, data: Bytes, prev_dropped_packets: u16) {
        let subscribers = self
            .subscribers
            .lock()
            .map(|subscribers| subscribers.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for subscriber in subscribers {
            subscriber.enqueue(AudioSample {
                data: data.clone(),
                duration: Duration::from_millis(20),
                prev_dropped_packets,
            });
        }
    }
}

fn high_quality_opus_encoder() -> Result<OpusEncoder> {
    let mut encoder =
        OpusEncoder::new(48_000, 2, Application::Audio).map_err(|error| anyhow::anyhow!(error))?;
    // Audio is a tiny fraction of the video budget, so keep Opus in its
    // high-quality stereo profile at all times. VBR gives transients the
    // bytes they need instead of forcing every packet to the same size.
    encoder.bitrate_bps = OPUS_BITRATE_BPS;
    encoder.complexity = OPUS_COMPLEXITY;
    encoder.use_cbr = false;
    Ok(encoder)
}

fn capture_sequence_gap(previous: &mut Option<u64>, current: u64) -> u32 {
    let gap = previous
        .and_then(|value| current.checked_sub(value.saturating_add(1)))
        .unwrap_or_default();
    *previous = Some(current);
    gap.min(u32::MAX as u64) as u32
}

fn capture_discontinuity_gap(
    previous_pts_ns: &mut Option<i64>,
    current_pts_ns: i64,
    discontinuity: bool,
) -> u32 {
    let previous = previous_pts_ns.replace(current_pts_ns);
    if !discontinuity {
        return 0;
    }
    let Some(previous) = previous else {
        return 0;
    };
    let elapsed_ns = current_pts_ns.saturating_sub(previous).max(0) as u64;
    let frame_ns = AUDIO_FRAME_DURATION.as_nanos() as u64;
    let elapsed_packets = elapsed_ns.saturating_add(frame_ns / 2) / frame_ns;
    elapsed_packets.saturating_sub(1).min(u32::MAX as u64) as u32
}

fn requires_discontinuity_concealment(
    discontinuity: bool,
    sequence_gap: u32,
    elapsed_gap: u32,
) -> bool {
    discontinuity && sequence_gap == 0 && elapsed_gap == 0
}

fn advance_audio_deadline(previous_deadline: Instant, now: Instant) -> Instant {
    let anchored = previous_deadline + AUDIO_FRAME_DURATION;
    if now > anchored {
        now + AUDIO_FRAME_DURATION
    } else {
        anchored
    }
}

fn elapsed_audio_gap(previous: Option<Instant>, current: Instant) -> u32 {
    let Some(previous) = previous else {
        return 0;
    };
    let elapsed_ns = current.saturating_duration_since(previous).as_nanos();
    let frame_ns = AUDIO_FRAME_DURATION.as_nanos();
    let elapsed_packets = elapsed_ns.saturating_add(frame_ns / 2) / frame_ns;
    elapsed_packets.saturating_sub(1).min(u32::MAX as u128) as u32
}

fn trim_capture_backlog_to_live_edge(
    backlog: &mut VecDeque<PacedAudioChunk>,
    pending_dropped_packets: &mut u32,
) -> u64 {
    let mut dropped = 0_u64;
    while backlog.len() > AUDIO_STALL_LIVE_EDGE_FRAMES {
        let stale = backlog
            .pop_front()
            .expect("backlog length checked before trimming");
        *pending_dropped_packets = pending_dropped_packets
            .saturating_add(stale.prev_dropped_packets)
            .saturating_add(1);
        dropped += 1;
    }
    dropped
}

fn enqueue_bounded_capture_chunk(
    backlog: &mut VecDeque<PacedAudioChunk>,
    chunk: PacedAudioChunk,
    pending_dropped_packets: &mut u32,
) -> u64 {
    let mut dropped = 0;
    if backlog.len() >= AUDIO_CAPTURE_BACKLOG_CAPACITY
        && let Some(stale) = backlog.pop_front()
    {
        *pending_dropped_packets = pending_dropped_packets
            .saturating_add(stale.prev_dropped_packets)
            .saturating_add(1);
        dropped = 1;
    }
    backlog.push_back(chunk);
    dropped
}

#[cfg(windows)]
struct AudioThreadPriority(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for AudioThreadPriority {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(self.0);
        }
    }
}

#[cfg(windows)]
fn prioritize_audio_thread() -> Option<AudioThreadPriority> {
    use windows::{
        Win32::System::Threading::{
            AVRT_PRIORITY_HIGH, AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority,
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
        },
        core::w,
    };

    let mut task_index = 0;
    if let Ok(handle) = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) } {
        unsafe {
            let _ = AvSetMmThreadPriority(handle, AVRT_PRIORITY_HIGH);
        }
        return Some(AudioThreadPriority(handle));
    }

    // MMCSS can be unavailable on stripped-down Windows installations. A
    // per-thread priority bump is a narrower fallback than raising the whole
    // process and lasts only until this audio worker exits.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
    None
}

#[cfg(not(windows))]
fn prioritize_audio_thread() {}

fn is_audio_capture_thread_name(name: &str) -> bool {
    matches!(
        name,
        "flexaudio-wasapi-system"
            | "flexaudio-wasapi-process"
            | "flexaudio-test-tone"
            | "flexaudio-intake"
    )
}

#[cfg(windows)]
fn prioritize_audio_capture_threads() -> u64 {
    use std::mem::size_of;
    use windows::Win32::{
        Foundation::{CloseHandle, HLOCAL, LocalFree},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            Threading::{
                GetCurrentProcessId, GetThreadDescription, OpenThread, SetThreadPriority,
                THREAD_PRIORITY_HIGHEST, THREAD_QUERY_LIMITED_INFORMATION, THREAD_SET_INFORMATION,
            },
        },
    };

    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }) else {
        return 0;
    };
    let process_id = unsafe { GetCurrentProcessId() };
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut promoted = 0;
    let mut next = unsafe { Thread32First(snapshot, &mut entry) };
    while next.is_ok() {
        if entry.th32OwnerProcessID == process_id
            && let Ok(thread_handle) = unsafe {
                OpenThread(
                    THREAD_QUERY_LIMITED_INFORMATION | THREAD_SET_INFORMATION,
                    false,
                    entry.th32ThreadID,
                )
            }
        {
            let matches_capture_thread = unsafe { GetThreadDescription(thread_handle) }
                .ok()
                .is_some_and(|description| {
                    let matches = unsafe { description.to_string() }
                        .is_ok_and(|name| is_audio_capture_thread_name(&name));
                    unsafe {
                        let _ = LocalFree(Some(HLOCAL(description.0.cast())));
                    }
                    matches
                });
            if matches_capture_thread
                && unsafe { SetThreadPriority(thread_handle, THREAD_PRIORITY_HIGHEST) }.is_ok()
            {
                promoted += 1;
            }
            unsafe {
                let _ = CloseHandle(thread_handle);
            }
        }
        next = unsafe { Thread32Next(snapshot, &mut entry) };
    }
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    promoted
}

#[cfg(not(windows))]
fn prioritize_audio_capture_threads() -> u64 {
    0
}

fn retry_delay(attempt: u32) -> Duration {
    RETRY_INITIAL_DELAY
        .checked_mul(1_u32 << attempt.min(5))
        .unwrap_or(RETRY_MAX_DELAY)
        .min(RETRY_MAX_DELAY)
}

fn audio_input_changed(previous: &CaptureSettings, next: &CaptureSettings) -> bool {
    previous.audio_mode != next.audio_mode
        || previous.excluded_audio_processes != next.excluded_audio_processes
        || (next.audio_mode == "window"
            && (previous.source_kind != next.source_kind
                || match (previous.source_native_id, next.source_native_id) {
                    (Some(previous_id), Some(next_id)) => previous_id != next_id,
                    (None, None) => previous.source_index != next.source_index,
                    _ => true,
                }))
}

fn open_stream(settings: &CaptureSettings) -> Result<Stream> {
    if settings.audio_mode == "test" {
        if settings.source_kind != "test" {
            anyhow::bail!("diagnostic test tone requires the test-pattern source")
        }
        return Stream::open(
            FlexAudioConfig {
                output: OutputFormat {
                    sample_rate: 48_000,
                    channels: 2,
                },
                ring_capacity_chunks: AUDIO_CAPTURE_BACKLOG_CAPACITY,
                ..Default::default()
            },
            Box::new(DiagnosticToneBackend::new()),
        )
        .map_err(|error| anyhow::anyhow!(error));
    }
    if settings.audio_mode == "window" {
        let pid = selected_window_pid(settings)?;
        return open_process_stream(pid);
    }
    if settings.audio_mode != "system" {
        anyhow::bail!("audio capture was not enabled")
    }
    let output = OutputFormat {
        sample_rate: 48_000,
        channels: 2,
    };
    if settings.excluded_audio_processes.is_empty() {
        let stream = flexaudio::open(FlexAudioConfig {
            kind: SourceKind::SystemLoopback,
            output,
            ring_capacity_chunks: AUDIO_CAPTURE_BACKLOG_CAPACITY,
            ..Default::default()
        })
        .map_err(|error| anyhow::anyhow!(error))?;
        return Ok(stream);
    }

    let excluded = settings
        .excluded_audio_processes
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let mut excluded_pids = HashSet::new();
    for source in capture::list_windows().unwrap_or_default() {
        let name = source.name.to_ascii_lowercase();
        if excluded.iter().any(|excluded| name.contains(excluded))
            && let Some(pid) = source.pid
        {
            excluded_pids.insert(pid);
        }
    }
    #[cfg(windows)]
    let excluded_pid = match resolve_running_excluded_pid(&settings.excluded_audio_processes)? {
        Some(pid) => Some(pid),
        None => resolve_excluded_pid(excluded_pids)?,
    };
    #[cfg(not(windows))]
    let excluded_pid = resolve_excluded_pid(excluded_pids)?;

    // Exclusions are fail-closed. The pipeline's retry loop will reopen this
    // source after the requested process becomes discoverable; falling back to
    // raw system capture could leak its audio if it starts later.
    let Some(excluded_pid) = excluded_pid else {
        anyhow::bail!(
            "cannot enforce audio exclusion: none of the requested processes are currently discoverable"
        )
    };
    flexaudio::open(FlexAudioConfig {
        // flexaudio exposes a native system-minus-one-process primitive.  It
        // cannot represent multiple independent exclusions in one stream, and
        // opening several such streams would duplicate all non-excluded audio.
        kind: SourceKind::ProcessLoopback,
        target_pid: Some(excluded_pid),
        mode: ProcessMode::Exclude,
        output,
        ring_capacity_chunks: AUDIO_CAPTURE_BACKLOG_CAPACITY,
        ..Default::default()
    })
    .map_err(|error| anyhow::anyhow!(error))
}

fn resolve_excluded_pid(excluded_pids: HashSet<u32>) -> Result<Option<u32>> {
    match excluded_pids.len() {
        1 => Ok(excluded_pids.iter().next().copied()),
        0 => Ok(None),
        _ => anyhow::bail!(
            "cannot enforce audio exclusion for multiple independent process trees with the current audio backend"
        ),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RunningProcess {
    pid: u32,
    parent_pid: u32,
    name: String,
}

#[cfg(windows)]
fn resolve_running_excluded_pid(requested_names: &[String]) -> Result<Option<u32>> {
    let processes = windows_process_snapshot().context("enumerate running audio processes")?;
    resolve_process_tree_root(requested_names, &processes)
        .with_context(|| format!("requested exclusions: {}", requested_names.join(", ")))
}

fn resolve_process_tree_root(
    requested_names: &[String],
    processes: &[RunningProcess],
) -> Result<Option<u32>> {
    let requested = requested_names
        .iter()
        .map(|name| normalize_process_name(name))
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    let matching = processes
        .iter()
        .filter(|process| {
            let actual = normalize_process_name(&process.name);
            requested
                .iter()
                .any(|requested| actual == *requested || actual.contains(requested))
        })
        .collect::<Vec<_>>();
    if matching.is_empty() {
        return Ok(None);
    }
    let matching_pids = matching
        .iter()
        .map(|process| process.pid)
        .collect::<HashSet<_>>();
    let roots = matching
        .iter()
        .filter(|process| !matching_pids.contains(&process.parent_pid))
        .map(|process| process.pid)
        .collect::<HashSet<_>>();
    resolve_excluded_pid(roots)
}

fn normalize_process_name(name: &str) -> String {
    let lowercase = name.trim().to_ascii_lowercase();
    lowercase
        .strip_suffix(".exe")
        .unwrap_or(&lowercase)
        .to_owned()
}

#[cfg(windows)]
fn windows_process_snapshot() -> Result<Vec<RunningProcess>> {
    use std::mem::size_of;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|error| anyhow::anyhow!(error))?;
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut processes = Vec::new();
    let mut next = unsafe { Process32FirstW(snapshot, &mut entry) };
    while next.is_ok() {
        let name_length = entry
            .szExeFile
            .iter()
            .position(|character| *character == 0)
            .unwrap_or(entry.szExeFile.len());
        processes.push(RunningProcess {
            pid: entry.th32ProcessID,
            parent_pid: entry.th32ParentProcessID,
            name: String::from_utf16_lossy(&entry.szExeFile[..name_length]),
        });
        next = unsafe { Process32NextW(snapshot, &mut entry) };
    }
    let _ = unsafe { CloseHandle(snapshot) };
    Ok(processes)
}

fn selected_window_pid(settings: &CaptureSettings) -> Result<u32> {
    if let Some(native_id) = settings.source_native_id {
        return capture::native_window_pid(native_id)
            .context("selected window has no process id for audio capture");
    }
    capture::list_sources()
        .context("enumerate sources for window audio")?
        .into_iter()
        .find(|source| source.kind == "window" && source.index == settings.source_index)
        .and_then(|source| source.pid)
        .context("selected window has no process id for audio capture")
}

fn open_process_stream(pid: u32) -> Result<Stream> {
    flexaudio::open(FlexAudioConfig {
        kind: SourceKind::ProcessLoopback,
        target_pid: Some(pid),
        mode: ProcessMode::Include,
        output: OutputFormat {
            sample_rate: 48_000,
            channels: 2,
        },
        ring_capacity_chunks: AUDIO_CAPTURE_BACKLOG_CAPACITY,
        ..Default::default()
    })
    .map_err(|error| anyhow::anyhow!(error))
}

impl AudioTrack {
    pub fn track(&self) -> Arc<TrackLocalStaticSample> {
        Arc::clone(&self.track)
    }

    fn enqueue(&self, sample: AudioSample) {
        if self.queue.closed.load(Ordering::Acquire) {
            return;
        }
        if let Ok(mut samples) = self.queue.samples.lock() {
            let dropped = enqueue_bounded_audio_sample(&mut samples, sample);
            self.diagnostics
                .subscriber_queue_drops
                .fetch_add(dropped, Ordering::Relaxed);
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
            if self
                .track
                .sample_writer(self.ssrc, OPUS_PAYLOAD_TYPE)
                .write_sample(&Sample {
                    data: sample.data,
                    duration: sample.duration,
                    prev_dropped_packets: sample.prev_dropped_packets,
                    ..Default::default()
                })
                .await
                .is_err()
            {
                self.diagnostics
                    .write_failures
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    }
}

fn enqueue_bounded_audio_sample(
    samples: &mut std::collections::VecDeque<AudioSample>,
    mut sample: AudioSample,
) -> u64 {
    let mut drop_count = 0;
    if samples.len() >= AUDIO_QUEUE_CAPACITY
        && let Some(dropped) = samples.pop_front()
    {
        drop_count = 1;
        let skipped = dropped.prev_dropped_packets.saturating_add(1);
        if let Some(next) = samples.front_mut() {
            next.prev_dropped_packets = next.prev_dropped_packets.saturating_add(skipped);
        } else {
            sample.prev_dropped_packets = sample.prev_dropped_packets.saturating_add(skipped);
        }
    }
    samples.push_back(sample);
    drop_count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn window_audio_reopens_when_native_window_changes() {
        let mut previous = CaptureSettings::from_config(&AppConfig::default());
        previous.audio_mode = "window".to_owned();
        previous.source_kind = "window".to_owned();
        previous.source_native_id = Some(10);
        let mut next = previous.clone();
        next.source_index = 99;
        assert!(!audio_input_changed(&previous, &next));

        next.source_native_id = Some(11);
        assert!(audio_input_changed(&previous, &next));
    }

    #[test]
    fn audio_mode_and_exclusion_changes_reopen_the_input() {
        let mut previous = CaptureSettings::from_config(&AppConfig::default());
        previous.audio_mode = "system".to_owned();
        let mut disabled = previous.clone();
        disabled.audio_mode = "off".to_owned();
        assert!(audio_input_changed(&previous, &disabled));

        let mut exclusions = previous.clone();
        exclusions
            .excluded_audio_processes
            .push("example".to_owned());
        assert!(audio_input_changed(&previous, &exclusions));
    }

    #[test]
    fn opus_encoder_always_uses_the_high_quality_stereo_profile() {
        let mut encoder = high_quality_opus_encoder().unwrap();
        assert_eq!(encoder.bitrate_bps, 256_000);
        assert_eq!(encoder.complexity, 10);
        assert!(!encoder.use_cbr);
        assert!(OPUS_FMTP_LINE.contains("stereo=1"));
        assert!(OPUS_FMTP_LINE.contains("maxaveragebitrate=256000"));

        let pcm = (0..STEREO_SAMPLES_PER_FRAME)
            .map(|sample| ((sample as f32 * 0.071).sin() * 0.5).clamp(-1.0, 1.0))
            .collect::<Vec<_>>();
        let mut packet = vec![0_u8; 1_500];
        let encoded = encoder
            .encode(&pcm, OPUS_FRAME_SAMPLES, &mut packet)
            .unwrap();
        assert!(encoded > 0 && encoded <= packet.len());
    }

    #[test]
    fn diagnostic_tone_is_gated_faded_and_has_the_expected_level() {
        assert_eq!(diagnostic_tone_sample(0), 0.0);
        assert!(diagnostic_tone_sample(252).abs() > 0.24);
        assert_eq!(diagnostic_tone_sample(48_123), 0.0);
        assert_eq!(diagnostic_tone_sample(95_999), 0.0);
        assert_eq!(diagnostic_tone_sample(96_000), 0.0);
    }

    #[test]
    fn diagnostic_tone_reaches_the_normalized_audio_path_without_a_device() {
        let mut settings = CaptureSettings::from_config(&AppConfig::default());
        settings.source_kind = "test".to_owned();
        settings.audio_mode = "test".to_owned();
        let mut stream = open_stream(&settings).unwrap();
        stream.start().unwrap();

        let deadline = Instant::now() + Duration::from_millis(1_500);
        let mut received = None;
        while Instant::now() < deadline && received.is_none() {
            if let Some(chunk) = stream.poll_chunk()
                && chunk.rms > 0.05
            {
                received = Some(chunk);
            }
            thread::sleep(Duration::from_millis(5));
        }
        stream.stop();

        let chunk = received.expect("diagnostic tone should produce a normalized chunk");
        assert_eq!(chunk.frames, OPUS_FRAME_SAMPLES);
        assert_eq!(chunk.data.len(), STEREO_SAMPLES_PER_FRAME);
        assert!(chunk.rms > 0.05);
    }

    #[test]
    fn capture_sequence_numbers_locate_the_gap_before_the_retained_chunk() {
        let mut previous = None;
        let gaps = [0, 1, 3, 4].map(|current| capture_sequence_gap(&mut previous, current));
        assert_eq!(gaps, [0, 0, 1, 0]);
    }

    #[test]
    fn capture_discontinuity_advances_rtp_by_the_missing_media_time() {
        let mut previous = None;
        assert_eq!(capture_discontinuity_gap(&mut previous, 0, false), 0);
        assert_eq!(
            capture_discontinuity_gap(&mut previous, 20_000_000, false),
            0
        );
        assert_eq!(
            capture_discontinuity_gap(&mut previous, 120_000_000, true),
            4
        );
    }

    #[test]
    fn sub_frame_discontinuity_is_concealed_instead_of_encoded_as_a_splice() {
        assert!(requires_discontinuity_concealment(true, 0, 0));
        assert!(!requires_discontinuity_concealment(false, 0, 0));
        assert!(!requires_discontinuity_concealment(true, 1, 0));
        assert!(!requires_discontinuity_concealment(true, 0, 1));
    }

    #[test]
    fn audio_deadline_absorbs_small_jitter_and_rebases_after_a_stall() {
        let start = Instant::now();
        let first_deadline = start + AUDIO_FRAME_DURATION;
        assert_eq!(
            advance_audio_deadline(first_deadline, first_deadline + Duration::from_millis(1)),
            start + AUDIO_FRAME_DURATION * 2
        );

        let stalled = start + Duration::from_millis(200);
        assert_eq!(
            advance_audio_deadline(start + AUDIO_FRAME_DURATION * 2, stalled),
            stalled + AUDIO_FRAME_DURATION
        );
    }

    #[test]
    fn elapsed_audio_gap_covers_an_outage_without_counting_normal_jitter() {
        let start = Instant::now();
        assert_eq!(elapsed_audio_gap(None, start), 0);
        assert_eq!(
            elapsed_audio_gap(Some(start), start + Duration::from_millis(21)),
            0
        );
        assert_eq!(
            elapsed_audio_gap(Some(start), start + Duration::from_millis(200)),
            9
        );
    }

    #[test]
    fn stalled_pacer_trims_to_live_edge_and_preserves_all_skipped_time() {
        let mut backlog = (0..10)
            .map(|seq| test_audio_chunk(seq, if seq == 0 { 2 } else { 0 }))
            .collect::<VecDeque<_>>();
        let mut pending_gap = 0;

        assert_eq!(
            trim_capture_backlog_to_live_edge(&mut backlog, &mut pending_gap),
            8
        );
        assert_eq!(backlog.len(), AUDIO_STALL_LIVE_EDGE_FRAMES);
        assert_eq!(backlog.front().unwrap().chunk.seq, 8);
        assert_eq!(pending_gap, 10);

        // Wall-clock and explicit drop evidence describe the same outage, so
        // the sender takes the maximum instead of double-counting them.
        assert_eq!(pending_gap.max(9), 10);
    }

    #[test]
    fn pacing_backlog_drops_oldest_audio_and_carries_its_rtp_gap() {
        let mut backlog = VecDeque::new();
        let mut pending_gap = 0;
        for seq in 0..AUDIO_CAPTURE_BACKLOG_CAPACITY as u64 {
            assert_eq!(
                enqueue_bounded_capture_chunk(
                    &mut backlog,
                    test_audio_chunk(seq, if seq == 0 { 2 } else { 0 }),
                    &mut pending_gap,
                ),
                0
            );
        }
        assert_eq!(
            enqueue_bounded_capture_chunk(
                &mut backlog,
                test_audio_chunk(AUDIO_CAPTURE_BACKLOG_CAPACITY as u64, 0),
                &mut pending_gap,
            ),
            1
        );
        assert_eq!(backlog.len(), AUDIO_CAPTURE_BACKLOG_CAPACITY);
        assert_eq!(backlog.front().unwrap().chunk.seq, 1);
        assert_eq!(pending_gap, 3);
    }

    #[test]
    fn only_native_capture_and_intake_thread_names_receive_priority() {
        assert!(is_audio_capture_thread_name("flexaudio-wasapi-system"));
        assert!(is_audio_capture_thread_name("flexaudio-wasapi-process"));
        assert!(is_audio_capture_thread_name("flexaudio-test-tone"));
        assert!(is_audio_capture_thread_name("flexaudio-intake"));
        assert!(!is_audio_capture_thread_name("flexaudio-watchdog"));
        assert!(!is_audio_capture_thread_name("flexaudio-intake-extra"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_audio_worker_enumerator_applies_high_priority() {
        use std::sync::mpsc;
        use windows::Win32::System::Threading::{
            GetCurrentThread, GetThreadPriority, THREAD_PRIORITY_HIGHEST,
        };

        let (ready_tx, ready_rx) = mpsc::channel();
        let (check_tx, check_rx) = mpsc::channel();
        let (priority_tx, priority_rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("flexaudio-intake".to_owned())
            .spawn(move || {
                ready_tx.send(()).unwrap();
                check_rx.recv().unwrap();
                let priority = unsafe { GetThreadPriority(GetCurrentThread()) };
                priority_tx.send(priority).unwrap();
            })
            .unwrap();

        ready_rx.recv().unwrap();
        assert!(prioritize_audio_capture_threads() >= 1);
        check_tx.send(()).unwrap();
        assert_eq!(priority_rx.recv().unwrap(), THREAD_PRIORITY_HIGHEST.0);
        worker.join().unwrap();
    }

    fn test_audio_chunk(seq: u64, prev_dropped_packets: u32) -> PacedAudioChunk {
        PacedAudioChunk {
            chunk: AudioChunk {
                data: vec![0.0; STEREO_SAMPLES_PER_FRAME],
                frames: OPUS_FRAME_SAMPLES,
                pts_ns: 0,
                seq,
                flags: ChunkFlags::empty(),
                dropped_before: 0,
                peak: 0.0,
                rms: 0.0,
            },
            prev_dropped_packets,
        }
    }

    #[test]
    fn deactivate_advances_revision_once_to_close_native_streams() {
        let audio = AudioPipeline::new();
        let initial = audio.revision.load(Ordering::Acquire);

        audio.activate();
        let active_revision = audio.revision.load(Ordering::Acquire);
        assert!(active_revision > initial);
        assert!(audio.active.load(Ordering::Acquire));

        audio.deactivate();
        let paused_revision = audio.revision.load(Ordering::Acquire);
        assert!(paused_revision > active_revision);
        assert!(!audio.active.load(Ordering::Acquire));

        audio.deactivate();
        assert_eq!(audio.revision.load(Ordering::Acquire), paused_revision);
    }

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(retry_delay(0), Duration::from_millis(250));
        assert_eq!(retry_delay(1), Duration::from_millis(500));
        assert_eq!(retry_delay(4), Duration::from_secs(4));
        assert_eq!(retry_delay(5), Duration::from_secs(5));
        assert_eq!(retry_delay(u32::MAX), Duration::from_secs(5));
    }

    #[test]
    fn bounded_writer_queue_preserves_the_rtp_gap_when_it_drops_audio() {
        let mut samples = std::collections::VecDeque::new();
        for value in 0..=AUDIO_QUEUE_CAPACITY {
            enqueue_bounded_audio_sample(
                &mut samples,
                AudioSample {
                    data: Bytes::from(vec![value as u8]),
                    duration: Duration::from_millis(20),
                    prev_dropped_packets: 0,
                },
            );
        }

        assert_eq!(samples.len(), AUDIO_QUEUE_CAPACITY);
        let first_retained = samples.pop_front().unwrap();
        assert_eq!(first_retained.data[0], 1);
        assert_eq!(first_retained.prev_dropped_packets, 1);

        for value in 0..AUDIO_QUEUE_CAPACITY {
            enqueue_bounded_audio_sample(
                &mut samples,
                AudioSample {
                    data: Bytes::from(vec![value as u8]),
                    duration: Duration::from_millis(20),
                    prev_dropped_packets: 0,
                },
            );
        }
        assert!(samples.front().unwrap().prev_dropped_packets > 0);
    }

    #[test]
    fn process_exclusion_requires_exactly_one_resolved_process() {
        assert_eq!(resolve_excluded_pid(HashSet::new()).unwrap(), None);
        assert_eq!(resolve_excluded_pid(HashSet::from([42])).unwrap(), Some(42));
        assert!(resolve_excluded_pid(HashSet::from([42, 43])).is_err());
    }

    #[test]
    fn process_exclusion_finds_the_root_of_a_minimized_application_tree() {
        let processes = vec![
            RunningProcess {
                pid: 10,
                parent_pid: 1,
                name: "Discord.exe".to_owned(),
            },
            RunningProcess {
                pid: 11,
                parent_pid: 10,
                name: "Discord.exe".to_owned(),
            },
            RunningProcess {
                pid: 12,
                parent_pid: 10,
                name: "Discord.exe".to_owned(),
            },
            RunningProcess {
                pid: 20,
                parent_pid: 1,
                name: "browser.exe".to_owned(),
            },
        ];
        assert_eq!(
            resolve_process_tree_root(&["Discord".to_owned()], &processes).unwrap(),
            Some(10)
        );
        assert_eq!(
            resolve_process_tree_root(&["not-running".to_owned()], &processes).unwrap(),
            None
        );
    }
}
