use std::io::Read;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::Serialize;

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

fn is_capturable_window(window: &xcap::Window) -> bool {
    window.width().unwrap_or(0) >= MIN_WINDOW_WIDTH
        && window.height().unwrap_or(0) >= MIN_WINDOW_HEIGHT
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
        .nth(index)
        .context("capture window does not exist")
}

pub fn capture_preview(
    source: &CaptureSourceInfo,
    max_width: usize,
    max_height: usize,
) -> Result<CapturePreview> {
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
    let scale = (max_width as f32 / width as f32)
        .min(max_height as f32 / height as f32)
        .min(1.0);
    let preview_width = ((width as f32 * scale).round() as usize).max(1);
    let preview_height = ((height as f32 * scale).round() as usize).max(1);
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

    #[cfg(not(target_os = "windows"))]
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
    let monitors = xcap::Monitor::all().context("enumerate displays")?;
    let mut sources: Vec<CaptureSourceInfo> = monitors
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
        .collect::<Result<Vec<_>>>()?;

    sources.extend(list_windows().unwrap_or_default());
    Ok(sources)
}

pub fn list_windows() -> Result<Vec<CaptureSourceInfo>> {
    let windows = xcap::Window::all().context("enumerate windows")?;
    Ok(windows
        .into_iter()
        .filter(is_capturable_window)
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
