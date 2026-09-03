//! Track CRUD for [`Library`] (split out of db.rs).

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::Library;
use crate::model::Track;

/// Shared upsert for the `track` table, used by both the single-row
/// [`Library::upsert_track`] and the batched [`Library::upsert_tracks`].
const UPSERT_TRACK_SQL: &str = r#"
    INSERT INTO track (path, title, artist, album, track_no, disc_no, duration_ms, genre, year)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
    ON CONFLICT(path) DO UPDATE SET
        title       = excluded.title,
        artist      = excluded.artist,
        album       = excluded.album,
        track_no    = excluded.track_no,
        disc_no     = excluded.disc_no,
        duration_ms = excluded.duration_ms,
        genre       = excluded.genre,
        year        = excluded.year
"#;

/// Binds a `Track` to [`UPSERT_TRACK_SQL`]'s placeholders. A macro (not a fn)
/// because `rusqlite::params!` borrows from `t` and cannot be returned.
macro_rules! track_upsert_params {
    ($t:expr) => {
        rusqlite::params![
            $t.path,
            $t.title,
            $t.artist,
            $t.album,
            $t.track_no,
            $t.disc_no,
            $t.duration_ms,
            $t.genre,
            $t.year,
        ]
    };
}

/// Column list of every `SELECT` that feeds [`row_to_track`]. The two belong
/// together: the mapping addresses its columns by index, so this order is what
/// makes those indices correct.
const TRACK_COLS: &str =
    "id, path, title, artist, album, track_no, duration_ms, resume_ms, disc_no, year";

/// Maps a row selected with [`TRACK_COLS`] to a [`Track`]. `genre` is not part
/// of that list, so it stays `None` here.
fn row_to_track(r: &rusqlite::Row) -> rusqlite::Result<Track> {
    Ok(Track {
        id: r.get(0)?,
        path: r.get(1)?,
        title: r.get(2)?,
        artist: r.get(3)?,
        album: r.get(4)?,
        genre: None,
        track_no: r.get::<_, Option<i64>>(5)?.map(|n| n as u32),
        duration_ms: r.get(6)?,
        resume_ms: r.get(7)?,
        disc_no: r.get::<_, Option<i64>>(8)?.map(|n| n as u32),
        year: r.get(9)?,
    })
}

impl Library {
    /// Inserts a track or updates its metadata (key: path).
    pub fn upsert_track(&self, t: &Track) -> Result<()> {
        self.conn
            .execute(UPSERT_TRACK_SQL, track_upsert_params!(t))?;
        Ok(())
    }

    /// Upserts many tracks in a single transaction. Atomic (a crash mid-scan
    /// leaves the previous state, not a half-written batch) and dramatically
    /// faster than one implicit transaction per row (one fsync per batch instead
    /// of per track). Used by the directory scan.
    pub fn upsert_tracks(&self, tracks: &[Track]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut count = 0;
        {
            let mut stmt = tx.prepare_cached(UPSERT_TRACK_SQL)?;
            for t in tracks {
                stmt.execute(track_upsert_params!(t))?;
                count += 1;
            }
        }
        tx.commit()?;
        Ok(count)
    }

    /// Upserts a batch like [`upsert_tracks`], but if the batched transaction
    /// fails it falls back to per-track upserts so a single bad row cannot drop
    /// the whole chunk. Never returns an error (best effort) — used by the
    /// library scan and cloud indexing, where one odd file must not abort the
    /// entire run. Returns how many tracks were stored.
    pub fn upsert_tracks_resilient(&self, tracks: &[Track]) -> usize {
        if tracks.is_empty() {
            return 0;
        }
        match self.upsert_tracks(tracks) {
            Ok(c) => c,
            Err(_) => tracks
                .iter()
                .filter(|t| self.upsert_track(t).is_ok())
                .count(),
        }
    }

    /// Stores the resume position by path. The
    /// queue is path-based; nothing happens for an unknown path.
    pub fn set_resume_path(&self, path: &str, resume_ms: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE track SET resume_ms = ?1 WHERE path = ?2",
            rusqlite::params![resume_ms, path],
        )?;
        Ok(())
    }

    /// Reads a single track by its path (incl. resume position).
    pub fn track_by_path(&self, path: &str) -> Result<Option<Track>> {
        let track = self
            .conn
            .query_row(
                &format!("SELECT {TRACK_COLS} FROM track WHERE path = ?1"),
                [path],
                row_to_track,
            )
            .optional()?;
        Ok(track)
    }

    /// Looks up many tracks by path in one (chunked) query, returning a
    /// `path -> Track` map. Avoids an N+1 of [`track_by_path`] when resolving a
    /// whole queue's or playlist's metadata at once.
    pub fn tracks_by_paths(
        &self,
        paths: &[String],
    ) -> Result<std::collections::HashMap<String, Track>> {
        let mut map = std::collections::HashMap::with_capacity(paths.len());
        // SQLite caps the number of bound parameters; chunk well under the limit.
        for chunk in paths.chunks(900) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!("SELECT {TRACK_COLS} FROM track WHERE path IN ({placeholders})");
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk), row_to_track)?;
            for t in rows {
                let t = t?;
                map.insert(t.path.clone(), t);
            }
        }
        Ok(map)
    }

    /// All tracks, sorted by album and track number.
    pub fn all_tracks(&self) -> Result<Vec<Track>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TRACK_COLS} FROM track
             ORDER BY album, COALESCE(disc_no, 1), track_no, title"
        ))?;
        let rows = stmt.query_map([], row_to_track)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Tracks of one album name only, sorted for album playback/subpages. This
    /// avoids loading the whole library when opening a single album.
    pub fn tracks_by_album_name(&self, album: &str) -> Result<Vec<Track>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TRACK_COLS} FROM track
             WHERE album = ?1 COLLATE NOCASE
             ORDER BY COALESCE(disc_no, 1), track_no, path"
        ))?;
        let rows = stmt.query_map([album], row_to_track)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Removes one track row by path. Only the `track` row goes — playlist and
    /// favorite entries keyed by the path stay, exactly as the scan-time prune
    /// leaves them, so a re-scan of a still-present file restores it seamlessly.
    /// Returns whether a row was removed.
    pub fn delete_track(&self, path: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM track WHERE path = ?1", [path])?
            > 0)
    }

    /// All tracks whose path starts with the raw `prefix` — no separator is
    /// appended, unlike [`Self::tracks_under_path`] — for a source root such as
    /// `nc:3:` whose relative paths may or may not begin with a slash. Same
    /// index-friendly range scan.
    pub fn tracks_with_prefix(&self, prefix: &str) -> Result<Vec<Track>> {
        let upper = format!("{prefix}\u{10FFFF}");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TRACK_COLS} FROM track
             WHERE path >= ?1 AND path < ?2"
        ))?;
        let rows = stmt.query_map([prefix, upper.as_str()], row_to_track)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// All tracks whose path lies under `dir`, via an index-friendly range scan
    /// on the path (`[dir/, dir/\u{10FFFF})`) — far cheaper than loading every
    /// track and filtering by prefix in Rust, and exact (no LIKE/GLOB wildcards).
    pub fn tracks_under_path(&self, dir: &str) -> Result<Vec<Track>> {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        let upper = format!("{prefix}\u{10FFFF}");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TRACK_COLS} FROM track
             WHERE path >= ?1 AND path < ?2"
        ))?;
        let rows = stmt.query_map([&prefix, &upper], row_to_track)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
