//! Recursive directory scan + metadata via lofty.

use anyhow::Result;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::core::db::Library;
use crate::model::Track;

const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "m4a", "aac", "wav", "wma", "mka",
];

/// Reads title, artist and playback duration (ms) of a file in a single pass
/// (for display in the file list).
pub fn read_meta(path: &Path) -> (Option<String>, Option<String>, Option<i64>) {
    let Ok(tagged) = lofty::read_from_path(path) else {
        return (None, None, None);
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

/// Reads album, year **and** genre in a single tag parse (for the folder detail
/// view, which would otherwise parse the same files once for the collection
/// summary and once for the first genre). The file is only read, never modified.
pub fn read_album_year_genre(path: &Path) -> (Option<String>, Option<u32>, Option<String>) {
    let Ok(tagged) = lofty::read_from_path(path) else {
        return (None, None, None);
    };
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return (None, None, None);
    };
    let album = tag
        .album()
        .map(|c| c.trim().to_string())
        .filter(|s| !s.is_empty());
    let genre = tag
        .genre()
        .map(|c| c.trim().to_string())
        .filter(|s| !s.is_empty());
    (album, tag.year(), genre)
}

/// Reads the (unsynchronized) lyrics from the tags, if present.
/// The audio file is only read in the process.
pub fn read_lyrics(path: &Path) -> Option<String> {
    let tagged = lofty::read_from_path(path).ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    tag.get_string(&lofty::tag::ItemKey::Lyrics)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Playback duration of an audio file in seconds (0 if not readable).
pub fn duration_secs(path: &Path) -> u64 {
    lofty::read_from_path(path)
        .ok()
        .map(|t| t.properties().duration().as_secs())
        .unwrap_or(0)
}

pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Recursively collects all audio files below `root`.
pub fn collect_audio_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if is_audio(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Reads a file's metadata into a `Track` model.
/// Falls back to the file name when tags are missing (important for audio dramas).
pub fn read_track(path: &Path) -> Result<Track> {
    read_track_detailed(path).map(|(track, _composer)| track)
}

/// Like [`read_track`] but also returns the composer, extracted from the **same**
/// single tag read. Used by the detail view, which would otherwise parse the
/// file twice (once for the track, once for genre/composer).
pub fn read_track_detailed(path: &Path) -> Result<(Track, Option<String>)> {
    let tagged = lofty::read_from_path(path)?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());

    let file_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown")
        .to_string();

    let (title, artist, album, genre, track_no, disc_no, year, composer) = match tag {
        Some(tag) => (
            tag.title().map(|c| c.to_string()).unwrap_or(file_stem),
            tag.artist().map(|c| c.to_string()),
            tag.album().map(|c| c.to_string()),
            tag.genre()
                .map(|c| c.to_string())
                .filter(|s| !s.trim().is_empty()),
            tag.track(),
            tag.disk(),
            tag.year().map(|y| y as i32),
            tag.get_string(&lofty::tag::ItemKey::Composer)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        ),
        None => (file_stem, None, None, None, None, None, None, None),
    };

    let duration_ms = tagged.properties().duration().as_millis() as i64;

    Ok((
        Track {
            id: 0,
            path: path.to_string_lossy().into_owned(),
            title,
            artist,
            album,
            genre,
            track_no,
            disc_no,
            duration_ms: Some(duration_ms),
            resume_ms: 0,
            year,
        },
        composer,
    ))
}

/// Scans `root` and writes all found tracks into the library, then removes
/// tracks under `root` whose files have vanished (deleted/moved).
/// Returns the number of successfully read files.
///
/// Upserts are committed in batches (one transaction per `BATCH` files) so a
/// large library is not thousands of separate fsyncs and a crash leaves whole
/// batches, never a half-written row.
pub fn scan_into(lib: &Library, root: &Path) -> Result<usize> {
    // No progress, never cancelled — the plain scan used where no UI feedback is
    // wanted (e.g. the enrichment worker's pre-scan).
    scan_into_progress(lib, root, &AtomicBool::new(false), |_, _, _, _| {})
}

/// Like [`scan_into`] but reports progress and can be cancelled, for the
/// interactive library import. `on_progress(done_files, total_files, done_bytes,
/// total_bytes)` is called once up front (with `done = 0`, so the UI gets the
/// totals immediately) and then throttled to ~200 updates over the run. When
/// `cancel` flips to `true` the scan stops at the next file and, crucially, does
/// **not** prune — a half-finished scan must never delete the rows it didn't
/// reach yet. Already-read files stay in the library.
pub fn scan_into_progress(
    lib: &Library,
    root: &Path,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(usize, usize, u64, u64),
) -> Result<usize> {
    const BATCH: usize = 500;
    let files = collect_audio_files(root);
    let total_files = files.len();
    // Pre-scan: stat each file so the UI can show "X MB of Y MB" from the start.
    let sizes: Vec<u64> = files
        .iter()
        .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .collect();
    let total_bytes: u64 = sizes.iter().sum();
    on_progress(0, total_files, 0, total_bytes);

    // Every audio file physically present under `root` (regardless of whether its
    // tags read cleanly) — the set that must survive orphan pruning.
    let present: Vec<String> = files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    // Throttle UI updates to at most ~200 over the whole run (plus the final one).
    let step = (total_files / 200).max(1);
    let mut count = 0;
    let mut done_bytes = 0u64;
    let mut cancelled = false;
    let mut batch: Vec<Track> = Vec::with_capacity(BATCH);
    for (i, path) in files.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        match read_track(path) {
            Ok(track) => batch.push(track),
            Err(e) => tracing::warn!("Failed to read {}: {e}", path.display()),
        }
        if batch.len() >= BATCH {
            count += lib.upsert_tracks_resilient(&batch);
            batch.clear();
        }
        done_bytes += sizes[i];
        if (i + 1) % step == 0 || i + 1 == total_files {
            on_progress(i + 1, total_files, done_bytes, total_bytes);
        }
    }
    count += lib.upsert_tracks_resilient(&batch);

    // Drop DB rows for files that no longer exist under `root`. Skipped when the
    // scan found nothing (so an unreadable/unmounted folder cannot wipe the DB)
    // or when the user cancelled (a partial scan must not prune the rest).
    if !cancelled {
        lib.prune_tracks_under(root, &present)?;
    }
    Ok(count)
}

/// Indexes a single, freshly arrived file (e.g. one received over device sync) by
/// **reading its actual tags** and upserting the `track` row — so it is read in
/// and sorted into the library exactly like a normal scan would, rather than
/// trusting second-hand metadata. If the tags can't be parsed, a minimal row is
/// still written from the file name so the file stays playable.
pub fn ingest_file(lib: &Library, path: &Path) {
    match read_track(path) {
        Ok(track) => {
            let _ = lib.upsert_track(&track);
        }
        Err(e) => {
            tracing::warn!("Indexed received file without tags {}: {e}", path.display());
            let title = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Unknown")
                .to_string();
            let _ = lib.upsert_track(&Track {
                path: path.to_string_lossy().into_owned(),
                title,
                ..Default::default()
            });
        }
    }
}
