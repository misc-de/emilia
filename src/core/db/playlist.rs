//! Playlist queries for [`Library`] (split out of db.rs).

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::Library;

impl Library {
    // ---- Playlists ----

    /// Creates a playlist and returns its ID.
    pub fn create_playlist(&self, name: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO playlist (name, created_at) VALUES (?1, strftime('%s','now'))",
            [name],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Renames a playlist.
    pub fn rename_playlist(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE playlist SET name = ?1 WHERE id = ?2",
            rusqlite::params![name, id],
        )?;
        Ok(())
    }

    /// Deletes a playlist along with its entries.
    pub fn delete_playlist(&self, id: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM playlist_item WHERE playlist_id = ?1", [id])?;
        tx.execute("DELETE FROM playlist WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// All playlists as (id, name, track count), sorted alphabetically.
    pub fn playlists(&self) -> Result<Vec<(i64, String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, COUNT(i.path)
             FROM playlist p
             LEFT JOIN playlist_item i ON i.playlist_id = p.id
             GROUP BY p.id
             ORDER BY p.name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Playlists with their origin marker: `(id, name, count, origin)`.
    /// `origin == None` ⇒ user playlist; `Some(url)` ⇒ YouTube/source mirror.
    /// Lets sync distinguish user playlists (shared as paths) from YT mirrors
    /// (shared as YouTube items).
    pub fn playlists_with_origin(&self) -> Result<Vec<(i64, String, i64, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, COUNT(i.path), p.origin
             FROM playlist p
             LEFT JOIN playlist_item i ON i.playlist_id = p.id
             GROUP BY p.id
             ORDER BY p.name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The user-chosen cover of a playlist (a path), or `None` if none was
    /// picked yet (the UI then derives one from the songs).
    pub fn playlist_cover(&self, id: i64) -> Result<Option<String>> {
        let cover = self
            .conn
            .query_row(
                "SELECT cover_path FROM playlist
                 WHERE id = ?1 AND cover_path IS NOT NULL AND cover_path <> ''",
                [id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(cover)
    }

    /// Stores the chosen cover of a playlist (clears it when `path` is empty).
    pub fn set_playlist_cover(&self, id: i64, path: &str) -> Result<()> {
        let value = (!path.is_empty()).then_some(path);
        self.conn.execute(
            "UPDATE playlist SET cover_path = ?2 WHERE id = ?1",
            rusqlite::params![id, value],
        )?;
        Ok(())
    }

    /// Total runtime of a playlist in milliseconds, summed over the tracks that
    /// the library knows a duration for (YouTube/stream entries without one
    /// simply contribute 0).
    pub fn playlist_duration_ms(&self, id: i64) -> Result<i64> {
        let ms: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(t.duration_ms), 0)
             FROM playlist_item i JOIN track t ON t.path = i.path
             WHERE i.playlist_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        Ok(ms)
    }

    /// Total runtime per playlist in one pass, for rebuilding the playlist
    /// overview without one aggregate query per row.
    pub fn playlist_durations_ms(&self) -> Result<std::collections::HashMap<i64, i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, COALESCE(SUM(t.duration_ms), 0)
             FROM playlist p
             LEFT JOIN playlist_item i ON i.playlist_id = p.id
             LEFT JOIN track t ON t.path = i.path
             GROUP BY p.id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()?)
    }

    /// Appends paths to the end of a playlist (duplicates allowed).
    pub fn add_to_playlist(&self, id: i64, paths: &[String]) -> Result<()> {
        // Compute the start position and insert in one transaction so two
        // concurrent appenders cannot read the same MAX(position) and collide.
        let tx = self.conn.unchecked_transaction()?;
        let start: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM playlist_item WHERE playlist_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO playlist_item (playlist_id, position, path) VALUES (?1, ?2, ?3)",
            )?;
            for (i, path) in paths.iter().enumerate() {
                stmt.execute(rusqlite::params![id, start + i as i64, path])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Paths of a playlist in their order.
    pub fn playlist_paths(&self, id: i64) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM playlist_item WHERE playlist_id = ?1 ORDER BY position")?;
        let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Repoints playlist items that are stream recordings to a regular local
    /// library file of the same song, where one now exists. A match needs the
    /// same artist + title (case-insensitive) **and** a duration within a few
    /// seconds; the candidate must itself not be a recording. The recording file
    /// is left untouched – only the playlist reference is updated. Returns the
    /// number of relinked items. Meant to be called when opening a playlist.
    pub fn relink_recordings_in_playlist(&self, playlist_id: i64) -> Result<usize> {
        // Duration tolerance: a recording rarely matches the released file to the
        // millisecond (lead-in/out, trimming), so allow a few seconds of slack.
        const TOLERANCE_MS: i64 = 5000;
        let tx = self.conn.unchecked_transaction()?;
        // Items in this playlist whose path is a known stream recording.
        let items: Vec<(i64, Option<String>, String, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT pi.position, r.artist, r.title, r.duration_ms
                 FROM playlist_item pi
                 JOIN recording r ON r.path = pi.path
                 WHERE pi.playlist_id = ?1",
            )?;
            let rows = stmt.query_map([playlist_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut relinked = 0;
        for (position, artist, title, dur_ms) in items {
            let local: Option<String> = tx
                .query_row(
                    "SELECT t.path FROM track t
                     WHERE lower(t.title) = lower(?1)
                       AND lower(IFNULL(t.artist, '')) = lower(IFNULL(?2, ''))
                       AND abs(IFNULL(t.duration_ms, 0) - ?3) <= ?4
                       AND t.path NOT IN (SELECT path FROM recording)
                     LIMIT 1",
                    rusqlite::params![title, artist, dur_ms, TOLERANCE_MS],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            if let Some(local_path) = local {
                tx.execute(
                    "UPDATE playlist_item SET path = ?1 WHERE playlist_id = ?2 AND position = ?3",
                    rusqlite::params![local_path, playlist_id, position],
                )?;
                relinked += 1;
            }
        }
        tx.commit()?;
        Ok(relinked)
    }

    /// Removes all occurrences of a path from a playlist. Currently no UI path
    /// triggers this (track removal was dropped from the playlist list), but it
    /// is kept as a library operation (and covered by a test).
    #[allow(dead_code)]
    pub fn remove_from_playlist(&self, id: i64, path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM playlist_item WHERE playlist_id = ?1 AND path = ?2",
            rusqlite::params![id, path],
        )?;
        Ok(())
    }
}
