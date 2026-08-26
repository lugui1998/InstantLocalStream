use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub const PUBLIC_IP_ENDPOINTS: &[&str] =
    &["https://ipv4.wtfismyip.com/text", "https://api.ipify.org"];

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
