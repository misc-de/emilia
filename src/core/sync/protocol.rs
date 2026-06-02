//! Wire format (JSON payloads) and the `emilia://pair?…` URL encoding.
//!
//! Used on both the server and client sides; deliberately free of GTK
//! and network details so it can be tested standalone.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Protocol version of the QR/pairing URL.
pub const PROTOCOL_VERSION: u32 = 1;
/// Version stamp of the library export (for future migrations).
pub const SCHEMA_VERSION: u32 = 1;

// --- Pairing handshake (`POST /pair`) ---

#[derive(Debug, Serialize, Deserialize)]
pub struct PairRequest {
    pub token: String,
    pub device_id: String,
    pub device_name: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PairResponse {
    pub ok: bool,
    #[serde(default)]
    pub session_token: String,
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub error: String,
}

// --- Library export (`GET /sync/export`, `POST /sync/import`) ---

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LibraryExport {
    pub schema: u32,
    pub device_name: String,
    #[serde(default)]
    pub favorites: Vec<FavoriteRec>,
    #[serde(default)]
    pub playlists: Vec<PlaylistRec>,
    #[serde(default)]
    pub podcasts: Vec<PodcastRec>,
    #[serde(default)]
    pub categories: Vec<CategoryRec>,
    #[serde(default)]
    pub eq: Vec<EqRec>,
    /// Audio files (paths relative to the music folder) – the actual bytes
    /// are transferred separately via `/files/get`.
    #[serde(default)]
    pub files: Vec<FileRec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FavoriteRec {
    pub scope: String,
    pub key: String,
    pub title: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistRec {
    pub name: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodcastRec {
    pub title: String,
    pub feed_url: String,
    #[serde(default)]
    pub image_url: Option<String>,
    /// Episodes incl. show notes – so they are available on the target device
    /// immediately and permanently, independent of the feed. Empty for older exports.
    #[serde(default)]
    pub episodes: Vec<EpisodeRec>,
}

/// A podcast episode in the sync format (mirror of [`crate::model::Episode`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeRec {
    #[serde(default)]
    pub guid: Option<String>,
    pub title: String,
    pub audio_url: String,
    #[serde(default)]
    pub published: Option<String>,
    #[serde(default)]
    pub duration: Option<String>,
    /// Show notes (HTML sanitized to plain text), if present.
    #[serde(default)]
    pub description: Option<String>,
    /// Saved playback position in ms (0 = from the start). Transferred along
    /// during sync if present.
    #[serde(default)]
    pub position_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryRec {
    pub scope: String,
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqRec {
    pub output: String,
    pub scope: String,
    pub key: String,
    pub bands: [f64; 10],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRec {
    /// Path relative to the music folder of the sending device.
    pub path: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
    #[serde(default)]
    pub size: u64,
}

// --- `emilia://pair?…` URL (out-of-band channel of the QR code) ---

/// Connection data parsed from the QR URL.
#[derive(Debug, Clone)]
pub struct PairingInfo {
    pub host: String,
    pub port: u16,
    pub fingerprint: String,
    pub token: String,
    /// Expiry time from the QR code (already checked in `parse_pair_url`).
    #[allow(dead_code)]
    pub expiry: u64,
}

/// Builds the QR/pairing URL. All values are URL-safe (IP, number,
/// base64url token without padding) – so no percent encoding is needed.
pub fn build_pair_url(host: &str, port: u16, fingerprint: &str, token: &str, expiry: u64) -> String {
    format!(
        "emilia://pair?v={PROTOCOL_VERSION}&h={host}&p={port}&fp={fingerprint}&t={token}&exp={expiry}"
    )
}

/// Parses a QR/pairing URL. `now` = current Unix timestamp (seconds)
/// for the expiry check.
pub fn parse_pair_url(text: &str, now: u64) -> Result<PairingInfo> {
    let rest = text
        .trim()
        .strip_prefix("emilia://pair?")
        .ok_or_else(|| anyhow!("not an Emilia pairing code"))?;

    let mut host = String::new();
    let mut port = 0u16;
    let mut fingerprint = String::new();
    let mut token = String::new();
    let mut expiry = 0u64;
    let mut version = 0u32;

    for part in rest.split('&') {
        let (k, v) = part
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid parameter in pairing code"))?;
        match k {
            "v" => version = v.parse().unwrap_or(0),
            "h" => host = v.to_string(),
            "p" => port = v.parse().map_err(|_| anyhow!("invalid port"))?,
            "fp" => fingerprint = v.to_string(),
            "t" => token = v.to_string(),
            "exp" => expiry = v.parse().unwrap_or(0),
            _ => {}
        }
    }

    if version != PROTOCOL_VERSION {
        return Err(anyhow!("unsupported protocol version"));
    }
    if host.is_empty() || port == 0 || fingerprint.is_empty() || token.is_empty() {
        return Err(anyhow!("incomplete pairing code"));
    }
    if expiry != 0 && now > expiry {
        return Err(anyhow!("pairing code expired"));
    }

    Ok(PairingInfo {
        host,
        port,
        fingerprint,
        token,
        expiry,
    })
}

/// Minimal percent encoding for query values (path separator `/` stays readable).
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Counterpart to [`percent_encode`].
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_roundtrip() {
        let s = "Künstler/Mein Lied (Live).mp3";
        assert_eq!(percent_decode(&percent_encode(s)), s);
    }

    #[test]
    fn url_roundtrip() {
        let url = build_pair_url("192.168.1.42", 8765, "abc-DEF_123", "tok-EN_xyz", 2_000_000_000);
        let info = parse_pair_url(&url, 1_000_000_000).expect("parses");
        assert_eq!(info.host, "192.168.1.42");
        assert_eq!(info.port, 8765);
        assert_eq!(info.fingerprint, "abc-DEF_123");
        assert_eq!(info.token, "tok-EN_xyz");
        assert_eq!(info.expiry, 2_000_000_000);
    }

    #[test]
    fn rejects_expired() {
        let url = build_pair_url("10.0.0.1", 8765, "fp", "t", 1_000);
        assert!(parse_pair_url(&url, 2_000).is_err());
    }

    #[test]
    fn rejects_foreign_scheme() {
        assert!(parse_pair_url("https://example.com", 0).is_err());
    }
}
