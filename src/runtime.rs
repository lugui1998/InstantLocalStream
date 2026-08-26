use std::fs;
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
    fs::write(
        status_path(status.http_port),
        serde_json::to_vec_pretty(status)?,
    )
    .context("write runtime status")?;
    Ok(())
}

pub fn read(http_port: u16) -> Result<Option<RuntimeStatus>> {
    let path = status_path(http_port);
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = fs::read(path).context("read runtime status")?;
    let status: RuntimeStatus = serde_json::from_slice(&bytes).context("parse runtime status")?;
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
