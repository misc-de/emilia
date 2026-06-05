//! Album metadata queries for [`Library`] (split out of db.rs).

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::{CategorySnapshot, Library};
use crate::model::*;

impl Library {
    /// Reads the online metadata for an album (if already looked up).
    pub fn get_album_meta(&self, artist: &str, album: &str) -> Result<Option<AlbumMeta>> {
        let meta = self
            .conn
            .query_row(
                "SELECT artist, album, mbid, cover_path, year, status
                 FROM album_meta WHERE artist = ?1 AND album = ?2",
                rusqlite::params![artist, album],
                Self::map_album_meta,
            )
            .optional()?;
        Ok(meta)
    }

    /// Any existing cover for an album name (across artists) --
    /// useful for single tracks whose album is known but was stored under a
    /// different artist credit.
    /// Distinct albums credited to exactly this artist (indexed via
    /// `idx_track_artist_album`). Used to find a fallback cover for an artist
    /// without a photo, without scanning the whole track table.
    pub fn albums_of_artist(&self, artist: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT album FROM track
             WHERE artist = ?1 AND album IS NOT NULL AND album <> ''
             ORDER BY album",
        )?;
        let rows = stmt.query_map([artist], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn album_cover(&self, album: &str) -> Result<Option<String>> {
        let cover = self
            .conn
            .query_row(
                "SELECT cover_path FROM album_meta
                 WHERE album = ?1 AND cover_path IS NOT NULL AND cover_path <> ''
                 LIMIT 1",
                [album],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(cover)
    }

    /// Stores/updates the online metadata of an album.
    pub fn upsert_album_meta(&self, m: &AlbumMeta) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO album_meta (artist, album, mbid, cover_path, year, status, fetched_at, attempts)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s','now'),
                    CASE WHEN ?4 IS NOT NULL AND ?4 <> '' THEN 0 ELSE 1 END)
            ON CONFLICT(artist, album) DO UPDATE SET
                mbid       = excluded.mbid,
                cover_path = excluded.cover_path,
                year       = excluded.year,
                status     = excluded.status,
                fetched_at = excluded.fetched_at,
                -- Reset the retry counter only when a cover was actually obtained.
                -- A bare "matched" *without* artwork (no front cover in the Cover
                -- Art Archive, or a failed/timed-out cover fetch) must still count
                -- as an attempt -- otherwise the album stays in
                -- `albums_missing_cover` and is re-queried every sweep forever,
                -- never reaching MAX_ATTEMPTS.
                attempts   = CASE WHEN excluded.cover_path IS NOT NULL AND excluded.cover_path <> '' THEN 0
                                  ELSE album_meta.attempts + 1 END
            "#,
            rusqlite::params![m.artist, m.album, m.mbid, m.cover_path, m.year, m.status],
        )?;
        Ok(())
    }

    /// Album overview for the UI: all unique albums from the library,
    /// enriched with (any available) online metadata and the track count.
    /// Sorted by album name (like the file view -- without artist groups).
    /// Reuses a pre-built category snapshot when one is passed (so a combined
    /// album+artist reload builds it only once), otherwise builds its own.
    pub(crate) fn albums_overview_with(
        &self,
        snap: Option<&CategorySnapshot>,
    ) -> Result<Vec<AlbumMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(t.artist, ''), t.album, m.mbid, m.cover_path, m.year,
                    COALESCE(m.status, 'pending'), COUNT(*)
             FROM track t
             LEFT JOIN album_meta m
                    ON m.artist = COALESCE(t.artist, '') AND m.album = t.album
             WHERE t.album IS NOT NULL AND t.album <> ''
             GROUP BY COALESCE(t.artist, ''), t.album
             ORDER BY t.album COLLATE NOCASE, t.artist COLLATE NOCASE",
        )?;
        let raw = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<i32>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Albums are merged **by album name alone** -- the
        // artist plays no role. Same-named tracks by different
        // artists (including "feat." variants) thus form exactly one card.
        // Display artist + cover come from the artist with the most
        // tracks on the album (gaps are filled from the rest).
        use std::collections::HashMap;
        // Per album key: statistics per primary artist (track count, cover,
        // year, MBID) for choosing the display artist/cover.
        type ArtistInfo = (i64, Option<String>, Option<i32>, Option<String>);
        let mut order: Vec<String> = Vec::new();
        let mut map: HashMap<String, AlbumMeta> = HashMap::new();
        let mut by_artist: HashMap<String, HashMap<String, ArtistInfo>> = HashMap::new();
        for (artist, album, mbid, cover, year, status, count) in raw {
            let key = album.to_lowercase();
            let entry = map.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                AlbumMeta {
                    artist: String::new(),
                    album: album.clone(),
                    mbid: None,
                    cover_path: None,
                    year: None,
                    status: "pending".to_string(),
                    track_count: 0,
                }
            });
            entry.track_count += count;
            if matches!(status.as_str(), "matched" | "local") {
                entry.status = status;
            }
            let primary = crate::core::artist::primary_artist(&artist);
            let slot = by_artist
                .entry(key)
                .or_default()
                .entry(primary)
                .or_insert((0, None, None, None));
            slot.0 += count;
            if slot.1.is_none() {
                slot.1 = cover;
            }
            if slot.2.is_none() {
                slot.2 = year;
            }
            if slot.3.is_none() {
                slot.3 = mbid;
            }
        }
        // Display artist = the one with the most tracks; prefer its
        // cover/year/MBID, fill missing fields from the remaining artists.
        for (key, meta) in map.iter_mut() {
            let Some(per) = by_artist.get(key) else {
                continue;
            };
            let mut artists: Vec<(&String, &ArtistInfo)> = per.iter().collect();
            artists.sort_by(|a, b| {
                b.1 .0
                    .cmp(&a.1 .0)
                    .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
            });
            // Display artist = the most frequent; year/MBID: first available.
            if let Some((name, _)) = artists.first() {
                meta.artist = (*name).clone();
            }
            for (_, info) in &artists {
                if meta.year.is_none() {
                    meta.year = info.2;
                }
                if meta.mbid.is_none() {
                    meta.mbid = info.3.clone();
                }
            }
            // Cover = the cover of the most representative artist (the list is
            // already sorted by track count, so the display artist comes first),
            // falling back to any member that has one. A representative image is
            // better than an empty placeholder — and the album detail shows the
            // same dominant artist's cover anyway, so the card matching it is
            // exactly what the user expects.
            meta.cover_path = artists.iter().find_map(|(_, i)| i.1.clone());
        }
        let mut out: Vec<AlbumMeta> = order.into_iter().filter_map(|k| map.remove(&k)).collect();
        // Properties: only show albums that are visible in the "Albums" area.
        // Resolve from one in-memory snapshot instead of querying per album.
        let owned_snap;
        let cats = match snap {
            Some(s) => s,
            None => {
                owned_snap = self.category_snapshot()?;
                &owned_snap
            }
        };
        out.retain(|a| {
            cats.album_areas(&a.artist, &a.album)
                .contains(&crate::core::category::Area::Albums)
        });
        out.sort_by_key(|a| a.album.to_lowercase());
        Ok(out)
    }

    fn map_album_meta(r: &rusqlite::Row<'_>) -> rusqlite::Result<AlbumMeta> {
        Ok(AlbumMeta {
            artist: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            album: r.get(1)?,
            mbid: r.get(2)?,
            cover_path: r.get(3)?,
            year: r.get(4)?,
            status: r.get(5)?,
            track_count: 0,
        })
    }

    /// Albums **without** a cover, each with a sample track path. Basis for the
    /// local cover extraction (embedded image) and the online gap filling.
    pub fn albums_missing_cover(&self) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(t.artist, ''), t.album, MIN(t.path)
             FROM track t
             LEFT JOIN album_meta m
                    ON m.artist = COALESCE(t.artist, '') AND m.album = t.album
             WHERE t.album IS NOT NULL AND t.album <> ''
               AND (m.cover_path IS NULL OR m.cover_path = '')
             GROUP BY COALESCE(t.artist, ''), t.album
             ORDER BY t.album COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// All track paths of an (artist, album) pair, ordered by path. Used by the
    /// local cover extraction: the `albums_missing_cover` sample is just
    /// `MIN(path)`, which may lack embedded art even though a sibling track on
    /// the same album carries one – then the whole list is scanned.
    pub fn album_track_paths(&self, artist: &str, album: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT path FROM track
             WHERE COALESCE(artist, '') = ?1 AND album = ?2
             ORDER BY path",
        )?;
        let rows = stmt.query_map([artist, album], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// All track paths of an album name, regardless of artist credit. Used as a
    /// UI cover fallback for the merged album overview, where same-named albums
    /// appear as one card.
    pub fn album_track_paths_by_name(&self, album: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT path FROM track
             WHERE album = ?1
             ORDER BY path",
        )?;
        let rows = stmt.query_map([album], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
