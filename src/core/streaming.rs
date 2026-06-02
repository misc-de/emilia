//! Streaming / Internet-Radio: Sender weltweit über die **Radio-Browser-API**
//! suchen (kein API-Key nötig). Die Wiedergabe selbst läuft – wie bei Podcasts –
//! direkt über `playbin3`; es wird nichts heruntergeladen.

use std::io::Read;
use std::time::Duration;

use anyhow::Result;

/// Ein Treffer der Sendersuche (Radio-Browser): genug, um ihn anzuzeigen und –
/// bei Auswahl – als Sender zu speichern.
#[derive(Debug, Clone)]
pub struct StationResult {
    pub name: String,
    /// Spielbare Stream-URL. Bevorzugt `url_resolved` (löst `.pls`/`.m3u` bereits
    /// auf), sonst die Roh-URL.
    pub url: String,
    pub favicon: Option<String>,
    /// Genre/Schlagworte (kommasepariert).
    pub tags: Option<String>,
    pub country: Option<String>,
    pub codec: Option<String>,
    pub bitrate: Option<i64>,
}

/// Bevorzugter Radio-Browser-Spiegel. Es gibt mehrere; einer genügt.
const API_BASE: &str = "https://de1.api.radio-browser.info";

/// Sucht Sender über die Radio-Browser-API. **Blockierend** – nur aus
/// Worker-Threads aufrufen. Leerer Suchbegriff ergibt eine leere Liste.
pub fn search_stations(term: &str) -> Result<Vec<StationResult>> {
    let term = term.trim();
    if term.is_empty() {
        return Ok(Vec::new());
    }
    // Nach Beliebtheit (Stimmen) sortiert; defekte Sender ausblenden.
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
        // Die API bittet um einen aussagekräftigen User-Agent.
        .set("User-Agent", &format!("Emilia/{}", env!("CARGO_PKG_VERSION")))
        .call()?
        .into_reader()
        .take(8 * 1024 * 1024) // Deckel gegen unerwartet große Antworten.
        .read_to_end(&mut bytes)?;
    parse_stations(&bytes)
}

/// Wertet die Radio-Browser-Antwort aus. Treffer ohne spielbare URL werden
/// verworfen.
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
                "Sender".to_string()
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

/// Leitet aus einer Stream-URL einen brauchbaren Anzeigenamen ab (Host ohne
/// „www."). Für manuell hinzugefügte Sender, die keine Metadaten mitbringen.
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
