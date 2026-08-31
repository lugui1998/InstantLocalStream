use std::process::Command;

use serde::Serialize;

const NVIDIA_CONCURRENT_SESSION_LIMIT: usize = 12;

#[derive(Debug, Clone, Serialize)]
pub struct FfmpegInfo {
    pub available: bool,
    pub command: String,
    pub version: Option<String>,
}

/// A video encoder that FFmpeg can use on a particular graphics adapter.
///
/// `gpu_index` is the ordinal used by the selected FFmpeg encoder backend.
/// NVIDIA uses FFmpeg's CUDA/NVENC ordinal; DirectX-backed encoders use their
/// native adapter ordinal.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EncodingDevice {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub hardware: bool,
    pub gpu_index: Option<usize>,
    pub h264_encoder: Option<String>,
    pub h265_encoder: Option<String>,
    /// A vendor-reported session limit when one is available.  Most desktop
    /// drivers do not expose a portable limit; `None` means capacity is
    /// negotiated by the driver when an encoder process starts.
    pub max_sessions: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedEncoder {
    pub name: String,
    pub gpu_index: Option<usize>,
    pub qsv_device: Option<usize>,
}

pub fn software_device() -> EncodingDevice {
    EncodingDevice {
        id: "software".to_owned(),
        name: "Software (CPU)".to_owned(),
        vendor: "CPU".to_owned(),
        hardware: false,
        gpu_index: None,
        h264_encoder: Some("libx264".to_owned()),
        h265_encoder: Some("libx265".to_owned()),
        max_sessions: None,
    }
}

/// Enumerates adapters and intersects them with the encoders available in
/// the selected FFmpeg binary.  Hardware discovery is intentionally best
/// effort: a machine may expose an adapter while its FFmpeg build lacks the
/// corresponding vendor integration.
pub fn discover_encoding_devices(ffmpeg: &str) -> Vec<EncodingDevice> {
    let listing = ffmpeg_encoder_listing(ffmpeg);
    let mut devices = vec![software_device()];

    #[cfg(windows)]
    {
        let nvenc_devices = ffmpeg_nvenc_devices(ffmpeg, &listing);
        discover_windows_devices(&listing, nvenc_devices.as_deref(), &mut devices);
    }

    devices
}

pub fn discover_local_encoding_devices() -> Vec<EncodingDevice> {
    match crate::packaging::prepare_ffmpeg() {
        Ok(prepared) => discover_encoding_devices(&prepared.command),
        Err(_) => vec![software_device()],
    }
}

pub fn resolve_video_encoder(
    requested_device: &str,
    codec: &str,
    devices: &[EncodingDevice],
) -> Option<SelectedEncoder> {
    let codec = codec.to_ascii_lowercase();
    let encoder_for = |device: &EncodingDevice| match codec.as_str() {
        "h264" => device.h264_encoder.clone(),
        "h265" | "hevc" => device.h265_encoder.clone(),
        _ => None,
    };

    let device = if requested_device.eq_ignore_ascii_case("auto") {
        devices
            .iter()
            .find(|device| device.hardware && encoder_for(device).is_some())
    } else {
        devices.iter().find(|device| device.id == requested_device)
    }?;
    let name = encoder_for(device)?;
    Some(SelectedEncoder {
        name,
        gpu_index: device.gpu_index,
        qsv_device: (device.vendor == "Intel")
            .then_some(device.gpu_index?)
            .or_else(|| {
                device
                    .h264_encoder
                    .as_deref()
                    .or(device.h265_encoder.as_deref())
                    .is_some_and(|encoder| encoder.ends_with("_qsv"))
                    .then_some(device.gpu_index?)
            }),
    })
}

/// Returns the startup budget for `max_quality_groups=auto`.
///
/// A hardware encoder uses its known concurrent-session ceiling when one is
/// available. CPU-only systems retain a smaller conservative default because
/// every quality group is a separate software encode process.
pub fn automatic_quality_group_budget(requested_device: &str, devices: &[EncodingDevice]) -> usize {
    if requested_device.eq_ignore_ascii_case("software") {
        return 2;
    }
    let selected = if requested_device.eq_ignore_ascii_case("auto") {
        devices.iter().find(|device| {
            device.hardware && (device.h264_encoder.is_some() || device.h265_encoder.is_some())
        })
    } else {
        devices.iter().find(|device| device.id == requested_device)
    };
    selected
        .map(|device| {
            if !device.hardware {
                2
            } else {
                device
                    .max_sessions
                    .unwrap_or(4)
                    .clamp(1, crate::config::MAX_QUALITY_GROUPS)
            }
        })
        .unwrap_or(2)
}

/// Returns the largest quality-group option that can be shown for any
/// detected device. This also sizes the runtime's lightweight slot pool so a
/// live GPU change does not exceed the preallocated graph.
pub fn maximum_quality_group_budget_for_codec(codec: &str, devices: &[EncodingDevice]) -> usize {
    if matches!(codec.to_ascii_lowercase().as_str(), "vp8" | "vp9") {
        return 2;
    }
    devices
        .iter()
        .map(|device| {
            if !device.hardware {
                2
            } else {
                device
                    .max_sessions
                    .unwrap_or(4)
                    .clamp(1, crate::config::MAX_QUALITY_GROUPS)
            }
        })
        .max()
        .unwrap_or(2)
}

pub fn automatic_quality_group_budget_for_codec(
    requested_device: &str,
    codec: &str,
    devices: &[EncodingDevice],
) -> usize {
    if matches!(codec.to_ascii_lowercase().as_str(), "vp8" | "vp9") {
        2
    } else {
        automatic_quality_group_budget(requested_device, devices)
    }
}

fn ffmpeg_encoder_listing(ffmpeg: &str) -> String {
    let mut command = Command::new(ffmpeg);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command.args(["-hide_banner", "-encoders"]).output();
    let Ok(output) = output else {
        return String::new();
    };
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct FfmpegNvencDevice {
    index: usize,
    name: String,
}

#[cfg(windows)]
fn ffmpeg_nvenc_devices(ffmpeg: &str, listing: &str) -> Option<Vec<FfmpegNvencDevice>> {
    let encoder = if listing.contains("h264_nvenc") {
        "h264_nvenc"
    } else if listing.contains("hevc_nvenc") {
        "hevc_nvenc"
    } else {
        return Some(Vec::new());
    };
    let mut command = Command::new(ffmpeg);
    command.args([
        "-hide_banner",
        "-loglevel",
        "verbose",
        "-f",
        "lavfi",
        "-i",
        "color=c=black:s=16x16:r=1",
        "-frames:v",
        "1",
        "-c:v",
        encoder,
        "-gpu",
        "list",
        "-f",
        "null",
        "NUL",
    ]);
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
    let output = command.output().ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let devices = parse_ffmpeg_nvenc_devices(&text);
    (!devices.is_empty()).then_some(devices)
}

#[cfg(windows)]
fn parse_ffmpeg_nvenc_devices(output: &str) -> Vec<FfmpegNvencDevice> {
    let mut current: Option<FfmpegNvencDevice> = None;
    let mut devices: Vec<FfmpegNvencDevice> = Vec::new();
    for line in output.lines() {
        if line.contains("[ GPU #") {
            if let Some(device) = current.take()
                && !devices.iter().any(|known| known.index == device.index)
            {
                devices.push(device);
            }
            if let Some(rest) = line.split_once("[ GPU #").map(|(_, rest)| rest)
                && let Some((index, rest)) = rest.split_once(" - <")
                && let Ok(index) = index.trim().parse::<usize>()
                && let Some((name, _)) = rest.split_once(" >")
            {
                current = Some(FfmpegNvencDevice {
                    index,
                    name: name.trim().to_owned(),
                });
            }
        }
        if line.contains("supports NVENC")
            && let Some(device) = current.take()
            && !devices.iter().any(|known| known.index == device.index)
        {
            devices.push(device);
        }
        if line.contains("does not support NVENC") {
            current = None;
        }
    }
    if let Some(device) = current
        && !devices.iter().any(|known| known.index == device.index)
    {
        devices.push(device);
    }
    devices
}

#[cfg(windows)]
fn normalized_device_name(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(windows)]
fn device_names_match(left: &str, right: &str) -> bool {
    let left = normalized_device_name(left);
    let right = normalized_device_name(right);
    left == right || left.contains(&right) || right.contains(&left)
}

#[cfg(windows)]
fn discover_windows_devices(
    listing: &str,
    nvenc_devices: Option<&[FfmpegNvencDevice]>,
    devices: &mut Vec<EncodingDevice>,
) {
    use std::collections::HashSet;

    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

    let Ok(factory): windows::core::Result<IDXGIFactory1> = (unsafe { CreateDXGIFactory1() })
    else {
        return;
    };
    let mut index = 0;
    let mut seen_adapter_ids = HashSet::new();
    let mut dxgi_devices = Vec::new();
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
        let Ok(description) = (unsafe { adapter.GetDesc1() }) else {
            index += 1;
            continue;
        };
        // Some Windows driver stacks expose the same physical adapter more
        // than once through DXGI (for example, once as a display node and
        // once as a render node). The LUID is the adapter identity; unlike
        // the name, it does not collapse two real GPUs with the same model.
        let adapter_id = (
            description.AdapterLuid.LowPart,
            description.AdapterLuid.HighPart,
        );
        if !seen_adapter_ids.insert(adapter_id) {
            index += 1;
            continue;
        }
        let software = (description.Flags & 0x2) != 0;
        let name = String::from_utf16_lossy(&description.Description)
            .trim_end_matches('\0')
            .to_owned();
        let vendor = match description.VendorId {
            0x10de => "NVIDIA",
            0x1002 => "AMD",
            0x8086 => "Intel",
            _ => "GPU",
        };
        if !software {
            dxgi_devices.push((index as usize, vendor.to_owned(), name));
        }
        index += 1;
    }

    let mut remaining_nvenc = nvenc_devices.map(<[FfmpegNvencDevice]>::to_vec);
    for (dxgi_index, vendor, name) in dxgi_devices {
        if vendor == "NVIDIA" {
            if let Some(remaining) = remaining_nvenc.as_mut() {
                let Some(position) = remaining
                    .iter()
                    .position(|device| device_names_match(&name, &device.name))
                else {
                    continue;
                };
                let device = remaining.remove(position);
                devices.push(nvidia_device(device.index, device.name, listing));
                continue;
            }
        }
        let (h264_encoder, h265_encoder) = match vendor.as_str() {
            "AMD" => (
                (listing.is_empty() || listing.contains("h264_amf")).then(|| "h264_amf".to_owned()),
                (listing.is_empty() || listing.contains("hevc_amf")).then(|| "hevc_amf".to_owned()),
            ),
            "Intel" => (
                (listing.is_empty() || listing.contains("h264_qsv")).then(|| "h264_qsv".to_owned()),
                (listing.is_empty() || listing.contains("hevc_qsv")).then(|| "hevc_qsv".to_owned()),
            ),
            "NVIDIA" => (
                (listing.is_empty() || listing.contains("h264_nvenc"))
                    .then(|| "h264_nvenc".to_owned()),
                (listing.is_empty() || listing.contains("hevc_nvenc"))
                    .then(|| "hevc_nvenc".to_owned()),
            ),
            _ => (None, None),
        };
        devices.push(EncodingDevice {
            id: format!("gpu:{dxgi_index}"),
            name: format!("{vendor} · {name}"),
            vendor: vendor.to_owned(),
            hardware: true,
            gpu_index: Some(dxgi_index),
            h264_encoder,
            h265_encoder,
            max_sessions: None,
        });
    }

    if let Some(remaining) = remaining_nvenc {
        for device in remaining {
            devices.push(nvidia_device(device.index, device.name, listing));
        }
    }
}

#[cfg(windows)]
fn nvidia_device(index: usize, name: String, listing: &str) -> EncodingDevice {
    EncodingDevice {
        id: format!("gpu:{index}"),
        name: format!("NVIDIA · {name}"),
        vendor: "NVIDIA".to_owned(),
        hardware: true,
        gpu_index: Some(index),
        h264_encoder: listing
            .contains("h264_nvenc")
            .then(|| "h264_nvenc".to_owned()),
        h265_encoder: listing
            .contains("hevc_nvenc")
            .then(|| "hevc_nvenc".to_owned()),
        // NVIDIA documents a 12-session concurrent limit for non-qualified
        // GPUs. This is a session ceiling, not a throughput guarantee; the
        // driver remains authoritative.
        max_sessions: Some(NVIDIA_CONCURRENT_SESSION_LIMIT),
    }
}

pub fn find_ffmpeg() -> FfmpegInfo {
    if let Ok(prepared) = crate::packaging::prepare_ffmpeg() {
        let mut command = Command::new(&prepared.command);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let result = command.arg("-version").output();
        if let Ok(output) = result
            && output.status.success()
        {
            let first_line = String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .map(str::to_owned);
            let command = if crate::packaging::EMBEDDED_FFMPEG.is_some() {
                "embedded FFmpeg".to_owned()
            } else {
                prepared.command.clone()
            };
            return FfmpegInfo {
                available: true,
                command,
                version: first_line,
            };
        }
    }
    FfmpegInfo {
        available: false,
        command: "ffmpeg".to_owned(),
        version: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nvidia() -> EncodingDevice {
        EncodingDevice {
            id: "gpu:2".to_owned(),
            name: "NVIDIA · Test GPU".to_owned(),
            vendor: "NVIDIA".to_owned(),
            hardware: true,
            gpu_index: Some(2),
            h264_encoder: Some("h264_nvenc".to_owned()),
            h265_encoder: Some("hevc_nvenc".to_owned()),
            max_sessions: Some(8),
        }
    }

    #[test]
    fn auto_selects_a_compatible_hardware_encoder() {
        let selected = resolve_video_encoder("auto", "h264", &[software_device(), nvidia()]);
        assert_eq!(
            selected,
            Some(SelectedEncoder {
                name: "h264_nvenc".to_owned(),
                gpu_index: Some(2),
                qsv_device: None,
            })
        );
    }

    #[test]
    fn automatic_budget_uses_known_session_limit() {
        assert_eq!(
            automatic_quality_group_budget("auto", &[software_device(), nvidia()]),
            8
        );
        assert_eq!(
            automatic_quality_group_budget(
                "gpu:2",
                &[EncodingDevice {
                    max_sessions: Some(2),
                    ..nvidia()
                }]
            ),
            2
        );
    }

    #[test]
    fn maximum_budget_exposes_more_than_four_hardware_slots() {
        assert_eq!(
            maximum_quality_group_budget_for_codec("h264", &[software_device(), nvidia()]),
            8
        );
    }

    #[cfg(windows)]
    #[test]
    fn ffmpeg_nvenc_parser_keeps_capable_devices_only() {
        let output = "[ GPU #0 - < NVIDIA GeForce RTX 5090 > has Compute SM 12.0 ]\n\
supports NVENC\n\
[ GPU #1 - < NVIDIA GeForce RTX 5090 > has Compute SM 12.0 ]\n\
does not support NVENC\n\
[ GPU #2 - < NVIDIA GeForce RTX 5090 > has Compute SM 12.0 ]\n\
supports NVENC\n";
        assert_eq!(
            parse_ffmpeg_nvenc_devices(output),
            vec![
                FfmpegNvencDevice {
                    index: 0,
                    name: "NVIDIA GeForce RTX 5090".to_owned(),
                },
                FfmpegNvencDevice {
                    index: 2,
                    name: "NVIDIA GeForce RTX 5090".to_owned(),
                },
            ]
        );
    }
}
