use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub pid: u32,
    pub http_port: u16,
    pub media_port: u16,
    pub viewer_url: String,
    pub source: String,
    pub codec: String,
}

pub fn write(status: &RuntimeStatus) -> Result<()> {
    let directory = status_directory();
    fs::create_dir_all(&directory).context("create runtime status directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .context("restrict runtime status directory permissions")?;
    }

    let path = status_path(status.http_port);
    let temporary = directory.join(format!(
        "status-{}-{}.tmp",
        status.http_port,
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .context("create temporary runtime status")?;
        file.write_all(&serde_json::to_vec_pretty(status)?)
            .context("write temporary runtime status")?;
        file.sync_all().context("flush temporary runtime status")?;
        #[cfg(windows)]
        if path.exists() {
            fs::remove_file(&path).context("replace previous runtime status")?;
        }
        fs::rename(&temporary, &path).context("publish runtime status")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(())
}

pub fn read(http_port: u16) -> Result<Option<RuntimeStatus>> {
    let path = status_path(http_port);
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = fs::read(&path).context("read runtime status")?;
    let status: RuntimeStatus = match serde_json::from_slice(&bytes) {
        Ok(status) => status,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "removing malformed runtime status");
            let _ = fs::remove_file(path);
            return Ok(None);
        }
    };
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, status.http_port));
    if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err() {
        remove(http_port);
        return Ok(None);
    }
    Ok(Some(status))
}

pub fn remove(http_port: u16) {
    let _ = fs::remove_file(status_path(http_port));
}

fn status_directory() -> PathBuf {
    std::env::temp_dir().join("InstantLocalStream")
}

fn status_path(http_port: u16) -> PathBuf {
    status_directory().join(format!("status-{http_port}.json"))
}
