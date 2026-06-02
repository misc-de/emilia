//! Recursive directory scan + metadata via lofty.

use anyhow::Result;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;
use std::path::{Path, PathBuf};

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

/// Reads album tag and release year in a single pass
/// (for the brief overview in "More info").
pub fn read_album_year(path: &Path) -> (Option<String>, Option<u32>) {
    let Ok(tagged) = lofty::read_from_path(path) else {
        return (None, None);
    };
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return (None, None);
    };
    let album = tag
        .album()
        .map(|c| c.trim().to_string())
        .filter(|s| !s.is_empty());
    (album, tag.year())
}

/// Reads genre and composer from the file tags (for "More info").
/// Both only if actually set; empty tags yield `None`. As everywhere
/// here, the file is only read, never modified.
pub fn read_genre_composer(path: &Path) -> (Option<String>, Option<String>) {
    let Ok(tagged) = lofty::read_from_path(path) else {
        return (None, None);
    };
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return (None, None);
    };
    let genre = tag
        .genre()
        .map(|c| c.trim().to_string())
        .filter(|s| !s.is_empty());
    let composer = tag
        .get_string(&lofty::tag::ItemKey::Composer)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    (genre, composer)
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
    let tagged = lofty::read_from_path(path)?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());

    let file_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown")
        .to_string();

    let (title, artist, album, genre, track_no, disc_no) = match tag {
        Some(tag) => (
            tag.title().map(|c| c.to_string()).unwrap_or(file_stem),
            tag.artist().map(|c| c.to_string()),
            tag.album().map(|c| c.to_string()),
            tag.genre()
                .map(|c| c.to_string())
                .filter(|s| !s.trim().is_empty()),
            tag.track(),
            tag.disk(),
        ),
        None => (file_stem, None, None, None, None, None),
    };

    let duration_ms = tagged.properties().duration().as_millis() as i64;

    Ok(Track {
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
    })
}

/// Scans `root` and writes all found tracks into the library, then removes
/// tracks under `root` whose files have vanished (deleted/moved).
/// Returns the number of successfully read files.
///
/// Upserts are committed in batches (one transaction per `BATCH` files) so a
/// large library is not thousands of separate fsyncs and a crash leaves whole
/// batches, never a half-written row.
pub fn scan_into(lib: &Library, root: &Path) -> Result<usize> {
    const BATCH: usize = 500;
    let files = collect_audio_files(root);
    // Every audio file physically present under `root` (regardless of whether its
    // tags read cleanly) — the set that must survive orphan pruning.
    let present: Vec<String> = files.iter().map(|p| p.to_string_lossy().into_owned()).collect();

    let mut count = 0;
    let mut batch: Vec<Track> = Vec::with_capacity(BATCH);
    for path in &files {
        match read_track(path) {
            Ok(track) => batch.push(track),
            Err(e) => tracing::warn!("Failed to read {}: {e}", path.display()),
        }
        if batch.len() >= BATCH {
            count += lib.upsert_tracks(&batch)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        count += lib.upsert_tracks(&batch)?;
    }

    // Drop DB rows for files that no longer exist under `root`. Skipped when the
    // scan found nothing, so an unreadable/unmounted folder cannot wipe the DB.
    lib.prune_tracks_under(root, &present)?;
    Ok(count)
}
