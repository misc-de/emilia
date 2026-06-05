//! Streaming / internet radio: searching stations worldwide via the
//! **Radio-Browser API** (no API key needed). Playback itself runs – as with
//! podcasts – directly through `playbin3`; nothing is downloaded.

use std::io::Read;
use std::time::Duration;

use anyhow::Result;

/// A station search result (Radio-Browser): enough to display it and –
/// when selected – save it as a station.
#[derive(Debug, Clone)]
pub struct StationResult {
    pub name: String,
    /// Playable stream URL. Prefers `url_resolved` (already resolves
    /// `.pls`/`.m3u`), otherwise the raw URL.
    pub url: String,
    pub favicon: Option<String>,
    /// Genre/tags (comma-separated).
    pub tags: Option<String>,
    pub country: Option<String>,
    pub codec: Option<String>,
    pub bitrate: Option<i64>,
}

/// Preferred Radio-Browser mirror. There are several; one is enough.
const API_BASE: &str = "https://de1.api.radio-browser.info";

/// Searches stations via the Radio-Browser API. **Blocking** – only call
/// from worker threads. An empty search term yields an empty list.
pub fn search_stations(term: &str) -> Result<Vec<StationResult>> {
    let term = term.trim();
    if term.is_empty() {
        return Ok(Vec::new());
    }
    // Sorted by popularity (votes); hide broken stations.
    let url = format!(
        "{API_BASE}/json/stations/search?limit=60&hidebroken=true&order=votes&reverse=true&name={}",
        crate::core::online::percent_encode(term),
    );
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(20))
        .build();
    let mut bytes = Vec::new();
    agent
        .get(&url)
        // The API asks for a meaningful User-Agent.
        .set(
            "User-Agent",
            &format!("Emilia/{}", env!("CARGO_PKG_VERSION")),
        )
        .call()?
        .into_reader()
        .take(8 * 1024 * 1024) // Cap against unexpectedly large responses.
        .read_to_end(&mut bytes)?;
    parse_stations(&bytes)
}

/// Parses the Radio-Browser response. Results without a playable URL are
/// discarded.
fn parse_stations(body: &[u8]) -> Result<Vec<StationResult>> {
    let raw: Vec<RbStation> = serde_json::from_slice(body)?;
    let results = raw
        .into_iter()
        .filter_map(|s| {
            let url = s
                .url_resolved
                .filter(|u| !u.trim().is_empty())
                .or(s.url)?
                .trim()
                .to_string();
            if url.is_empty() {
                return None;
            }
            let name = s.name.unwrap_or_default().trim().to_string();
            let name = if name.is_empty() {
                "Station".to_string()
            } else {
                name
            };
            Some(StationResult {
                name,
                url,
                favicon: s.favicon.filter(|s| !s.trim().is_empty()),
                tags: s.tags.filter(|s| !s.trim().is_empty()),
                country: s.country.filter(|s| !s.trim().is_empty()),
                codec: s.codec.filter(|s| !s.trim().is_empty()),
                bitrate: s.bitrate.filter(|&b| b > 0),
            })
        })
        .collect();
    Ok(results)
}

/// Derives a usable display name from a stream URL (host without
/// "www."). For manually added stations that bring no metadata.
pub fn name_from_url(url: &str) -> String {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
        .trim_start_matches("www.")
        .trim();
    if host.is_empty() {
        url.trim().to_string()
    } else {
        host.to_string()
    }
}

#[derive(serde::Deserialize)]
struct RbStation {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    url_resolved: Option<String>,
    #[serde(default)]
    favicon: Option<String>,
    #[serde(default)]
    tags: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    codec: Option<String>,
    #[serde(default)]
    bitrate: Option<i64>,
}
