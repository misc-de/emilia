//! "Recently heard" log: songs recognized from a station's ICY title while
//! streaming, plus the lookups that resolve such a song to a locally playable
//! copy (a timeshift recording or a library track). Split out of db.rs.
//!
//! Unlike [`recording`](super::stream), nothing here points at an audio file —
//! the log is pure metadata about what played, deduplicated to one row per song.

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::Library;
use crate::model::HeardItem;

impl Library {
    /// Records that a song was recognized while streaming. **Deduplicates** to
    /// one row per song: an existing entry with the same title (case-folded) and
    /// matching artist — where either side's artist may have been unknown — is
    /// reused, bumping its `heard_at`/`station` to now and incrementing `count`.
    /// A still-missing artist is filled in when this detection knows it.
    pub fn note_heard(
        &self,
        artist: Option<&str>,
        title: &str,
        station: Option<&str>,
    ) -> Result<()> {
        let title = title.trim();
        if title.is_empty() {
            return Ok(());
        }
        let artist = artist.map(str::trim).filter(|s| !s.is_empty());
        // Same song already logged? Match on title; allow the artist to differ
        // only when one side was still unknown (mirrors `add_recording`).
        let existing: Option<(i64, Option<String>)> = self
            .conn
            .query_row(
                "SELECT id, artist FROM heard
                 WHERE lower(title) = lower(?1)
                   AND (IFNULL(lower(artist), '') = IFNULL(lower(?2), '')
                        OR IFNULL(TRIM(artist), '') = '' OR IFNULL(TRIM(?2), '') = '')
                 ORDER BY heard_at DESC, id DESC LIMIT 1",
                rusqlite::params![title, artist],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        if let Some((id, old_artist)) = existing {
            // Fill in the artist if it was missing before.
            if old_artist.as_deref().unwrap_or("").trim().is_empty() {
                if let Some(a) = artist {
                    self.conn.execute(
                        "UPDATE heard SET artist = ?1 WHERE id = ?2",
                        rusqlite::params![a, id],
                    )?;
                }
            }
            self.conn.execute(
                "UPDATE heard
                 SET station = ?1, heard_at = strftime('%s','now'), count = count + 1
                 WHERE id = ?2",
                rusqlite::params![station, id],
            )?;
            return Ok(());
        }

        self.conn.execute(
            "INSERT INTO heard (artist, title, station, heard_at, count)
             VALUES (?1, ?2, ?3, strftime('%s','now'), 1)",
            rusqlite::params![artist, title, station],
        )?;
        Ok(())
    }

    /// The recognized-songs log, newest first.
    pub fn heard_songs(&self) -> Result<Vec<HeardItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, artist, title, station, heard_at, count
             FROM heard ORDER BY heard_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(HeardItem {
                id: r.get(0)?,
                artist: r.get(1)?,
                title: r.get(2)?,
                station: r.get(3)?,
                heard_at: r.get(4)?,
                count: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Removes one entry from the recognized-songs log.
    pub fn delete_heard(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM heard WHERE id = ?1", [id])?;
        Ok(())
    }

    /// File path of a saved timeshift **recording** of this song, if any —
    /// the first candidate for "play the saved variant". Prefers a complete
    /// copy and, among those, the most recent. When an artist is given it must
    /// match (case-folded); a missing artist matches on title alone.
    pub fn find_recording(&self, artist: Option<&str>, title: &str) -> Result<Option<String>> {
        let artist = artist.map(str::trim).filter(|s| !s.is_empty());
        let path = self
            .conn
            .query_row(
                "SELECT path FROM recording
                 WHERE lower(title) = lower(?1)
                   AND (?2 IS NULL OR lower(IFNULL(artist,'')) = lower(?2))
                 ORDER BY incomplete ASC, recorded_at DESC, id DESC LIMIT 1",
                rusqlite::params![title, artist],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(path)
    }

    /// File path of a **library track** matching this song, if any — the second
    /// candidate for "play the saved variant". When an artist is given it must
    /// match (case-folded); a missing artist matches on title alone.
    pub fn find_track(&self, artist: Option<&str>, title: &str) -> Result<Option<String>> {
        let artist = artist.map(str::trim).filter(|s| !s.is_empty());
        let path = self
            .conn
            .query_row(
                "SELECT path FROM track
                 WHERE lower(title) = lower(?1)
                   AND (?2 IS NULL OR lower(IFNULL(artist,'')) = lower(?2))
                 ORDER BY id LIMIT 1",
                rusqlite::params![title, artist],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use crate::core::db::Library;

    #[test]
    fn note_heard_dedupes_and_counts() {
        let lib = Library::open_in_memory().unwrap();
        lib.note_heard(Some("Daft Punk"), "Get Lucky", Some("1LIVE"))
            .unwrap();
        // Re-hearing the same song (different station) bumps, does not duplicate.
        lib.note_heard(Some("daft punk"), "get lucky", Some("SWR3"))
            .unwrap();
        let songs = lib.heard_songs().unwrap();
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].count, 2);
        assert_eq!(songs[0].station.as_deref(), Some("SWR3"));
    }

    #[test]
    fn note_heard_fills_missing_artist_later() {
        let lib = Library::open_in_memory().unwrap();
        lib.note_heard(None, "Hello", Some("SWR3")).unwrap();
        lib.note_heard(Some("Adele"), "Hello", Some("SWR3"))
            .unwrap();
        let songs = lib.heard_songs().unwrap();
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].artist.as_deref(), Some("Adele"));
    }

    #[test]
    fn distinct_songs_stay_separate() {
        let lib = Library::open_in_memory().unwrap();
        lib.note_heard(Some("A"), "One", None).unwrap();
        lib.note_heard(Some("B"), "Two", None).unwrap();
        assert_eq!(lib.heard_songs().unwrap().len(), 2);
    }
}
