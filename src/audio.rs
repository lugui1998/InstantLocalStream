use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use flexaudio::{OutputFormat, ProcessMode, SourceKind, Stream, StreamConfig as FlexAudioConfig};
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
const OPUS_FRAME_SAMPLES: usize = 960;
const STEREO_SAMPLES_PER_FRAME: usize = OPUS_FRAME_SAMPLES * 2;
const AUDIO_TICK: Duration = Duration::from_millis(20);
const RETRY_INITIAL_DELAY: Duration = Duration::from_millis(250);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct AudioTrack {
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    queue: Arc<AudioQueue>,
}

struct AudioSample {
    data: Bytes,
    duration: Duration,
}

struct AudioQueue {
    samples: Mutex<std::collections::VecDeque<AudioSample>>,
    notify: Notify,
    closed: AtomicBool,
}

pub struct AudioPipeline {
    subscribers: Arc<Mutex<HashMap<Uuid, AudioTrack>>>,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    ended: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    settings: Arc<Mutex<Option<CaptureSettings>>>,
    revision: Arc<AtomicU64>,
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
                    sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                    rtcp_feedback: vec![],
                },
                ..Default::default()
            }],
        ))?;
        let audio_track = AudioTrack {
            track: Arc::new(track),
            ssrc,
            queue: Arc::new(AudioQueue {
                samples: Mutex::new(std::collections::VecDeque::with_capacity(3)),
                notify: Notify::new(),
                closed: AtomicBool::new(false),
            }),
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

    fn run(&self) -> Result<()> {
        let mut encoder = OpusEncoder::new(48_000, 2, Application::Audio)
            .map_err(|error| anyhow::anyhow!(error))?;
        encoder.bitrate_bps = 96_000;
        encoder.complexity = 5;
        encoder.use_cbr = true;
        let mut encoded = vec![0_u8; 1_500];

        let mut retry_attempt = 0;
        let mut retry_revision = None;
        while !self.stop.load(Ordering::Acquire) {
            if !self.active.load(Ordering::Acquire) {
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
            let mut streams = match open_streams(&settings) {
                Ok(streams) => streams,
                Err(error) => {
                    self.record_failure(error.to_string());
                    self.wait_for_retry(revision, retry_attempt);
                    retry_attempt = retry_attempt.saturating_add(1);
                    continue;
                }
            };
            let mut start_failure = None;
            for stream in &mut streams {
                if let Err(error) = stream.start() {
                    start_failure = Some(error.to_string());
                    break;
                }
            }
            if let Some(error) = start_failure {
                for stream in &mut streams {
                    stream.stop();
                }
                self.record_failure(error);
                self.wait_for_retry(revision, retry_attempt);
                retry_attempt = retry_attempt.saturating_add(1);
                continue;
            }
            retry_attempt = 0;
            if let Ok(mut failure) = self.failure.lock() {
                *failure = None;
            }

            let mut mixer = ClockMixer::new(streams.len());
            while !self.stop.load(Ordering::Acquire)
                && self.revision.load(Ordering::Acquire) == revision
            {
                for (stream_index, stream) in streams.iter_mut().enumerate() {
                    while let Some(chunk) = stream.poll_chunk() {
                        mixer.push(stream_index, chunk.pts_ns, chunk.data);
                    }
                }
                if let Some(mixed) = mixer.mix_due(Instant::now())
                    && self.active.load(Ordering::Acquire)
                {
                    let size = encoder
                        .encode(&mixed, OPUS_FRAME_SAMPLES, &mut encoded)
                        .map_err(|error| anyhow::anyhow!(error))?;
                    self.broadcast(Bytes::copy_from_slice(&encoded[..size]));
                }
                thread::sleep(Duration::from_millis(2));
            }
            for stream in &mut streams {
                stream.stop();
            }
        }
        Ok(())
    }

    fn record_failure(&self, error: String) {
        if let Ok(mut failure) = self.failure.lock() {
            *failure = Some(error);
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

    fn broadcast(&self, data: Bytes) {
        let subscribers = self
            .subscribers
            .lock()
            .map(|subscribers| subscribers.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for subscriber in subscribers {
            subscriber.enqueue(AudioSample {
                data: data.clone(),
                duration: Duration::from_millis(20),
            });
        }
    }
}

/// Aligns independently delivered flexaudio chunks to one 20 ms output clock.
/// Chunks are timestamped by flexaudio's normalized monotonic clock, but callback
/// scheduling can be skewed between per-process capture streams.  The mixer only
/// emits once per tick and substitutes silence for a late stream, so a burst from
/// one callback can never advance RTP time ahead of the other streams.
struct ClockMixer {
    pending: Vec<VecDeque<(i64, Vec<f32>)>>,
    next_pts_ns: Option<i64>,
    next_deadline: Option<Instant>,
}

impl ClockMixer {
    fn new(stream_count: usize) -> Self {
        Self {
            pending: (0..stream_count)
                .map(|_| VecDeque::with_capacity(3))
                .collect(),
            next_pts_ns: None,
            next_deadline: None,
        }
    }

    fn push(&mut self, stream_index: usize, pts_ns: i64, data: Vec<f32>) {
        let Some(queue) = self.pending.get_mut(stream_index) else {
            return;
        };
        if queue.len() >= 3 {
            queue.pop_front();
        }
        queue.push_back((pts_ns, data));
        // Before the clock starts, anchor it to the earliest first chunk. Once
        // output has started, a late callback must not move the media clock
        // backwards.
        if self.next_deadline.is_none() {
            self.next_pts_ns = Some(
                self.next_pts_ns
                    .map_or(pts_ns, |current| current.min(pts_ns)),
            );
        }
    }

    fn mix_due(&mut self, now: Instant) -> Option<Vec<f32>> {
        let mut target_pts_ns = self.next_pts_ns?;
        let deadline = self.next_deadline.get_or_insert_with(|| now + AUDIO_TICK);
        if now < *deadline {
            return None;
        }

        // If the mixer task was descheduled for several ticks, advance both
        // clocks together. Emitting a catch-up burst would increase latency,
        // while advancing only the wall clock would leave every subsequent
        // capture chunk permanently ahead of the media clock.
        let missed_ticks = now.duration_since(*deadline).as_nanos() / AUDIO_TICK.as_nanos();
        let missed_ticks = i64::try_from(missed_ticks).unwrap_or(i64::MAX);
        let tick_ns = AUDIO_TICK.as_nanos() as i64;
        target_pts_ns = target_pts_ns.saturating_add(tick_ns.saturating_mul(missed_ticks));

        let mut mixed = vec![0.0_f32; STEREO_SAMPLES_PER_FRAME];
        for queue in &mut self.pending {
            while matches!(queue.front(), Some((pts_ns, _)) if *pts_ns < target_pts_ns) {
                queue.pop_front();
            }
            if let Some((pts_ns, data)) = queue.front()
                && *pts_ns <= target_pts_ns + AUDIO_TICK.as_nanos() as i64 / 2
            {
                for (destination, sample) in mixed.iter_mut().zip(data.iter()) {
                    *destination = (*destination + *sample).clamp(-1.0, 1.0);
                }
                queue.pop_front();
            }
        }
        self.next_pts_ns = Some(target_pts_ns.saturating_add(tick_ns));
        // Do not "catch up" after scheduling stalls: doing so would emit a
        // burst of RTP frames even though the capture callbacks were skewed.
        *deadline = now + AUDIO_TICK;
        Some(mixed)
    }
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

fn open_streams(settings: &CaptureSettings) -> Result<Vec<Stream>> {
    if settings.audio_mode == "window" {
        let pid = selected_window_pid(settings)?;
        return open_process_streams([pid]);
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
            ..Default::default()
        })
        .map_err(|error| anyhow::anyhow!(error))?;
        return Ok(vec![stream]);
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
    let excluded_pid = resolve_excluded_pid(&settings.excluded_audio_processes, excluded_pids)?;
    flexaudio::open(FlexAudioConfig {
        // flexaudio exposes a native system-minus-one-process primitive.  It
        // cannot represent multiple independent exclusions in one stream, and
        // opening several such streams would duplicate all non-excluded audio.
        kind: SourceKind::ProcessLoopback,
        target_pid: Some(excluded_pid),
        mode: ProcessMode::Exclude,
        output,
        ..Default::default()
    })
    .map(|stream| vec![stream])
    .map_err(|error| anyhow::anyhow!(error))
}

fn resolve_excluded_pid(requested_names: &[String], excluded_pids: HashSet<u32>) -> Result<u32> {
    let result: Result<u32> = match excluded_pids.len() {
        1 => Ok(*excluded_pids.iter().next().expect("length checked")),
        0 => anyhow::bail!(
            "cannot enforce audio exclusion: none of the requested processes are currently discoverable"
        ),
        _ => anyhow::bail!(
            "cannot enforce audio exclusion for multiple processes with the current audio backend"
        ),
    };
    result.with_context(|| format!("requested exclusions: {}", requested_names.join(", ")))
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

fn open_process_streams(pids: impl IntoIterator<Item = u32>) -> Result<Vec<Stream>> {
    pids.into_iter()
        .map(|pid| {
            flexaudio::open(FlexAudioConfig {
                kind: SourceKind::ProcessLoopback,
                target_pid: Some(pid),
                mode: ProcessMode::Include,
                output: OutputFormat {
                    sample_rate: 48_000,
                    channels: 2,
                },
                ..Default::default()
            })
            .map_err(|error| anyhow::anyhow!(error))
        })
        .collect()
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
            if samples.len() >= 3 {
                samples.pop_front();
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
                    ..Default::default()
                })
                .await
                .is_err()
            {
                return;
            }
        }
    }
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
    fn clock_mixer_emits_one_frame_per_tick_despite_skewed_callbacks() {
        let start = Instant::now();
        let mut mixer = ClockMixer::new(2);
        mixer.push(0, 1_000, vec![0.2; STEREO_SAMPLES_PER_FRAME]);
        mixer.push(1, 1_000, vec![0.3; STEREO_SAMPLES_PER_FRAME]);

        assert!(mixer.mix_due(start).is_none());
        let first = mixer.mix_due(start + AUDIO_TICK).expect("first tick");
        assert_eq!(first, vec![0.5; STEREO_SAMPLES_PER_FRAME]);

        // A fast callback from just one process must wait for the next output
        // tick, and the missing stream contributes silence instead of advancing
        // RTP time a second time in the same tick.
        mixer.push(
            0,
            1_000 + AUDIO_TICK.as_nanos() as i64,
            vec![0.4; STEREO_SAMPLES_PER_FRAME],
        );
        assert!(mixer.mix_due(start + AUDIO_TICK).is_none());
        let second = mixer.mix_due(start + AUDIO_TICK * 2).expect("second tick");
        assert_eq!(second, vec![0.4; STEREO_SAMPLES_PER_FRAME]);

        // A late chunk from an earlier tick is discarded; it cannot rewind
        // the output clock or produce another frame for an old timestamp.
        mixer.push(1, 1_000, vec![0.9; STEREO_SAMPLES_PER_FRAME]);
        assert!(mixer.mix_due(start + AUDIO_TICK * 2).is_none());
        let third = mixer.mix_due(start + AUDIO_TICK * 3).expect("third tick");
        assert_eq!(third, vec![0.0; STEREO_SAMPLES_PER_FRAME]);
    }

    #[test]
    fn clock_mixer_reanchors_media_time_after_scheduler_stall() {
        let mut mixer = ClockMixer::new(1);
        let started_at = Instant::now();
        mixer.push(0, 0, vec![0.25; STEREO_SAMPLES_PER_FRAME]);
        assert!(mixer.mix_due(started_at).is_none());
        assert!(mixer.mix_due(started_at + AUDIO_TICK).is_some());

        let stalled_until = started_at + AUDIO_TICK * 6;
        mixer.push(
            0,
            (AUDIO_TICK.as_nanos() as i64) * 5,
            vec![0.5; STEREO_SAMPLES_PER_FRAME],
        );
        let mixed = mixer.mix_due(stalled_until).expect("stalled tick is due");
        assert!(
            mixed
                .iter()
                .all(|sample| (*sample - 0.5).abs() < f32::EPSILON)
        );
    }

    #[test]
    fn process_exclusion_requires_exactly_one_resolved_process() {
        assert!(resolve_excluded_pid(&["browser".to_owned()], HashSet::new()).is_err());
        assert_eq!(
            resolve_excluded_pid(&["browser".to_owned()], HashSet::from([42])).unwrap(),
            42
        );
        assert!(resolve_excluded_pid(&["browser".to_owned()], HashSet::from([42, 43])).is_err());
    }
}
