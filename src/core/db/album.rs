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

    /// Distinct albums with their release year, optionally narrowed by `artist`
    /// and/or an inclusive `[year_from, year_to]` range. The album's year is the
    /// **earliest** `track.year` among its tracks — robust against reissue tags
    /// (a 1991 remaster of a 1984 album still reports 1984), matching the
    /// library's metadata date-sort convention.
    ///
    /// A year bound only matches a *known* year, so it drops albums whose tracks
    /// carry no year tag. Without any year filter the result is ordered by artist
    /// then album; with one it is ordered chronologically (year, then album).
    /// Returns `(artist, album, year)`; `year` is `None` only for an unfiltered
    /// album that has no tagged year.
    pub fn albums_with_year(
        &self,
        artist: Option<&str>,
        year_from: Option<i64>,
        year_to: Option<i64>,
    ) -> Result<Vec<(String, String, Option<i64>)>> {
        // Each `?n` placeholder is referenced twice (the `IS NULL OR …` guard),
        // which lets one bound parameter switch a filter on or off without
        // rebuilding the SQL string. `MIN(year)` ignores NULLs, so an album keeps
        // its earliest tagged year even when some tracks are untagged.
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(artist, '') AS a, album, MIN(year) AS y
               FROM track
              WHERE album IS NOT NULL AND album <> ''
                AND (?1 IS NULL OR artist = ?1)
              GROUP BY a, album
             HAVING (?2 IS NULL OR y >= ?2)
                AND (?3 IS NULL OR y <= ?3)
              ORDER BY CASE WHEN ?2 IS NULL AND ?3 IS NULL THEN NULL ELSE y END, a, album",
        )?;
        let rows = stmt.query_map(rusqlite::params![artist, year_from, year_to], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One album's tallies: track count, total runtime (ms), and release year
    /// (earliest tagged track year — see [`Self::albums_with_year`]). `album` is
    /// matched case-insensitively; pass `artist` to disambiguate a name shared
    /// across artists (e.g. "Greatest Hits"), otherwise all tracks under that
    /// album name are aggregated. Returns `(0, 0, None)` for an unknown album.
    pub fn album_summary(
        &self,
        artist: Option<&str>,
        album: &str,
    ) -> Result<(u32, i64, Option<i64>)> {
        let (count, dur, year): (i64, i64, Option<i64>) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(duration_ms), 0), MIN(year)
               FROM track
              WHERE album = ?1 COLLATE NOCASE
                AND (?2 IS NULL OR artist = ?2)",
            rusqlite::params![album, artist],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok((count as u32, dur, year))
    }

    /// Force an album's classification (by album name, case-insensitive),
    /// overriding the heuristic. See [`Self::albums_classified`].
    pub fn set_album_kind(&self, album: &str, kind: AlbumKind) -> Result<()> {
        self.conn.execute(
            "INSERT INTO album_kind (album, kind) VALUES (?1, ?2)
             ON CONFLICT(album) DO UPDATE SET kind = excluded.kind",
            rusqlite::params![album.to_lowercase(), kind.as_str()],
        )?;
        Ok(())
    }

    /// Drop a manual album-type override, reverting to the heuristic.
    pub fn clear_album_kind(&self, album: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM album_kind WHERE album = ?1",
            [album.to_lowercase()],
        )?;
        Ok(())
    }

    /// The manual override for an album name, or `None` when it follows the
    /// heuristic ("automatic"). For the in-app type switch.
    pub fn album_kind_override(&self, album: &str) -> Option<AlbumKind> {
        self.conn
            .query_row(
                "SELECT kind FROM album_kind WHERE album = ?1",
                [album.to_lowercase()],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|k| AlbumKind::from_str(&k))
    }

    /// All manual album-type overrides, keyed by lowercased album name.
    fn album_kind_overrides(&self) -> Result<std::collections::HashMap<String, AlbumKind>> {
        let mut stmt = self.conn.prepare("SELECT album, kind FROM album_kind")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = std::collections::HashMap::new();
        for (album, kind) in rows.flatten() {
            if let Some(k) = AlbumKind::from_str(&kind) {
                out.insert(album, k);
            }
        }
        Ok(out)
    }

    /// Albums of one [`AlbumKind`], derived heuristically (the library stores no
    /// album-type tags). A **compilation** is an album name whose tracks carry
    /// more than one *primary* artist with none dominant — so a solo album with
    /// "feat." guests ("Beginner feat. …") is **not** a compilation; a **single**
    /// is a one-artist album with at most `SINGLE_MAX` tracks that is not part of
    /// a compilation; everything else is a regular album. Compilations come back
    /// merged per name (artist = "Various Artists"); singles/albums per
    /// artist+album. Sorted by album, then artist. A manual override
    /// ([`Self::set_album_kind`]) wins over the heuristic.
    pub fn albums_classified(&self, kind: AlbumKind) -> Result<Vec<ClassifiedAlbum>> {
        use crate::core::artist::{norm_key, primary_artist};
        const SINGLE_MAX: u32 = 3;

        let overrides = self.album_kind_overrides()?;

        /// Minimum of two optional years (a missing year never wins).
        fn min_year(a: Option<i64>, b: Option<i64>) -> Option<i64> {
            match (a, b) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (None, y) => y,
                (x, None) => x,
            }
        }

        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(artist, ''), album, year FROM track
             WHERE album IS NOT NULL AND album <> ''",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        })?;

        // album-name (lowercased) -> (primary artist -> track count). Drives the
        // compilation test: several primaries *and* none of them dominant.
        let mut name_primaries: std::collections::HashMap<
            String,
            std::collections::HashMap<String, u32>,
        > = std::collections::HashMap::new();
        // (artist, album) -> (track count, earliest year), for singles/albums.
        let mut pairs: std::collections::BTreeMap<(String, String), (u32, Option<i64>)> =
            std::collections::BTreeMap::new();
        // album-name (lowercased) -> (display name, track count, earliest year),
        // merged across artists, for compilations.
        let mut name_agg: std::collections::BTreeMap<String, (String, u32, Option<i64>)> =
            std::collections::BTreeMap::new();

        for (artist, album, year) in rows.flatten() {
            let key = album.to_lowercase();
            let primary = primary_artist(&artist);
            *name_primaries
                .entry(key.clone())
                .or_default()
                .entry(norm_key(&primary))
                .or_insert(0) += 1;
            // Group by *primary* artist so a "feat." track stays with its album
            // instead of splitting the count off into a phantom single.
            let p = pairs.entry((primary, album.clone())).or_insert((0, None));
            p.0 += 1;
            p.1 = min_year(p.1, year);
            let na = name_agg.entry(key).or_insert((album, 0, None));
            na.1 += 1;
            na.2 = min_year(na.2, year);
        }

        // A manual override wins; otherwise a genuine compilation has several
        // primary artists where none owns the bulk of the tracks (if one
        // dominates ≥70%, it is that artist's album with guests).
        const DOMINANCE: f64 = 0.7;
        let is_comp = |key: &str| match overrides.get(key) {
            Some(ov) => *ov == AlbumKind::Compilation,
            None => name_primaries.get(key).is_some_and(|m| {
                if m.len() <= 1 {
                    return false;
                }
                let total: u32 = m.values().sum();
                let max = m.values().copied().max().unwrap_or(0);
                (max as f64) < DOMINANCE * (total as f64)
            }),
        };

        let mut out = Vec::new();
        match kind {
            AlbumKind::Compilation => {
                for (key, (display, tracks, year)) in &name_agg {
                    if is_comp(key) {
                        out.push(ClassifiedAlbum {
                            artist: "Various Artists".to_string(),
                            album: display.clone(),
                            year: *year,
                            tracks: *tracks,
                            kind: AlbumKind::Compilation,
                        });
                    }
                }
            }
            AlbumKind::Single | AlbumKind::Album => {
                for ((artist, album), (tracks, year)) in &pairs {
                    let akey = album.to_lowercase();
                    if is_comp(&akey) {
                        continue; // these tracks belong to a compilation
                    }
                    // A manual override forces single/album regardless of count.
                    let this = match overrides.get(&akey) {
                        Some(ov) => *ov,
                        None if *tracks <= SINGLE_MAX => AlbumKind::Single,
                        None => AlbumKind::Album,
                    };
                    if this == kind {
                        out.push(ClassifiedAlbum {
                            artist: artist.clone(),
                            album: album.clone(),
                            year: *year,
                            tracks: *tracks,
                            kind,
                        });
                    }
                }
            }
        }
        out.sort_by(|a, b| {
            a.album
                .to_lowercase()
                .cmp(&b.album.to_lowercase())
                .then_with(|| a.artist.to_lowercase().cmp(&b.artist.to_lowercase()))
        });
        Ok(out)
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

    /// Map of lowercased album name -> a stored cover path (any artist credit),
    /// fetched in one query. Lets the album overview resolve every album's cover
    /// from one scan instead of an `album_cover` lookup per coverless album.
    pub fn album_meta_covers(&self) -> Result<std::collections::HashMap<String, String>> {
        let mut stmt = self.conn.prepare(
            "SELECT LOWER(album), cover_path FROM album_meta
             WHERE cover_path IS NOT NULL AND cover_path <> ''",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut map = std::collections::HashMap::new();
        for (album, cover) in rows.flatten() {
            map.entry(album).or_insert(cover);
        }
        Ok(map)
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
                    COALESCE(m.status, 'pending'), COUNT(*), SUM(t.duration_ms), MIN(t.year)
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
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<i32>>(8)?,
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
        // Per-album year from the file tags — the **earliest** across the
        // album's tracks (the original release year; later-tagged reissue/bonus
        // tracks must not win). Preferred over the online match below, which can
        // return a reissue/remaster year (e.g. a 1996 album coming back as 2015).
        let mut tag_years: HashMap<String, Option<i32>> = HashMap::new();
        for (artist, album, mbid, cover, year, status, count, duration, tag_year) in raw {
            let key = album.to_lowercase();
            if let Some(ty) = tag_year {
                let slot = tag_years.entry(key.clone()).or_insert(None);
                *slot = Some(slot.map_or(ty, |e| e.min(ty)));
            }
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
                    total_duration_ms: None,
                }
            });
            entry.track_count += count;
            if let Some(ms) = duration {
                entry.total_duration_ms = Some(entry.total_duration_ms.unwrap_or(0) + ms);
            }
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
            // Display artist = the most frequent; MBID: first available.
            if let Some((name, _)) = artists.first() {
                meta.artist = (*name).clone();
            }
            for (_, info) in &artists {
                if meta.mbid.is_none() {
                    meta.mbid = info.3.clone();
                }
            }
            // Year: the embedded tag year (the user's own metadata = the original
            // release year) wins; only when no track carries a year do we fall
            // back to the online match. This is deliberately metadata-first so a
            // wrong online reissue year can't override a correctly tagged album.
            meta.year = tag_years
                .get(key)
                .copied()
                .flatten()
                .or_else(|| artists.iter().find_map(|(_, info)| info.2));
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

    /// The album overview restricted to one [`AlbumKind`], for the Singles /
    /// Compilations pages. Reuses [`Self::albums_overview_with`] (covers,
    /// durations, display artist, `Area::Albums` visibility) and keeps only the
    /// albums the classification assigns to `kind` — so these pages are extra
    /// filtered views; the main "Albums" page is unchanged.
    pub(crate) fn albums_overview_by_kind(
        &self,
        kind: AlbumKind,
        snap: Option<&CategorySnapshot>,
    ) -> Result<Vec<AlbumMeta>> {
        let names: std::collections::HashSet<String> = self
            .albums_classified(kind)?
            .into_iter()
            .map(|c| c.album.to_lowercase())
            .collect();
        Ok(self
            .albums_overview_with(snap)?
            .into_iter()
            .filter(|m| names.contains(&m.album.to_lowercase()))
            .collect())
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
            total_duration_ms: None,
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

    /// Albums (with tracks) that still have no release year and have not yet
    /// exhausted the year-lookup attempts (≤ 3). For the background year
    /// backfill — independent of the cover state.
    pub fn albums_missing_year(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(t.artist, ''), t.album
             FROM track t
             LEFT JOIN album_meta m
                    ON m.artist = COALESCE(t.artist, '') AND m.album = t.album
             WHERE t.album IS NOT NULL AND t.album <> ''
               AND m.year IS NULL
               AND COALESCE(m.year_attempts, 0) < 3
             GROUP BY COALESCE(t.artist, ''), t.album
             ORDER BY t.album COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Records a release-year backfill result: stores the `year` when found and
    /// always bumps `year_attempts` (so an unfindable year is not retried
    /// forever). Preserves any existing cover/mbid/status.
    pub fn set_album_year(
        &self,
        artist: &str,
        album: &str,
        year: Option<i32>,
        mbid: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO album_meta (artist, album, year, mbid, status, fetched_at, year_attempts)
             VALUES (?1, ?2, ?3, ?4, 'pending', strftime('%s','now'), 1)
             ON CONFLICT(artist, album) DO UPDATE SET
                 year          = COALESCE(excluded.year, album_meta.year),
                 mbid          = COALESCE(album_meta.mbid, excluded.mbid),
                 year_attempts = album_meta.year_attempts + 1,
                 fetched_at    = excluded.fetched_at",
            rusqlite::params![artist, album, year, mbid],
        )?;
        Ok(())
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
