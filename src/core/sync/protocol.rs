//! Wire format (JSON payloads) and the `emilia://pair?…` URL encoding.
//!
//! Used on both the server and client sides; deliberately free of GTK
//! and network details so it can be tested standalone.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Protocol version of the QR/pairing URL. Bumped to 2 for the selective-share
/// redesign (offer/review/accept) — a hard cutover, since both ends are the same
/// app build and [`parse_pair_url`] rejects a mismatched `v`.
pub const PROTOCOL_VERSION: u32 = 2;
/// Version stamp of the share manifest / library payloads.
pub const SCHEMA_VERSION: u32 = 2;

/// Capabilities a device advertises at pair time, so the peer can tailor what it
/// offers (e.g. only offer YouTube items if the receiver has YouTube enabled).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub youtube_enabled: bool,
}

// --- Pairing handshake (`POST /pair`) ---

#[derive(Debug, Serialize, Deserialize)]
pub struct PairRequest {
    pub token: String,
    pub device_id: String,
    pub device_name: String,
    /// What the pairing (client) device can accept.
    #[serde(default)]
    pub caps: Capabilities,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PairResponse {
    pub ok: bool,
    #[serde(default)]
    pub session_token: String,
    #[serde(default)]
    pub device_name: String,
    /// What the answering (server) device can accept.
    #[serde(default)]
    pub caps: Capabilities,
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

/// Collected (online-enriched) metadata of a shared artist: the photo travels
/// inline as base64 so the receiver does not have to re-fetch it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaArtistRec {
    pub name: String,
    /// Base64 of the cached artist photo (PNG/JPEG bytes), if any.
    #[serde(default)]
    pub image: Option<String>,
}

/// Collected metadata of a shared album: cover (base64) and release year.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaAlbumRec {
    pub artist: String,
    pub album: String,
    #[serde(default)]
    pub year: Option<i32>,
    /// Base64 of the cached cover image (PNG/JPEG bytes), if any.
    #[serde(default)]
    pub cover: Option<String>,
}

/// A saved internet-radio station (name + stream URL + optional logo url/genre).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StationRec {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub favicon: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub genre: Option<String>,
}

/// A timeshift recording's library row. The audio travels as a normal file
/// (recordings live under `<Music>/Streaming`, inside the music folder), keyed
/// by `rel_path`; this row makes it show up in the Streaming → recordings list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingRec {
    /// Path relative to the music folder (matches the transferred file entry).
    pub rel_path: String,
    #[serde(default)]
    pub artist: Option<String>,
    pub title: String,
    #[serde(default)]
    pub station: Option<String>,
    #[serde(default)]
    pub incomplete: bool,
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
pub fn build_pair_url(
    host: &str,
    port: u16,
    fingerprint: &str,
    token: &str,
    expiry: u64,
) -> String {
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
            // Slice the raw bytes (never the &str): a multibyte UTF-8 char right
            // after the `%` would not sit on a char boundary and panic a str
            // slice. `from_utf8` + `from_str_radix` reject any non-ASCII-hex pair.
            if let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
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
    fn percent_decode_tolerates_malformed_input() {
        // A bare `%` directly followed by a multibyte char must NOT panic
        // (used to slice the &str off a non-char boundary). Left verbatim.
        assert_eq!(percent_decode("%ä"), "%ä");
        // `%` at the very end, and an incomplete/invalid escape: passed through.
        assert_eq!(percent_decode("abc%"), "abc%");
        assert_eq!(percent_decode("a%ZZb"), "a%ZZb");
        // A valid escape next to a multibyte char still decodes.
        assert_eq!(percent_decode("ä%2Fb"), "ä/b");
    }

    #[test]
    fn url_roundtrip() {
        let url = build_pair_url(
            "192.168.1.42",
            8765,
            "abc-DEF_123",
            "tok-EN_xyz",
            2_000_000_000,
        );
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
