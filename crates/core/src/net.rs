//! Minimal HTTPS helpers. Only called from explicit user actions (a button press),
//! never in the background, and never with personal data in the request.

use std::time::Duration;

use anyhow::{Context, Result};

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent(concat!("win-manager-rs/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// GET `url` and return the body as text.
pub fn get_text(url: &str) -> Result<String> {
    agent()
        .get(url)
        .call()
        .with_context(|| format!("requesting {url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("reading {url}"))
}

/// GET `url` and return the body as bytes (up to `limit` bytes).
pub fn get_bytes(url: &str, limit: u64) -> Result<Vec<u8>> {
    agent()
        .get(url)
        .call()
        .with_context(|| format!("requesting {url}"))?
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .with_context(|| format!("reading {url}"))
}

/// This connection's public IP address, as seen by a public echo service.
pub fn external_ip() -> Result<String> {
    let ip = get_text("https://api.ipify.org")?.trim().to_owned();
    anyhow::ensure!(ip.parse::<std::net::IpAddr>().is_ok(), "unexpected answer: {ip}");
    Ok(ip)
}
