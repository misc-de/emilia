//! Online-fetch failure counters for [`Library`] (split out of db.rs).

use super::Library;

impl Library {
    // ---- Failure counters (limit the repeated online retrying) ----

    /// Previous unsuccessful online attempts for an album (0 if unknown).
    pub fn album_attempts(&self, artist: &str, album: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT attempts FROM album_meta WHERE artist = ?1 AND album = ?2",
                rusqlite::params![artist, album],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    /// Previous unsuccessful online attempts for an artist.
    pub fn artist_attempts(&self, name: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT attempts FROM artist_meta WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    /// Reset an artist's online-fetch failure counter so a refresh retries it.
    pub fn reset_artist_attempts(&self, name: &str) {
        let _ = self.conn.execute(
            "UPDATE artist_meta SET attempts = 0 WHERE name = ?1",
            [name],
        );
    }

    /// Reset an album's online-fetch failure counters so a refresh retries it —
    /// the cover **and** the release year, since the manual refresh is the way
    /// back for an album the automatic sweep has given up on.
    pub fn reset_album_attempts(&self, artist: &str, album: &str) {
        let _ = self.conn.execute(
            "UPDATE album_meta SET attempts = 0, year_attempts = 0
             WHERE artist = ?1 AND album = ?2",
            rusqlite::params![artist, album],
        );
    }

    /// Previous unsuccessful fingerprint attempts for a track (path).
    pub fn track_attempts(&self, path: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT attempts FROM track_meta WHERE path = ?1",
                [path],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }
}
