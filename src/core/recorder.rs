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

/// Locking that recovers from poisoning instead of panicking. The worker thread
/// and the GTK main thread share these mutexes; the main loop calls `snapshot()`
/// on a 1-second tick, so a panic *inside* the lock on the worker would poison
/// the mutex and otherwise take the whole app down at the next tick. The guarded
/// data is only ever simple field assignments (always left consistent), so
/// recovering the inner value is safe.
trait LockExt<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

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
    /// Generous lead guard (bytes) to extend the saved file *before* `start`
    /// (into the previous song), sized by the start boundary's uncertainty. The
    /// overlap is trimmed away later in the editor.
    pub lead_pad: u64,
    /// Generous tail guard (bytes) to extend the saved file *after* `end` (into
    /// the next song), sized by the end boundary's uncertainty.
    pub tail_pad: u64,
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
    /// Generous save guard (bytes) for cutting *at* this boundary: how far a
    /// saved song may reach past it to absorb ICY/refine imprecision (the
    /// neighbouring songs trim the overlap away in the editor). Large when the
    /// cut is uncertain (ICY sluggish / no silence found), small when it snapped
    /// cleanly onto a real gap.
    pad: u64,
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
    /// Worker thread handle. Joined on drop (off the calling thread) so the
    /// buffer file is only removed once the worker has stopped writing to it.
    worker: Option<std::thread::JoinHandle<()>>,
    /// Extension of the buffered audio data (e.g. "mp3"/"aac"). Set by the
    /// worker as soon as the Content-Type is known – therefore shared and read
    /// fresh when saving.
    ext: Arc<Mutex<String>>,
}

impl Recorder {
    /// Starts the recording worker for `url` with a buffer of `cap_minutes`
    /// minutes. Returns immediately; the worker runs in the background.
    pub fn start(url: &str, cap_minutes: u32, station: Option<&str>) -> Recorder {
        let n = BUFFER_SEQ.fetch_add(1, Ordering::Relaxed);
        let cache_dir = crate::core::online::cover_cache_dir();
        // Sweep ring-buffer files orphaned by a previous run (a hard kill leaves
        // the off-thread `Drop` cleanup unfinished); ours are reaped on drop.
        cleanup_stale_buffers(&cache_dir);
        let mut buffer_path = cache_dir;
        buffer_path.push(format!("stream_buffer_{}_{n}.dat", std::process::id()));

        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(AtomicBool::new(false));
        // Extension is set by the worker as soon as the Content-Type is known;
        // as a sensible default "mp3" (most common ICY codec).
        let ext = Arc::new(Mutex::new(String::from("mp3")));

        let worker = {
            let (url, station, shared, stop, buffer_path, ext) = (
                url.to_string(),
                station.map(str::to_string),
                shared.clone(),
                stop.clone(),
                buffer_path.clone(),
                ext.clone(),
            );
            std::thread::spawn(move || {
                if let Err(e) = run(
                    &url,
                    cap_minutes,
                    station.as_deref(),
                    &buffer_path,
                    &shared,
                    &stop,
                    &ext,
                ) {
                    tracing::info!("Stream recorder ended: {e}");
                }
                shared.lock_or_recover().ended = true;
            })
        };

        Recorder {
            shared,
            stop,
            buffer_path,
            worker: Some(worker),
            ext,
        }
    }

    /// Snapshot of the buffer state for the UI.
    pub fn snapshot(&self) -> Snapshot {
        let s = self.shared.lock_or_recover();
        let avail = s.total.saturating_sub(s.cap);
        let mut songs: Vec<BufferedSong> = Vec::new();
        for (i, m) in s.markers.iter().enumerate() {
            let next = s.markers.get(i + 1);
            songs.push(BufferedSong {
                start: m.offset,
                end: next.map(|n| n.offset),
                title: m.title.clone(),
                complete: m.offset >= avail,
                // Pad toward each boundary by that boundary's own uncertainty:
                // the start marker for the lead, the next marker for the tail.
                lead_pad: m.pad,
                tail_pad: next.map_or(0, |n| n.pad),
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
        lead_pad: u64,
        tail_pad: u64,
        artist: Option<&str>,
        title: &str,
        dest_dir: &Path,
    ) -> Result<PathBuf> {
        let (cap, avail, total) = {
            let s = self.shared.lock_or_recover();
            (s.cap, s.total.saturating_sub(s.cap), s.total)
        };
        // Extend the cut generously on both sides — clamped to what is still
        // buffered — so a slightly-off boundary never clips the song; the overlap
        // with the neighbours is trimmed away later in the editor.
        let start = start.saturating_sub(lead_pad).max(avail);
        let end = end.saturating_add(tail_pad).min(total);
        if end <= start {
            return Err(anyhow!("nothing buffered for this segment"));
        }
        let data = read_ring(&self.buffer_path, cap, start, end)?;
        std::fs::create_dir_all(dest_dir)?;
        let base = sanitize_filename(artist, title);
        let ext = self.ext.lock_or_recover().clone();
        let path = unique_path(dest_dir, &base, &ext);
        std::fs::write(&path, &data)?;
        tag_file(&path, artist, title);
        Ok(path)
    }

    /// Cuts `[start, end)` into a **temporary** file (for previewing in the
    /// replay) and returns its path.
    pub fn extract_temp(&self, start: u64, end: u64) -> Result<PathBuf> {
        let cap = self.shared.lock_or_recover().cap;
        let avail = self.shared.lock_or_recover().total.saturating_sub(cap);
        let start = start.max(avail);
        if end <= start {
            return Err(anyhow!("nothing buffered for this segment"));
        }
        let data = read_ring(&self.buffer_path, cap, start, end)?;
        let ext = self.ext.lock_or_recover().clone();
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
        // Join off the calling thread (Drop usually runs on the GTK main thread
        // when switching stations): the worker may sit in a blocking read for up
        // to its read timeout, and we must not stall the UI. Removing the buffer
        // file only *after* the worker has stopped also avoids deleting a file it
        // is still appending to.
        let worker = self.worker.take();
        let path = std::mem::take(&mut self.buffer_path);
        std::thread::spawn(move || {
            if let Some(h) = worker {
                let _ = h.join();
            }
            let _ = std::fs::remove_file(&path);
        });
    }
}

/// Removes orphaned ring-buffer files in `dir` left by a previous run. A file is
/// kept only while its owning process is still alive (`/proc/<pid>`), so a
/// concurrently running second instance is never disturbed; everything else is
/// stale and removed. Best effort — any I/O error is ignored.
fn cleanup_stale_buffers(dir: &Path) {
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Expected shape: `stream_buffer_<pid>_<seq>.dat`.
        let Some(rest) = name.strip_prefix("stream_buffer_") else {
            continue;
        };
        let Some(pid) = rest.split('_').next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me || Path::new(&format!("/proc/{pid}")).exists() {
            continue; // ours, or owned by another live instance
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Worker loop: reads the ICY stream, buffers audio in the ring and tracks the
/// song boundaries.
fn run(
    url: &str,
    cap_minutes: u32,
    station: Option<&str>,
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
    *ext_out.lock_or_recover() = ext.to_string();
    // Derive buffer capacity in bytes from the bitrate (default 256 kbit/s).
    let br_kbps: u64 = resp
        .header("icy-br")
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse().ok())
        .filter(|&b: &u64| b > 0)
        .unwrap_or(256);
    let bytes_per_sec = (br_kbps * 1000 / 8).max(8_000);
    let cap = (cap_minutes as u64 * 60 * bytes_per_sec).max(bytes_per_sec * 10);
    shared.lock_or_recover().cap = cap;

    // Station idents/self-promo that some stations inject into the ICY title
    // (e.g. "1LIVE DIGGI auch als Stream: 1LIVEDIGGI.de") are not songs. Build the
    // identity needles once (from the configured station name and the advertised
    // `icy-name`); such titles are then treated like a cleared title (a gap)
    // instead of being saved as a bogus song that also splits the real one.
    let idents = station_idents(station, resp.header("icy-name"));

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
        // Decide what (if anything) to commit under a short lock, then refine the
        // cut point *outside* the lock — decoding the window must not block UI
        // snapshots. New-song and gap commits are mutually exclusive in any one
        // iteration (a non-empty title clears `empty_since`; an empty one clears
        // `pending`), so at most one fires here.
        let (commit_new, commit_gap, prev_off) = {
            let mut s = shared.lock_or_recover();
            s.total = total;
            let commit_new = pending
                .as_ref()
                .filter(|(off, _)| total.saturating_sub(*off) >= min_bytes)
                .cloned();
            let commit_gap = empty_since
                .filter(|&eo| s.current_title.is_some() && total.saturating_sub(eo) >= min_gap);
            let avail = total.saturating_sub(s.cap);
            // A refined boundary may never move before the previous marker (or out
            // of the buffer) — that would invert the song order.
            let prev_off = s.markers.last().map_or(avail, |m| m.offset).max(avail);
            (commit_new, commit_gap, prev_off)
        };

        if let Some((off, title)) = commit_new {
            // Snap the raw ICY boundary onto the real silence gap nearby; `pad` is
            // the generous save guard derived from how confident that snap was.
            let (at, pad) =
                refine_marker(buffer_path, cap, bytes_per_sec, ext, off, prev_off, total);
            let mut s = shared.lock_or_recover();
            s.markers.push(Marker {
                offset: at,
                title: title.clone(),
                pad,
            });
            s.current_title = Some(title);
            if s.markers.len() > 256 {
                s.markers.remove(0);
            }
            prune_markers(&mut s);
            pending = None;
        } else if let Some(empty_off) = commit_gap {
            // Cleared title that has persisted long enough → end the running song
            // at the (refined) clear point and start an untitled gap segment (the
            // talk/ads between songs, saved to its own file, not identified).
            let (at, pad) = refine_marker(
                buffer_path,
                cap,
                bytes_per_sec,
                ext,
                empty_off,
                prev_off,
                total,
            );
            let mut s = shared.lock_or_recover();
            s.markers.push(Marker {
                offset: at,
                title: String::new(),
                pad,
            });
            s.current_title = None;
            if s.markers.len() > 256 {
                s.markers.remove(0);
            }
            prune_markers(&mut s);
            empty_since = None;
        } else {
            let mut s = shared.lock_or_recover();
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
                if title.is_empty() || is_station_ident(&title, &idents) {
                    // ICY title cleared *or* a station ident/self-promo (not a
                    // song) → remember where. If it persists (see the commit in the
                    // section above), the running song ends there and an untitled
                    // gap segment begins — instead of saving the promo as a bogus
                    // song that also splits the real one around it.
                    empty_since.get_or_insert(total);
                    pending = None;
                } else {
                    empty_since = None; // a title is present again
                    let current = shared.lock_or_recover().current_title.clone();
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

/// Half-width (seconds) of the window searched around a raw ICY marker for the
/// real silence gap to snap the cut onto. Stations are usually only a few seconds
/// off the audio transition, so a narrow window avoids snapping to a quiet
/// passage *inside* a song.
const REFINE_WINDOW_SECS: u64 = 5;

/// Worst-case generous save guard (seconds) applied to a cut boundary when the
/// refine step could **not** confirm it on a real silence (ICY too sluggish, or
/// the songs crossfade). The neighbouring songs absorb the overlap; the editor
/// trims it. A confident, silence-snapped cut uses only a small fraction of this.
const SAVE_GUARD_MAX_SECS: u64 = 30;

/// Minimum generous guard (seconds) even for a confident cut, so there is always
/// a little material to trim on either side.
const SAVE_GUARD_MIN_SECS: u64 = 2;

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

/// Refines a raw ICY marker byte offset to the nearest real silence gap within
/// ±[`REFINE_WINDOW_SECS`], by decoding that window and snapping to the quiet
/// point closest to the raw position. Falls back to the raw offset when nothing
/// decodes or the window has no genuine gap (e.g. a crossfade) — a cut is never
/// moved further from the truth than the metadata already put it. Best effort;
/// `lo_bound`/`hi_bound` keep the boundary between its neighbours. Also returns a
/// generous save guard (bytes): small when the cut snapped confidently onto a
/// silence (sized by how far the ICY marker was off, a proxy for its
/// sluggishness), the full worst-case [`SAVE_GUARD_MAX_SECS`] when it could not.
fn refine_marker(
    buffer_path: &Path,
    cap: u64,
    bytes_per_sec: u64,
    ext: &str,
    raw: u64,
    lo_bound: u64,
    hi_bound: u64,
) -> (u64, u64) {
    let max_pad = SAVE_GUARD_MAX_SECS.saturating_mul(bytes_per_sec);
    if bytes_per_sec == 0 {
        return (raw, 0);
    }
    let w = REFINE_WINDOW_SECS.saturating_mul(bytes_per_sec);
    let lo = raw.saturating_sub(w).max(lo_bound);
    let hi = raw.saturating_add(w).min(hi_bound);
    // Need a usable window straddling the marker (the decoder also drops the
    // first partial frame, so demand at least ~2 s).
    if raw <= lo || hi <= raw || hi.saturating_sub(lo) < bytes_per_sec * 2 {
        return (raw, max_pad);
    }
    let data = match read_ring(buffer_path, cap, lo, hi) {
        Ok(d) => d,
        Err(_) => return (raw, max_pad),
    };
    // Decode via a temp file so the existing GStreamer `decode_pcm` (filesrc) can
    // sniff the codec; commits are minutes apart, so the I/O is negligible.
    let mut tmp = std::env::temp_dir();
    let n = BUFFER_SEQ.fetch_add(1, Ordering::Relaxed);
    tmp.push(format!("emilia_refine_{}_{n}.{ext}", std::process::id()));
    if std::fs::write(&tmp, &data).is_err() {
        return (raw, max_pad);
    }
    let target = (raw - lo) as f64 / bytes_per_sec as f64;
    let snapped = crate::core::waveform::snap_to_silence(&tmp, target);
    let _ = std::fs::remove_file(&tmp);
    match snapped {
        Some(t) => {
            let refined = (lo + (t * bytes_per_sec as f64) as u64).clamp(lo, hi);
            // Confident cut on a real silence → modest guard that grows with how
            // far the ICY marker was off (its sluggishness), never the full 30 s.
            let pad = (refined.abs_diff(raw) + SAVE_GUARD_MIN_SECS.saturating_mul(bytes_per_sec))
                .min(max_pad);
            if refined != raw {
                tracing::debug!(
                    "stream cut refined {:+.2}s onto silence",
                    (refined as f64 - raw as f64) / bytes_per_sec as f64
                );
            }
            (refined, pad)
        }
        None => (raw, max_pad),
    }
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

/// Lowercased, alphanumeric-only form for ident matching (drops spaces and
/// punctuation so "1LIVE DIGGI" and "1Livediggi" compare equal).
fn normalize_ident(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Builds the normalized "identity needles" of a station from its configured
/// name and the advertised `icy-name` header. A `StreamTitle` containing one of
/// these (and lacking an "Artist - Title" structure) is a station
/// ident/self-promo rather than a song.
fn station_idents(station: Option<&str>, icy_name: Option<&str>) -> Vec<String> {
    let mut needles: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let n = normalize_ident(s);
        // Require a few chars so a short/generic token cannot match real songs.
        if n.len() >= 4 && !needles.contains(&n) {
            needles.push(n);
        }
    };
    if let Some(s) = station {
        push(s);
    }
    if let Some(name) = icy_name {
        // `icy-name` is often "Station, Broadcaster City" — use the leading part.
        if let Some(first) = name.split(',').next() {
            push(first);
        }
    }
    needles
}

/// Whether the title carries a dash-style "Artist - Title" separator (ASCII
/// hyphen or en/em dash, surrounded by spaces) — the radio convention for songs.
fn has_track_separator(title: &str) -> bool {
    title.contains(" - ") || title.contains(" – ") || title.contains(" — ")
}

/// True if `title` looks like a station ident/self-promo rather than a song: it
/// contains a station identity needle **and** has no "Artist - Title" separator.
/// The separator guard keeps a real song that merely *mentions* the station
/// (e.g. "Queen - Radio Ga Ga" on a station called "Radio") from being dropped.
fn is_station_ident(title: &str, idents: &[String]) -> bool {
    if idents.is_empty() || has_track_separator(title) {
        return false;
    }
    let n = normalize_ident(title);
    idents.iter().any(|needle| n.contains(needle.as_str()))
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

#[cfg(test)]
mod tests {
    use super::{is_station_ident, station_idents};

    // Live ICY metadata observed on WDR 1Live Diggi: the station alternates the
    // StreamTitle between real songs and a self-promo every ~minute.
    const ICY_NAME: &str = "1Livediggi, Westdeutscher Rundfunk Koeln";

    #[test]
    fn flags_station_self_promo() {
        let idents = station_idents(Some("1Live Diggi"), Some(ICY_NAME));
        assert!(is_station_ident(
            "1LIVE DIGGI auch als Stream: 1LIVEDIGGI.de",
            &idents
        ));
    }

    #[test]
    fn keeps_real_songs() {
        let idents = station_idents(Some("1Live Diggi"), Some(ICY_NAME));
        for s in [
            "Mauvais djo - Maladie",
            "Lil Uzi Vert - What You Saying",
            "GORDO & Reinier Zonneveld - Loco Loco",
        ] {
            assert!(
                !is_station_ident(s, &idents),
                "{s} wrongly flagged as ident"
            );
        }
    }

    #[test]
    fn icy_name_alone_catches_promo() {
        // Even without a configured station name, the advertised icy-name suffices.
        let idents = station_idents(None, Some(ICY_NAME));
        assert!(is_station_ident(
            "1LIVE DIGGI auch als Stream: 1LIVEDIGGI.de",
            &idents
        ));
    }

    #[test]
    fn separator_guard_protects_song_mentioning_station() {
        // A song whose title contains the station token survives because it has an
        // "Artist - Title" separator …
        let idents = station_idents(Some("Radio"), None);
        assert!(!is_station_ident("Queen - Radio Ga Ga", &idents));
        // … while a bare promo with the same token is still caught.
        assert!(is_station_ident("Radio Webstream jetzt live", &idents));
    }

    #[test]
    fn no_idents_means_no_false_positives() {
        assert!(!is_station_ident("Some Promo Without Needles", &[]));
    }
}
