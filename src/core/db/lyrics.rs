//! Lyrics cache and per-track lyric preferences for [`Library`] (split out of db.rs).

use rusqlite::OptionalExtension;

use super::Library;

impl Library {
    /// Returns cached lyrics for a track path, or `None` when nothing positive
    /// is cached (a negative result is also reported as `None`).
    pub fn get_cached_lyrics(&self, path: &str) -> Option<crate::core::lyrics::Lyrics> {
        let (plain, synced, source) = self
            .conn
            .query_row(
                "SELECT plain, synced, source FROM lyrics_cache WHERE path = ?1",
                [path],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .ok()?;
        if source == "none" {
            return None;
        }
        let lyr = crate::core::lyrics::Lyrics::from_parts(plain, synced);
        lyr.has_any().then_some(lyr)
    }

    /// Whether a **recent** negative result is cached for this path – used to
    /// avoid hammering the online service for tracks that genuinely have no
    /// lyrics. Stale negatives (older than ~14 days) report `false` so the
    /// lookup is retried eventually.
    pub fn lyrics_recently_missing(&self, path: &str) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM lyrics_cache \
                 WHERE path = ?1 AND source = 'none' \
                   AND cached_at > strftime('%s','now') - 1209600",
                [path],
                |_| Ok(()),
            )
            .is_ok()
    }

    /// Stores (or replaces) the lyrics cache entry for a path. Pass
    /// `source = "none"` with empty texts to record a negative result.
    pub fn store_lyrics(
        &self,
        path: &str,
        plain: Option<&str>,
        synced: Option<&str>,
        source: &str,
    ) {
        let _ = self.conn.execute(
            "INSERT OR REPLACE INTO lyrics_cache(path, plain, synced, source, cached_at) \
             VALUES(?1, ?2, ?3, ?4, strftime('%s','now'))",
            rusqlite::params![path, plain, synced, source],
        );
    }

    /// Per-track lyric preferences: `(karaoke_off, delay_ms)`. Defaults to
    /// `(false, 0)` when nothing is stored.
    pub fn lyrics_pref(&self, path: &str) -> (bool, i64) {
        self.conn
            .query_row(
                "SELECT karaoke_off, delay_ms FROM lyrics_pref WHERE path = ?1",
                [path],
                |r| Ok((r.get::<_, i64>(0)? != 0, r.get::<_, i64>(1)?)),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or((false, 0))
    }

    /// Turns the timed karaoke highlighting on/off for a track (preserving the
    /// stored delay).
    pub fn set_lyrics_karaoke_off(&self, path: &str, off: bool) {
        let _ = self.conn.execute(
            "INSERT INTO lyrics_pref(path, karaoke_off) VALUES(?1, ?2)
             ON CONFLICT(path) DO UPDATE SET karaoke_off = excluded.karaoke_off",
            rusqlite::params![path, off as i64],
        );
    }

    /// Sets the manual karaoke timing offset (ms) for a track (preserving the
    /// karaoke on/off flag).
    pub fn set_lyrics_delay(&self, path: &str, delay_ms: i64) {
        let _ = self.conn.execute(
            "INSERT INTO lyrics_pref(path, delay_ms) VALUES(?1, ?2)
             ON CONFLICT(path) DO UPDATE SET delay_ms = excluded.delay_ms",
            rusqlite::params![path, delay_ms],
        );
    }
}
