use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread;
use std::time::Duration;

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
            "InstantLocalStream audio".to_owned(),
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
        self.active.store(true, Ordering::Release);
    }

    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
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

        while !self.stop.load(Ordering::Acquire) {
            let revision = self.revision.load(Ordering::Acquire);
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
                    self.wait_for_reconfiguration(revision);
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
                self.wait_for_reconfiguration(revision);
                continue;
            }
            if let Ok(mut failure) = self.failure.lock() {
                *failure = None;
            }

            while !self.stop.load(Ordering::Acquire)
                && self.revision.load(Ordering::Acquire) == revision
            {
                let mut mixed = vec![0.0_f32; 1_920];
                let mut received = false;
                for stream in &mut streams {
                    let mut latest = None;
                    while let Some(chunk) = stream.poll_chunk() {
                        latest = Some(chunk);
                    }
                    if let Some(chunk) = latest {
                        received = true;
                        for (destination, sample) in mixed.iter_mut().zip(chunk.data.iter()) {
                            *destination = (*destination + *sample).clamp(-1.0, 1.0);
                        }
                    }
                }
                if received && self.active.load(Ordering::Acquire) {
                    let size = encoder
                        .encode(&mixed, 960, &mut encoded)
                        .map_err(|error| anyhow::anyhow!(error))?;
                    self.broadcast(Bytes::copy_from_slice(&encoded[..size]));
                }
                if !received {
                    thread::sleep(Duration::from_millis(2));
                }
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

    fn wait_for_reconfiguration(&self, revision: u64) {
        while !self.stop.load(Ordering::Acquire)
            && self.revision.load(Ordering::Acquire) == revision
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
    let mut included_pids = HashSet::new();
    let mut excluded_process_found = false;
    for source in capture::list_windows().unwrap_or_default() {
        let name = source.name.to_ascii_lowercase();
        if excluded.iter().any(|excluded| name.contains(excluded)) {
            excluded_process_found = true;
            continue;
        }
        if let Some(pid) = source.pid {
            included_pids.insert(pid);
        }
    }
    if !excluded_process_found {
        let stream = flexaudio::open(FlexAudioConfig {
            kind: SourceKind::SystemLoopback,
            output,
            ..Default::default()
        })
        .map_err(|error| anyhow::anyhow!(error))?;
        return Ok(vec![stream]);
    }
    open_process_streams(included_pids)
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
        let previous = CaptureSettings::from_config(&AppConfig::default());
        let mut disabled = previous.clone();
        disabled.audio_mode = "off".to_owned();
        assert!(audio_input_changed(&previous, &disabled));

        let mut exclusions = previous.clone();
        exclusions
            .excluded_audio_processes
            .push("example".to_owned());
        assert!(audio_input_changed(&previous, &exclusions));
    }
}
