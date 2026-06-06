//! Listening statistics queries for [`Library`] (split out of db.rs).

use anyhow::Result;

use super::file_stem_of;
use super::Library;
use crate::model::*;

impl Library {
    // ---- Listening statistics (play_event) ----

    /// SQL predicate (over columns of `play_event`) from which an event counts
    /// as a "play": Last.fm rule -- at least 30 s **or** half the
    /// track length heard. Below that it counts as a skip/abort.
    /// `play_event` is aliased as `e` in all analysis queries (columns
    /// like `duration_ms` also exist in `track` → otherwise ambiguous).
    const COUNTS_AS_PLAY: &'static str =
        "(e.played_ms >= 30000 OR (e.duration_ms > 0 AND e.played_ms * 2 >= e.duration_ms))";

    /// Writes a listening event and incidentally updates `track.last_played`
    /// (the column has always existed but was unused).
    pub fn log_play(
        &self,
        path: &str,
        started_at: i64,
        played_ms: i64,
        duration_ms: i64,
        completed: bool,
        source: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO play_event (path, started_at, played_ms, duration_ms, completed, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                path,
                started_at,
                played_ms.max(0),
                (duration_ms > 0).then_some(duration_ms),
                completed as i64,
                source,
            ],
        )?;
        // Only move forward (a resume from the past must not turn it back).
        self.conn.execute(
            "UPDATE track SET last_played = ?2
             WHERE path = ?1 AND (last_played IS NULL OR last_played < ?2)",
            rusqlite::params![path, started_at],
        )?;
        Ok(())
    }

    /// Overall metrics from `since` (Unix seconds; 0 = since the beginning).
    /// `distinct_tracks` counts only what actually counts as a play.
    /// `distinct_artists`/`distinct_albums` are left at 0 here and filled by the
    /// caller from the full top lists (which it fetches anyway), so the feat./
    /// album-name folding isn't computed twice.
    pub fn stats_totals(&self, since: i64) -> Result<StatTotals> {
        let (total_played_ms, plays, skips): (i64, i64, i64) = self.conn.query_row(
            &format!(
                "SELECT COALESCE(SUM(e.played_ms), 0),
                        COALESCE(SUM(CASE WHEN {p} THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN {p} THEN 0 ELSE 1 END), 0)
                 FROM play_event e WHERE e.started_at >= ?1",
                p = Self::COUNTS_AS_PLAY
            ),
            [since],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let distinct_tracks: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM (
                     SELECT e.path FROM play_event e
                     WHERE e.started_at >= ?1 AND {p} GROUP BY e.path
                 )",
                p = Self::COUNTS_AS_PLAY
            ),
            [since],
            |r| r.get(0),
        )?;
        Ok(StatTotals {
            total_played_ms,
            plays,
            skips,
            distinct_tracks,
            // Filled by the caller from the full top lists (see doc).
            distinct_artists: 0,
            distinct_albums: 0,
        })
    }

    /// Top tracks from `since`, sorted by plays (then time heard).
    ///
    /// Besides local tracks (joined from `track`), this also resolves the
    /// display name of podcast episodes and YouTube videos, which have no
    /// `track` row: a `yt:<id>` path takes its title from `yt_title` and its
    /// channel from `yt_recent`; a podcast URL takes title and show name from
    /// `episode`/`podcast`. Scalar subqueries (not joins) keep the per-path
    /// SUMs free of fan-out.
    pub fn stats_top_tracks(&self, since: i64, limit: usize) -> Result<Vec<StatEntry>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT COALESCE(
                        NULLIF(t.title, ''),
                        (SELECT y.title FROM yt_title y
                         WHERE e.path LIKE 'yt:%' AND y.video_id = substr(e.path, 4)),
                        (SELECT ep.title FROM episode ep WHERE ep.audio_url = e.path LIMIT 1)
                    ) AS title,
                    e.path,
                    COALESCE(
                        NULLIF(t.artist, ''),
                        (SELECT yr.artist FROM yt_recent yr
                         WHERE e.path LIKE 'yt:%' AND yr.video_id = substr(e.path, 4)),
                        (SELECT pc.title FROM podcast pc
                         JOIN episode ep ON ep.podcast_id = pc.id
                         WHERE ep.audio_url = e.path LIMIT 1),
                        ''
                    ) AS artist,
                    SUM(CASE WHEN {p} THEN 1 ELSE 0 END) AS plays,
                    SUM(e.played_ms) AS ms
             FROM play_event e
             LEFT JOIN track t ON t.path = e.path
             WHERE e.started_at >= ?1
             GROUP BY e.path
             HAVING plays > 0
             ORDER BY plays DESC, ms DESC
             LIMIT ?2",
            p = Self::COUNTS_AS_PLAY
        ))?;
        let rows = stmt.query_map(rusqlite::params![since, limit as i64], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        Ok(rows
            .filter_map(|r| r.ok())
            .map(|(title, path, artist, plays, played_ms)| StatEntry {
                name: title
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| file_stem_of(&path)),
                detail: artist,
                plays,
                played_ms,
            })
            .collect())
    }

    /// Top albums from `since`. Folded over the album name like
    /// [`Self::albums_overview`]; display artist = primary artist with the most plays.
    pub fn stats_top_albums(&self, since: i64, limit: usize) -> Result<Vec<StatEntry>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT COALESCE(t.artist, '') AS artist, t.album,
                    SUM(CASE WHEN {p} THEN 1 ELSE 0 END) AS plays,
                    SUM(e.played_ms) AS ms
             FROM play_event e
             JOIN track t ON t.path = e.path
             WHERE e.started_at >= ?1 AND t.album IS NOT NULL AND t.album <> ''
             GROUP BY COALESCE(t.artist, ''), t.album",
            p = Self::COUNTS_AS_PLAY
        ))?;
        let raw = stmt
            .query_map([since], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>();

        use std::collections::HashMap;
        let mut map: HashMap<String, StatEntry> = HashMap::new();
        let mut votes: HashMap<String, HashMap<String, i64>> = HashMap::new();
        for (artist, album, plays, ms) in raw {
            let key = album.to_lowercase();
            let e = map.entry(key.clone()).or_insert_with(|| StatEntry {
                name: album.clone(),
                detail: String::new(),
                plays: 0,
                played_ms: 0,
            });
            e.plays += plays;
            e.played_ms += ms;
            let primary = crate::core::artist::primary_artist(&artist);
            *votes.entry(key).or_default().entry(primary).or_insert(0) += plays;
        }
        for (key, e) in map.iter_mut() {
            if let Some((name, _)) = votes
                .get(key)
                .and_then(|v| v.iter().max_by_key(|(_, c)| **c))
            {
                e.detail = name.clone();
            }
        }
        Ok(Self::rank(map.into_values().collect(), limit))
    }

    /// Top artists from `since`. Folded over the primary artist (feat. resolution),
    /// so that "A" and "A feat. B" collapse together.
    pub fn stats_top_artists(&self, since: i64, limit: usize) -> Result<Vec<StatEntry>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT COALESCE(t.artist, '') AS artist,
                    SUM(CASE WHEN {p} THEN 1 ELSE 0 END) AS plays,
                    SUM(e.played_ms) AS ms
             FROM play_event e
             JOIN track t ON t.path = e.path
             WHERE e.started_at >= ?1 AND t.artist IS NOT NULL AND t.artist <> ''
             GROUP BY COALESCE(t.artist, '')",
            p = Self::COUNTS_AS_PLAY
        ))?;
        let raw = stmt
            .query_map([since], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>();

        use std::collections::HashMap;
        let mut map: HashMap<String, StatEntry> = HashMap::new();
        for (artist, plays, ms) in raw {
            let primary = crate::core::artist::primary_artist(&artist);
            let e = map
                .entry(crate::core::artist::norm_key(&primary))
                .or_insert_with(|| StatEntry {
                    name: primary.clone(),
                    detail: String::new(),
                    plays: 0,
                    played_ms: 0,
                });
            e.plays += plays;
            e.played_ms += ms;
        }
        Ok(Self::rank(map.into_values().collect(), limit))
    }

    /// Top genres by plays (from the track genres stored in the
    /// library). Only tracks with a genre set count; tracks without a genre
    /// (or scanned before the genre migration) are not considered.
    pub fn stats_top_genres(&self, since: i64, limit: usize) -> Result<Vec<StatEntry>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT t.genre AS genre,
                    SUM(CASE WHEN {p} THEN 1 ELSE 0 END) AS plays,
                    SUM(e.played_ms) AS ms
             FROM play_event e
             JOIN track t ON t.path = e.path
             WHERE e.started_at >= ?1 AND t.genre IS NOT NULL AND t.genre <> ''
             GROUP BY t.genre",
            p = Self::COUNTS_AS_PLAY
        ))?;
        let entries = stmt
            .query_map([since], |r| {
                Ok(StatEntry {
                    name: r.get::<_, String>(0)?,
                    detail: String::new(),
                    plays: r.get::<_, i64>(1)?,
                    played_ms: r.get::<_, i64>(2)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>();
        Ok(Self::rank(entries, limit))
    }

    /// Keep only actual plays, sort by plays (then time)
    /// and truncate to `limit`.
    fn rank(mut entries: Vec<StatEntry>, limit: usize) -> Vec<StatEntry> {
        entries.retain(|e| e.plays > 0);
        entries.sort_by(|a, b| {
            b.plays
                .cmp(&a.plays)
                .then(b.played_ms.cmp(&a.played_ms))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        entries.truncate(limit);
        entries
    }

    /// Time heard (ms) per hour of the day (index 0..23, local time).
    pub fn stats_by_hour(&self, since: i64) -> Result<[i64; 24]> {
        let mut out = [0i64; 24];
        let mut stmt = self.conn.prepare(
            "SELECT CAST(strftime('%H', started_at, 'unixepoch', 'localtime') AS INTEGER),
                    SUM(played_ms)
             FROM play_event WHERE started_at >= ?1 GROUP BY 1",
        )?;
        let rows = stmt.query_map([since], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        for (h, ms) in rows.flatten() {
            if (0..24).contains(&h) {
                out[h as usize] = ms;
            }
        }
        Ok(out)
    }

    /// Time heard (ms) per weekday (index 0 = Sunday … 6 = Saturday, local).
    pub fn stats_by_weekday(&self, since: i64) -> Result<[i64; 7]> {
        let mut out = [0i64; 7];
        let mut stmt = self.conn.prepare(
            "SELECT CAST(strftime('%w', started_at, 'unixepoch', 'localtime') AS INTEGER),
                    SUM(played_ms)
             FROM play_event WHERE started_at >= ?1 GROUP BY 1",
        )?;
        let rows = stmt.query_map([since], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        for (d, ms) in rows.flatten() {
            if (0..7).contains(&d) {
                out[d as usize] = ms;
            }
        }
        Ok(out)
    }
}
