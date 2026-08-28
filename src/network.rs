use std::io::{self, Read};
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub const PUBLIC_IP_ENDPOINTS: &[&str] =
    &["https://ipv4.wtfismyip.com/text", "https://api.ipify.org"];
const CLOUDFLARE_UPLOAD_ENDPOINT: &str = "https://speed.cloudflare.com/__up";
const INITIAL_UPLOAD_PROBE_SIZES: &[usize] = &[1 << 20, 4 << 20, 16 << 20, 64 << 20];
const UPLOAD_PROBE_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const UPLOAD_PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

type UploadProgressCallback = Arc<Mutex<Box<dyn FnMut(UploadSpeedTestProgress) + Send>>>;

#[derive(Debug, Clone, Copy)]
pub struct UploadSpeedTestProgress {
    pub upload_bps: u64,
}

/// Measures sustained outbound HTTP throughput using an adaptive sequence of
/// uploads to Cloudflare's edge. The largest completed upload is returned so
/// connection/setup overhead from smaller requests does not dominate the
/// result. A slower completed sample is reported but not accepted, and stops
/// the ramp so a transient slowdown cannot replace the best stable sample.
pub fn measure_cloudflare_upload_bps_with_progress(
    on_progress: impl FnMut(UploadSpeedTestProgress) + Send + 'static,
) -> Result<u64> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .context("create upload speed test client")?;
    let progress_callback: UploadProgressCallback = Arc::new(Mutex::new(Box::new(on_progress)));
    let mut size = INITIAL_UPLOAD_PROBE_SIZES[0];
    let mut previous_upload_bps = None;
    let mut largest_measurement = None;
    loop {
        let started = Instant::now();
        let request = client
            .post(CLOUDFLARE_UPLOAD_ENDPOINT)
            .query(&[("bytes", size)])
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .timeout(UPLOAD_PROBE_REQUEST_TIMEOUT)
            .body(reqwest::blocking::Body::sized(
                UploadProbeReader::new(size, Arc::clone(&progress_callback)),
                size as u64,
            ))
            .send();
        let response = match request {
            Ok(response) => response
                .error_for_status()
                .context("Cloudflare upload speed test returned an error")?,
            Err(error) if error.is_timeout() && largest_measurement.is_some() => break,
            Err(error) => {
                return Err(error).context("Cloudflare upload speed test request failed");
            }
        };
        drop(response);

        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        let upload_bps = (size as f64 * 8.0 / elapsed) as u64;
        let is_slower_than_previous =
            previous_upload_bps.is_some_and(|previous_upload_bps| upload_bps < previous_upload_bps);
        if !is_slower_than_previous {
            largest_measurement = Some(upload_bps);
        }
        emit_upload_progress(&progress_callback, upload_bps);

        if is_slower_than_previous {
            break;
        }

        previous_upload_bps = Some(upload_bps);
        let next_size = next_upload_probe_size(size);
        let Some(next_size) = next_size else {
            break;
        };
        size = next_size;
    }
    largest_measurement
        .filter(|bps| *bps > 0)
        .context("Cloudflare upload speed test returned no usable measurements")
}

fn next_upload_probe_size(size: usize) -> Option<usize> {
    if let Some(index) = INITIAL_UPLOAD_PROBE_SIZES
        .iter()
        .position(|probe| *probe == size)
    {
        if let Some(next_size) = INITIAL_UPLOAD_PROBE_SIZES.get(index + 1) {
            return Some(*next_size);
        }
    }

    size.checked_mul(2)
}

/// Converts the measured host upload rate into a conservative viewer limit.
/// The reserved headroom covers stream overhead, bitrate variation, and other
/// traffic competing for the host's uplink.
pub fn recommended_max_viewers(upload_bps: u64, per_viewer_bitrate_bps: u32) -> usize {
    const USABLE_UPLOAD_RATIO: f64 = 0.65;
    const STREAM_OVERHEAD_RATIO: f64 = 1.10;
    let per_viewer = f64::from(per_viewer_bitrate_bps.max(250_000)) * STREAM_OVERHEAD_RATIO;
    ((upload_bps as f64 * USABLE_UPLOAD_RATIO) / per_viewer)
        .floor()
        .max(1.0) as usize
}

struct UploadProbeReader {
    remaining: u64,
    state: u64,
    uploaded: u64,
    started: Instant,
    last_report: Instant,
    progress_callback: UploadProgressCallback,
}

impl UploadProbeReader {
    fn new(size: usize, progress_callback: UploadProgressCallback) -> Self {
        let now = Instant::now();
        Self {
            remaining: size as u64,
            state: 0xA11C_E55D_1234_5678_u64,
            uploaded: 0,
            started: now,
            last_report: now,
            progress_callback,
        }
    }

    fn report_progress(&mut self) {
        if self.uploaded == 0 || self.last_report.elapsed() < UPLOAD_PROGRESS_INTERVAL {
            return;
        }
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let upload_bps = (self.uploaded as f64 * 8.0 / elapsed) as u64;
        emit_upload_progress(&self.progress_callback, upload_bps);
        self.last_report = Instant::now();
    }
}

impl Read for UploadProbeReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let amount = self.remaining.min(buffer.len() as u64) as usize;
        for byte in &mut buffer[..amount] {
            self.state ^= self.state << 13;
            self.state ^= self.state >> 7;
            self.state ^= self.state << 17;
            *byte = self.state as u8;
        }
        self.remaining -= amount as u64;
        self.uploaded += amount as u64;
        self.report_progress();
        Ok(amount)
    }
}

#[cfg(test)]
fn upload_probe_payload(size: usize) -> Vec<u8> {
    let progress_callback: UploadProgressCallback = Arc::new(Mutex::new(Box::new(|_| {})));
    let mut reader = UploadProbeReader::new(size, progress_callback);
    let mut bytes = vec![0_u8; size];
    reader
        .read_exact(&mut bytes)
        .expect("reading from the in-memory upload probe cannot fail");
    bytes
}

fn emit_upload_progress(callback: &UploadProgressCallback, upload_bps: u64) {
    if let Ok(mut callback) = callback.lock() {
        callback(UploadSpeedTestProgress { upload_bps });
    }
}

pub fn lookup_public_ipv4() -> Result<Ipv4Addr> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("create public IP lookup client")?;
    let mut failures = Vec::new();
    for endpoint in PUBLIC_IP_ENDPOINTS {
        let result = client
            .get(*endpoint)
            .send()
            .and_then(|response| response.error_for_status())
            .and_then(|response| response.text())
            .map_err(|error| error.to_string())
            .and_then(|response| {
                let value = response.trim();
                if value.is_empty() || value.chars().any(char::is_whitespace) {
                    return Err("service returned an invalid response".to_owned());
                }
                value
                    .parse::<Ipv4Addr>()
                    .map_err(|_| format!("service returned '{value}', not an IPv4 address"))
            });
        match result {
            Ok(address) => return Ok(address),
            Err(error) => failures.push(format!("{endpoint}: {error}")),
        }
    }
    bail!(
        "all public IP lookup services failed: {}",
        failures.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommendation_reserves_upload_headroom() {
        assert_eq!(recommended_max_viewers(100_000_000, 14_000_000), 4);
    }

    #[test]
    fn recommendation_always_allows_one_configurable_viewer() {
        assert_eq!(recommended_max_viewers(100_000, 14_000_000), 1);
    }

    #[test]
    fn upload_probe_payload_is_not_repeating() {
        let payload = upload_probe_payload(256);
        assert_ne!(&payload[..64], &payload[64..128]);
    }

    #[test]
    fn upload_probe_ramp_doubles_after_initial_sizes() {
        assert_eq!(next_upload_probe_size(64 << 20), Some(128 << 20));
        assert_eq!(next_upload_probe_size(128 << 20), Some(256 << 20));
        assert_eq!(next_upload_probe_size(256 << 20), Some(512 << 20));
    }
}
