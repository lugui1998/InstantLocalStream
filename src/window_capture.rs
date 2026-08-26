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
            Arc, Condvar, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use anyhow::{Context, Result, bail};
    use windows::{
        Foundation::TypedEventHandler,
        Graphics::{
            Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
            DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        },
        Win32::{
            Foundation::{HMODULE, HWND, POINT},
            Graphics::{
                Direct3D::D3D_DRIVER_TYPE_HARDWARE,
                Direct3D11::{
                    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
                    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
                    ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
                },
                Dxgi::IDXGIDevice,
            },
            System::WinRT::{
                Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
                Graphics::Capture::IGraphicsCaptureItemInterop,
                RO_INIT_MULTITHREADED, RoInitialize,
            },
            UI::WindowsAndMessaging::{
                GA_ROOT, GetAncestor, GetCursorPos, GetWindowRect, IsWindow, WindowFromPoint,
            },
        },
        core::{Error as WindowsError, IInspectable, Interface, Ref, factory},
    };

    #[derive(Debug)]
    pub struct WindowFrame {
        pub width: u32,
        pub height: u32,
        /// Packed RGBA8 pixels, one complete frame per value.
        pub rgba: Vec<u8>,
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

    impl FrameMailbox {
        fn publish(&self, frame: WindowFrame) {
            if let Ok(mut state) = self.state.lock() {
                state.latest = Some(frame);
                self.ready.notify_all();
            }
        }

        fn fail(&self, error: impl Into<String>) {
            if let Ok(mut state) = self.state.lock() {
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
        session: GraphicsCaptureSession,
        closed: bool,
    }

    impl WgcRuntime {
        fn close(&mut self) {
            if self.closed {
                return;
            }
            self.closed = true;
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

        pub fn start(index: usize, native_id: Option<u64>, capture_cursor: bool) -> Result<Self> {
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
            frame_pool.FrameArrived(&TypedEventHandler::<
                Direct3D11CaptureFramePool,
                IInspectable,
            >::new(move |frame_pool, _| {
                match frame_from_pool(frame_pool, &d3d_device, &d3d_context) {
                    Ok(frame) => callback_mailbox.publish(frame),
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
            if let Ok(mut runtime) = self.runtime.lock()
                && let Some(mut runtime) = runtime.take()
            {
                runtime.close();
            }
            self.mailbox.close();
        }
    }

    impl Drop for WindowCapture {
        fn drop(&mut self) {
            self.stop();
        }
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

    fn frame_from_pool(
        frame_pool: Ref<'_, Direct3D11CaptureFramePool>,
        d3d_device: &ID3D11Device,
        d3d_context: &ID3D11DeviceContext,
    ) -> windows::core::Result<WindowFrame> {
        let frame_pool = frame_pool.as_ref().ok_or(WindowsError::empty())?;
        let frame = frame_pool.TryGetNextFrame()?;
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
        let mut staging_desc = source_desc;
        staging_desc.BindFlags = 0;
        staging_desc.MiscFlags = 0;
        staging_desc.Usage = D3D11_USAGE_STAGING;
        staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        unsafe {
            d3d_device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
        }
        let staging = staging.ok_or(WindowsError::empty())?;
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
        let mut rgba = vec![0_u8; (width as usize) * (height as usize) * 4];
        let source = mapped.pData.cast::<u8>();
        unsafe {
            for row in 0..height as usize {
                let source_row = std::slice::from_raw_parts(
                    source.add(row * mapped.RowPitch as usize),
                    width as usize * 4,
                );
                let destination_row =
                    &mut rgba[row * width as usize * 4..(row + 1) * width as usize * 4];
                for (destination, source) in destination_row
                    .chunks_exact_mut(4)
                    .zip(source_row.chunks_exact(4))
                {
                    destination.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
                }
            }
        }
        unsafe {
            d3d_context.Unmap(Some(&resource), 0);
        }
        frame.Close()?;
        Ok(WindowFrame {
            width,
            height,
            rgba,
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
    }
}

#[cfg(windows)]
pub use imp::{WindowCapture, cursor_position_for};

#[cfg(not(windows))]
pub struct WindowCapture;
