//! Streaming & recording queries for [`Library`] (split out of db.rs).

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::Library;

impl Library {
    // ---- Streaming (internet radio) ----

    /// Stores a station (or updates its fields for a known URL)
    /// and returns the station ID.
    #[allow(clippy::too_many_arguments)]
    pub fn add_stream(
        &self,
        name: &str,
        url: &str,
        favicon: Option<&str>,
        tags: Option<&str>,
        country: Option<&str>,
        codec: Option<&str>,
        bitrate: Option<i64>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO stream (name, url, favicon, tags, country, codec, bitrate, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%s','now'))
             ON CONFLICT(url) DO UPDATE SET
                name = excluded.name, favicon = excluded.favicon, tags = excluded.tags,
                country = excluded.country, codec = excluded.codec, bitrate = excluded.bitrate",
            rusqlite::params![name, url, favicon, tags, country, codec, bitrate],
        )?;
        Ok(self
            .conn
            .query_row("SELECT id FROM stream WHERE url = ?1", [url], |r| r.get(0))?)
    }

    /// All stored stations, sorted alphanumerically by name
    /// (case-insensitive), with the most recently added breaking ties.
    pub fn streams(&self) -> Result<Vec<crate::model::StreamItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, url, favicon, tags, country FROM stream
             ORDER BY name COLLATE NOCASE, added_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::model::StreamItem {
                id: r.get(0)?,
                name: r.get(1)?,
                url: r.get(2)?,
                favicon: r.get(3)?,
                tags: r.get(4)?,
                country: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Renames a station.
    pub fn rename_stream(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE stream SET name = ?1 WHERE id = ?2",
            rusqlite::params![name, id],
        )?;
        Ok(())
    }

    /// Removes a station.
    pub fn delete_stream(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM stream WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- Recordings (timeshift recordings) ----

    /// Stores a recording entry and returns its ID.
    /// Stores a recording, **deduplicating** against rows of the same song. A
    /// single broadcast track can surface under slightly varying ICY titles or
    /// be (re-)detected more than once, which previously produced one song
    /// "spread over several recordings". When a recording with the same station
    /// and title (case-insensitive; artist may have been unknown before) already
    /// exists, that row is reused/updated instead of inserting a new one.
    ///
    /// Returns `(id, inserted, superseded_path)`:
    /// * `inserted` is `true` when a brand-new row was created (the new file is
    ///   the canonical copy).
    /// * `superseded_path` is set when an existing **incomplete** row was
    ///   upgraded to this **complete** copy: the row now points at the new file
    ///   and the caller should delete the returned old file. Otherwise the
    ///   caller should drop the *new* file (a matching copy already exists).
    pub fn add_recording(
        &self,
        path: &str,
        artist: Option<&str>,
        title: &str,
        station: Option<&str>,
        incomplete: bool,
    ) -> Result<(i64, bool, Option<String>)> {
        // Existing recording of the same song? Match on station + title; allow
        // the artist to differ only when one side was still unknown (a later,
        // better identification fills it in).
        let existing: Option<(i64, String, Option<String>, i64)> = self
            .conn
            .query_row(
                "SELECT id, path, artist, incomplete FROM recording
                 WHERE lower(title) = lower(?1)
                   AND IFNULL(lower(station), '') = IFNULL(lower(?2), '')
                   AND (IFNULL(lower(artist), '') = IFNULL(lower(?3), '')
                        OR IFNULL(TRIM(artist), '') = '' OR IFNULL(TRIM(?3), '') = '')
                 ORDER BY recorded_at ASC, id ASC LIMIT 1",
                rusqlite::params![title, station, artist],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;

        if let Some((id, old_path, old_artist, old_incomplete)) = existing {
            // Fill in the artist if it was missing before.
            if old_artist.as_deref().unwrap_or("").trim().is_empty() {
                if let Some(a) = artist.filter(|a| !a.trim().is_empty()) {
                    self.conn.execute(
                        "UPDATE recording SET artist = ?1 WHERE id = ?2",
                        rusqlite::params![a, id],
                    )?;
                }
            }
            // Upgrade an incomplete copy to a complete one: repoint to the new
            // file and let the caller delete the old (truncated) file.
            if old_incomplete != 0 && !incomplete {
                self.conn.execute(
                    "UPDATE recording SET path = ?1, incomplete = 0 WHERE id = ?2",
                    rusqlite::params![path, id],
                )?;
                return Ok((id, false, Some(old_path)));
            }
            return Ok((id, false, None));
        }

        self.conn.execute(
            "INSERT INTO recording (path, artist, title, station, recorded_at, incomplete)
             VALUES (?1, ?2, ?3, ?4, strftime('%s','now'), ?5)",
            rusqlite::params![path, artist, title, station, incomplete as i64],
        )?;
        Ok((self.conn.last_insert_rowid(), true, None))
    }

    /// All recordings, newest first.
    pub fn recordings(&self) -> Result<Vec<crate::model::RecordingItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, artist, title, station, recorded_at, duration_ms, incomplete
             FROM recording ORDER BY recorded_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::model::RecordingItem {
                id: r.get(0)?,
                path: r.get(1)?,
                artist: r.get(2)?,
                title: r.get(3)?,
                station: r.get(4)?,
                recorded_at: r.get(5)?,
                duration_ms: r.get(6)?,
                incomplete: r.get::<_, i64>(7)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Backfills the cached playback length of a recording (probed lazily when
    /// the list is shown, since older rows were stored without a duration).
    pub fn set_recording_duration(&self, id: i64, duration_ms: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE recording SET duration_ms = ?2 WHERE id = ?1",
            rusqlite::params![id, duration_ms],
        )?;
        Ok(())
    }

    /// Removes a recording from management and returns its file path
    /// (so that the caller can delete the file).
    pub fn delete_recording(&self, id: i64) -> Result<Option<String>> {
        let path: Option<String> = self
            .conn
            .query_row("SELECT path FROM recording WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        self.conn
            .execute("DELETE FROM recording WHERE id = ?1", [id])?;
        Ok(path)
    }

    /// Updates a recording's file path and duration after the editor re-encoded
    /// it (the cut changes the length, and an unencodable container such as AAC
    /// is rewritten as MP3, which changes the extension).
    pub fn update_recording_file(&self, id: i64, path: &str, duration_ms: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE recording SET path = ?2, duration_ms = ?3 WHERE id = ?1",
            rusqlite::params![id, path, duration_ms],
        )?;
        Ok(())
    }

    /// All episodes along with podcast info (for the "Newest" view). The
    /// chronological sorting by publication date is handled by the UI
    /// (the stored date is only text).
    pub fn all_episodes(&self) -> Result<Vec<crate::model::EpisodeRef>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.title, p.image_url, e.title, e.audio_url, e.published, e.duration, e.description
             FROM episode e JOIN podcast p ON p.id = e.podcast_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::model::EpisodeRef {
                podcast_title: r.get(0)?,
                podcast_image: r.get(1)?,
                title: r.get(2)?,
                audio_url: r.get(3)?,
                published: r.get(4)?,
                duration: r.get(5)?,
                description: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
