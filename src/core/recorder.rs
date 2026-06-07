//! Timeshift recording for streaming stations ("mini DVR"). A background thread
//! reads the stream over its **own** ICY connection (in addition to the
//! GStreamer playback), keeps the last N minutes in a **ring file** in the
//! cache and remembers the song boundaries (changes of `StreamTitle`). From this,
//! songs can be saved as a file **retroactively** – even if you only press
//! "record" at the end of the song.
//!
//! Deliberately decoupled from playback: GStreamer plays, this worker buffers.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};

/// A song detected in the buffer (boundary to boundary).
#[derive(Debug, Clone)]
pub struct BufferedSong {
    /// Running (absolute, monotonic) byte offset of the song start.
    pub start: u64,
    /// Byte offset of the song end (= start of the next song); `None` = the song
    /// is still playing.
    pub end: Option<u64>,
    /// "Artist - Title" from the ICY metadata.
    pub title: String,
    /// Is the **start** still in the buffer? Otherwise a recording would be incomplete.
    pub complete: bool,
}

/// Snapshot of the buffer state for the UI (replay page, recording).
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Start of the running song (absolute offset), if known.
    pub current_start: Option<u64>,
    /// Detected songs (newest last), including the running one.
    pub songs: Vec<BufferedSong>,
    /// Running (absolute, monotonic) byte offset of the buffer end – i.e. the
    /// most recent data written. Used to finalize the running song on stop.
    pub total: u64,
    /// Worker finished (stream ended/error)?
    pub ended: bool,
}

struct Marker {
    offset: u64,
    title: String,
}

#[derive(Default)]
struct Shared {
    current_title: Option<String>,
    markers: Vec<Marker>,
    total: u64,
    cap: u64,
    ended: bool,
}

/// Unique buffer file names so that a new recorder does not overwrite the file
/// of an old worker that is still winding down.
static BUFFER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Controls the recording worker of a station. On drop (`Drop`), the
/// worker is stopped and the buffer file is removed.
pub struct Recorder {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    buffer_path: PathBuf,
    /// Extension of the buffered audio data (e.g. "mp3"/"aac"). Set by the
    /// worker as soon as the Content-Type is known – therefore shared and read
    /// fresh when saving.
    ext: Arc<Mutex<String>>,
}

impl Recorder {
    /// Starts the recording worker for `url` with a buffer of `cap_minutes`
    /// minutes. Returns immediately; the worker runs in the background.
    pub fn start(url: &str, cap_minutes: u32) -> Recorder {
        let n = BUFFER_SEQ.fetch_add(1, Ordering::Relaxed);
        let mut buffer_path = crate::core::online::cover_cache_dir();
        buffer_path.push(format!("stream_buffer_{}_{n}.dat", std::process::id()));

        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(AtomicBool::new(false));
        // Extension is set by the worker as soon as the Content-Type is known;
        // as a sensible default "mp3" (most common ICY codec).
        let ext = Arc::new(Mutex::new(String::from("mp3")));

        {
            let (url, shared, stop, buffer_path, ext) = (
                url.to_string(),
                shared.clone(),
                stop.clone(),
                buffer_path.clone(),
                ext.clone(),
            );
            std::thread::spawn(move || {
                if let Err(e) = run(&url, cap_minutes, &buffer_path, &shared, &stop, &ext) {
                    tracing::info!("Stream recorder ended: {e}");
                }
                shared.lock().unwrap().ended = true;
            });
        }

        Recorder {
            shared,
            stop,
            buffer_path,
            ext,
        }
    }

    /// Snapshot of the buffer state for the UI.
    pub fn snapshot(&self) -> Snapshot {
        let s = self.shared.lock().unwrap();
        let avail = s.total.saturating_sub(s.cap);
        let mut songs: Vec<BufferedSong> = Vec::new();
        for (i, m) in s.markers.iter().enumerate() {
            let end = s.markers.get(i + 1).map(|n| n.offset);
            songs.push(BufferedSong {
                start: m.offset,
                end,
                title: m.title.clone(),
                complete: m.offset >= avail,
            });
        }
        let current_start = s.markers.last().map(|m| m.offset);
        Snapshot {
            current_start,
            songs,
            total: s.total,
            ended: s.ended,
        }
    }

    /// Cuts the byte range `[start, end)` out of the ring buffer and saves
    /// it as a tagged audio file in `dest_dir`. Returns the file path.
    /// `incomplete` marks (hint only) that the start may have been missing.
    pub fn save_song(
        &self,
        start: u64,
        end: u64,
        artist: Option<&str>,
        title: &str,
        dest_dir: &Path,
    ) -> Result<PathBuf> {
        let cap = self.shared.lock().unwrap().cap;
        let avail = {
            let s = self.shared.lock().unwrap();
            s.total.saturating_sub(s.cap)
        };
        let start = start.max(avail);
        if end <= start {
            return Err(anyhow!("nothing buffered for this segment"));
        }
        let data = read_ring(&self.buffer_path, cap, start, end)?;
        std::fs::create_dir_all(dest_dir)?;
        let base = sanitize_filename(artist, title);
        let ext = self.ext.lock().unwrap().clone();
        let path = unique_path(dest_dir, &base, &ext);
        std::fs::write(&path, &data)?;
        tag_file(&path, artist, title);
        Ok(path)
    }

    /// Cuts `[start, end)` into a **temporary** file (for previewing in the
    /// replay) and returns its path.
    pub fn extract_temp(&self, start: u64, end: u64) -> Result<PathBuf> {
        let cap = self.shared.lock().unwrap().cap;
        let avail = self.shared.lock().unwrap().total.saturating_sub(cap);
        let start = start.max(avail);
        if end <= start {
            return Err(anyhow!("nothing buffered for this segment"));
        }
        let data = read_ring(&self.buffer_path, cap, start, end)?;
        let ext = self.ext.lock().unwrap().clone();
        let mut path = std::env::temp_dir();
        let n = BUFFER_SEQ.fetch_add(1, Ordering::Relaxed);
        path.push(format!("emilia_replay_{}_{n}.{ext}", std::process::id()));
        std::fs::write(&path, &data)?;
        Ok(path)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.buffer_path);
    }
}

/// Worker loop: reads the ICY stream, buffers audio in the ring and tracks the
/// song boundaries.
fn run(
    url: &str,
    cap_minutes: u32,
    buffer_path: &Path,
    shared: &Arc<Mutex<Shared>>,
    stop: &Arc<AtomicBool>,
    ext_out: &Arc<Mutex<String>>,
) -> Result<()> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(20))
        .build();
    let resp = agent
        .get(url)
        .set("Icy-MetaData", "1")
        .set(
            "User-Agent",
            &format!("Emilia/{}", env!("CARGO_PKG_VERSION")),
        )
        .call()?;

    let metaint: usize = resp
        .header("icy-metaint")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let ext = ext_from_content_type(resp.header("Content-Type"));
    *ext_out.lock().unwrap() = ext.to_string();
    // Derive buffer capacity in bytes from the bitrate (default 256 kbit/s).
    let br_kbps: u64 = resp
        .header("icy-br")
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse().ok())
        .filter(|&b: &u64| b > 0)
        .unwrap_or(256);
    let bytes_per_sec = (br_kbps * 1000 / 8).max(8_000);
    let cap = (cap_minutes as u64 * 60 * bytes_per_sec).max(bytes_per_sec * 10);
    shared.lock().unwrap().cap = cap;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(buffer_path)?;
    let mut reader = std::io::BufReader::new(resp.into_reader());
    let mut total: u64 = 0;
    let mut buf = vec![0u8; 16 * 1024];
    // Debounce: a new `StreamTitle` only counts as a real song boundary once
    // it has been present for at least `min_bytes` (≈ MIN_SONG_SECS seconds). This
    // ignores station idents, ad insertions and fast back-and-forth
    // changes of the display (otherwise several files per song would arise).
    let min_bytes = MIN_SONG_SECS * bytes_per_sec;
    // Like `min_bytes`, but for the *cleared*-title gap: the title must stay empty
    // this long before we end the running song (filters brief title flicker).
    let min_gap = MIN_GAP_SECS * bytes_per_sec;
    // Candidate for the next song boundary: (start offset, title).
    let mut pending: Option<(u64, String)> = None;
    // Offset at which the ICY title most recently went empty (while a song was
    // running). Once the clear persists for `min_gap`, the song ends here and an
    // untitled gap segment begins. Reset when a title reappears.
    let mut empty_since: Option<u64> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Read the audio portion (either up to the next metadata marker, or –
        // without ICY metadata – simply block by block).
        let chunk = if metaint > 0 { metaint } else { buf.len() };
        let mut remaining = chunk;
        while remaining > 0 {
            let want = remaining.min(buf.len());
            let n = reader.read(&mut buf[..want])?;
            if n == 0 {
                return Ok(());
            }
            write_ring(&mut file, cap, total, &buf[..n])?;
            total += n as u64;
            remaining -= n;
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
        }
        {
            let mut s = shared.lock().unwrap();
            s.total = total;
            // If a candidate persists long enough → commit it as a real song boundary.
            let commit = pending
                .as_ref()
                .filter(|(off, _)| total.saturating_sub(*off) >= min_bytes)
                .cloned();
            if let Some((off, title)) = commit {
                s.markers.push(Marker {
                    offset: off,
                    title: title.clone(),
                });
                s.current_title = Some(title);
                if s.markers.len() > 256 {
                    s.markers.remove(0);
                }
                pending = None;
            }
            // Cleared title that has persisted long enough → end the running song
            // exactly where the title cleared and start an untitled gap segment
            // (the talk/ads between songs, saved to its own file, not identified).
            if let Some(empty_off) = empty_since {
                let cuttable =
                    s.current_title.is_some() && total.saturating_sub(empty_off) >= min_gap;
                if cuttable {
                    s.markers.push(Marker {
                        offset: empty_off,
                        title: String::new(),
                    });
                    s.current_title = None;
                    if s.markers.len() > 256 {
                        s.markers.remove(0);
                    }
                    empty_since = None;
                }
            }
            prune_markers(&mut s);
        }

        if metaint == 0 {
            continue;
        }

        // Metadata block: 1 length byte, then len*16 bytes of text.
        let mut lenb = [0u8; 1];
        if reader.read(&mut lenb)? == 0 {
            return Ok(());
        }
        let mlen = lenb[0] as usize * 16;
        if mlen > 0 {
            let mut meta = vec![0u8; mlen];
            reader.read_exact(&mut meta)?;
            if let Some(title) = parse_stream_title(&meta) {
                let title = title.trim().to_string();
                if title.is_empty() {
                    // ICY title cleared → remember where. If it stays cleared (see
                    // the commit in the section above), the running song ends there
                    // and an untitled gap segment begins.
                    empty_since.get_or_insert(total);
                    pending = None;
                } else {
                    empty_since = None; // a title is present again
                    let current = shared.lock().unwrap().current_title.clone();
                    if current.as_deref() == Some(title.as_str()) {
                        // Back to the running song (e.g. after an insertion)
                        // → discard candidate, no new song.
                        pending = None;
                    } else if pending.as_ref().map(|(_, t)| t.as_str()) != Some(title.as_str()) {
                        // New candidate – start offset = here; it must hold first.
                        pending = Some((total, title));
                    }
                    // Same candidate: keep the start offset (do nothing).
                }
            }
        }
    }
}

/// Minimum duration (seconds) a title must be present to count as its own song.
/// Shorter insertions (ads, station idents, fast changes)
/// are attributed to the running song instead of producing a second file.
const MIN_SONG_SECS: u64 = 30;

/// Minimum duration (seconds) the ICY title must stay *empty* before the running
/// song is ended at the clear point. Filters brief title flicker between songs
/// while still catching real gaps (talk/ads with no metadata).
const MIN_GAP_SECS: u64 = 6;

/// Removes song boundaries that have fully scrolled out of the buffer (whose
/// following boundary already lies beyond the available window) – but keeps the
/// oldest song that is still partially present.
fn prune_markers(s: &mut Shared) {
    let avail = s.total.saturating_sub(s.cap);
    while s.markers.len() >= 2 && s.markers[1].offset <= avail {
        s.markers.remove(0);
    }
}

/// Writes `data` at the absolute position `total` into the ring file (wraps at
/// the capacity end).
fn write_ring(file: &mut std::fs::File, cap: u64, total: u64, data: &[u8]) -> Result<()> {
    let pos = total % cap;
    let first = ((cap - pos) as usize).min(data.len());
    file.seek(SeekFrom::Start(pos))?;
    file.write_all(&data[..first])?;
    if first < data.len() {
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&data[first..])?;
    }
    Ok(())
}

/// Reads the absolute byte range `[start, end)` from the ring file (with
/// its own read handle; wraps at the capacity end).
fn read_ring(buffer_path: &Path, cap: u64, start: u64, end: u64) -> Result<Vec<u8>> {
    // Defensive: never underflow `end - start` (would allocate a huge buffer in
    // release builds) and never claim more than the ring can hold.
    let len = end
        .checked_sub(start)
        .filter(|&l| cap > 0 && l <= cap)
        .ok_or_else(|| anyhow!("invalid ring range"))? as usize;
    let mut out = vec![0u8; len];
    let mut file = std::fs::File::open(buffer_path)?;
    let pos = start % cap;
    let first = ((cap - pos) as usize).min(len);
    file.seek(SeekFrom::Start(pos))?;
    file.read_exact(&mut out[..first])?;
    if first < len {
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut out[first..])?;
    }
    Ok(out)
}

/// Reads the value of `StreamTitle='…'` from an ICY metadata block.
fn parse_stream_title(meta: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(meta);
    let start = text.find("StreamTitle=")? + "StreamTitle=".len();
    let rest = &text[start..];
    // Value is in single quotes: 'Artist - Title';
    let rest = rest.strip_prefix('\'').unwrap_or(rest);
    let endq = rest
        .find("';")
        .or_else(|| rest.find('\''))
        .unwrap_or(rest.len());
    let value = rest[..endq].trim_matches(char::from(0)).trim();
    // Return the value even when empty: an explicit empty `StreamTitle=''` is a
    // meaningful signal that the running song just ended. `None` means the field
    // was absent from this metadata block.
    Some(value.to_string())
}

fn ext_from_content_type(ct: Option<&str>) -> &'static str {
    match ct.map(|s| s.to_ascii_lowercase()) {
        Some(c) if c.contains("aac") => "aac",
        Some(c) if c.contains("ogg") || c.contains("opus") => "ogg",
        Some(c) if c.contains("flac") => "flac",
        _ => "mp3",
    }
}

/// Turns "Artist - Title" into a usable file name stem.
fn sanitize_filename(artist: Option<&str>, title: &str) -> String {
    let raw = match artist {
        Some(a) if !a.trim().is_empty() => format!("{a} - {title}"),
        _ => title.to_string(),
    };
    let cleaned: String = raw
        .chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').trim();
    if cleaned.is_empty() {
        "Recording".to_string()
    } else {
        cleaned.chars().take(120).collect()
    }
}

/// Finds a free file path (appends " (2)" etc. if needed).
fn unique_path(dir: &Path, base: &str, ext: &str) -> PathBuf {
    let mut p = dir.join(format!("{base}.{ext}"));
    let mut i = 2;
    while p.exists() {
        p = dir.join(format!("{base} ({i}).{ext}"));
        i += 1;
    }
    p
}

/// Writes artist/title/album **and a cover** into an already saved
/// recording (best effort). Called in the background after a cover has been
/// found online.
pub fn embed_cover(
    path: &Path,
    artist: Option<&str>,
    title: &str,
    album: Option<&str>,
    image: &[u8],
) {
    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::prelude::{Accessor, TagExt};
    use lofty::tag::{Tag, TagType};

    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_title(title.to_string());
    if let Some(a) = artist.filter(|a| !a.trim().is_empty()) {
        tag.set_artist(a.to_string());
    }
    if let Some(al) = album.filter(|a| !a.trim().is_empty()) {
        tag.set_album(al.to_string());
    }
    let mime = if image.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        MimeType::Png
    } else {
        MimeType::Jpeg
    };
    let pic = Picture::new_unchecked(PictureType::CoverFront, Some(mime), None, image.to_vec());
    tag.push_picture(pic);
    if let Err(e) = tag.save_to_path(path, WriteOptions::default()) {
        tracing::debug!("Could not embed cover into {}: {e}", path.display());
    }
}

/// Writes artist/title as a tag (best effort – if it fails, the
/// file just stays untagged; the file name already carries the info).
fn tag_file(path: &Path, artist: Option<&str>, title: &str) {
    use lofty::config::WriteOptions;
    use lofty::prelude::{Accessor, TagExt};
    use lofty::tag::{Tag, TagType};

    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_title(title.to_string());
    if let Some(a) = artist.filter(|a| !a.trim().is_empty()) {
        tag.set_artist(a.to_string());
    }
    if let Err(e) = tag.save_to_path(path, WriteOptions::default()) {
        tracing::debug!("Could not tag recording {}: {e}", path.display());
    }
}
