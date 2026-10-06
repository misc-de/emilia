//! Online metadata from open/free sources:
//! - **MusicBrainz** – album matching (CC0)
//! - **Cover Art Archive** – album covers (CC0)
//! - **Deezer** – artist photos (no API key needed)
//! - **AcoustID** + **Chromaprint** – track detection via audio fingerprint
//!   (needs a free application key)
//!
//! Important: this module **never** reads audio files and certainly never writes
//! anything back into their tags. All data found ends up exclusively in the
//! database and in the XDG cache (`~/.cache/emilia`).
//!
//! Layout:
//! - [`client`] – the HTTP client ([`OnlineClient`]) and the wire formats of
//!   the services it talks to
//! - [`cache`] – cache/download directories and the on-disk image caches
//!   (covers, podcast/station/YouTube images, recordings)
//! - [`matching`] – title/artist normalization and track lookups
//! - [`enrich`] – library enrichment (album covers/years, artist photos,
//!   galleries, fingerprints)
//!
//! Everything public is re-exported here, so callers keep using
//! `crate::core::online::…`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use crate::core::fingerprint;

mod cache;
mod client;
mod enrich;
mod matching;

pub use cache::*;
pub use client::*;
pub use enrich::*;
pub use matching::*;

/// MusicBrainz requires a meaningful User-Agent with contact info. The version
/// is taken from the crate so it never drifts out of date.
const USER_AGENT: &str = concat!("Emilia/", env!("CARGO_PKG_VERSION"), " ( https://cais.de )");

/// MusicBrainz policy: at most one request per second.
pub const RATE_LIMIT: Duration = Duration::from_millis(1100);
/// Number of parallel fetches for artist photos (Deezer handles this well).
pub const ARTIST_FETCH_THREADS: usize = 8;
/// Number of parallel fetches when caching a batch of thumbnails.
pub const THUMB_FETCH_THREADS: usize = 6;
/// Longest edge a downloaded image is stored at. Cover hosts hand out
/// 3000 px originals (a podcast cover is 2 MB as a PNG); the app shows covers at
/// 48 px in lists, ~360 px in detail views, and at most window-sized as the
/// unfiltered background — 1600 px keeps that last one crisp on a desktop
/// window and is native-or-better on any phone, at a fraction of the bytes on
/// disk, the memory, and the decode time (which on the phone ran into seconds
/// per cover) of the originals.
pub const MAX_IMAGE_EDGE: i32 = 1600;
/// Attempt budget for a single image download. Artwork is optional — the UI
/// falls back to a placeholder — so a slow or throttling host is dropped after
/// one retry instead of holding up a whole listing (see
/// [`crate::core::net::get_with_retry_max`]).
const IMAGE_RETRY_MAX: usize = 1;

/// Whether fingerprint detection is possible (Chromaprint/`fpcalc` present).
pub fn fingerprint_available() -> bool {
    fingerprint::available()
}

/// Stable file name from an arbitrary string (for cache files).
fn name_hash(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Result of a MusicBrainz release search.
pub struct ReleaseMatch {
    pub mbid: String,
    pub release_group: Option<String>,
    pub year: Option<i32>,
}

/// One entry of a release's **canonical** tracklist (from MusicBrainz). Used to
/// flag tracks that are missing from the local album.
pub struct CanonicalTrack {
    /// Medium / disc number (1-based).
    pub disc: u32,
    /// Track position within the disc (1-based).
    pub position: u32,
    pub title: String,
    /// Track length in milliseconds, if MusicBrainz knows it.
    pub length_ms: Option<i64>,
}

/// Canonical tags of a single track, as a music database (Deezer) knows them.
/// Used to tag a download from its *real* metadata instead of a YouTube channel
/// name and video title.
pub struct TrackTags {
    /// The database's artist — not the uploader/channel.
    pub artist: Option<String>,
    pub title: String,
    pub album: Option<String>,
    /// Front cover of the single/album, if one could be fetched.
    pub cover: Option<Vec<u8>>,
}

impl TrackTags {
    /// Reduces the hit to what the cover-only callers want.
    fn into_cover(self) -> Option<(Vec<u8>, Option<String>)> {
        self.cover.map(|c| (c, self.album))
    }
}

/// Match of an AcoustID fingerprint search.
pub struct AcoustIdMatch {
    pub recording_mbid: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

/// A process-wide [`OnlineClient`] for the on-demand image/cover cache helpers
/// below. Reuses one connection pool across all thumbnail/cover downloads
/// instead of building a fresh agent (and pool) on every call.
fn shared_client() -> &'static OnlineClient {
    static CLIENT: std::sync::OnceLock<OnlineClient> = std::sync::OnceLock::new();
    CLIENT.get_or_init(OnlineClient::new)
}

/// Escapes Lucene special characters in free text, so the query stays valid.
fn escape_lucene(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Minimal percent-encoding for query strings (RFC 3986 unreserved is kept).
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_hash_is_deterministic_fixed_width_hex() {
        let a = name_hash("https://example.com/cover.jpg");
        assert_eq!(a, name_hash("https://example.com/cover.jpg"));
        assert_eq!(a.len(), 16);
        assert!(a
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        // Short hashes are zero-padded to the same width.
        assert_eq!(name_hash("").len(), 16);
    }

    #[test]
    fn name_hash_separates_different_inputs() {
        assert_ne!(name_hash("a"), name_hash("b"));
        assert_ne!(name_hash("Artist\u{1}Album"), name_hash("Artist Album"));
    }

    #[test]
    fn escape_lucene_escapes_quotes_and_backslashes_only() {
        assert_eq!(escape_lucene("plain text"), "plain text");
        assert_eq!(escape_lucene(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape_lucene(r"a\b"), r"a\\b");
        // Other Lucene operators are harmless inside the quoted phrase.
        assert_eq!(escape_lucene("AC/DC: (live) +1"), "AC/DC: (live) +1");
    }

    #[test]
    fn percent_encode_keeps_unreserved_and_encodes_the_rest() {
        assert_eq!(percent_encode("AZaz09-_.~"), "AZaz09-_.~");
        assert_eq!(percent_encode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(percent_encode("/?#"), "%2F%3F%23");
        // UTF-8 is encoded byte by byte, upper-case hex.
        assert_eq!(percent_encode("é"), "%C3%A9");
        assert_eq!(percent_encode(""), "");
    }

    #[test]
    fn track_tags_into_cover_needs_a_cover() {
        let tags = |cover: Option<Vec<u8>>| TrackTags {
            artist: Some("A".into()),
            title: "T".into(),
            album: Some("Album".into()),
            cover,
        };
        assert_eq!(tags(None).into_cover(), None);
        assert_eq!(
            tags(Some(vec![1, 2])).into_cover(),
            Some((vec![1, 2], Some("Album".to_string())))
        );
    }
}
