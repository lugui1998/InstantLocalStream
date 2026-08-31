use std::collections::{HashMap, HashSet};
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
pub const OPUS_FMTP_LINE: &str = "minptime=10;stereo=1;sprop-stereo=1;maxaveragebitrate=256000";
const OPUS_FRAME_SAMPLES: usize = 960;
const STEREO_SAMPLES_PER_FRAME: usize = OPUS_FRAME_SAMPLES * 2;
const OPUS_BITRATE_BPS: i32 = 256_000;
const OPUS_COMPLEXITY: i32 = 10;
const AUDIO_QUEUE_CAPACITY: usize = 10;
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
    prev_dropped_packets: u16,
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
        let _audio_priority = prioritize_audio_thread();
        let mut encoder = high_quality_opus_encoder()?;
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
            retry_attempt = 0;
            if let Ok(mut failure) = self.failure.lock() {
                *failure = None;
            }
            let mut previous_capture_drops = 0;
            let mut pending_dropped_packets = 0_u32;

            while !self.stop.load(Ordering::Acquire)
                && self.revision.load(Ordering::Acquire) == revision
            {
                let mut encoded_any = false;
                while let Some(chunk) = stream.poll_chunk() {
                    encoded_any = true;
                    let newly_dropped =
                        cumulative_drop_delta(&mut previous_capture_drops, chunk.dropped_before);
                    if !self.active.load(Ordering::Acquire) {
                        // Starting playback establishes a fresh RTP timeline;
                        // capture loss while nobody is listening is irrelevant.
                        pending_dropped_packets = 0;
                        continue;
                    }
                    pending_dropped_packets = pending_dropped_packets.saturating_add(newly_dropped);
                    if chunk.frames != OPUS_FRAME_SAMPLES
                        || chunk.data.len() != STEREO_SAMPLES_PER_FRAME
                    {
                        tracing::warn!(
                            frames = chunk.frames,
                            samples = chunk.data.len(),
                            "discarding malformed normalized audio chunk"
                        );
                        pending_dropped_packets = pending_dropped_packets.saturating_add(1);
                        continue;
                    }
                    let size = encoder
                        .encode(&chunk.data, OPUS_FRAME_SAMPLES, &mut encoded)
                        .map_err(|error| anyhow::anyhow!(error))?;
                    let rtp_gap = pending_dropped_packets.min(u16::MAX as u32) as u16;
                    pending_dropped_packets = 0;
                    self.broadcast(Bytes::copy_from_slice(&encoded[..size]), rtp_gap);
                }
                if !encoded_any {
                    // flexaudio already produces normalized 20 ms chunks. Do
                    // not re-clock them against another wall timer: that used
                    // to discard PCM or insert hard silence after scheduling
                    // jitter, which was directly audible as crackle.
                    thread::sleep(Duration::from_millis(2));
                }
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

fn cumulative_drop_delta(previous: &mut u32, current: u32) -> u32 {
    // flexaudio stores the stream's cumulative native-ring drop total on
    // every chunk. Only the increase since the preceding chunk belongs to
    // this RTP sample. Treat a counter reset as a fresh cumulative value.
    let delta = current.checked_sub(*previous).unwrap_or(current);
    *previous = current;
    delta
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
            enqueue_bounded_audio_sample(&mut samples, sample);
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
                return;
            }
        }
    }
}

fn enqueue_bounded_audio_sample(
    samples: &mut std::collections::VecDeque<AudioSample>,
    mut sample: AudioSample,
) {
    if samples.len() >= AUDIO_QUEUE_CAPACITY
        && let Some(dropped) = samples.pop_front()
    {
        let skipped = dropped.prev_dropped_packets.saturating_add(1);
        if let Some(next) = samples.front_mut() {
            next.prev_dropped_packets = next.prev_dropped_packets.saturating_add(skipped);
        } else {
            sample.prev_dropped_packets = sample.prev_dropped_packets.saturating_add(skipped);
        }
    }
    samples.push_back(sample);
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
    fn cumulative_capture_drop_counts_become_per_sample_rtp_gaps() {
        let mut previous = 0;
        let deltas = [0, 1, 1, 2].map(|current| cumulative_drop_delta(&mut previous, current));
        assert_eq!(deltas, [0, 1, 0, 1]);

        // A native stream may reset its counter without manufacturing a
        // huge wrapping delta.
        assert_eq!(cumulative_drop_delta(&mut previous, 0), 0);
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
