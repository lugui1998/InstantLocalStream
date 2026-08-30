use std::io::Read;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::shared_capture::{SourceFrame, SourcePixelFormat};

#[derive(Debug, Clone, Serialize)]
pub struct CaptureSourceInfo {
    pub kind: String,
    pub index: usize,
    pub native_id: Option<u64>,
    pub width: u32,
    pub height: u32,
    pub fps: Option<u32>,
    pub pid: Option<u32>,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct CapturePreview {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

const MIN_WINDOW_WIDTH: u32 = 160;
const MIN_WINDOW_HEIGHT: u32 = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Monitor,
    Window,
}

fn source_kind(kind: &str) -> Result<SourceKind> {
    match kind {
        "monitor" => Ok(SourceKind::Monitor),
        "window" => Ok(SourceKind::Window),
        _ => anyhow::bail!("unsupported capture source kind '{kind}'"),
    }
}

#[cfg(windows)]
pub fn native_window_exists(native_id: u64) -> bool {
    use std::ffi::c_void;
    use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::IsWindow};

    let hwnd = HWND(native_id as usize as *mut c_void);
    !hwnd.0.is_null() && unsafe { IsWindow(Some(hwnd)).as_bool() }
}

#[cfg(not(windows))]
pub fn native_window_exists(_native_id: u64) -> bool {
    false
}

#[cfg(windows)]
pub fn native_window_pid(native_id: u64) -> Option<u32> {
    use std::ffi::c_void;
    use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::GetWindowThreadProcessId};

    if !native_window_exists(native_id) {
        return None;
    }
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(HWND(native_id as usize as *mut c_void), Some(&mut pid));
    }
    (pid != 0).then_some(pid)
}

#[cfg(not(windows))]
pub fn native_window_pid(_native_id: u64) -> Option<u32> {
    None
}

#[cfg(windows)]
pub fn native_window_is_minimized(native_id: u64) -> bool {
    use std::ffi::c_void;
    use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::IsIconic};

    native_window_exists(native_id)
        && unsafe { IsIconic(HWND(native_id as usize as *mut c_void)).as_bool() }
}

#[cfg(not(windows))]
pub fn native_window_is_minimized(_native_id: u64) -> bool {
    false
}

const NON_WINDOW_ELEMENT_CLASSES: &[&str] = &[
    "Windows.UI.Core.CoreWindow",
    "Xaml_WindowedPopupClass",
    "NotifyIconOverflowWindow",
    "Shell_SecondaryTrayWnd",
    "TrayNotifyWnd",
    "TaskListThumbnailWnd",
    "TaskSwitcherWnd",
    "ForegroundStaging",
    "PopupHost",
];

const NON_WINDOW_ELEMENT_APPS: &[&str] = &[
    "shellexperiencehost",
    "windows shell experience host",
    "searchhost",
    "textinputhost",
    "startmenuexperiencehost",
    "lockapp",
    "applicationframehost",
];

// Windows does not expose every transient surface with a distinctive class
// name. These extended styles identify owned popups, tool windows, and
// non-activating overlays without tying the filter to a particular app.
const WS_EX_APPWINDOW_FLAG: u32 = 0x0004_0000;
const WS_EX_NOACTIVATE_FLAG: u32 = 0x0800_0000;
const WS_EX_TOOLWINDOW_FLAG: u32 = 0x0000_0080;

fn window_style_is_non_window_element(ex_style: u32, has_owner: bool) -> bool {
    let is_app_window = ex_style & WS_EX_APPWINDOW_FLAG != 0;
    let is_tool_window = ex_style & WS_EX_TOOLWINDOW_FLAG != 0;
    let does_not_activate = ex_style & WS_EX_NOACTIVATE_FLAG != 0;

    !is_app_window && (has_owner || is_tool_window || does_not_activate)
}

#[cfg(windows)]
fn window_class_name(native_id: u64) -> Option<String> {
    use std::ffi::c_void;
    use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::GetClassNameW};

    let window_id = u32::try_from(native_id).ok()?;
    let hwnd = HWND(window_id as usize as *mut c_void);
    if hwnd.0.is_null() {
        return None;
    }
    let mut class_name = [0_u16; 256];
    let class_name_length = unsafe { GetClassNameW(hwnd, &mut class_name) } as usize;
    (class_name_length > 0).then(|| String::from_utf16_lossy(&class_name[..class_name_length]))
}

#[cfg(windows)]
fn native_window_has_non_window_style(native_id: u64) -> bool {
    use std::ffi::c_void;
    use windows::Win32::UI::WindowsAndMessaging::{
        GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongPtrW,
    };

    let Ok(window_id) = u32::try_from(native_id) else {
        return false;
    };
    let hwnd = windows::Win32::Foundation::HWND(window_id as usize as *mut c_void);
    if hwnd.0.is_null() {
        return false;
    }

    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 };
    let has_owner = unsafe { GetWindow(hwnd, GW_OWNER) }
        .ok()
        .is_some_and(|owner| !owner.0.is_null());
    window_style_is_non_window_element(ex_style, has_owner)
}

#[cfg(not(windows))]
fn native_window_has_non_window_style(_native_id: u64) -> bool {
    false
}

#[cfg(windows)]
pub fn native_window_is_non_window_element(native_id: u64) -> bool {
    let known_class = window_class_name(native_id).is_some_and(|class_name| {
        NON_WINDOW_ELEMENT_CLASSES
            .iter()
            .any(|known| class_name.eq_ignore_ascii_case(known))
    });
    known_class || native_window_has_non_window_style(native_id)
}

#[cfg(not(windows))]
pub fn native_window_is_non_window_element(_native_id: u64) -> bool {
    false
}

fn is_capturable_window(window: &xcap::Window) -> bool {
    window.width().unwrap_or(0) >= MIN_WINDOW_WIDTH
        && window.height().unwrap_or(0) >= MIN_WINDOW_HEIGHT
}

#[cfg(windows)]
fn is_non_window_element(window: &xcap::Window) -> bool {
    let native_id = window.id().ok().map(u64::from);
    let class_name = native_id.and_then(window_class_name);
    let class_is_non_window = class_name.is_some_and(|class_name| {
        NON_WINDOW_ELEMENT_CLASSES
            .iter()
            .any(|known| class_name.eq_ignore_ascii_case(known))
    });
    if class_is_non_window || native_id.is_some_and(native_window_has_non_window_style) {
        return true;
    }
    let app_name = window.app_name().unwrap_or_default().to_ascii_lowercase();
    NON_WINDOW_ELEMENT_APPS
        .iter()
        .any(|known| app_name.contains(known))
}

#[cfg(not(windows))]
fn is_non_window_element(_window: &xcap::Window) -> bool {
    false
}

fn should_include_window(window: &xcap::Window, display_non_window_elements: bool) -> bool {
    is_capturable_window(window) && (display_non_window_elements || !is_non_window_element(window))
}

pub(crate) fn selected_window(index: usize, native_id: Option<u64>) -> Result<xcap::Window> {
    let windows = xcap::Window::all()?
        .into_iter()
        .filter(is_capturable_window)
        .collect::<Vec<_>>();
    if let Some(native_id) = native_id {
        return windows
            .into_iter()
            .find(|window| window.id().ok().map(u64::from) == Some(native_id))
            .context("selected capture window no longer exists");
    }
    windows
        .into_iter()
        .filter(|window| !is_non_window_element(window))
        .nth(index)
        .context("capture window does not exist")
}

pub fn capture_preview(
    source: &CaptureSourceInfo,
    max_width: usize,
    max_height: usize,
) -> Result<CapturePreview> {
    #[cfg(windows)]
    if source.kind == "window" {
        // Thumbnail capture must not synchronously invoke the target
        // application (as PrintWindow does). WGC supplies a compositor frame
        // asynchronously and the helper returns after one bounded attempt.
        let preview = crate::window_capture::capture_preview_frame(
            source.index,
            source.native_id,
            max_width,
            max_height,
            std::time::Duration::from_millis(400),
        )?;
        return Ok(CapturePreview {
            width: preview.width as usize,
            height: preview.height as usize,
            rgba: preview.pixels,
        });
    }

    #[cfg(windows)]
    if source.kind == "monitor" {
        // Use the compositor surface directly so a thumbnail never requires a
        // full-resolution GDI bitmap before it is downscaled.
        let preview = crate::window_capture::capture_monitor_preview_frame(
            source.index,
            max_width,
            max_height,
            std::time::Duration::from_millis(400),
        )?;
        return Ok(CapturePreview {
            width: preview.width as usize,
            height: preview.height as usize,
            rgba: preview.pixels,
        });
    }

    let image = if source.kind == "monitor" {
        xcap::Monitor::all()?
            .into_iter()
            .nth(source.index)
            .context("capture monitor does not exist")?
            .capture_image()?
    } else if source.kind == "window" {
        selected_window(source.index, source.native_id)?.capture_image()?
    } else {
        anyhow::bail!("source previews are unavailable for '{}'", source.kind)
    };
    let width = image.width() as usize;
    let height = image.height() as usize;
    let rgba = image.into_raw();
    Ok(downscale_preview(
        width, height, rgba, max_width, max_height,
    ))
}

pub fn capture_test_pattern_preview(
    ffmpeg: &str,
    width: u32,
    height: u32,
    fps: u32,
    max_width: usize,
    max_height: usize,
) -> Result<CapturePreview> {
    let size = format!("{width}x{height}");
    let mut command = Command::new(ffmpeg);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size={size}:rate={fps}"),
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start FFmpeg test preview from '{ffmpeg}'"))?;
    let mut rgba = Vec::new();
    child
        .stdout
        .take()
        .context("FFmpeg test preview did not expose stdout")?
        .read_to_end(&mut rgba)?;
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("FFmpeg test preview exited with {status}");
    }
    let expected = width as usize * height as usize * 4;
    if rgba.len() < expected {
        anyhow::bail!(
            "FFmpeg test preview returned {} bytes, expected {expected}",
            rgba.len()
        );
    }
    rgba.truncate(expected);
    Ok(downscale_preview(
        width as usize,
        height as usize,
        rgba,
        max_width,
        max_height,
    ))
}

fn downscale_preview(
    width: usize,
    height: usize,
    rgba: Vec<u8>,
    max_width: usize,
    max_height: usize,
) -> CapturePreview {
    let (preview_width, preview_height) = preview_dimensions(width, height, max_width, max_height);
    let mut preview = vec![0_u8; preview_width * preview_height * 4];
    for y in 0..preview_height {
        let source_y = y * height / preview_height;
        for x in 0..preview_width {
            let source_x = x * width / preview_width;
            let source_offset = (source_y * width + source_x) * 4;
            let preview_offset = (y * preview_width + x) * 4;
            preview[preview_offset..preview_offset + 4]
                .copy_from_slice(&rgba[source_offset..source_offset + 4]);
        }
    }
    CapturePreview {
        width: preview_width,
        height: preview_height,
        rgba: preview,
    }
}

/// Builds a small RGBA thumbnail from the shared capture's latest frame
/// without copying its full `Arc<[u8]>` pixel buffer.
pub fn capture_frame_preview(
    frame: &SourceFrame,
    max_width: usize,
    max_height: usize,
) -> Result<CapturePreview> {
    let width = usize::try_from(frame.width).context("preview frame width exceeds usize")?;
    let height = usize::try_from(frame.height).context("preview frame height exceeds usize")?;
    anyhow::ensure!(
        width > 0 && height > 0,
        "preview frame dimensions are empty"
    );
    anyhow::ensure!(
        max_width > 0 && max_height > 0,
        "preview dimensions must be non-zero"
    );
    let pixels = width
        .checked_mul(height)
        .context("preview frame dimensions overflow")?;
    let expected = match frame.pixel_format {
        SourcePixelFormat::Bgra => pixels
            .checked_mul(4)
            .context("RGBA preview frame size overflows")?,
        SourcePixelFormat::Yuv420p => pixels
            .checked_mul(3)
            .and_then(|bytes| bytes.checked_div(2))
            .context("YUV preview frame size overflows")?,
    };
    if frame.pixel_format == SourcePixelFormat::Yuv420p {
        anyhow::ensure!(
            width.is_multiple_of(2) && height.is_multiple_of(2),
            "YUV420P preview dimensions must be even"
        );
    }
    anyhow::ensure!(
        frame.data.len() == expected,
        "preview frame has {} bytes; expected {expected}",
        frame.data.len()
    );
    let (preview_width, preview_height) = preview_dimensions(width, height, max_width, max_height);
    let mut rgba = vec![0_u8; preview_width * preview_height * 4];
    for y in 0..preview_height {
        let source_y = y * height / preview_height;
        for x in 0..preview_width {
            let source_x = x * width / preview_width;
            let destination = &mut rgba[(y * preview_width + x) * 4..][..4];
            match frame.pixel_format {
                SourcePixelFormat::Bgra => {
                    let source_offset = (source_y * width + source_x) * 4;
                    let source = &frame.data[source_offset..source_offset + 4];
                    destination.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
                }
                SourcePixelFormat::Yuv420p => {
                    let chroma_width = width / 2;
                    let y_plane_size = pixels;
                    let chroma_plane_size = chroma_width * (height / 2);
                    let y_value = frame.data[source_y * width + source_x];
                    let chroma_offset = (source_y / 2) * chroma_width + source_x / 2;
                    let u_value = frame.data[y_plane_size + chroma_offset];
                    let v_value = frame.data[y_plane_size + chroma_plane_size + chroma_offset];
                    destination.copy_from_slice(&yuv420_pixel(y_value, u_value, v_value));
                }
            }
        }
    }
    Ok(CapturePreview {
        width: preview_width,
        height: preview_height,
        rgba,
    })
}

fn preview_dimensions(
    width: usize,
    height: usize,
    max_width: usize,
    max_height: usize,
) -> (usize, usize) {
    let scale = (max_width as f32 / width as f32)
        .min(max_height as f32 / height as f32)
        .min(1.0);
    (
        ((width as f32 * scale).round() as usize).max(1),
        ((height as f32 * scale).round() as usize).max(1),
    )
}

fn yuv420_pixel(y: u8, u: u8, v: u8) -> [u8; 4] {
    let c = (i32::from(y) - 16).max(0);
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    let clamp = |value: i32| ((value + 128) >> 8).clamp(0, 255) as u8;
    [
        clamp(298 * c + 409 * e),
        clamp(298 * c - 100 * d - 208 * e),
        clamp(298 * c + 516 * d),
        255,
    ]
}

pub fn ffmpeg_input_args(
    kind: &str,
    index: usize,
    _native_id: Option<u64>,
    fps: Option<u32>,
    draw_mouse: bool,
) -> Result<Vec<String>> {
    let draw_mouse = if draw_mouse { "1" } else { "0" };
    #[cfg(target_os = "windows")]
    {
        if kind == "monitor" {
            let monitor = xcap::Monitor::all()?
                .into_iter()
                .nth(index)
                .context("capture monitor does not exist")?;
            let mut args = vec![
                "-f".to_owned(),
                "gdigrab".to_owned(),
                "-draw_mouse".to_owned(),
                draw_mouse.to_owned(),
                "-offset_x".to_owned(),
                monitor.x()?.to_string(),
                "-offset_y".to_owned(),
                monitor.y()?.to_string(),
                "-video_size".to_owned(),
                format!("{}x{}", monitor.width()?, monitor.height()?),
                "-i".to_owned(),
                "desktop".to_owned(),
            ];
            insert_framerate(&mut args, fps);
            return Ok(args);
        }
        if kind == "window" {
            anyhow::bail!(
                "Windows window capture is provided by Windows Graphics Capture, not FFmpeg gdigrab"
            );
        }
    }

    #[cfg(target_os = "linux")]
    {
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0.0".to_owned());
        if kind == "monitor" {
            let monitor = xcap::Monitor::all()?
                .into_iter()
                .nth(index)
                .context("capture monitor does not exist")?;
            let mut args = vec![
                "-f".to_owned(),
                "x11grab".to_owned(),
                "-draw_mouse".to_owned(),
                draw_mouse.to_owned(),
                "-video_size".to_owned(),
                format!("{}x{}", monitor.width()?, monitor.height()?),
                "-i".to_owned(),
                format!("{}+{}+{}", display, monitor.x()?, monitor.y()?),
            ];
            insert_framerate(&mut args, fps);
            return Ok(args);
        }
        if kind == "window" {
            let window = selected_window(index, _native_id)?;
            let mut args = vec![
                "-f".to_owned(),
                "x11grab".to_owned(),
                "-draw_mouse".to_owned(),
                draw_mouse.to_owned(),
                "-window_id".to_owned(),
                window.id()?.to_string(),
                "-i".to_owned(),
                display,
            ];
            insert_framerate(&mut args, fps);
            return Ok(args);
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (index, _native_id, fps, draw_mouse);
        anyhow::bail!(
            "screen capture is currently supported only on Windows and Linux/X11; '{kind}' is unavailable on this platform"
        );
    }

    #[cfg(any(target_os = "windows", target_os = "linux"))]
    anyhow::bail!("unsupported FFmpeg capture source '{kind}'")
}

pub fn source_dimensions(kind: &str, index: usize, native_id: Option<u64>) -> Result<(u32, u32)> {
    if kind == "monitor" {
        let monitor = xcap::Monitor::all()?
            .into_iter()
            .nth(index)
            .context("capture monitor does not exist")?;
        return Ok((monitor.width()?, monitor.height()?));
    }
    if kind == "window" {
        let window = selected_window(index, native_id)?;
        return Ok((window.width()?, window.height()?));
    }
    anyhow::bail!("unsupported capture source '{kind}'")
}

fn insert_framerate(args: &mut Vec<String>, fps: Option<u32>) {
    if let Some(fps) = fps {
        args.splice(2..2, ["-framerate".to_owned(), fps.to_string()]);
    }
}

pub fn list_sources() -> Result<Vec<CaptureSourceInfo>> {
    let mut sources = list_sources_for_kind("monitor")?;
    // Preserve the historical aggregate behavior: an unavailable window
    // enumerator must not prevent monitor sources from being listed.
    sources.extend(list_sources_for_kind("window").unwrap_or_default());
    Ok(sources)
}

/// Lists only one source class, avoiding unrelated desktop enumeration.
pub fn list_sources_for_kind(kind: &str) -> Result<Vec<CaptureSourceInfo>> {
    list_sources_for_kind_with_options(kind, false)
}

pub(crate) fn list_sources_for_kind_with_options(
    kind: &str,
    display_non_window_elements: bool,
) -> Result<Vec<CaptureSourceInfo>> {
    match source_kind(kind)? {
        SourceKind::Monitor => list_monitors(),
        SourceKind::Window => list_windows_with_options(display_non_window_elements),
    }
}

fn list_monitors() -> Result<Vec<CaptureSourceInfo>> {
    let monitors = xcap::Monitor::all().context("enumerate displays")?;
    monitors
        .into_iter()
        .enumerate()
        .map(|(index, monitor)| {
            let width = monitor.width()?;
            let height = monitor.height()?;
            let fps = monitor
                .frequency()
                .ok()
                .map(|frequency| frequency.round() as u32)
                .filter(|fps| *fps > 0);
            let name = monitor
                .friendly_name()
                .or_else(|_| monitor.name())
                .unwrap_or_else(|_| {
                    if index == 0 {
                        "Primary monitor".to_owned()
                    } else {
                        format!("Monitor {index}")
                    }
                });
            Ok(CaptureSourceInfo {
                kind: "monitor".to_owned(),
                index,
                native_id: None,
                width,
                height,
                fps,
                pid: None,
                name,
            })
        })
        .collect::<Result<Vec<_>>>()
}

pub fn list_windows() -> Result<Vec<CaptureSourceInfo>> {
    list_windows_with_options(false)
}

fn list_windows_with_options(display_non_window_elements: bool) -> Result<Vec<CaptureSourceInfo>> {
    let windows = xcap::Window::all().context("enumerate windows")?;
    Ok(windows
        .into_iter()
        .filter(|window| should_include_window(window, display_non_window_elements))
        .enumerate()
        .filter_map(|(index, window)| {
            let width = window.width().ok()?;
            let height = window.height().ok()?;
            let fps = window
                .current_monitor()
                .ok()
                .and_then(|monitor| monitor.frequency().ok())
                .map(|frequency| frequency.round() as u32)
                .filter(|fps| *fps > 0);
            let title = window
                .title()
                .unwrap_or_else(|_| "Untitled window".to_owned());
            let app = window
                .app_name()
                .unwrap_or_else(|_| "Unknown application".to_owned());
            Some(CaptureSourceInfo {
                kind: "window".to_owned(),
                index,
                native_id: window.id().ok().map(u64::from),
                width,
                height,
                fps,
                pid: window.pid().ok(),
                name: format!("{app}: {title}"),
            })
        })
        .collect())
}

pub fn print_sources(json_output: bool) -> Result<()> {
    let sources = list_sources()?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&sources)?);
    } else if sources.is_empty() {
        println!("No monitors found.");
    } else {
        for source in sources {
            println!(
                "{}:{}  {} ({}x{})",
                source.kind, source.index, source.name, source.width, source.height
            );
        }
    }
    Ok(())
}

pub fn print_windows(json_output: bool) -> Result<()> {
    let windows = list_windows()?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&windows)?);
    } else if windows.is_empty() {
        println!("No capturable windows found.");
    } else {
        for window in windows {
            println!(
                "window:{}  {} ({}x{})",
                window.index, window.name, window.width, window.height
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_kind_dispatch_rejects_unknown_kind() {
        assert_eq!(source_kind("monitor").unwrap(), SourceKind::Monitor);
        assert_eq!(source_kind("window").unwrap(), SourceKind::Window);
        let error = source_kind("camera").expect_err("unknown kinds must fail");
        assert!(
            error
                .to_string()
                .contains("unsupported capture source kind 'camera'")
        );
    }

    #[test]
    fn popup_window_styles_are_filtered_without_app_specific_names() {
        assert!(window_style_is_non_window_element(
            WS_EX_TOOLWINDOW_FLAG,
            false
        ));
        assert!(window_style_is_non_window_element(
            WS_EX_NOACTIVATE_FLAG,
            false
        ));
        assert!(window_style_is_non_window_element(0, true));

        assert!(!window_style_is_non_window_element(0, false));
        assert!(!window_style_is_non_window_element(
            WS_EX_APPWINDOW_FLAG | WS_EX_TOOLWINDOW_FLAG,
            false
        ));
        assert!(!window_style_is_non_window_element(
            WS_EX_APPWINDOW_FLAG | WS_EX_NOACTIVATE_FLAG,
            true
        ));
    }

    #[test]
    fn downscale_preview_preserves_aspect_ratio_and_samples_rgba() {
        let rgba = vec![
            1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255, 13, 14, 15, 255, 16, 17, 18,
            255, 19, 20, 21, 255, 22, 23, 24, 255,
        ];
        let preview = downscale_preview(4, 2, rgba, 2, 2);
        assert_eq!((preview.width, preview.height), (2, 1));
        assert_eq!(preview.rgba, vec![1, 2, 3, 255, 7, 8, 9, 255]);
    }

    #[test]
    fn shared_bgra_frame_is_swizzled_only_for_the_thumbnail() {
        let frame = SourceFrame {
            width: 1,
            height: 1,
            pixel_format: SourcePixelFormat::Bgra,
            captured_at_unix_nanos: 0,
            data: std::sync::Arc::from(vec![3, 2, 1, 255]),
        };

        let preview = capture_frame_preview(&frame, 1, 1).unwrap();

        assert_eq!(preview.rgba, vec![1, 2, 3, 255]);
    }

    #[test]
    fn shared_yuv420_frame_is_converted_to_rgba_thumbnail() {
        let frame = SourceFrame {
            width: 2,
            height: 2,
            pixel_format: SourcePixelFormat::Yuv420p,
            captured_at_unix_nanos: 0,
            data: std::sync::Arc::from(vec![235, 235, 235, 235, 128, 128]),
        };

        let preview = capture_frame_preview(&frame, 1, 1).unwrap();

        assert_eq!((preview.width, preview.height), (1, 1));
        assert_eq!(preview.rgba, vec![255, 255, 255, 255]);
    }
}
