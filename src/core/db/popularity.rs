//! Cache of online popularity data (see `core::online::popularity`) for
//! [`Library`].

use rusqlite::OptionalExtension;

use super::Library;

/// A found entry is reused for 30 days (popularity moves slowly) …
const FRESH_SECS: i64 = 30 * 86_400;
/// … a lookup without a match is retried after a week.
const MISS_SECS: i64 = 7 * 86_400;

impl Library {
    /// Cached popularity JSON of `(kind, key)`: `None` when nothing fresh is
    /// cached (→ fetch), `Some(None)` for a recent lookup without a match,
    /// `Some(Some(json))` for data.
    pub fn cached_popularity(&self, kind: &str, key: &str) -> Option<Option<String>> {
        self.conn
            .query_row(
                "SELECT data FROM popularity \
                 WHERE kind = ?1 AND key = ?2 \
                   AND fetched_at > strftime('%s','now') - \
                       CASE WHEN data IS NULL THEN ?3 ELSE ?4 END",
                rusqlite::params![kind, key, MISS_SECS, FRESH_SECS],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    /// Stores (or replaces) the popularity entry of `(kind, key)`; `None`
    /// records a lookup without a match.
    pub fn store_popularity(&self, kind: &str, key: &str, data: Option<&str>) {
        let _ = self.conn.execute(
            "INSERT OR REPLACE INTO popularity(kind, key, data, fetched_at) \
             VALUES(?1, ?2, ?3, strftime('%s','now'))",
            rusqlite::params![kind, key, data],
        );
    }
}
