//! Minimal, **read-only** WebDAV client (Nextcloud) via the blocking
//! `ureq`. Can list directories (PROPFIND), read tags from the first
//! kilobytes of a file (range GET) and download files (GET).
//!
//! Deliberately kept lean and called exclusively from background workers
//! (see `src/ui/app_streaming` or the `Cmd::Remote*` paths).
//! The audio files in the cloud are never modified in the process.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};
use base64::Engine;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};

use crate::core::net;
use crate::core::scanner;
use crate::model::Source;

/// Characters to encode in a single path segment (excluding the `/` separator).
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'\\');

/// Characters to encode in the user-info part (`user:pass@`) of a URL.
const USERINFO: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/')
    .add(b':')
    .add(b'@')
    .add(b'\\')
    .add(b'[')
    .add(b']');

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop>
<d:resourcetype/><d:displayname/><d:getcontentlength/><d:getcontenttype/>
</d:prop></d:propfind>"#;

/// Credentials + music root of a Nextcloud/WebDAV source.
#[derive(Debug, Clone)]
pub struct Creds {
    /// Base URL without trailing slash, e.g. `https://cloud.example.com`
    /// (may contain a subpath, e.g. `https://host/nextcloud`).
    pub base_url: String,
    pub user: String,
    pub pass: String,
    /// Subpath to the music (normalized: leading slash, no trailing slash;
    /// empty = cloud root), e.g. `/Music`.
    pub music_path: String,
}

impl Creds {
    /// From a `webdav` source. `None` if required fields are missing.
    pub fn from_source(s: &Source) -> Option<Self> {
        let pass = crate::core::secrets::resolve_source_password(s.id, s.password.as_deref()?)?;
        let user = crate::core::secrets::resolve_source_username(s.id, s.username.as_deref()?)?;
        Some(Self {
            base_url: ensure_scheme(s.base_url.as_deref()?),
            user,
            pass,
            music_path: normalize_path(s.music_path.as_deref().unwrap_or("")),
        })
    }
}

/// Ensures the base URL carries a scheme. Users routinely enter just a host
/// (`cloud.example.com`); without a scheme `ureq` rejects the request URL, so
/// the whole PROPFIND/listing silently fails. Default to HTTPS (Nextcloud is
/// always TLS); an explicit `http://`/`https://` is kept as-is.
fn ensure_scheme(base: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    if b.contains("://") {
        b.to_string()
    } else {
        format!("https://{b}")
    }
}

/// An entry from a WebDAV directory (folder or audio file).
#[derive(Debug, Clone)]
pub struct DavEntry {
    /// Path **relative to the music root** (leading slash), e.g. `/Alben/X`.
    pub rel_path: String,
    /// Display name (last path segment or `displayname`).
    pub name: String,
    pub is_dir: bool,
}

// ---------------------------------------------------------------------------
// URL/path helpers
// ---------------------------------------------------------------------------

fn normalize_path(p: &str) -> String {
    let p = p.trim();
    if p.is_empty() || p == "/" {
        return String::new();
    }
    let p = p.trim_end_matches('/');
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
}

fn scheme_rest(base: &str) -> (&str, &str) {
    base.split_once("://").unwrap_or(("https", base))
}

/// Splits `authority[/path]` into (authority, path) – path including leading
/// slash, or empty.
fn authority_and_path(rest: &str) -> (&str, &str) {
    match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    }
}

/// Encodes a path segment by segment (the `/` separators are preserved).
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|seg| utf8_percent_encode(seg, PATH_SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// DAV path suffix (encoded) starting from the authority: `/remote.php/dav/files/USER/...`.
fn dav_suffix(c: &Creds, rel: &str) -> String {
    let enc_user = utf8_percent_encode(&c.user, PATH_SEGMENT).to_string();
    let full = format!("{}{}", c.music_path, rel);
    format!("/remote.php/dav/files/{}{}", enc_user, encode_path(&full))
}

/// Full DAV URL (for `ureq`; authentication goes through a header).
fn url_for(c: &Creds, rel: &str) -> String {
    format!("{}{}", c.base_url, dav_suffix(c, rel))
}

/// Playable URI with embedded credentials (for GStreamer/`play_uri`).
pub fn stream_uri(c: &Creds, rel: &str) -> String {
    let (scheme, rest) = scheme_rest(&c.base_url);
    let enc_user = utf8_percent_encode(&c.user, USERINFO);
    let enc_pass = utf8_percent_encode(&c.pass, USERINFO);
    format!(
        "{scheme}://{enc_user}:{enc_pass}@{rest}{}",
        dav_suffix(c, rel)
    )
}

/// Expected (decoded) path of the PROPFIND request – prefix of the child hrefs.
fn req_path_decoded(c: &Creds, rel: &str) -> String {
    let (_, rest) = scheme_rest(&c.base_url);
    let (_authority, base_path) = authority_and_path(rest);
    format!(
        "{}/remote.php/dav/files/{}{}{}",
        base_path.trim_end_matches('/'),
        c.user,
        c.music_path,
        rel
    )
}

/// Extracts the (decoded) path part from an href (path or full URL).
fn href_to_path(href: &str) -> String {
    let path = if href.starts_with("http") {
        href.split_once("://")
            .and_then(|(_, r)| r.find('/').map(|i| &r[i..]))
            .unwrap_or(href)
    } else {
        href
    };
    percent_decode_str(path).decode_utf8_lossy().to_string()
}

fn auth_header(c: &Creds) -> String {
    let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", c.user, c.pass));
    format!("Basic {token}")
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(30))
        .build()
}

/// Retries a request-producing closure against **transient** `ureq` failures
/// (5xx/429/transport) with the shared backoff policy, so the per-verb call
/// sites (PROPFIND, range GET) carry no duplicated retry bookkeeping. The WebDAV
/// verbs have no typed `ureq` helper, so [`net::get_with_retry`] can't be used
/// directly; this is its non-GET sibling. `label` names the operation for the
/// error message.
fn with_retry(
    label: &str,
    // The `Err` is boxed so the large `ureq::Error` (≈272 B) stays small across
    // the closure boundary (clippy `result_large_err`); call sites add
    // `.map_err(Box::new)`.
    mut send: impl FnMut() -> std::result::Result<ureq::Response, Box<ureq::Error>>,
) -> Result<ureq::Response> {
    let mut backoff = net::RETRY_BASE_BACKOFF;
    let mut attempt = 0usize;
    loop {
        match send() {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                attempt += 1;
                if !net::is_transient(&e) || attempt > net::RETRY_MAX {
                    return Err(anyhow!("{label} failed: {e}"));
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(net::RETRY_MAX_BACKOFF);
            }
        }
    }
}

/// Issues a `PROPFIND` with transient-failure retry and returns the response
/// body.
fn propfind(c: &Creds, rel: &str, depth: &str) -> Result<String> {
    let url = url_for(c, rel);
    let auth = auth_header(c);
    let agent = agent();
    with_retry("PROPFIND", || {
        agent
            .request("PROPFIND", &url)
            .set("Depth", depth)
            .set("Authorization", &auth)
            .set("Content-Type", "application/xml")
            .send_string(PROPFIND_BODY)
            .map_err(Box::new)
    })?
    .into_string()
    .map_err(|e| anyhow!("Response not readable: {e}"))
}

/// Whether an `UnexpectedEof` while reading a range body may be ignored: it may,
/// when the server already delivered every advertised byte. Some servers (e.g.
/// Nextcloud on php-legacy) finish a range response and then close the TCP
/// connection without the TLS `close_notify` alert; rustls reports that as
/// `UnexpectedEof`. For a prefix read (tags sit at the file start) a missing
/// alert on an otherwise complete body is cosmetic, so the data is kept. A short
/// read is a real truncation and stays an error.
fn eof_is_benign(err: &std::io::Error, got: usize, advertised: Option<u64>, max: u64) -> bool {
    err.kind() == std::io::ErrorKind::UnexpectedEof
        && advertised.is_some_and(|n| got as u64 >= n.min(max))
}

/// Range-GETs the first bytes of a remote file with transient-failure retry.
/// `Ok(buf)` on success (the buffer may be shorter than `max` for small files);
/// `Err` only on a **final** transport/HTTP failure — so a caller can tell a
/// genuine network problem apart from a file that merely has no readable tags.
fn fetch_prefix(c: &Creds, rel: &str, range: &str, max: u64) -> Result<Vec<u8>> {
    let url = url_for(c, rel);
    let auth = auth_header(c);
    let agent = agent();
    let resp = with_retry("range GET", || {
        agent
            .get(&url)
            .set("Authorization", &auth)
            .set("Range", range)
            .call()
            .map_err(Box::new)
    })?;
    // The length the server says this (range) body has, so a stream that ends
    // early can be told apart from a genuine truncation below.
    let advertised = resp
        .header("Content-Length")
        .and_then(|s| s.trim().parse::<u64>().ok());
    let mut buf = Vec::new();
    if let Err(e) = resp.into_reader().take(max).read_to_end(&mut buf) {
        // Tolerate a missing TLS `close_notify` once every advertised byte has
        // arrived: the track stays indexable instead of being dropped over a
        // cosmetic shutdown quirk. A short read is still a real failure, so the
        // track is skipped and retried on the next pass rather than stored
        // tag-less.
        if !eof_is_benign(&e, buf.len(), advertised, max) {
            return Err(e.into());
        }
    }
    Ok(buf)
}

// ---------------------------------------------------------------------------
// PROPFIND – list directory
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RawEntry {
    href: String,
    display_name: Option<String>,
    is_dir: bool,
}

/// Which text value is currently being read (between start and end tag).
#[derive(Clone, Copy)]
enum Field {
    Href,
    Display,
}

/// Parses a WebDAV `multistatus` response into raw entries.
fn parse_propfind(xml: &str) -> Vec<RawEntry> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = Vec::new();
    let mut cur: Option<RawEntry> = None;
    let mut field: Option<Field> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = local_name(e.name().as_ref());
                match name.as_str() {
                    "response" => cur = Some(RawEntry::default()),
                    "href" => field = Some(Field::Href),
                    "displayname" => field = Some(Field::Display),
                    "collection" => {
                        if let Some(c) = cur.as_mut() {
                            c.is_dir = true;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if let (Some(c), Some(f)) = (cur.as_mut(), field) {
                    let val = t.unescape().unwrap_or_default().trim().to_string();
                    if !val.is_empty() {
                        match f {
                            Field::Href => c.href = val,
                            Field::Display => c.display_name = Some(val),
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "response" {
                    if let Some(c) = cur.take() {
                        out.push(c);
                    }
                }
                field = None;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Local element name without namespace prefix (`d:href` → `href`).
fn local_name(qname: &[u8]) -> String {
    let s = String::from_utf8_lossy(qname);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.to_string(),
    }
}

/// Lists a directory (Depth: 1) relative to the music root. Returns only
/// folders and audio files; the self-entry is filtered out.
pub fn list(c: &Creds, rel: &str) -> Result<Vec<DavEntry>> {
    let body = propfind(c, rel, "1")?;

    let prefix = req_path_decoded(c, rel);
    let prefix = prefix.trim_end_matches('/');
    let mut out = Vec::new();
    for raw in parse_propfind(&body) {
        let hp = href_to_path(&raw.href);
        let hp = hp.trim_end_matches('/');
        if hp == prefix {
            continue; // self-entry
        }
        let Some(rem) = hp.strip_prefix(prefix) else {
            continue;
        };
        let child = rem.trim_start_matches('/');
        if child.is_empty() {
            continue;
        }
        // With Depth:1 only one level – take the first component to be safe.
        let child_name = child.split('/').next().unwrap_or(child).to_string();
        let name = raw
            .display_name
            .clone()
            .unwrap_or_else(|| child_name.clone());
        if !raw.is_dir && !scanner::is_audio(Path::new(&name)) {
            continue; // hide non-audio files
        }
        out.push(DavEntry {
            rel_path: format!("{rel}/{child_name}"),
            name,
            is_dir: raw.is_dir,
        });
    }
    Ok(out)
}

/// Connection test: PROPFIND (Depth 0) on the music root. `Ok` = reachable
/// and authenticated.
pub fn test_connection(c: &Creds) -> Result<()> {
    propfind(c, "", "0")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tags & download
// ---------------------------------------------------------------------------

/// Reads title/artist/duration from the first ~512 KB of a file (range GET)
/// into an in-memory buffer and runs `lofty` over it. Best effort: for
/// formats with metadata at the end of the file (e.g. unoptimized MP4) this
/// fails and returns `None` – the callers then fall back to the file name.
pub fn read_tags(c: &Creds, rel: &str) -> (Option<String>, Option<String>, Option<i64>) {
    let buf = match fetch_prefix(c, rel, "bytes=0-524287", 600_000) {
        Ok(b) => b,
        Err(_) => return (None, None, None),
    };
    // `lofty::read_from` expects a `File`; with an in-memory buffer it works
    // via `Probe` (Read + Seek on the `Cursor`, purely local – no HTTP seek).
    let tagged = match lofty::probe::Probe::new(Cursor::new(buf)).guess_file_type() {
        Ok(p) => match p.read() {
            Ok(t) => t,
            Err(_) => return (None, None, None),
        },
        Err(_) => return (None, None, None),
    };
    let duration_ms = match tagged.properties().duration().as_millis() {
        0 => None,
        ms => Some(ms as i64),
    };
    let (title, artist) = match tagged.primary_tag().or_else(|| tagged.first_tag()) {
        Some(tag) => (
            tag.title()
                .map(|c| c.trim().to_string())
                .filter(|s| !s.is_empty()),
            tag.artist()
                .map(|c| c.trim().to_string())
                .filter(|s| !s.is_empty()),
        ),
        None => (None, None),
    };
    (title, artist, duration_ms)
}

/// Complete metadata of a remote track (for indexing into the
/// same database as local songs).
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

/// Like [`read_tags`], but reads **all** fields needed for the library
/// (additionally album, genre, track/CD no.) from the first ~512 KB of the file.
/// `Err` signals a **network** failure (so a caller indexing into the DB can
/// skip the track instead of storing a degraded, tag-less entry); a reachable
/// file with no readable tags yields `Ok` with default fields.
pub fn read_meta(c: &Creds, rel: &str) -> Result<RemoteMeta> {
    let buf = fetch_prefix(c, rel, "bytes=0-524287", 600_000)?;
    Ok(parse_remote_meta(buf))
}

/// Runs `lofty` over an in-memory file prefix and pulls the library fields out.
/// Unreadable/absent tags are not an error here — they just leave fields empty.
fn parse_remote_meta(buf: Vec<u8>) -> RemoteMeta {
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

/// Synthetic path of a remote track: `nc:<source_id>:<rel>`. This way
/// cloud tracks live in the same `track` table as local ones and behave 1:1.
pub fn nc_path(source_id: i64, rel: &str) -> String {
    format!("nc:{source_id}:{rel}")
}

/// Reads the first **embedded** cover image of a remote track. Fetches a larger
/// prefix than the tag read (covers usually sit right behind the text tags) and
/// extracts the picture via lofty from the in-memory buffer. **Blocking** –
/// only from worker threads.
pub fn fetch_cover(c: &Creds, rel: &str) -> Option<Vec<u8>> {
    let buf = fetch_prefix(c, rel, "bytes=0-4194303", 4_400_000).ok()?;
    let tagged = lofty::probe::Probe::new(Cursor::new(buf))
        .guess_file_type()
        .ok()?
        .read()
        .ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    Some(tag.pictures().first()?.data().to_vec())
}

/// Splits a synthetic path `nc:<id>:<rel>` into (source id, rel).
pub fn parse_nc_path(path: &str) -> Option<(i64, String)> {
    let rest = path.strip_prefix("nc:")?;
    let (id, rel) = rest.split_once(':')?;
    Some((id.parse().ok()?, rel.to_string()))
}

/// **Recursively** collects all audio file paths (relative to the music root)
/// under `rel`. Defensively capped (directory/file count) so that a very large
/// cloud does not run forever.
pub fn walk(c: &Creds, rel: &str) -> Vec<String> {
    const MAX_DIRS: usize = 4000;
    const MAX_FILES: usize = 50_000;
    let mut files = Vec::new();
    let mut stack = vec![rel.to_string()];
    let mut dirs_seen = 0usize;
    while let Some(dir) = stack.pop() {
        dirs_seen += 1;
        if dirs_seen > MAX_DIRS || files.len() >= MAX_FILES {
            tracing::warn!("WebDAV walk capped (dirs/files limit reached)");
            break;
        }
        let Ok(entries) = list(c, &dir) else {
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

/// Recursively reads in the complete music library of a source and stores the
/// tracks in the database (synthetic path). Afterwards they appear like
/// local songs in artists/albums. **Blocking** – only from worker threads.
/// Returns the number of indexed tracks.
pub fn index_into(lib: &crate::core::db::Library, source: &Source) -> Result<usize> {
    let Some(c) = Creds::from_source(source) else {
        return Err(anyhow!("incomplete source credentials"));
    };
    let files = walk(&c, "");
    // Upsert in batches: one transaction (one fsync) per chunk instead of one per
    // file — a large cloud can hold tens of thousands of tracks. The per-file
    // metadata read over HTTP stays the dominant cost.
    const BATCH: usize = 256;
    let mut batch: Vec<crate::model::Track> = Vec::with_capacity(BATCH.min(files.len()));
    let mut n = 0;
    for rel in files {
        // A network failure must not produce a degraded entry (filename as
        // title, no tags) that then sticks in the DB; skip the track so a later
        // re-index picks it up once the source is reachable again. A reachable
        // file with no tags still indexes (Ok with empty fields).
        let meta = match read_meta(&c, &rel) {
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

/// Downloads a file completely to `dest` (atomically via a `.part` file). The
/// transfer is capped at [`crate::core::net::MAX_DOWNLOAD_BYTES`] so a broken or
/// hostile server cannot fill the disk.
pub fn download(c: &Creds, rel: &str, dest: &Path) -> Result<()> {
    use crate::core::net;
    let url = url_for(c, rel);
    let resp = agent()
        .get(&url)
        .set("Authorization", &auth_header(c))
        .call()
        .map_err(|e| anyhow!("Download failed: {e}"))?;
    net::check_content_length(&resp, net::MAX_DOWNLOAD_BYTES)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("part");
    let mut file = std::fs::File::create(&tmp)?;
    if let Err(e) = net::copy_capped(resp.into_reader(), &mut file, net::MAX_DOWNLOAD_BYTES) {
        drop(file);
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    file.sync_all().ok();
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// Local cache path of a remote file:
/// `$XDG_DATA_HOME/emilia/cache/<source-id>/<rel-path>`.
pub fn cache_path(source_id: i64, rel: &str) -> PathBuf {
    let mut dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.push("emilia");
    dir.push("cache");
    dir.push(source_id.to_string());
    // `rel` comes from the server's PROPFIND href: drop `.`/`..` segments so a
    // hostile href can never traverse out of this source's cache directory.
    for seg in rel
        .split('/')
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
    {
        dir.push(seg);
    }
    dir
}

// ---------------------------------------------------------------------------
// Nextcloud login QR
// ---------------------------------------------------------------------------

/// Parses a Nextcloud login QR `nc://login/server:URL&user:USER&password:PW`
/// → `(server, user, password)`.
pub fn parse_nc_login(qr: &str) -> Option<(String, String, String)> {
    let rest = qr.trim().strip_prefix("nc://login/")?;
    let (mut server, mut user, mut password) = (None, None, None);
    for part in rest.split('&') {
        if let Some(v) = part.strip_prefix("server:") {
            server = Some(v.trim_end_matches('/').to_string());
        } else if let Some(v) = part.strip_prefix("user:") {
            user = Some(v.to_string());
        } else if let Some(v) = part.strip_prefix("password:") {
            password = Some(v.to_string());
        }
    }
    Some((server?, user?, password?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> Creds {
        Creds {
            base_url: "https://cloud.example.com".into(),
            user: "alice".into(),
            pass: "se cret".into(),
            music_path: "/My Music".into(),
        }
    }

    #[test]
    fn builds_dav_url_and_stream_uri() {
        let c = creds();
        assert_eq!(
            url_for(&c, "/Alben/X"),
            "https://cloud.example.com/remote.php/dav/files/alice/My%20Music/Alben/X"
        );
        assert_eq!(
            stream_uri(&c, "/Alben/X"),
            "https://alice:se%20cret@cloud.example.com/remote.php/dav/files/alice/My%20Music/Alben/X"
        );
    }

    #[test]
    fn strips_self_and_keeps_children() {
        let c = creds();
        let xml = r#"<?xml version="1.0"?>
        <d:multistatus xmlns:d="DAV:">
          <d:response><d:href>/remote.php/dav/files/alice/My%20Music/</d:href>
            <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
          </d:response>
          <d:response><d:href>/remote.php/dav/files/alice/My%20Music/Alben/</d:href>
            <d:propstat><d:prop><d:displayname>Alben</d:displayname>
            <d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
          </d:response>
          <d:response><d:href>/remote.php/dav/files/alice/My%20Music/song.mp3</d:href>
            <d:propstat><d:prop><d:displayname>song.mp3</d:displayname>
            <d:getcontentlength>123</d:getcontentlength>
            <d:resourcetype/></d:prop></d:propstat>
          </d:response>
        </d:multistatus>"#;
        // parse + filtering as in `list`, but without network:
        let prefix = req_path_decoded(&c, "");
        let prefix = prefix.trim_end_matches('/');
        let names: Vec<(String, bool)> = parse_propfind(xml)
            .into_iter()
            .filter_map(|raw| {
                let hp = href_to_path(&raw.href);
                let hp = hp.trim_end_matches('/').to_string();
                if hp == prefix {
                    return None;
                }
                let rem = hp.strip_prefix(prefix)?.trim_start_matches('/').to_string();
                if rem.is_empty() {
                    return None;
                }
                Some((rem, raw.is_dir))
            })
            .collect();
        assert_eq!(
            names,
            vec![("Alben".to_string(), true), ("song.mp3".to_string(), false)]
        );
    }

    #[test]
    fn unexpected_eof_is_benign_only_on_a_complete_body() {
        use std::io::{Error, ErrorKind};
        let eof = || Error::from(ErrorKind::UnexpectedEof);

        // Full advertised body arrived → missing close_notify is cosmetic, keep it.
        assert!(eof_is_benign(&eof(), 524_288, Some(524_288), 600_000));
        // Read past the advertised length (cap higher) still counts as complete.
        assert!(eof_is_benign(&eof(), 524_290, Some(524_288), 600_000));
        // Server advertised more than our read cap: reaching the cap is enough.
        assert!(eof_is_benign(&eof(), 600_000, Some(900_000), 600_000));

        // A short read is a real truncation, not cosmetic.
        assert!(!eof_is_benign(&eof(), 100, Some(524_288), 600_000));
        // No Content-Length → can't prove completeness, stay conservative.
        assert!(!eof_is_benign(&eof(), 524_288, None, 600_000));
        // A different I/O error is never silently swallowed.
        let reset = Error::from(ErrorKind::ConnectionReset);
        assert!(!eof_is_benign(&reset, 524_288, Some(524_288), 600_000));
    }

    #[test]
    fn parses_nc_login() {
        let qr = "nc://login/server:https://cloud.example.com&user:alice&password:abc-123";
        assert_eq!(
            parse_nc_login(qr),
            Some((
                "https://cloud.example.com".into(),
                "alice".into(),
                "abc-123".into()
            ))
        );
    }
}
