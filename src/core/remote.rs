//! Remote music sources behind **one** backend: Nextcloud/WebDAV
//! ([`crate::core::webdav`]), SMB shares ([`crate::core::smb`]) and Google
//! Drive ([`crate::core::gdrive`]).
//!
//! The UI, the indexer and the enrichment worker only ever talk to
//! [`Backend`]; what differs per kind (listing, byte-range reads, streaming
//! transport) is dispatched here. Everything that is the *same* for all three —
//! tag parsing from a file prefix, embedded-cover extraction, recursive
//! indexing into the `track` table, the download-to-cache path — lives here
//! once. All remote kinds share the synthetic track path `nc:<source-id>:<rel>`
//! (historic prefix; it predates SMB/Drive) and the cache layout
//! `cache/<source-id>/<rel>`, so the rest of the app needs no per-kind branches.
//!
//! Every function here is **blocking** (network I/O) and meant for worker
//! threads.

use std::io::{Cursor, Read};
use std::path::Path;

use anyhow::{anyhow, Result};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;

use crate::core::{gdrive, media_proxy, net, smb, webdav};
use crate::model::Source;

pub use crate::core::webdav::{cache_path, nc_path, parse_nc_path, DavEntry as RemoteEntry};

pub const KIND_LOCAL: &str = "local";
pub const KIND_WEBDAV: &str = "webdav";
pub const KIND_SMB: &str = "smb";
pub const KIND_GDRIVE: &str = "gdrive";

/// Whether a `source.kind` is a network source (as opposed to a local folder).
pub fn is_remote_kind(kind: &str) -> bool {
    matches!(kind, KIND_WEBDAV | KIND_SMB | KIND_GDRIVE)
}

/// Untranslated product name of a source kind — for keyring item labels and
/// log lines (the UI translates its own strings).
pub fn kind_name(kind: &str) -> &'static str {
    match kind {
        KIND_WEBDAV => "Nextcloud",
        KIND_SMB => "SMB",
        KIND_GDRIVE => "Google Drive",
        KIND_LOCAL => "Folder",
        _ => "Source",
    }
}

/// Bytes fetched from the start of a file to read its tags (text tags sit at
/// the start of MP3/FLAC/M4A files when the file is laid out normally).
pub const META_PREFIX: u64 = 524_288;
/// Bytes fetched to pull the first embedded cover picture (usually right
/// behind the text tags, but a large JPEG can be a few MB in).
pub const COVER_PREFIX: u64 = 4_194_304;

/// A remote source with resolved credentials. Cheap to clone (SMB shares one
/// lazily opened connection between clones; Drive caches tokens process-wide).
#[derive(Debug, Clone)]
pub enum Backend {
    WebDav(webdav::Creds),
    Smb(smb::SmbCreds),
    GDrive(gdrive::GdCreds),
}

/// One byte range of a remote file, as handed to the local streaming proxy or
/// the downloader: `start..=end` of a file `total` bytes long, plus the reader
/// delivering exactly those bytes.
pub struct RangeBody {
    pub total: u64,
    pub start: u64,
    pub end: u64,
    pub body: Box<dyn Read + Send>,
}

impl RangeBody {
    /// Number of bytes the body delivers.
    pub fn byte_len(&self) -> u64 {
        if self.end < self.start {
            0
        } else {
            self.end - self.start + 1
        }
    }
}

/// Clamps a requested range against the file length: `end = None` means "to
/// the end". Returns `None` when `start` lies past the last byte (HTTP 416).
pub fn clamp_range(total: u64, start: u64, end: Option<u64>) -> Option<(u64, u64)> {
    if total == 0 || start >= total {
        return None;
    }
    let last = total - 1;
    let end = end.map_or(last, |e| e.min(last));
    if end < start {
        return None;
    }
    Some((start, end))
}

impl Backend {
    /// Resolves a source's stored credentials (Secret Service references
    /// included). `None` for local folders or an incompletely configured row.
    pub fn from_source(s: &Source) -> Option<Self> {
        match s.kind.as_str() {
            KIND_WEBDAV => webdav::Creds::from_source(s).map(Self::WebDav),
            KIND_SMB => smb::SmbCreds::from_source(s).map(Self::Smb),
            KIND_GDRIVE => gdrive::GdCreds::from_source(s).map(Self::GDrive),
            _ => None,
        }
    }

    /// Lists one folder (relative to the music root; `""` = root): only
    /// subfolders and audio files.
    pub fn list(&self, rel: &str) -> Result<Vec<RemoteEntry>> {
        match self {
            Self::WebDav(c) => webdav::list(c, rel),
            Self::Smb(c) => smb::list(c, rel),
            Self::GDrive(c) => gdrive::list(c, rel),
        }
    }

    /// Reachability + authentication check on the music root.
    pub fn test_connection(&self) -> Result<()> {
        match self {
            Self::WebDav(c) => webdav::test_connection(c),
            Self::Smb(c) => smb::test_connection(c),
            Self::GDrive(c) => gdrive::test_connection(c),
        }
    }

    /// The first `len` bytes of a file (fewer for a shorter file). `Err` only
    /// on a network/auth failure — so an indexer can tell "unreachable" apart
    /// from "no readable tags".
    pub fn fetch_prefix(&self, rel: &str, len: u64) -> Result<Vec<u8>> {
        match self {
            Self::WebDav(c) => webdav::fetch_prefix(c, rel, len),
            Self::Smb(c) => smb::fetch_prefix(c, rel, len),
            Self::GDrive(c) => gdrive::fetch_prefix(c, rel, len),
        }
    }

    /// Opens a byte range for streaming (`end = None` → to the end of file).
    pub fn open_range(&self, rel: &str, start: u64, end: Option<u64>) -> Result<RangeBody> {
        match self {
            Self::WebDav(c) => webdav::open_range(c, rel, start, end),
            Self::Smb(c) => smb::open_range(c, rel, start, end),
            Self::GDrive(c) => gdrive::open_range(c, rel, start, end),
        }
    }

    /// Downloads a file completely to `dest` (atomically via a `.part` file),
    /// capped at [`net::MAX_DOWNLOAD_BYTES`].
    pub fn download(&self, rel: &str, dest: &Path) -> Result<()> {
        match self {
            Self::WebDav(c) => webdav::download(c, rel, dest),
            _ => download_via_range(self, rel, dest),
        }
    }

    /// A URI GStreamer can play for this file. Nextcloud is fetched directly
    /// (HTTPS with embedded credentials); SMB and Drive go through the local
    /// range proxy, which `souphttpsrc` can seek in.
    pub fn stream_uri(&self, source_id: i64, rel: &str) -> Result<String> {
        match self {
            Self::WebDav(c) => Ok(webdav::stream_uri(c, rel)),
            Self::Smb(_) | Self::GDrive(_) => media_proxy::stream_uri(source_id, rel),
        }
    }

    /// Complete library metadata of a remote track from its first
    /// [`META_PREFIX`] bytes. `Err` = network failure; a reachable file with no
    /// tags yields `Ok` with empty fields.
    pub fn read_meta(&self, rel: &str) -> Result<RemoteMeta> {
        let buf = self.fetch_prefix(rel, META_PREFIX)?;
        Ok(parse_remote_meta(buf))
    }

    /// Title/artist/duration — the subset the file list shows. Best effort:
    /// any failure yields `None`s and the callers fall back to the file name.
    pub fn read_tags(&self, rel: &str) -> (Option<String>, Option<String>, Option<i64>) {
        match self.read_meta(rel) {
            Ok(m) => (m.title, m.artist, m.duration_ms),
            Err(_) => (None, None, None),
        }
    }

    /// The first embedded cover picture of a remote track, if any.
    pub fn fetch_cover(&self, rel: &str) -> Option<Vec<u8>> {
        let buf = self.fetch_prefix(rel, COVER_PREFIX).ok()?;
        cover_from_prefix(buf)
    }

    /// **Recursively** collects all audio file paths (relative to the music
    /// root) under `rel`. Defensively capped so a huge share never runs
    /// forever.
    pub fn walk(&self, rel: &str) -> Vec<String> {
        const MAX_DIRS: usize = 4000;
        const MAX_FILES: usize = 50_000;
        let mut files = Vec::new();
        let mut stack = vec![rel.to_string()];
        let mut dirs_seen = 0usize;
        while let Some(dir) = stack.pop() {
            dirs_seen += 1;
            if dirs_seen > MAX_DIRS || files.len() >= MAX_FILES {
                tracing::warn!("remote walk capped (dirs/files limit reached)");
                break;
            }
            let Ok(entries) = self.list(&dir) else {
                continue; // directory not readable – skip
            };
            for e in entries {
                if e.is_dir {
                    stack.push(e.rel_path);
                } else {
                    files.push(e.rel_path);
                }
            }
        }
        files
    }
}

/// Generic download for backends without a dedicated GET path: streams the
/// whole file through [`Backend::open_range`] into a `.part` file, verifies the
/// size and renames it into place.
fn download_via_range(backend: &Backend, rel: &str, dest: &Path) -> Result<()> {
    let range = backend.open_range(rel, 0, None)?;
    if range.total > net::MAX_DOWNLOAD_BYTES {
        return Err(anyhow!(
            "file too large ({} bytes, limit {})",
            range.total,
            net::MAX_DOWNLOAD_BYTES
        ));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = net::part_path(dest);
    let mut file = std::fs::File::create(&tmp)?;
    let expected = Some(range.total);
    let copied = net::copy_capped(range.body, &mut file, net::MAX_DOWNLOAD_BYTES);
    file.sync_all().ok();
    drop(file);
    // A dropped connection ends the copy without an error, so verify the size
    // before committing: a truncated file must never be served from the cache.
    let complete = copied.and_then(|n| net::check_complete(n, expected));
    if let Err(e) = complete {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// Complete metadata of a remote track (for indexing into the same database
/// as local songs).
#[derive(Default)]
pub struct RemoteMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub duration_ms: Option<i64>,
    pub year: Option<i32>,
}

/// Runs `lofty` over an in-memory file prefix and pulls the library fields
/// out. Unreadable/absent tags are not an error here — they just leave fields
/// empty.
pub fn parse_remote_meta(buf: Vec<u8>) -> RemoteMeta {
    // `lofty::read_from` expects a `File`; with an in-memory buffer it works
    // via `Probe` (Read + Seek on the `Cursor`, purely local – no network seek).
    let tagged = match lofty::probe::Probe::new(Cursor::new(buf)).guess_file_type() {
        Ok(p) => match p.read() {
            Ok(t) => t,
            Err(_) => return RemoteMeta::default(),
        },
        Err(_) => return RemoteMeta::default(),
    };
    let duration_ms = match tagged.properties().duration().as_millis() {
        0 => None,
        ms => Some(ms as i64),
    };
    let mut m = RemoteMeta {
        duration_ms,
        ..Default::default()
    };
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        let clean = |s: Option<std::borrow::Cow<str>>| {
            s.map(|c| c.trim().to_string()).filter(|s| !s.is_empty())
        };
        m.title = clean(tag.title());
        m.artist = clean(tag.artist());
        m.album = clean(tag.album());
        m.genre = clean(tag.genre());
        m.track_no = tag.track();
        m.disc_no = tag.disk();
        m.year = tag.year().map(|y| y as i32);
    }
    m
}

/// Extracts the first embedded picture from an in-memory file prefix.
pub fn cover_from_prefix(buf: Vec<u8>) -> Option<Vec<u8>> {
    let tagged = lofty::probe::Probe::new(Cursor::new(buf))
        .guess_file_type()
        .ok()?
        .read()
        .ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    Some(tag.pictures().first()?.data().to_vec())
}

/// Recursively reads in the complete music library of a source and stores the
/// tracks in the database (synthetic `nc:` path). Afterwards they appear like
/// local songs in artists/albums. Returns the number of indexed tracks.
pub fn index_into(lib: &crate::core::db::Library, source: &Source) -> Result<usize> {
    let Some(backend) = Backend::from_source(source) else {
        return Err(anyhow!("incomplete source credentials"));
    };
    let files = backend.walk("");
    // Upsert in batches: one transaction (one fsync) per chunk instead of one
    // per file — a large share can hold tens of thousands of tracks. The
    // per-file metadata read over the network stays the dominant cost.
    const BATCH: usize = 256;
    let mut batch: Vec<crate::model::Track> = Vec::with_capacity(BATCH.min(files.len()));
    let mut n = 0;
    for rel in files {
        // A network failure must not produce a degraded entry (filename as
        // title, no tags) that then sticks in the DB; skip the track so a later
        // re-index picks it up once the source is reachable again. A reachable
        // file with no tags still indexes (Ok with empty fields).
        let meta = match backend.read_meta(&rel) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("skipping {rel}: metadata read failed: {e}");
                continue;
            }
        };
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        let title = meta.title.unwrap_or_else(|| {
            Path::new(name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(name)
                .to_string()
        });
        batch.push(crate::model::Track {
            id: 0,
            path: nc_path(source.id, &rel),
            title,
            artist: meta.artist,
            album: meta.album,
            genre: meta.genre,
            track_no: meta.track_no,
            disc_no: meta.disc_no,
            duration_ms: meta.duration_ms,
            resume_ms: 0,
            year: meta.year,
        });
        if batch.len() >= BATCH {
            n += lib.upsert_tracks_resilient(&batch);
            batch.clear();
        }
    }
    n += lib.upsert_tracks_resilient(&batch);
    Ok(n)
}

/// Normalizes a music subpath (leading slash, no trailing slash; empty =
/// root). Shared by the setup dialogs and the credential loaders.
pub fn normalize_music_path(p: &str) -> String {
    let p = p.trim().trim_end_matches('/');
    if p.is_empty() {
        String::new()
    } else if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_classified() {
        assert!(!is_remote_kind(KIND_LOCAL));
        assert!(is_remote_kind(KIND_WEBDAV));
        assert!(is_remote_kind(KIND_SMB));
        assert!(is_remote_kind(KIND_GDRIVE));
        assert!(!is_remote_kind("bogus"));
    }

    #[test]
    fn ranges_are_clamped_to_the_file() {
        assert_eq!(clamp_range(100, 0, None), Some((0, 99)));
        assert_eq!(clamp_range(100, 10, Some(19)), Some((10, 19)));
        assert_eq!(clamp_range(100, 10, Some(500)), Some((10, 99)));
        assert_eq!(clamp_range(100, 100, None), None);
        assert_eq!(clamp_range(0, 0, None), None);
        assert_eq!(clamp_range(100, 20, Some(10)), None);
    }

    #[test]
    fn music_paths_are_normalized() {
        assert_eq!(normalize_music_path(""), "");
        assert_eq!(normalize_music_path("   "), "");
        assert_eq!(normalize_music_path("/"), "");
        assert_eq!(normalize_music_path("///"), "");
        assert_eq!(normalize_music_path("Music"), "/Music");
        assert_eq!(normalize_music_path("/Music/"), "/Music");
        assert_eq!(normalize_music_path(" /Music/ "), "/Music");
        assert_eq!(normalize_music_path("a/b/c//"), "/a/b/c");
    }

    #[test]
    fn unreadable_prefix_yields_empty_meta() {
        let m = parse_remote_meta(vec![0u8; 64]);
        assert!(m.title.is_none() && m.artist.is_none() && m.duration_ms.is_none());
        assert!(cover_from_prefix(vec![0u8; 64]).is_none());
    }
}
