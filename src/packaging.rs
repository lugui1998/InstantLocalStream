use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

include!(concat!(env!("OUT_DIR"), "/embedded_ffmpeg.rs"));

pub struct PreparedFfmpeg {
    pub command: String,
    cleanup_dir: Option<PathBuf>,
}

impl Drop for PreparedFfmpeg {
    fn drop(&mut self) {
        if let Some(directory) = self.cleanup_dir.take() {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

pub fn prepare_ffmpeg() -> Result<PreparedFfmpeg> {
    cleanup_stale_runtime_dirs();
    if let Some(bytes) = EMBEDDED_FFMPEG {
        if bytes.is_empty() {
            bail!("embedded FFmpeg is empty; rebuild with a valid FFmpeg executable");
        }
        let directory = std::env::temp_dir()
            .join("InstantLocalStream")
            .join(format!(
                "run-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
        fs::create_dir_all(&directory).context("create FFmpeg runtime directory")?;
        let filename = if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        };
        let path = directory.join(filename);
        fs::write(&path, bytes).context("extract embedded FFmpeg")?;
        set_executable(&path)?;
        return Ok(PreparedFfmpeg {
            command: path.display().to_string(),
            cleanup_dir: Some(directory),
        });
    }

    for candidate in candidate_paths() {
        if command_works(&candidate) {
            return Ok(PreparedFfmpeg {
                command: candidate.display().to_string(),
                cleanup_dir: None,
            });
        }
    }
    bail!(
        "FFmpeg was not found. Put it on PATH or build with ILS_FFMPEG_PATH pointing to an FFmpeg executable"
    )
}

fn cleanup_stale_runtime_dirs() {
    let root = std::env::temp_dir().join("InstantLocalStream");
    let Ok(entries) = fs::read_dir(&root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_runtime_dir = path.is_dir()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("run-"));
        if !is_runtime_dir {
            continue;
        }
        let old_enough = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map(|modified| {
                now.duration_since(modified).unwrap_or_default() > Duration::from_secs(60)
            })
            .unwrap_or(false);
        if old_enough {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn candidate_paths() -> [PathBuf; 4] {
    [
        PathBuf::from("ffmpeg"),
        PathBuf::from("ffmpeg.exe"),
        PathBuf::from("ffmpeg/ffmpeg.exe"),
        PathBuf::from("ffmpeg/ffmpeg"),
    ]
}

fn command_works(path: &Path) -> bool {
    let mut command = std::process::Command::new(path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
        .arg("-version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}
