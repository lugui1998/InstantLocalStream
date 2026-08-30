use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

include!(concat!(env!("OUT_DIR"), "/embedded_ffmpeg.rs"));

const BUNDLED_LICENSES: [(&str, &[u8]); 5] = [
    ("LICENSE", include_bytes!("../LICENSE")),
    (
        "THIRD_PARTY_NOTICES.md",
        include_bytes!("../THIRD_PARTY_NOTICES.md"),
    ),
    (
        "THIRD_PARTY_LICENSES-RUST.txt",
        include_bytes!("../packaging/THIRD_PARTY_LICENSES-RUST.txt"),
    ),
    (
        "THIRD_PARTY_LICENSES-NPM.txt",
        include_bytes!("../packaging/THIRD_PARTY_LICENSES-NPM.txt"),
    ),
    (
        "FFMPEG_SOURCE_OFFER.md",
        include_bytes!("../packaging/FFMPEG_SOURCE_OFFER.md"),
    ),
];

pub fn write_bundled_licenses(output: &Path) -> Result<usize> {
    fs::create_dir_all(output).context("create license output directory")?;
    for (name, contents) in BUNDLED_LICENSES {
        fs::write(output.join(name), contents)
            .with_context(|| format!("write bundled notice {name}"))?;
    }
    let mut count = BUNDLED_LICENSES.len();
    if let Some(contents) = EMBEDDED_FFMPEG_LICENSE {
        fs::write(output.join("FFMPEG-LICENSE.txt"), contents)
            .context("write bundled FFmpeg license")?;
        count += 1;
    }
    Ok(count)
}

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
            .join("Instant-Local-Stream")
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
    let root = std::env::temp_dir().join("Instant-Local-Stream");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_license_files_can_be_extracted() {
        let output = std::env::temp_dir().join(format!(
            "instant-local-stream-license-test-{}",
            rand::random::<u64>()
        ));
        let count = write_bundled_licenses(&output).unwrap();

        assert!(count >= BUNDLED_LICENSES.len());
        for (name, expected) in BUNDLED_LICENSES {
            assert_eq!(fs::read(output.join(name)).unwrap(), expected);
        }

        fs::remove_dir_all(output).unwrap();
    }
}
