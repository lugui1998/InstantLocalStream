use std::process::Command;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct FfmpegInfo {
    pub available: bool,
    pub command: String,
    pub version: Option<String>,
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
