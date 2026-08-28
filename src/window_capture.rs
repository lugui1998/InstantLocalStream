//! Isolated Windows Graphics Capture for a selected application window.
//!
//! Unlike a desktop crop, Windows Graphics Capture receives the target
//! window's compositor surface directly, so a foreground window does not
//! obscure the selected source.

#[cfg(windows)]
mod imp {
    use std::{
        ffi::c_void,
        sync::{
            Arc, Condvar, Mutex, OnceLock,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    use anyhow::{Context, Result, bail};
    use windows::{
        Foundation::TypedEventHandler,
        Graphics::{
            Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
            DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
            SizeInt32,
        },
        Win32::{
            Foundation::{HMODULE, HWND, LPARAM, POINT, WPARAM},
            Graphics::{
                Direct3D::D3D_DRIVER_TYPE_HARDWARE,
                Direct3D11::{
                    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
                    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
                    ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
                },
                Dxgi::IDXGIDevice,
                Gdi::{MONITOR_DEFAULTTONULL, MonitorFromPoint},
            },
            System::WinRT::{
                Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
                Graphics::Capture::IGraphicsCaptureItemInterop,
                RO_INIT_MULTITHREADED, RoInitialize,
            },
            UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            UI::WindowsAndMessaging::{
                CHILDID_SELF, EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_MOVESIZEEND, GA_ROOT,
                GetAncestor, GetCursorPos, GetMessageW, GetWindowRect, IsWindow, MSG, OBJID_WINDOW,
                PM_NOREMOVE, PeekMessageW, PostThreadMessageW, WINEVENT_OUTOFCONTEXT, WM_QUIT,
                WindowFromPoint,
            },
        },
        core::{Error as WindowsError, IInspectable, Interface, Ref, factory},
    };

    #[derive(Debug)]
    pub struct WindowFrame {
        /// Dimensions reported by the compositor for this frame. Live capture
        /// can keep publishing into a stable output canvas briefly while the
        /// server debounces and rebuilds the media graph for a resized window.
        pub source_width: u32,
        pub source_height: u32,
        pub width: u32,
        pub height: u32,
        /// Packed four-channel pixels. Live WGC frames remain BGRA for FFmpeg;
        /// one-shot thumbnail helpers return sampled RGBA.
        pub pixels: Vec<u8>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum WindowResizeEvent {
        LocationChange,
        MoveSizeEnd,
    }

    static RESIZE_EVENT_TARGET: AtomicUsize = AtomicUsize::new(0);
    static RESIZE_EVENT_SENDER: OnceLock<
        Mutex<Option<tokio::sync::mpsc::UnboundedSender<WindowResizeEvent>>>,
    > = OnceLock::new();

    pub struct WindowResizeWatcher {
        stop: Arc<AtomicBool>,
        thread_id: u32,
        thread: Option<JoinHandle<()>>,
    }

    impl WindowResizeWatcher {
        pub fn start(
            index: usize,
            native_id: Option<u64>,
            events: tokio::sync::mpsc::UnboundedSender<WindowResizeEvent>,
        ) -> Option<Self> {
            let hwnd = selected_hwnd(index, native_id).ok()?;
            let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
            let target = if root.0.is_null() { hwnd } else { root };
            Self::start_for_window(target, events)
        }

        fn start_for_window(
            target: HWND,
            events: tokio::sync::mpsc::UnboundedSender<WindowResizeEvent>,
        ) -> Option<Self> {
            RESIZE_EVENT_TARGET.store(target.0 as usize, Ordering::Release);
            if let Ok(mut sender) = resize_event_sender().lock() {
                *sender = Some(events);
            }

            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            let thread = thread::spawn(move || {
                let location_hook = unsafe {
                    SetWinEventHook(
                        EVENT_OBJECT_LOCATIONCHANGE,
                        EVENT_OBJECT_LOCATIONCHANGE,
                        None,
                        Some(window_resize_event_proc),
                        0,
                        0,
                        WINEVENT_OUTOFCONTEXT,
                    )
                };
                if location_hook.is_invalid() {
                    let _ = ready_tx.send(None);
                    return;
                }
                let move_size_hook = unsafe {
                    SetWinEventHook(
                        EVENT_SYSTEM_MOVESIZEEND,
                        EVENT_SYSTEM_MOVESIZEEND,
                        None,
                        Some(window_resize_event_proc),
                        0,
                        0,
                        WINEVENT_OUTOFCONTEXT,
                    )
                };
                if move_size_hook.is_invalid() {
                    unsafe {
                        let _ = UnhookWinEvent(location_hook);
                    }
                    let _ = ready_tx.send(None);
                    return;
                }

                let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
                let mut message = MSG::default();
                unsafe {
                    // Force creation of this thread's message queue before
                    // reporting readiness so Drop can always wake GetMessageW.
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
                    let _ = UnhookWinEvent(location_hook);
                    let _ = UnhookWinEvent(move_size_hook);
                }
            });

            let Ok(Some(thread_id)) = ready_rx.recv() else {
                stop.store(true, Ordering::Release);
                let _ = thread.join();
                clear_resize_event_target();
                return None;
            };
            Some(Self {
                stop,
                thread_id,
                thread: Some(thread),
            })
        }
    }

    impl Drop for WindowResizeWatcher {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            clear_resize_event_target();
        }
    }

    fn resize_event_sender()
    -> &'static Mutex<Option<tokio::sync::mpsc::UnboundedSender<WindowResizeEvent>>> {
        RESIZE_EVENT_SENDER.get_or_init(|| Mutex::new(None))
    }

    fn clear_resize_event_target() {
        RESIZE_EVENT_TARGET.store(0, Ordering::Release);
        if let Ok(mut sender) = resize_event_sender().lock() {
            *sender = None;
        }
    }

    unsafe extern "system" fn window_resize_event_proc(
        _hook: HWINEVENTHOOK,
        event: u32,
        hwnd: HWND,
        id_object: i32,
        id_child: i32,
        _event_thread: u32,
        _event_time: u32,
    ) {
        let target = RESIZE_EVENT_TARGET.load(Ordering::Acquire);
        if target == 0 || hwnd.0.is_null() || hwnd.0 as usize != target {
            return;
        }
        let event = if event == EVENT_OBJECT_LOCATIONCHANGE
            && id_object == OBJID_WINDOW.0
            && id_child == CHILDID_SELF as i32
        {
            WindowResizeEvent::LocationChange
        } else if event == EVENT_SYSTEM_MOVESIZEEND {
            WindowResizeEvent::MoveSizeEnd
        } else {
            return;
        };
        if let Ok(sender) = resize_event_sender().lock()
            && let Some(sender) = sender.as_ref()
        {
            let _ = sender.send(event);
        }
    }

    #[derive(Default)]
    struct FrameState {
        latest: Option<WindowFrame>,
        failure: Option<String>,
        closed: bool,
    }

    struct FrameMailbox {
        state: Mutex<FrameState>,
        ready: Condvar,
    }

    struct StagingTexture {
        width: u32,
        height: u32,
        texture: ID3D11Texture2D,
    }

    #[derive(Default)]
    struct LiveReadbackState {
        next_readback: Option<Instant>,
        staging: Option<StagingTexture>,
        frame_pool_size: Option<(u32, u32)>,
    }

    impl FrameMailbox {
        fn publish(&self, frame: WindowFrame) {
            if let Ok(mut state) = self.state.lock()
                && !state.closed
            {
                state.latest = Some(frame);
                self.ready.notify_all();
            }
        }

        fn fail(&self, error: impl Into<String>) {
            if let Ok(mut state) = self.state.lock()
                && !state.closed
            {
                state.failure = Some(error.into());
                self.ready.notify_all();
            }
        }

        fn close(&self) {
            if let Ok(mut state) = self.state.lock() {
                state.closed = true;
                self.ready.notify_all();
            }
        }
    }

    struct WgcRuntime {
        item: GraphicsCaptureItem,
        closed_token: i64,
        frame_pool: Direct3D11CaptureFramePool,
        frame_arrived_token: i64,
        session: GraphicsCaptureSession,
        closed: bool,
    }

    impl WgcRuntime {
        fn close(&mut self) {
            if self.closed {
                return;
            }
            self.closed = true;
            let _ = self.frame_pool.RemoveFrameArrived(self.frame_arrived_token);
            let _ = self.item.RemoveClosed(self.closed_token);
            let _ = self.session.Close();
            let _ = self.frame_pool.Close();
        }
    }

    impl Drop for WgcRuntime {
        fn drop(&mut self) {
            self.close();
        }
    }

    /// A continuous latest-frame reader for one selected application window.
    pub struct WindowCapture {
        mailbox: Arc<FrameMailbox>,
        runtime: Mutex<Option<WgcRuntime>>,
        dimensions: (u32, u32),
        target_root: usize,
        capture_cursor: bool,
        cursor_visible: AtomicBool,
    }

    impl WindowCapture {
        pub fn dimensions_for(index: usize, native_id: Option<u64>) -> Result<(u32, u32)> {
            let (item, _) = capture_item_for(index, native_id)?;
            item_dimensions(&item)
        }

        pub fn start(
            index: usize,
            native_id: Option<u64>,
            capture_cursor: bool,
            frame_interval: Duration,
        ) -> Result<Self> {
            let (item, target_root) = capture_item_for(index, native_id)?;
            let dimensions = item_dimensions(&item)?;
            let d3d_device = create_d3d_device()?;
            let d3d_context = unsafe { d3d_device.GetImmediateContext()? };
            let dxgi_device: IDXGIDevice = d3d_device.cast()?;
            let direct3d_device = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)? }
                .cast::<IDirect3DDevice>()?;
            let size = item.Size()?;
            let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &direct3d_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                size,
            )?;
            let mailbox = Arc::new(FrameMailbox {
                state: Mutex::new(FrameState::default()),
                ready: Condvar::new(),
            });
            let callback_mailbox = Arc::clone(&mailbox);
            let readback_state = Arc::new(Mutex::new(LiveReadbackState::default()));
            let callback_readback_state = Arc::clone(&readback_state);
            let frame_arrived_token =
                frame_pool.FrameArrived(&TypedEventHandler::<
                    Direct3D11CaptureFramePool,
                    IInspectable,
                >::new(move |frame_pool, _| {
                    let Ok(mut readback_state) = callback_readback_state.lock() else {
                        callback_mailbox.fail("window capture readback lock poisoned");
                        return Ok(());
                    };
                    if !should_readback(
                        &mut readback_state.next_readback,
                        Instant::now(),
                        frame_interval,
                    ) {
                        if let Err(error) = discard_frame(frame_pool) {
                            callback_mailbox.fail(error.to_string());
                        }
                        return Ok(());
                    }
                    match frame_from_pool(
                        frame_pool,
                        &d3d_device,
                        &d3d_context,
                        dimensions,
                        &mut readback_state,
                    ) {
                        Ok(Some(frame)) => callback_mailbox.publish(frame),
                        Ok(None) => {}
                        Err(error) => callback_mailbox.fail(error.to_string()),
                    }
                    Ok(())
                }))?;
            let closed_mailbox = Arc::clone(&mailbox);
            let closed_token = item.Closed(&TypedEventHandler::<
                GraphicsCaptureItem,
                IInspectable,
            >::new(move |_, _| {
                closed_mailbox.close();
                Ok(())
            }))?;
            let session = frame_pool.CreateCaptureSession(&item)?;
            // These are capability/OS-version dependent.  Capture itself still
            // works if a particular preference cannot be applied.
            let _ = session.SetIsBorderRequired(false);
            // Cursor inclusion is controlled dynamically below.  Leaving it
            // enabled globally would draw the cursor when it hovers over an
            // occluding window that is not part of this capture item.
            let _ = session.SetIsCursorCaptureEnabled(false);
            session.StartCapture()?;
            let capture = Self {
                mailbox,
                runtime: Mutex::new(Some(WgcRuntime {
                    item,
                    closed_token,
                    frame_pool,
                    frame_arrived_token,
                    session,
                    closed: false,
                })),
                dimensions,
                target_root: target_root.0 as usize,
                capture_cursor,
                cursor_visible: AtomicBool::new(false),
            };
            capture.refresh_cursor_visibility();
            Ok(capture)
        }

        pub fn dimensions(&self) -> (u32, u32) {
            self.dimensions
        }

        pub fn is_closed(&self) -> bool {
            self.mailbox
                .state
                .lock()
                .map(|state| state.closed)
                .unwrap_or(true)
        }

        /// Returns the newest frame available before `timeout`, dropping stale
        /// frames by retaining only one mailbox slot.
        pub fn next_frame(&self, timeout: Duration) -> Result<Option<WindowFrame>> {
            let mut state = self
                .mailbox
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("window capture frame lock poisoned"))?;
            if let Some(frame) = state.latest.take() {
                return Ok(Some(frame));
            }
            if let Some(failure) = &state.failure {
                bail!("Windows Graphics Capture failed: {failure}");
            }
            if state.closed {
                return Ok(None);
            }
            let (next, _) = self
                .mailbox
                .ready
                .wait_timeout(state, timeout)
                .map_err(|_| anyhow::anyhow!("window capture frame lock poisoned"))?;
            state = next;
            if let Some(frame) = state.latest.take() {
                return Ok(Some(frame));
            }
            if let Some(failure) = &state.failure {
                bail!("Windows Graphics Capture failed: {failure}");
            }
            Ok(None)
        }

        /// Enables WGC's cursor overlay only when the OS reports that the
        /// pointer belongs to the captured top-level window.  An occluding
        /// window wins the WindowFromPoint hit-test, so its cursor is excluded.
        pub fn refresh_cursor_visibility(&self) {
            let visible =
                cursor_visibility(self.capture_cursor, cursor_is_over_target(self.target_root));
            if self.cursor_visible.load(Ordering::Acquire) == visible {
                return;
            }
            let Ok(runtime) = self.runtime.lock() else {
                return;
            };
            let Some(runtime) = runtime.as_ref() else {
                return;
            };
            if runtime.session.SetIsCursorCaptureEnabled(visible).is_ok() {
                self.cursor_visible.store(visible, Ordering::Release);
            }
        }

        pub fn stop(&self) {
            self.mailbox.close();
            if let Ok(mut runtime) = self.runtime.lock()
                && let Some(mut runtime) = runtime.take()
            {
                runtime.close();
            }
        }
    }

    impl Drop for WindowCapture {
        fn drop(&mut self) {
            self.stop();
        }
    }

    /// Captures one compositor frame for a thumbnail and then closes the WGC
    /// session. This deliberately has no PrintWindow/XCap fallback: a stale
    /// thumbnail is preferable to blocking the window being dragged.
    pub fn capture_preview_frame(
        index: usize,
        native_id: Option<u64>,
        max_width: usize,
        max_height: usize,
        timeout: Duration,
    ) -> Result<WindowFrame> {
        let (item, _) = capture_item_for(index, native_id)?;
        capture_preview_item(item, max_width, max_height, timeout)
    }

    /// Captures one compositor frame for the monitor at `index`. The monitor
    /// is resolved by the center of its XCap-reported desktop rectangle, then
    /// converted into a GraphicsCaptureItem without a GDI bitmap readback.
    pub fn capture_monitor_preview_frame(
        index: usize,
        max_width: usize,
        max_height: usize,
        timeout: Duration,
    ) -> Result<WindowFrame> {
        let monitor = xcap::Monitor::all()?
            .into_iter()
            .nth(index)
            .context("capture monitor does not exist")?;
        let x = monitor.x()?;
        let y = monitor.y()?;
        let width = monitor.width()?;
        let height = monitor.height()?;
        let center = monitor_center(x, y, width, height)?;
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        let monitor = unsafe { MonitorFromPoint(center, MONITOR_DEFAULTTONULL) };
        if monitor.is_invalid() {
            bail!("capture monitor no longer exists");
        }
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item = unsafe { interop.CreateForMonitor::<GraphicsCaptureItem>(monitor)? };
        capture_preview_item(item, max_width, max_height, timeout)
    }

    fn monitor_center(x: i32, y: i32, width: u32, height: u32) -> Result<POINT> {
        let x = x
            .checked_add(i32::try_from(width / 2).context("monitor width exceeds i32")?)
            .context("monitor center x overflows i32")?;
        let y = y
            .checked_add(i32::try_from(height / 2).context("monitor height exceeds i32")?)
            .context("monitor center y overflows i32")?;
        Ok(POINT { x, y })
    }

    fn capture_preview_item(
        item: GraphicsCaptureItem,
        max_width: usize,
        max_height: usize,
        timeout: Duration,
    ) -> Result<WindowFrame> {
        if max_width == 0 || max_height == 0 {
            bail!("preview dimensions must be non-zero");
        }
        let d3d_device = create_d3d_device()?;
        let d3d_context = unsafe { d3d_device.GetImmediateContext()? };
        let dxgi_device: IDXGIDevice = d3d_device.cast()?;
        let direct3d_device = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)? }
            .cast::<IDirect3DDevice>()?;
        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &direct3d_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            1,
            item.Size()?,
        )?;
        let mailbox = Arc::new(FrameMailbox {
            state: Mutex::new(FrameState::default()),
            ready: Condvar::new(),
        });
        let callback_mailbox = Arc::clone(&mailbox);
        let frame_arrived_token =
            frame_pool.FrameArrived(&TypedEventHandler::<
                Direct3D11CaptureFramePool,
                IInspectable,
            >::new(move |frame_pool, _| {
                match preview_from_pool(
                    frame_pool,
                    &d3d_device,
                    &d3d_context,
                    max_width,
                    max_height,
                ) {
                    Ok(frame) => callback_mailbox.publish(frame),
                    Err(error) => callback_mailbox.fail(error.to_string()),
                }
                Ok(())
            }))?;
        let closed_mailbox = Arc::clone(&mailbox);
        let closed_token = item.Closed(
            &TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                closed_mailbox.close();
                Ok(())
            }),
        )?;
        let session = frame_pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetIsCursorCaptureEnabled(false);
        let mut runtime = WgcRuntime {
            item,
            closed_token,
            frame_pool,
            frame_arrived_token,
            session,
            closed: false,
        };
        runtime.session.StartCapture()?;

        let mut state = mailbox
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("window preview frame lock poisoned"))?;
        let deadline = Instant::now() + timeout;
        let result = loop {
            if let Some(frame) = state.latest.take() {
                break Ok(frame);
            }
            if let Some(failure) = &state.failure {
                break Err(anyhow::anyhow!(
                    "Windows Graphics Capture failed: {failure}"
                ));
            }
            if state.closed {
                break Err(anyhow::anyhow!("selected capture window closed"));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break Err(anyhow::anyhow!(
                    "Windows Graphics Capture preview timed out after {} ms",
                    timeout.as_millis()
                ));
            }
            let (next, wait) = mailbox
                .ready
                .wait_timeout(state, remaining)
                .map_err(|_| anyhow::anyhow!("window preview frame lock poisoned"))?;
            state = next;
            if wait.timed_out() && state.latest.is_none() {
                break Err(anyhow::anyhow!(
                    "Windows Graphics Capture preview timed out after {} ms",
                    timeout.as_millis()
                ));
            }
        };
        drop(state);
        mailbox.close();
        runtime.close();
        result
    }

    fn capture_item_for(
        index: usize,
        native_id: Option<u64>,
    ) -> Result<(GraphicsCaptureItem, HWND)> {
        // This may be called from a Tokio control worker or from the dedicated
        // reader thread. Ensure a WinRT MTA in either case before interacting
        // with the GraphicsCaptureItem activation factory.
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        let hwnd = selected_hwnd(index, native_id)?;
        let target_root = unsafe { GetAncestor(hwnd, GA_ROOT) };
        let target_root = (!target_root.0.is_null())
            .then_some(target_root)
            .unwrap_or(hwnd);
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item = unsafe { interop.CreateForWindow::<GraphicsCaptureItem>(hwnd)? };
        Ok((item, target_root))
    }

    fn cursor_is_over_target(target_root: usize) -> bool {
        cursor_point_for_target(target_root).is_some()
    }

    fn cursor_point_for_target(target_root: usize) -> Option<POINT> {
        let mut point = POINT::default();
        unsafe {
            GetCursorPos(&mut point).ok()?;
            let hovered = WindowFromPoint(point);
            if hovered.0.is_null()
                || GetAncestor(hovered, GA_ROOT) != HWND(target_root as *mut c_void)
            {
                return None;
            }
        }
        Some(point)
    }

    fn cursor_visibility(capture_cursor: bool, pointer_over_target: bool) -> bool {
        capture_cursor && pointer_over_target
    }

    pub fn cursor_position_for(index: usize, native_id: Option<u64>) -> Option<(i32, i32)> {
        let hwnd = selected_hwnd(index, native_id).ok()?;
        let target_root = unsafe { GetAncestor(hwnd, GA_ROOT) };
        let target_root = (!target_root.0.is_null())
            .then_some(target_root)
            .unwrap_or(hwnd);
        let point = cursor_point_for_target(target_root.0 as usize)?;
        if let Ok(window) = crate::capture::selected_window(index, native_id) {
            return Some((point.x - window.x().ok()?, point.y - window.y().ok()?));
        }
        let mut rect = windows::Win32::Foundation::RECT::default();
        unsafe { GetWindowRect(hwnd, &mut rect).ok()? };
        Some((point.x - rect.left, point.y - rect.top))
    }

    fn selected_hwnd(index: usize, native_id: Option<u64>) -> Result<HWND> {
        let hwnd = if let Some(native_id) = native_id {
            HWND(native_id as usize as *mut c_void)
        } else {
            let window = crate::capture::selected_window(index, None)?;
            HWND(window.id()? as usize as *mut c_void)
        };
        if hwnd.0.is_null() || !unsafe { IsWindow(Some(hwnd)).as_bool() } {
            bail!("selected capture window no longer exists")
        }
        Ok(hwnd)
    }

    fn item_dimensions(item: &GraphicsCaptureItem) -> Result<(u32, u32)> {
        let size = item.Size()?;
        if size.Width <= 0 || size.Height <= 0 {
            bail!("Windows Graphics Capture returned an empty window surface")
        }
        Ok((size.Width as u32, size.Height as u32))
    }

    fn create_d3d_device() -> Result<ID3D11Device> {
        let mut device = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )?;
        }
        device.context("create Direct3D11 device for window capture")
    }

    fn should_readback(
        next_readback: &mut Option<Instant>,
        now: Instant,
        frame_interval: Duration,
    ) -> bool {
        match next_readback {
            None => {
                *next_readback = Some(now + frame_interval);
                true
            }
            Some(next) if now >= *next => {
                *next_readback = Some(now + frame_interval);
                true
            }
            Some(_) => false,
        }
    }

    fn discard_frame(frame_pool: Ref<'_, Direct3D11CaptureFramePool>) -> windows::core::Result<()> {
        let frame_pool = frame_pool.as_ref().ok_or(WindowsError::empty())?;
        frame_pool.TryGetNextFrame()?.Close()
    }

    fn staging_texture(
        d3d_device: &ID3D11Device,
        source_desc: &D3D11_TEXTURE2D_DESC,
        cached: &mut Option<StagingTexture>,
    ) -> windows::core::Result<ID3D11Texture2D> {
        if let Some(staging) = cached.as_ref()
            && can_reuse_staging(
                Some((staging.width, staging.height)),
                source_desc.Width,
                source_desc.Height,
            )
        {
            return Ok(staging.texture.clone());
        }
        let mut staging_desc = *source_desc;
        staging_desc.BindFlags = 0;
        staging_desc.MiscFlags = 0;
        staging_desc.Usage = D3D11_USAGE_STAGING;
        staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut texture = None;
        unsafe {
            d3d_device.CreateTexture2D(&staging_desc, None, Some(&mut texture))?;
        }
        let texture = texture.ok_or(WindowsError::empty())?;
        *cached = Some(StagingTexture {
            width: source_desc.Width,
            height: source_desc.Height,
            texture: texture.clone(),
        });
        Ok(texture)
    }

    fn can_reuse_staging(cached: Option<(u32, u32)>, width: u32, height: u32) -> bool {
        cached == Some((width, height))
    }

    fn frame_from_pool(
        frame_pool: Ref<'_, Direct3D11CaptureFramePool>,
        d3d_device: &ID3D11Device,
        d3d_context: &ID3D11DeviceContext,
        output_dimensions: (u32, u32),
        readback_state: &mut LiveReadbackState,
    ) -> windows::core::Result<Option<WindowFrame>> {
        let frame_pool = frame_pool.as_ref().ok_or(WindowsError::empty())?;
        let frame = frame_pool.TryGetNextFrame()?;
        let content_size = frame.ContentSize()?;
        let content_width = u32::try_from(content_size.Width).map_err(|_| WindowsError::empty())?;
        let content_height =
            u32::try_from(content_size.Height).map_err(|_| WindowsError::empty())?;
        if content_width == 0 || content_height == 0 {
            frame.Close()?;
            return Err(WindowsError::empty());
        }
        let content_dimensions = (content_width, content_height);
        if readback_state
            .frame_pool_size
            .is_some_and(|size| size != content_dimensions)
        {
            frame.Close()?;
            let dxgi_device: IDXGIDevice = d3d_device.cast()?;
            let direct3d_device = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)? }
                .cast::<IDirect3DDevice>()?;
            frame_pool.Recreate(
                &direct3d_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                SizeInt32 {
                    Width: content_size.Width,
                    Height: content_size.Height,
                },
            )?;
            readback_state.frame_pool_size = Some(content_dimensions);
            readback_state.staging = None;
            // Do not publish the final frame from the old compositor surface.
            // The next frame arrives from the newly sized pool.
            return Ok(None);
        }
        readback_state.frame_pool_size = Some(content_dimensions);
        let surface = frame.Surface()?;
        let access = surface.cast::<IDirect3DDxgiInterfaceAccess>()?;
        let source_texture = unsafe { access.GetInterface::<ID3D11Texture2D>()? };
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            source_texture.GetDesc(&mut source_desc);
        }
        let width = source_desc.Width;
        let height = source_desc.Height;
        if width == 0 || height == 0 {
            frame.Close()?;
            return Err(WindowsError::empty());
        }
        let staging = staging_texture(d3d_device, &source_desc, &mut readback_state.staging)?;
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
            front: 0,
            back: 1,
        };
        unsafe {
            d3d_context.CopySubresourceRegion(
                Some(&staging.cast()?),
                0,
                0,
                0,
                0,
                Some(&source_texture.cast()?),
                0,
                Some(&region),
            );
        }
        let resource: ID3D11Resource = staging.cast()?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            d3d_context.Map(Some(&resource), 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let mut source_pixels = vec![0_u8; (width as usize) * (height as usize) * 4];
        let source = mapped.pData.cast::<u8>();
        unsafe {
            for row in 0..height as usize {
                let source_row = std::slice::from_raw_parts(
                    source.add(row * mapped.RowPitch as usize),
                    width as usize * 4,
                );
                let destination_row =
                    &mut source_pixels[row * width as usize * 4..(row + 1) * width as usize * 4];
                destination_row.copy_from_slice(source_row);
            }
        }
        unsafe {
            d3d_context.Unmap(Some(&resource), 0);
        }
        frame.Close()?;
        let (output_width, output_height) = output_dimensions;
        let pixels = if (width, height) == output_dimensions {
            source_pixels
        } else {
            resize_bgra(&source_pixels, width, height, output_width, output_height)
                .ok_or(WindowsError::empty())?
        };
        Ok(Some(WindowFrame {
            source_width: content_width,
            source_height: content_height,
            width: output_width,
            height: output_height,
            pixels,
        }))
    }

    /// Resizes a packed BGRA frame into the dimensions negotiated for the
    /// stream. Windows Graphics Capture changes the compositor surface size
    /// while a window is being resized, but the existing RTP track still has
    /// the original canvas. Keeping that canvas stable prevents the decoder
    /// from mixing a newly smaller frame with the previous larger render
    /// surface (or cropping a newly larger frame).
    fn resize_bgra(
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
        let scaled_height =
            ((source_height as f64 * scale).round() as usize).clamp(1, output_height);
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

    /// Reads only sampled thumbnail pixels from the mapped BGRA texture. The
    /// one-shot preview path therefore avoids allocating or swizzling a full
    /// resolution RGBA frame on the UI's background capture worker.
    fn preview_from_pool(
        frame_pool: Ref<'_, Direct3D11CaptureFramePool>,
        d3d_device: &ID3D11Device,
        d3d_context: &ID3D11DeviceContext,
        max_width: usize,
        max_height: usize,
    ) -> windows::core::Result<WindowFrame> {
        let frame_pool = frame_pool.as_ref().ok_or(WindowsError::empty())?;
        let frame = frame_pool.TryGetNextFrame()?;
        let surface = frame.Surface()?;
        let access = surface.cast::<IDirect3DDxgiInterfaceAccess>()?;
        let source_texture = unsafe { access.GetInterface::<ID3D11Texture2D>()? };
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source_texture.GetDesc(&mut source_desc) };
        let width = source_desc.Width;
        let height = source_desc.Height;
        if width == 0 || height == 0 {
            frame.Close()?;
            return Err(WindowsError::empty());
        }
        let scale = (max_width as f32 / width as f32)
            .min(max_height as f32 / height as f32)
            .min(1.0);
        let preview_width = ((width as f32 * scale).round() as u32).max(1);
        let preview_height = ((height as f32 * scale).round() as u32).max(1);
        let mut staging_desc = source_desc;
        staging_desc.BindFlags = 0;
        staging_desc.MiscFlags = 0;
        staging_desc.Usage = D3D11_USAGE_STAGING;
        staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        unsafe { d3d_device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
        let staging = staging.ok_or(WindowsError::empty())?;
        unsafe {
            d3d_context.CopyResource(Some(&staging.cast()?), Some(&source_texture.cast()?));
        }
        let resource: ID3D11Resource = staging.cast()?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            d3d_context.Map(Some(&resource), 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let mut pixels = vec![0_u8; preview_width as usize * preview_height as usize * 4];
        let source = mapped.pData.cast::<u8>();
        unsafe {
            for y in 0..preview_height as usize {
                let source_y = y * height as usize / preview_height as usize;
                let source_row = source.add(source_y * mapped.RowPitch as usize);
                for x in 0..preview_width as usize {
                    let source_x = x * width as usize / preview_width as usize;
                    let pixel = source_row.add(source_x * 4);
                    let destination = &mut pixels[(y * preview_width as usize + x) * 4..][..4];
                    destination.copy_from_slice(&[
                        *pixel.add(2),
                        *pixel.add(1),
                        *pixel,
                        *pixel.add(3),
                    ]);
                }
            }
            d3d_context.Unmap(Some(&resource), 0);
        }
        frame.Close()?;
        Ok(WindowFrame {
            source_width: width,
            source_height: height,
            width: preview_width,
            height: preview_height,
            pixels,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn cursor_is_hidden_when_an_occluding_window_owns_the_pointer() {
            assert!(cursor_visibility(true, true));
            assert!(!cursor_visibility(true, false));
            assert!(!cursor_visibility(false, true));
        }

        #[test]
        fn monitor_center_uses_a_point_inside_the_reported_rectangle() {
            let center = monitor_center(-1920, 40, 1920, 1080).unwrap();
            assert_eq!((center.x, center.y), (-960, 580));
        }

        #[test]
        fn live_readback_schedule_keeps_the_first_frame_and_skips_early_arrivals() {
            let start = Instant::now();
            let interval = Duration::from_millis(33);
            let mut next = None;
            assert!(should_readback(&mut next, start, interval));
            assert!(!should_readback(
                &mut next,
                start + Duration::from_millis(10),
                interval
            ));
            assert!(should_readback(&mut next, start + interval, interval));
        }

        #[test]
        fn staging_reuse_requires_unchanged_dimensions() {
            assert!(can_reuse_staging(Some((1920, 1080)), 1920, 1080));
            assert!(!can_reuse_staging(Some((1920, 1080)), 1280, 720));
            assert!(!can_reuse_staging(None, 1920, 1080));
        }

        #[test]
        fn changed_window_dimensions_are_scaled_to_the_stable_stream_canvas() {
            let source = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
            let resized = resize_bgra(&source, 2, 2, 4, 4).unwrap();

            assert_eq!(resized.len(), 4 * 4 * 4);
            assert_eq!(
                resized,
                vec![
                    1, 2, 3, 4, 1, 2, 3, 4, 5, 6, 7, 8, 5, 6, 7, 8, 1, 2, 3, 4, 1, 2, 3, 4, 5, 6,
                    7, 8, 5, 6, 7, 8, 9, 10, 11, 12, 9, 10, 11, 12, 13, 14, 15, 16, 13, 14, 15, 16,
                    9, 10, 11, 12, 9, 10, 11, 12, 13, 14, 15, 16, 13, 14, 15, 16,
                ]
            );
        }

        #[test]
        fn closed_mailbox_ignores_late_frames_and_failures() {
            let mailbox = FrameMailbox {
                state: Mutex::new(FrameState::default()),
                ready: Condvar::new(),
            };
            mailbox.close();
            mailbox.publish(WindowFrame {
                source_width: 1,
                source_height: 1,
                width: 1,
                height: 1,
                pixels: vec![0, 0, 0, 255],
            });
            mailbox.fail("late callback");

            let state = mailbox.state.lock().unwrap();
            assert!(state.closed);
            assert!(state.latest.is_none());
            assert!(state.failure.is_none());
        }
    }
}

#[cfg(windows)]
pub use imp::{
    WindowCapture, WindowResizeEvent, WindowResizeWatcher, capture_monitor_preview_frame,
    capture_preview_frame, cursor_position_for,
};

#[cfg(not(windows))]
pub struct WindowCapture;
