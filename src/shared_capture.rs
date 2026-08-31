//! A single shared capture producer with latest-frame fan-out for video encoders.
//!
//! Consumers create one [`SharedCapture`] and give each encoder variant its own
//! [`SourceSubscription`].  A subscriber that cannot keep up skips directly to the
//! newest complete frame instead of accumulating latency.

use std::ffi::{OsStr, OsString};
use std::io::{ErrorKind, Read};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::media::CaptureSettings;
#[cfg(windows)]
use crate::window_capture::WindowCaptureUpdate;

const STARTUP_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const READER_STOP_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(windows)]
const WGC_FIRST_FRAME_TIMEOUT: Duration = Duration::from_millis(750);

/// Pixel representation of a frame published to encoder variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourcePixelFormat {
    Yuv420p,
    Bgra,
}

impl SourcePixelFormat {
    pub const fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::Yuv420p => "yuv420p",
            Self::Bgra => "bgra",
        }
    }

    fn frame_size(self, width: u32, height: u32) -> Result<usize> {
        match self {
            Self::Yuv420p => yuv420p_frame_size(width, height),
            Self::Bgra => packed_frame_size(width, height),
        }
    }
}

/// A complete raw frame emitted by the shared capture source.
#[derive(Clone, Debug)]
pub struct SourceFrame {
    pub width: u32,
    pub height: u32,
    pub pixel_format: SourcePixelFormat,
    /// Wall-clock timestamp used for per-rendered-frame capture correlation.
    /// absolute-capture-time extension so receivers can measure the rendered
    /// frame's capture-to-display delay.
    pub captured_at_unix_nanos: u64,
    /// Raw bytes in [`pixel_format`](Self::pixel_format).
    pub data: Arc<[u8]>,
}

#[derive(Default)]
struct LatestFrame {
    sequence: u64,
    frame: Option<SourceFrame>,
    ended: bool,
    failure: Option<String>,
}

#[derive(Clone, Copy)]
struct ObservedSourceDimensions {
    generation: u64,
    packed: u64,
}

fn observed_source_dimensions_state(
    generation: u64,
    dimensions: (u32, u32),
) -> Mutex<ObservedSourceDimensions> {
    Mutex::new(ObservedSourceDimensions {
        generation,
        packed: pack_dimensions(dimensions),
    })
}

struct SharedCaptureInner {
    format: Mutex<CaptureFormat>,
    /// Latest native compositor dimensions observed by the window backend.
    /// The published frame bus remains at `format` until the server has
    /// debounced the resize and can replace all encoders as one transaction.
    observed_source_dimensions: Mutex<ObservedSourceDimensions>,
    backend: Mutex<&'static str>,
    stopped: AtomicBool,
    generation: AtomicU64,
    restart_lock: Mutex<()>,
    child: Mutex<Option<Child>>,
    reader: Mutex<Option<JoinHandle<()>>>,
    latest: Mutex<LatestFrame>,
    frame_ready: Condvar,
}

/// The raw frame format currently published by the capture process.
///
/// This deliberately lives behind a mutex instead of being fixed for the
/// lifetime of `SharedCapture`: choosing a different monitor can legitimately
/// change the source aspect ratio.  Encoder variants are recreated by the
/// server when this changes, so they never have to reinterpret a frame from a
/// new format as the old one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CaptureFormat {
    width: u32,
    height: u32,
    fps: u32,
    pixel_format: SourcePixelFormat,
}

/// One FFmpeg producer and its latest raw frame.
pub struct SharedCapture {
    inner: Arc<SharedCaptureInner>,
    ffmpeg: OsString,
}

/// A cursor over a [`SharedCapture`]'s latest frame.
///
/// Each subscription holds at most one frame logically: it remembers a sequence
/// number and reads the producer's single latest-frame slot.
pub struct SourceSubscription {
    inner: Arc<SharedCaptureInner>,
    seen_sequence: u64,
}

impl SharedCapture {
    /// Starts the appropriate capture producer and exposes its latest frames.
    pub fn start(ffmpeg: impl AsRef<OsStr>, settings: CaptureSettings) -> Result<Self> {
        let format = target_format(&settings)?;
        let inner = Arc::new(SharedCaptureInner {
            format: Mutex::new(format),
            observed_source_dimensions: observed_source_dimensions_state(
                1,
                (format.width, format.height),
            ),
            backend: Mutex::new("starting"),
            stopped: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            restart_lock: Mutex::new(()),
            child: Mutex::new(None),
            reader: Mutex::new(None),
            latest: Mutex::new(LatestFrame::default()),
            frame_ready: Condvar::new(),
        });
        let capture = Self {
            inner,
            ffmpeg: ffmpeg.as_ref().to_os_string(),
        };
        let previous_sequence = capture.latest_sequence()?;
        capture.start_producer(&settings)?;
        if let Err(error) = capture.wait_for_new_frame(previous_sequence) {
            let backend = capture.backend_name();
            let observed = capture.observed_source_dimensions();
            let _ = capture.stop();
            return Err(error.context(format!(
                "shared FFmpeg capture did not produce an initial frame (backend={backend}, expected={}x{}, observed={}x{})",
                format.width, format.height, observed.0, observed.1
            )));
        }
        Ok(capture)
    }

    /// Creates an independent latest-frame cursor for an encoder variant.
    pub fn subscribe(&self) -> SourceSubscription {
        // Start after the frame currently in the single-slot cache.  This is
        // important across a live capture restart: replacement encoders must
        // wait for the first frame from the new producer rather than consuming
        // a cached frame whose dimensions belong to the previous source.
        let seen_sequence = self
            .inner
            .latest
            .lock()
            .map(|latest| latest.sequence)
            .unwrap_or_default();
        SourceSubscription {
            inner: Arc::clone(&self.inner),
            seen_sequence,
        }
    }

    /// Creates a cursor that can consume the current cached frame. This is
    /// used only for the primary encoder immediately after a fresh capture
    /// starts; unlike a replacement encoder, that frame belongs to the new
    /// capture generation and may be the only frame available from a static
    /// window until its compositor produces another update.
    pub fn subscribe_including_latest(&self) -> SourceSubscription {
        let seen_sequence = self
            .inner
            .latest
            .lock()
            .map(|latest| latest.sequence.saturating_sub(1))
            .unwrap_or_default();
        SourceSubscription {
            inner: Arc::clone(&self.inner),
            seen_sequence,
        }
    }

    /// Returns the current latest frame without waiting or advancing an
    /// encoder subscription. Pixel storage remains shared through its `Arc`,
    /// so thumbnail consumers do not copy a full capture frame.
    pub fn latest_frame_snapshot(&self) -> Option<SourceFrame> {
        self.inner
            .latest
            .lock()
            .ok()
            .and_then(|latest| latest.frame.clone())
    }

    /// Switches the producer to a new monitor, window, or test source.
    ///
    /// The output format is recalculated from the selected source.  A monitor
    /// switch therefore preserves its native display aspect ratio instead of
    /// stretching or padding it into the previous monitor's frame canvas.
    /// Callers must recreate the encoder variants after this returns, because
    /// their raw input dimensions may have changed.
    pub fn restart(&self, settings: &CaptureSettings) -> Result<()> {
        self.restart_with_format(settings, target_format(settings)?)
    }

    fn restart_with_format(&self, settings: &CaptureSettings, format: CaptureFormat) -> Result<()> {
        let _restart = self
            .inner
            .restart_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("shared capture restart lock poisoned"))?;
        if self.inner.stopped.load(Ordering::Acquire) {
            bail!("shared capture has already stopped")
        }
        let previous_sequence = self.latest_sequence()?;
        self.stop_producer()?;
        if self.inner.stopped.load(Ordering::Acquire) {
            bail!("shared capture stopped while its producer was restarting")
        }
        if let Ok(mut current) = self.inner.format.lock() {
            *current = format;
        }
        if let Ok(mut observed) = self.inner.observed_source_dimensions.lock() {
            observed.generation = self.inner.generation.load(Ordering::Acquire);
            observed.packed = pack_dimensions((format.width, format.height));
        }
        if let Err(error) = self
            .start_producer(settings)
            .and_then(|()| self.wait_for_new_frame(previous_sequence))
        {
            let _ = self.stop_producer();
            return Err(error.context("shared FFmpeg capture did not become ready"));
        }
        Ok(())
    }

    /// Stops FFmpeg and waits for the blocking reader thread to exit.
    pub fn stop(&self) -> Result<()> {
        let _restart = self
            .inner
            .restart_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("shared capture restart lock poisoned"))?;
        let first_stop = !self.inner.stopped.swap(true, Ordering::AcqRel);
        self.stop_producer()?;
        if first_stop {
            finish_reader(&self.inner, None);
        }
        Ok(())
    }

    /// Immediately invalidates the current producer without waiting for its
    /// owner thread or the restart mutex. This is the emergency half of Stop:
    /// a potentially stuck Windows capture callback may remain detached, but
    /// it can no longer publish and any FFmpeg child is asked to terminate.
    pub fn request_stop(&self) {
        self.inner.stopped.store(true, Ordering::Release);
        advance_capture_generation(&self.inner);
        if let Ok(mut child) = self.inner.child.try_lock()
            && let Some(child) = child.as_mut()
        {
            let _ = child.kill();
        }
        finish_reader(&self.inner, None);
        self.inner.frame_ready.notify_all();
    }

    pub fn source_dimensions(&self) -> (u32, u32) {
        self.inner
            .format
            .lock()
            .map(|format| (format.width, format.height))
            .unwrap_or((2, 2))
    }

    /// Returns the most recent native source dimensions reported by the live
    /// window compositor, which may temporarily differ from the stable frame
    /// bus while an interactive resize is being debounced.
    pub fn observed_source_dimensions(&self) -> (u32, u32) {
        self.inner
            .observed_source_dimensions
            .lock()
            .map(|observed| unpack_dimensions(observed.packed))
            .unwrap_or_else(|_| self.source_dimensions())
    }

    pub fn source_fps(&self) -> u32 {
        self.inner
            .format
            .lock()
            .map(|format| format.fps)
            .unwrap_or(1)
    }

    pub fn source_pixel_format(&self) -> SourcePixelFormat {
        self.inner
            .format
            .lock()
            .map(|format| format.pixel_format)
            .unwrap_or(SourcePixelFormat::Yuv420p)
    }

    pub fn backend_name(&self) -> &'static str {
        self.inner
            .backend
            .lock()
            .map(|backend| *backend)
            .unwrap_or("unknown")
    }

    pub fn failure(&self) -> Option<String> {
        self.inner
            .latest
            .lock()
            .ok()
            .and_then(|latest| latest.failure.clone())
    }

    fn start_producer(&self, settings: &CaptureSettings) -> Result<()> {
        let format = self
            .inner
            .format
            .lock()
            .map(|format| *format)
            .map_err(|_| anyhow::anyhow!("shared capture format lock poisoned"))?;
        #[cfg(windows)]
        if settings.source_kind == "window" {
            return self.start_window_producer(settings, format);
        }
        let frame_size = format
            .pixel_format
            .frame_size(format.width, format.height)?;
        let args = ffmpeg_args(settings, format.width, format.height, format.fps)?;
        let mut command = Command::new(&self.ffmpeg);
        hide_console(&mut command);
        let child = command
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| "start shared FFmpeg capture")?;
        if let Ok(mut backend) = self.inner.backend.lock() {
            *backend = "ffmpeg";
        }
        {
            let mut current = self
                .inner
                .child
                .lock()
                .map_err(|_| anyhow::anyhow!("shared capture child lock poisoned"))?;
            *current = Some(child);
        }
        if let Ok(mut latest) = self.inner.latest.lock() {
            latest.ended = false;
            latest.failure = None;
        }
        let generation = self.inner.generation.load(Ordering::Acquire);
        let reader = match spawn_reader(Arc::clone(&self.inner), format, frame_size, generation) {
            Ok(reader) => reader,
            Err(error) => {
                if let Ok(mut child) = self.inner.child.lock()
                    && let Some(mut child) = child.take()
                {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                return Err(error);
            }
        };
        *self
            .inner
            .reader
            .lock()
            .map_err(|_| anyhow::anyhow!("shared capture reader lock poisoned"))? = Some(reader);
        Ok(())
    }

    #[cfg(windows)]
    fn start_window_producer(
        &self,
        settings: &CaptureSettings,
        format: CaptureFormat,
    ) -> Result<()> {
        if let Ok(mut latest) = self.inner.latest.lock() {
            latest.ended = false;
            latest.failure = None;
        }
        let generation = self.inner.generation.load(Ordering::Acquire);
        // Windows Graphics Capture objects are apartment-sensitive. Create and
        // own the session inside its dedicated reader thread instead of moving
        // a live session from Tokio's control thread across a COM boundary.
        let reader = spawn_window_reader(
            Arc::clone(&self.inner),
            settings.source_index,
            settings.source_native_id,
            settings.draw_mouse,
            format,
            generation,
        );
        *self
            .inner
            .reader
            .lock()
            .map_err(|_| anyhow::anyhow!("shared capture reader lock poisoned"))? = Some(reader);
        Ok(())
    }

    fn latest_sequence(&self) -> Result<u64> {
        self.inner
            .latest
            .lock()
            .map(|latest| latest.sequence)
            .map_err(|_| anyhow::anyhow!("shared capture frame lock poisoned"))
    }

    /// Verifies that a newly started producer actually emitted a raw frame
    /// before a live source switch is committed.
    fn wait_for_new_frame(&self, previous_sequence: u64) -> Result<()> {
        let deadline = Instant::now() + STARTUP_FRAME_TIMEOUT;
        let mut latest = self
            .inner
            .latest
            .lock()
            .map_err(|_| anyhow::anyhow!("shared capture frame lock poisoned"))?;
        loop {
            if latest.ended {
                let detail = latest
                    .failure
                    .as_deref()
                    .unwrap_or("capture producer stopped before a frame was available");
                bail!("{detail}");
            }
            if latest.sequence > previous_sequence && latest.frame.is_some() {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "capture producer did not produce a frame within {} seconds",
                    STARTUP_FRAME_TIMEOUT.as_secs()
                );
            }
            let (next, _) = self
                .inner
                .frame_ready
                .wait_timeout(latest, remaining)
                .map_err(|_| anyhow::anyhow!("shared capture frame lock poisoned"))?;
            latest = next;
        }
    }

    fn stop_producer(&self) -> Result<()> {
        advance_capture_generation(&self.inner);
        if let Ok(mut child) = self.inner.child.lock()
            && let Some(child) = child.as_mut()
        {
            let _ = child.kill();
        }
        self.inner.frame_ready.notify_all();
        let reader = self
            .inner
            .reader
            .lock()
            .map_err(|_| anyhow::anyhow!("shared capture reader lock poisoned"))?
            .take();
        if let Some(reader) = reader {
            let deadline = Instant::now() + READER_STOP_TIMEOUT;
            while !reader.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if reader.is_finished() {
                reader
                    .join()
                    .map_err(|_| anyhow::anyhow!("shared capture reader thread panicked"))?;
            } else {
                // Do not let a driver-level WGC/pipe stall permanently block
                // Stop or every later command. Dropping a JoinHandle detaches
                // the old generation; it can no longer publish and will exit
                // normally if the underlying API call returns.
                tracing::warn!(
                    timeout_ms = READER_STOP_TIMEOUT.as_millis(),
                    "capture reader did not stop in time; detaching stale generation"
                );
            }
        }
        if let Ok(mut child) = self.inner.child.lock()
            && let Some(mut child) = child.take()
        {
            let _ = child.wait();
        }
        Ok(())
    }
}

impl Drop for SharedCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl SourceSubscription {
    /// Waits for a newer frame, returning `None` once the source has stopped.
    pub fn recv(&mut self) -> Result<Option<SourceFrame>> {
        let mut latest = self
            .inner
            .latest
            .lock()
            .map_err(|_| anyhow::anyhow!("shared capture frame lock poisoned"))?;
        while latest.sequence <= self.seen_sequence && !latest.ended {
            latest = self
                .inner
                .frame_ready
                .wait(latest)
                .map_err(|_| anyhow::anyhow!("shared capture frame lock poisoned"))?;
        }
        let sequence = latest.sequence;
        let frame = latest.frame.clone();
        let failure = latest.failure.clone();
        drop(latest);
        self.take_newest(sequence, frame, failure)
    }

    fn take_newest(
        &mut self,
        sequence: u64,
        frame: Option<SourceFrame>,
        failure: Option<String>,
    ) -> Result<Option<SourceFrame>> {
        if sequence > self.seen_sequence {
            self.seen_sequence = sequence;
            return Ok(frame);
        }
        if let Some(failure) = failure {
            bail!("shared FFmpeg capture failed: {failure}");
        }
        Ok(None)
    }
}

fn spawn_reader(
    inner: Arc<SharedCaptureInner>,
    format: CaptureFormat,
    frame_size: usize,
    generation: u64,
) -> Result<JoinHandle<()>> {
    let stdout = inner
        .child
        .lock()
        .map_err(|_| anyhow::anyhow!("shared capture child lock poisoned"))?
        .as_mut()
        .and_then(|child| child.stdout.take())
        .context("shared FFmpeg capture did not expose stdout")?;
    Ok(thread::spawn(move || {
        let mut stdout = stdout;
        let frame_duration = Duration::from_secs_f64(1.0 / format.fps.max(1) as f64);
        loop {
            if inner.stopped.load(Ordering::Acquire)
                || inner.generation.load(Ordering::Acquire) != generation
            {
                break;
            }
            let frame_started = Instant::now();
            let mut bytes = vec![0_u8; frame_size];
            match stdout.read_exact(&mut bytes) {
                Ok(()) => {
                    if inner.stopped.load(Ordering::Acquire)
                        || inner.generation.load(Ordering::Acquire) != generation
                    {
                        break;
                    }
                    publish_frame_for_generation(&inner, format, Arc::from(bytes), generation);
                    let elapsed = frame_started.elapsed();
                    if elapsed < frame_duration {
                        thread::sleep(frame_duration - elapsed);
                    }
                }
                Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
                    if !inner.stopped.load(Ordering::Acquire)
                        && inner.generation.load(Ordering::Acquire) == generation
                    {
                        finish_reader_for_generation(
                            &inner,
                            generation,
                            Some(
                                "FFmpeg ended before a complete raw frame was available".to_owned(),
                            ),
                        );
                    }
                    break;
                }
                Err(error) => {
                    if !inner.stopped.load(Ordering::Acquire)
                        && inner.generation.load(Ordering::Acquire) == generation
                    {
                        finish_reader_for_generation(&inner, generation, Some(error.to_string()));
                    }
                    break;
                }
            }
        }
        if inner.generation.load(Ordering::Acquire) == generation
            && let Ok(mut child) = inner.child.lock()
            && let Some(mut child) = child.take()
        {
            let _ = child.wait();
        }
    }))
}

#[cfg(windows)]
#[derive(Clone)]
struct CapturedWindowFrame {
    data: Arc<[u8]>,
    captured_at_unix_nanos: u64,
}

#[cfg(windows)]
impl CapturedWindowFrame {
    fn new(data: Arc<[u8]>) -> Self {
        let captured_at_unix_nanos = capture_timestamp();
        Self {
            data,
            captured_at_unix_nanos,
        }
    }

    fn publish(&self, inner: &SharedCaptureInner, format: CaptureFormat, generation: u64) {
        publish_frame_at_for_generation(
            inner,
            format,
            Arc::clone(&self.data),
            self.captured_at_unix_nanos,
            generation,
        );
    }

    fn publish_repeat(&self, inner: &SharedCaptureInner, format: CaptureFormat, generation: u64) {
        // A repeated compositor surface is a new sample of unchanged content.
        // Timestamp it at publication time so latency measures the live
        // capture/encode path rather than how long the pixels stayed unchanged.
        publish_frame_for_generation(inner, format, Arc::clone(&self.data), generation);
    }
}

#[cfg(windows)]
fn spawn_window_reader(
    inner: Arc<SharedCaptureInner>,
    source_index: usize,
    source_native_id: Option<u64>,
    capture_cursor: bool,
    format: CaptureFormat,
    generation: u64,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let frame_duration = Duration::from_secs_f64(1.0 / format.fps.max(1) as f64);
        let wgc_capture = match crate::window_capture::WindowCapture::start(
            source_index,
            source_native_id,
            capture_cursor,
            frame_duration,
        ) {
            Ok(capture) if capture.dimensions() == (format.width, format.height) => Some(capture),
            Ok(capture) => {
                tracing::warn!(
                    expected_width = format.width,
                    expected_height = format.height,
                    actual_width = capture.dimensions().0,
                    actual_height = capture.dimensions().1,
                    "WGC dimensions differ from the selected window; rejecting this capture generation"
                );
                capture.stop();
                None
            }
            Err(error) => {
                tracing::warn!(%error, "WGC startup failed");
                None
            }
        };
        if let Some(capture) = wgc_capture.as_ref() {
            let first_frame_deadline = Instant::now() + WGC_FIRST_FRAME_TIMEOUT;
            let mut latest_frame: Option<CapturedWindowFrame> = None;
            while Instant::now() < first_frame_deadline {
                if inner.stopped.load(Ordering::Acquire)
                    || inner.generation.load(Ordering::Acquire) != generation
                {
                    capture.stop();
                    return;
                }
                if capture.is_minimized() {
                    thread::sleep(frame_duration);
                    continue;
                }
                capture.refresh_cursor_visibility();
                let next_frame = capture.next_update(frame_duration);
                if capture.is_minimized() {
                    // The window can minimize while next_frame is waiting.
                    // Never promote that transition surface as the initial
                    // full-window frame.
                    continue;
                }
                match next_frame {
                    Ok(Some(WindowCaptureUpdate::Frame(frame)))
                        if frame.width == format.width && frame.height == format.height =>
                    {
                        record_observed_source_dimensions(
                            &inner,
                            generation,
                            (frame.source_width, frame.source_height),
                        );
                        latest_frame = Some(CapturedWindowFrame::new(Arc::from(frame.pixels)));
                        break;
                    }
                    Ok(Some(WindowCaptureUpdate::Frame(frame))) => {
                        record_observed_source_dimensions(
                            &inner,
                            generation,
                            (frame.source_width, frame.source_height),
                        );
                        tracing::warn!(
                            expected_width = format.width,
                            expected_height = format.height,
                            actual_width = frame.width,
                            actual_height = frame.height,
                            "WGC frame dimensions changed during startup"
                        );
                        break;
                    }
                    Ok(Some(WindowCaptureUpdate::Resized(dimensions))) => {
                        record_observed_source_dimensions(&inner, generation, dimensions);
                        tracing::debug!(
                            width = dimensions.0,
                            height = dimensions.1,
                            "window resized during WGC startup; deferring to a fresh capture session"
                        );
                        break;
                    }
                    Ok(None) => {
                        if capture.is_closed() {
                            break;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "WGC did not deliver its first frame");
                        break;
                    }
                }
            }

            if let Some(mut latest_frame) = latest_frame {
                if let Ok(mut backend) = inner.backend.lock() {
                    *backend = "windows-graphics-capture";
                }
                tracing::info!("using Windows Graphics Capture window backend");
                // The first frame was held back while deciding whether WGC
                // had started successfully. Publish it before waiting for a
                // second compositor event; a static window may not produce
                // another event before the startup readiness timeout.
                latest_frame.publish(&inner, format, generation);
                let mut was_minimized = false;
                loop {
                    if inner.stopped.load(Ordering::Acquire)
                        || inner.generation.load(Ordering::Acquire) != generation
                    {
                        break;
                    }
                    if source_native_id
                        .is_some_and(|native_id| !crate::capture::native_window_exists(native_id))
                    {
                        tracing::warn!("selected capture window closed");
                        finish_reader_for_generation(
                            &inner,
                            generation,
                            Some("selected capture window closed".to_owned()),
                        );
                        break;
                    }
                    let frame_started = Instant::now();
                    let minimized = capture.is_minimized();
                    if minimized {
                        // PrintWindow and WGC commonly expose only the title
                        // bar while a window is minimized. Drain those frames
                        // and retain the last complete compositor surface.
                        let _ = capture.next_update(Duration::ZERO);
                        latest_frame.publish_repeat(&inner, format, generation);
                        was_minimized = true;
                        let elapsed = frame_started.elapsed();
                        if elapsed < frame_duration {
                            thread::sleep(frame_duration - elapsed);
                        }
                        continue;
                    }
                    // WGC is event-driven and may not deliver another frame
                    // while the captured window is completely static. Keep
                    // publishing the most recent pixels at the configured
                    // capture cadence so encoders can start, late viewers can
                    // receive a keyframe, and RTP timing continues to advance.
                    let mut candidate_frame = None;
                    capture.refresh_cursor_visibility();
                    match capture.next_update(frame_duration) {
                        Ok(Some(WindowCaptureUpdate::Frame(frame)))
                            if frame.width == format.width && frame.height == format.height =>
                        {
                            candidate_frame = Some((
                                CapturedWindowFrame::new(Arc::from(frame.pixels)),
                                (frame.source_width, frame.source_height),
                            ));
                        }
                        Ok(Some(WindowCaptureUpdate::Frame(frame))) => {
                            // A compositor resize can race the first few
                            // frames after startup. Keep the producer alive
                            // and let the resize monitor replace the graph
                            // once the new native dimensions settle; ending
                            // the reader here makes the encoder report a
                            // misleading "did not produce its first frame".
                            tracing::debug!(
                                expected_width = format.width,
                                expected_height = format.height,
                                actual_width = frame.width,
                                actual_height = frame.height,
                                "ignoring a transient WGC frame size change while resize is pending"
                            );
                            if let Some(pixels) = resize_rgba(
                                &frame.pixels,
                                frame.width,
                                frame.height,
                                format.width,
                                format.height,
                            ) {
                                candidate_frame = Some((
                                    CapturedWindowFrame::new(Arc::from(pixels)),
                                    (frame.source_width, frame.source_height),
                                ));
                            }
                        }
                        Ok(Some(WindowCaptureUpdate::Resized(dimensions))) => {
                            // Keep the stable last frame flowing while the
                            // resize monitor restarts capture and encoders on
                            // a fresh WGC frame pool at these dimensions.
                            record_observed_source_dimensions(&inner, generation, dimensions);
                            tracing::debug!(
                                width = dimensions.0,
                                height = dimensions.1,
                                "window resize queued a fresh WGC capture session"
                            );
                        }
                        Ok(None) => {
                            if capture.is_closed() {
                                tracing::warn!("selected capture window closed");
                                finish_reader_for_generation(
                                    &inner,
                                    generation,
                                    Some("selected capture window closed".to_owned()),
                                );
                                break;
                            }
                        }
                        Err(error) => {
                            finish_reader_for_generation(
                                &inner,
                                generation,
                                Some(error.to_string()),
                            );
                            break;
                        }
                    }
                    if capture.is_minimized() {
                        // Minimize can race the blocking next_frame call.
                        // Discard its surface even if it looked dimensionally
                        // valid when it was read back.
                        was_minimized = true;
                        latest_frame.publish_repeat(&inner, format, generation);
                    } else if let Some((candidate, source_dimensions)) = candidate_frame {
                        let stale_minimized_surface = restored_surface_is_stale(
                            was_minimized,
                            capture.current_dimensions().ok(),
                            source_dimensions,
                        );
                        if stale_minimized_surface {
                            tracing::debug!(
                                source_width = source_dimensions.0,
                                source_height = source_dimensions.1,
                                "discarding a queued minimized WGC surface after restore"
                            );
                            latest_frame.publish_repeat(&inner, format, generation);
                        } else {
                            record_observed_source_dimensions(
                                &inner,
                                generation,
                                source_dimensions,
                            );
                            latest_frame = candidate;
                            latest_frame.publish(&inner, format, generation);
                            was_minimized = false;
                        }
                    } else {
                        latest_frame.publish_repeat(&inner, format, generation);
                    }
                    let elapsed = frame_started.elapsed();
                    if elapsed < frame_duration {
                        thread::sleep(frame_duration - elapsed);
                    }
                }
                capture.stop();
                return;
            }
            capture.stop();
        }
        drop(wgc_capture);
        // XCap's Windows backend uses synchronous PrintWindow calls. If this
        // thread enters that fallback while the target is still in its modal
        // move/size loop, the target can block before it releases global mouse
        // capture. Report the WGC startup failure instead; resize recovery will
        // retry a fresh compositor session without calling into the target UI.
        finish_reader_for_generation(
            &inner,
            generation,
            Some("Windows Graphics Capture did not become ready".to_owned()),
        );
    })
}

#[cfg(windows)]
fn resize_rgba(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
) -> Option<Vec<u8>> {
    let source_width = usize::try_from(source_width).ok()?;
    let source_height = usize::try_from(source_height).ok()?;
    let output_width = usize::try_from(output_width).ok()?;
    let output_height = usize::try_from(output_height).ok()?;
    let source_len = source_width.checked_mul(source_height)?.checked_mul(4)?;
    let output_len = output_width.checked_mul(output_height)?.checked_mul(4)?;
    if source_width == 0
        || source_height == 0
        || output_width == 0
        || output_height == 0
        || source.len() != source_len
    {
        return None;
    }
    let scale = (output_width as f64 / source_width as f64)
        .min(output_height as f64 / source_height as f64);
    let scaled_width = ((source_width as f64 * scale).round() as usize).clamp(1, output_width);
    let scaled_height = ((source_height as f64 * scale).round() as usize).clamp(1, output_height);
    let offset_x = (output_width - scaled_width) / 2;
    let offset_y = (output_height - scaled_height) / 2;
    let mut output = vec![0_u8; output_len];
    for pixel in output.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    for y in 0..scaled_height {
        let source_y = y * source_height / scaled_height;
        for x in 0..scaled_width {
            let source_x = x * source_width / scaled_width;
            let source_offset = (source_y * source_width + source_x) * 4;
            let output_offset = ((offset_y + y) * output_width + offset_x + x) * 4;
            output[output_offset..output_offset + 4]
                .copy_from_slice(&source[source_offset..source_offset + 4]);
        }
    }
    Some(output)
}

fn capture_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

#[cfg(test)]
fn publish_frame(inner: &SharedCaptureInner, format: CaptureFormat, data: Arc<[u8]>) {
    let captured_at_unix_nanos = capture_timestamp();
    publish_frame_at(inner, format, data, captured_at_unix_nanos);
}

fn publish_frame_for_generation(
    inner: &SharedCaptureInner,
    format: CaptureFormat,
    data: Arc<[u8]>,
    generation: u64,
) {
    let captured_at_unix_nanos = capture_timestamp();
    publish_frame_at_for_generation(inner, format, data, captured_at_unix_nanos, generation);
}

fn publish_frame_at_for_generation(
    inner: &SharedCaptureInner,
    format: CaptureFormat,
    data: Arc<[u8]>,
    captured_at_unix_nanos: u64,
    generation: u64,
) {
    if inner.stopped.load(Ordering::Acquire)
        || inner.generation.load(Ordering::Acquire) != generation
    {
        return;
    }
    let Ok(mut latest) = inner.latest.lock() else {
        return;
    };
    // The generation can advance while this publisher waits for the frame
    // mutex. Recheck under the lock so a detached old reader cannot satisfy a
    // replacement producer's readiness wait with stale pixels.
    if inner.stopped.load(Ordering::Acquire)
        || inner.generation.load(Ordering::Acquire) != generation
    {
        return;
    }
    latest.sequence += 1;
    latest.frame = Some(SourceFrame {
        width: format.width,
        height: format.height,
        pixel_format: format.pixel_format,
        captured_at_unix_nanos,
        data,
    });
    inner.frame_ready.notify_all();
}

#[cfg(test)]
fn publish_frame_at(
    inner: &SharedCaptureInner,
    format: CaptureFormat,
    data: Arc<[u8]>,
    captured_at_unix_nanos: u64,
) {
    let Ok(mut latest) = inner.latest.lock() else {
        return;
    };
    latest.sequence += 1;
    latest.frame = Some(SourceFrame {
        width: format.width,
        height: format.height,
        pixel_format: format.pixel_format,
        captured_at_unix_nanos,
        data,
    });
    inner.frame_ready.notify_all();
}

fn finish_reader(inner: &SharedCaptureInner, failure: Option<String>) {
    if let Ok(mut latest) = inner.latest.lock() {
        latest.ended = true;
        latest.failure = failure;
        inner.frame_ready.notify_all();
    }
}

fn finish_reader_for_generation(
    inner: &SharedCaptureInner,
    generation: u64,
    failure: Option<String>,
) {
    if inner.stopped.load(Ordering::Acquire)
        || inner.generation.load(Ordering::Acquire) != generation
    {
        return;
    }
    if let Ok(mut latest) = inner.latest.lock() {
        if inner.stopped.load(Ordering::Acquire)
            || inner.generation.load(Ordering::Acquire) != generation
        {
            return;
        }
        latest.ended = true;
        latest.failure = failure;
        inner.frame_ready.notify_all();
    }
}

fn pack_dimensions((width, height): (u32, u32)) -> u64 {
    (u64::from(width) << 32) | u64::from(height)
}

fn unpack_dimensions(dimensions: u64) -> (u32, u32) {
    ((dimensions >> 32) as u32, dimensions as u32)
}

#[cfg(windows)]
fn restored_surface_is_stale(
    was_minimized: bool,
    current_dimensions: Option<(u32, u32)>,
    frame_dimensions: (u32, u32),
) -> bool {
    was_minimized && current_dimensions.is_some_and(|current| current != frame_dimensions)
}

#[cfg(windows)]
fn record_observed_source_dimensions(
    inner: &SharedCaptureInner,
    generation: u64,
    dimensions: (u32, u32),
) {
    if let Ok(mut observed) = inner.observed_source_dimensions.lock()
        && observed.generation == generation
    {
        observed.packed = pack_dimensions(dimensions);
    }
}

fn advance_capture_generation(inner: &SharedCaptureInner) -> u64 {
    match inner.observed_source_dimensions.lock() {
        Ok(mut observed) => {
            let generation = inner.generation.fetch_add(1, Ordering::AcqRel) + 1;
            observed.generation = generation;
            generation
        }
        Err(_) => inner.generation.fetch_add(1, Ordering::AcqRel) + 1,
    }
}

fn target_dimensions(settings: &CaptureSettings) -> Result<(u32, u32)> {
    let source_dimensions = if settings.source_kind == "test" {
        (settings.width.max(2), settings.height.max(2))
    } else {
        crate::capture::source_dimensions(
            &settings.source_kind,
            settings.source_index,
            settings.source_native_id,
        )?
    };
    target_dimensions_from_source(settings, source_dimensions)
}

fn target_format(settings: &CaptureSettings) -> Result<CaptureFormat> {
    let fps = settings.output_fps.unwrap_or(settings.fps).max(1);
    #[cfg(windows)]
    if settings.source_kind == "window" {
        if settings
            .source_native_id
            .is_some_and(crate::capture::native_window_is_minimized)
        {
            bail!("selected window is minimized; restore it before starting capture")
        }
        // Resolve a persisted HWND directly so a minimized selected window
        // does not become invalid merely because XCap stopped enumerating it.
        let (width, height) = crate::window_capture::WindowCapture::dimensions_for(
            settings.source_index,
            settings.source_native_id,
        )
        .or_else(|_| {
            crate::capture::source_dimensions(
                &settings.source_kind,
                settings.source_index,
                settings.source_native_id,
            )
        })?;
        if width < 2 || height < 2 {
            bail!("Windows Graphics Capture dimensions must be at least 2x2")
        }
        return Ok(CaptureFormat {
            width,
            height,
            fps,
            pixel_format: SourcePixelFormat::Bgra,
        });
    }
    let (width, height) = target_dimensions(settings)?;
    Ok(CaptureFormat {
        width,
        height,
        fps,
        pixel_format: SourcePixelFormat::Yuv420p,
    })
}

fn target_dimensions_from_source(
    settings: &CaptureSettings,
    (source_width, source_height): (u32, u32),
) -> Result<(u32, u32)> {
    let source_width = source_width.max(2);
    let source_height = source_height.max(2);
    let (width, height) = if settings.source_kind == "test" {
        settings.test_pattern_dimensions()
    } else if let Some(height) = settings.output_height {
        let width = ((source_width as u64 * height as u64) / source_height as u64) as u32;
        (width, height)
    } else {
        (source_width, source_height)
    };
    let width = width & !1;
    let height = height & !1;
    if width < 2 || height < 2 {
        bail!("shared capture dimensions must be at least 2x2 and even")
    }
    Ok((width, height))
}

fn yuv420p_frame_size(width: u32, height: u32) -> Result<usize> {
    let pixels = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .context("shared capture dimensions overflow frame size")?;
    pixels
        .checked_mul(3)
        .and_then(|value| value.checked_div(2))
        .context("shared capture dimensions overflow YUV420P frame size")
}

fn packed_frame_size(width: u32, height: u32) -> Result<usize> {
    let pixels = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .context("shared capture dimensions overflow BGRA frame size")?;
    pixels
        .checked_mul(4)
        .context("shared capture dimensions overflow BGRA frame size")
}

fn ffmpeg_args(
    settings: &CaptureSettings,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Vec<String>> {
    let mut args = if settings.source_kind == "test" {
        vec![
            "-hide_banner".to_owned(),
            "-loglevel".to_owned(),
            "error".to_owned(),
            "-f".to_owned(),
            "lavfi".to_owned(),
            "-i".to_owned(),
            format!("testsrc2=size={width}x{height}:rate={fps}"),
        ]
    } else {
        crate::capture::ffmpeg_input_args(
            &settings.source_kind,
            settings.source_index,
            settings.source_native_id,
            Some(fps),
            settings.draw_mouse,
        )?
    };
    args.push("-an".to_owned());
    if settings.source_kind == "test" && settings.audio_mode == "test" {
        let marker_size = (height / 8).clamp(24, 160);
        let marker_margin = (height / 40).clamp(8, 32);
        args.extend([
            "-vf".to_owned(),
            format!(
                "drawbox=x={marker_margin}:y={marker_margin}:w={marker_size}:h={marker_size}:color=lime@0.9:t=fill:enable=lt(mod(time(0)\\,2)\\,1)"
            ),
        ]);
    } else if settings.source_kind != "test" {
        // The raw reader has a fixed frame-size contract. Always produce the
        // exact advertised canvas; preserve the real source aspect ratio with
        // letterboxing rather than stretching it.
        args.extend(["-vf".to_owned(), raw_capture_filter(width, height)]);
    }
    args.extend([
        "-pix_fmt".to_owned(),
        "yuv420p".to_owned(),
        "-f".to_owned(),
        "rawvideo".to_owned(),
        "pipe:1".to_owned(),
    ]);
    Ok(args)
}

fn raw_capture_filter(width: u32, height: u32) -> String {
    format!(
        "scale={width}:{height}:force_original_aspect_ratio=decrease:flags=fast_bilinear,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1"
    )
}

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_dimensions_scale_to_the_requested_height() {
        let mut settings = test_settings();
        settings.width = 1_920;
        settings.height = 1_080;
        settings.output_height = Some(720);
        assert_eq!(
            target_dimensions_from_source(&settings, (1_920, 1_080)).unwrap(),
            (1_280, 720)
        );
    }

    #[test]
    fn test_source_uses_test_pattern_dimensions() {
        let mut settings = test_settings();
        settings.source_kind = "test".to_owned();
        settings.output_height = Some(360);
        assert_eq!(target_dimensions(&settings).unwrap(), (640, 360));
    }

    #[test]
    fn diagnostic_test_pattern_contains_the_tone_gate_marker() {
        let mut settings = test_settings();
        settings.source_kind = "test".to_owned();
        settings.audio_mode = "test".to_owned();
        let args = ffmpeg_args(&settings, 1_280, 720, 60).unwrap();
        let filter = args
            .windows(2)
            .find_map(|pair| (pair[0] == "-vf").then_some(pair[1].as_str()))
            .expect("diagnostic test pattern should have a video marker filter");
        assert!(filter.contains("drawbox="));
        assert!(filter.contains("color=lime@0.9"));
        assert!(filter.contains("enable=lt(mod(time(0)"));
    }

    #[test]
    fn target_dimensions_preserve_an_ultrawide_source_ratio() {
        let mut settings = test_settings();
        settings.output_height = Some(1080);
        assert_eq!(
            target_dimensions_from_source(&settings, (3440, 1440)).unwrap(),
            (2580, 1080)
        );
    }

    #[test]
    fn raw_capture_filter_preserves_aspect_with_an_exact_frame_canvas() {
        let filter = raw_capture_filter(1_280, 720);
        assert!(filter.contains("scale=1280:720:force_original_aspect_ratio=decrease"));
        assert!(filter.contains("pad=1280:720"));
        assert!(filter.ends_with("setsar=1"));
    }

    #[test]
    fn yuv420p_frame_size_is_one_and_a_half_bytes_per_pixel() {
        assert_eq!(yuv420p_frame_size(4, 2).unwrap(), 12);
    }

    #[test]
    fn detached_reader_cannot_publish_or_fail_a_new_generation() {
        let format = CaptureFormat {
            width: 4,
            height: 2,
            fps: 30,
            pixel_format: SourcePixelFormat::Yuv420p,
        };
        let inner = SharedCaptureInner {
            format: Mutex::new(format),
            observed_source_dimensions: observed_source_dimensions_state(1, (4, 2)),
            backend: Mutex::new("test"),
            stopped: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            restart_lock: Mutex::new(()),
            child: Mutex::new(None),
            reader: Mutex::new(None),
            latest: Mutex::new(LatestFrame::default()),
            frame_ready: Condvar::new(),
        };
        publish_frame_for_generation(&inner, format, Arc::from(vec![1_u8; 12]), 1);
        advance_capture_generation(&inner);

        publish_frame_for_generation(&inner, format, Arc::from(vec![2_u8; 12]), 1);
        finish_reader_for_generation(&inner, 1, Some("stale failure".to_owned()));

        let latest = inner.latest.lock().unwrap();
        assert_eq!(latest.sequence, 1);
        assert_eq!(latest.frame.as_ref().unwrap().data[0], 1);
        assert!(!latest.ended);
        assert!(latest.failure.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn repeated_window_frame_gets_a_fresh_publication_timestamp() {
        let format = CaptureFormat {
            width: 2,
            height: 2,
            fps: 30,
            pixel_format: SourcePixelFormat::Bgra,
        };
        let inner = SharedCaptureInner {
            format: Mutex::new(format),
            observed_source_dimensions: observed_source_dimensions_state(1, (2, 2)),
            backend: Mutex::new("test"),
            stopped: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            restart_lock: Mutex::new(()),
            child: Mutex::new(None),
            reader: Mutex::new(None),
            latest: Mutex::new(LatestFrame::default()),
            frame_ready: Condvar::new(),
        };
        let captured = CapturedWindowFrame {
            data: Arc::from(vec![1_u8; 16]),
            captured_at_unix_nanos: 123,
        };

        let generation = inner.generation.load(Ordering::Acquire);
        captured.publish(&inner, format, generation);
        captured.publish_repeat(&inner, format, generation);

        let latest = inner.latest.lock().unwrap();
        assert_eq!(latest.sequence, 2);
        assert_ne!(latest.frame.as_ref().unwrap().captured_at_unix_nanos, 123);
    }

    #[cfg(windows)]
    #[test]
    fn restore_rejects_a_queued_minimized_surface() {
        assert!(restored_surface_is_stale(
            true,
            Some((1_280, 720)),
            (640, 96)
        ));
        assert!(!restored_surface_is_stale(
            true,
            Some((1_280, 720)),
            (1_280, 720)
        ));
        assert!(!restored_surface_is_stale(
            false,
            Some((1_280, 720)),
            (640, 96)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn window_dimension_observations_are_scoped_to_the_capture_generation() {
        let format = CaptureFormat {
            width: 800,
            height: 600,
            fps: 30,
            pixel_format: SourcePixelFormat::Bgra,
        };
        let inner = SharedCaptureInner {
            format: Mutex::new(format),
            observed_source_dimensions: observed_source_dimensions_state(4, (800, 600)),
            backend: Mutex::new("test"),
            stopped: AtomicBool::new(false),
            generation: AtomicU64::new(4),
            restart_lock: Mutex::new(()),
            child: Mutex::new(None),
            reader: Mutex::new(None),
            latest: Mutex::new(LatestFrame::default()),
            frame_ready: Condvar::new(),
        };

        record_observed_source_dimensions(&inner, 4, (1200, 700));
        assert_eq!(
            unpack_dimensions(inner.observed_source_dimensions.lock().unwrap().packed),
            (1200, 700)
        );
        advance_capture_generation(&inner);
        record_observed_source_dimensions(&inner, 4, (300, 200));
        assert_eq!(
            unpack_dimensions(inner.observed_source_dimensions.lock().unwrap().packed),
            (1200, 700)
        );
        record_observed_source_dimensions(&inner, 5, (1400, 900));
        assert_eq!(
            unpack_dimensions(inner.observed_source_dimensions.lock().unwrap().packed),
            (1400, 900)
        );
    }

    #[test]
    fn new_subscriber_skips_a_cached_frame_from_before_its_creation() {
        let format = CaptureFormat {
            width: 4,
            height: 2,
            fps: 30,
            pixel_format: SourcePixelFormat::Yuv420p,
        };
        let inner = Arc::new(SharedCaptureInner {
            format: Mutex::new(format),
            observed_source_dimensions: observed_source_dimensions_state(1, (4, 2)),
            backend: Mutex::new("test"),
            stopped: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            restart_lock: Mutex::new(()),
            child: Mutex::new(None),
            reader: Mutex::new(None),
            latest: Mutex::new(LatestFrame::default()),
            frame_ready: Condvar::new(),
        });
        publish_frame(&inner, format, Arc::from(vec![1_u8; 12]));
        let capture = SharedCapture {
            inner: Arc::clone(&inner),
            ffmpeg: OsString::from("ffmpeg"),
        };
        let mut subscription = capture.subscribe();
        let producer = Arc::clone(&inner);
        let published = std::thread::spawn(move || {
            publish_frame(&producer, format, Arc::from(vec![2_u8; 12]));
        });

        let frame = subscription.recv().unwrap().unwrap();
        published.join().unwrap();
        assert_eq!(frame.data.as_ref(), &[2_u8; 12]);
    }

    #[test]
    fn primary_subscriber_can_consume_the_cached_initial_frame() {
        let format = CaptureFormat {
            width: 4,
            height: 2,
            fps: 30,
            pixel_format: SourcePixelFormat::Yuv420p,
        };
        let inner = Arc::new(SharedCaptureInner {
            format: Mutex::new(format),
            observed_source_dimensions: observed_source_dimensions_state(1, (4, 2)),
            backend: Mutex::new("test"),
            stopped: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            restart_lock: Mutex::new(()),
            child: Mutex::new(None),
            reader: Mutex::new(None),
            latest: Mutex::new(LatestFrame::default()),
            frame_ready: Condvar::new(),
        });
        publish_frame(&inner, format, Arc::from(vec![7_u8; 12]));
        let capture = SharedCapture {
            inner,
            ffmpeg: OsString::from("ffmpeg"),
        };
        let mut subscription = capture.subscribe_including_latest();

        let frame = subscription.recv().unwrap().unwrap();
        assert_eq!(frame.data.as_ref(), &[7_u8; 12]);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an interactive Windows desktop with a capturable window"]
    fn live_window_bgra_is_accepted_by_ffmpeg() {
        use std::io::Write as _;

        let source = crate::capture::list_windows()
            .unwrap()
            .into_iter()
            .min_by_key(|source| u64::from(source.width) * u64::from(source.height))
            .expect("interactive desktop has no capturable window");
        let mut settings = test_settings();
        settings.source_kind = "window".to_owned();
        settings.source_index = source.index;
        settings.source_native_id = source.native_id;
        settings.width = source.width;
        settings.height = source.height;
        settings.fps = source.fps.unwrap_or(30).min(30);
        settings.output_height = Some(360);
        settings.output_fps = Some(5);
        let ffmpeg = crate::packaging::prepare_ffmpeg().unwrap();
        let capture = SharedCapture::start(&ffmpeg.command, settings).unwrap();
        assert_eq!(capture.source_pixel_format(), SourcePixelFormat::Bgra);
        let frame = capture.latest_frame_snapshot().unwrap();
        let (width, height) = (frame.width.to_string(), frame.height.to_string());
        capture.stop().unwrap();

        let mut child = Command::new(&ffmpeg.command)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "bgra",
                "-video_size",
                &format!("{width}x{height}"),
                "-framerate",
                "5",
                "-i",
                "pipe:0",
                "-frames:v",
                "1",
                "-vf",
                "scale=-2:360",
                "-c:v",
                "libvpx",
                "-deadline",
                "realtime",
                "-cpu-used",
                "8",
                "-lag-in-frames",
                "0",
                "-auto-alt-ref",
                "0",
                "-pix_fmt",
                "yuv420p",
                "-f",
                "ivf",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(frame.data.as_ref())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "FFmpeg rejected BGRA: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.len() > 32, "FFmpeg emitted no IVF frame");
    }

    #[test]
    fn readiness_waits_for_a_frame_from_the_new_producer_generation() {
        let format = CaptureFormat {
            width: 4,
            height: 2,
            fps: 30,
            pixel_format: SourcePixelFormat::Yuv420p,
        };
        let inner = Arc::new(SharedCaptureInner {
            format: Mutex::new(format),
            observed_source_dimensions: observed_source_dimensions_state(1, (4, 2)),
            backend: Mutex::new("test"),
            stopped: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            restart_lock: Mutex::new(()),
            child: Mutex::new(None),
            reader: Mutex::new(None),
            latest: Mutex::new(LatestFrame::default()),
            frame_ready: Condvar::new(),
        });
        publish_frame(&inner, format, Arc::from(vec![1_u8; 12]));
        let capture = SharedCapture {
            inner: Arc::clone(&inner),
            ffmpeg: OsString::from("ffmpeg"),
        };
        let previous_sequence = capture.latest_sequence().unwrap();
        let producer = Arc::clone(&inner);
        let published = std::thread::spawn(move || {
            publish_frame(&producer, format, Arc::from(vec![2_u8; 12]));
        });

        capture.wait_for_new_frame(previous_sequence).unwrap();
        published.join().unwrap();
    }

    fn test_settings() -> CaptureSettings {
        CaptureSettings {
            source_kind: "monitor".to_owned(),
            source_index: 0,
            source_native_id: None,
            draw_mouse: true,
            width: 1_920,
            height: 1_080,
            fps: 60,
            output_height: None,
            output_fps: None,
            bitrate: 1,
            quality_mode: "manual".to_owned(),
            bitrate_mode: "fixed".to_owned(),
            adaptive_quality_ceiling: "source".to_owned(),
            adaptive_fps_ceiling: "source".to_owned(),
            max_quality_groups: "1".to_owned(),
            latency_preference: "low".to_owned(),
            audio_mode: "off".to_owned(),
            excluded_audio_processes: Vec::new(),
        }
    }
}
