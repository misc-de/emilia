//! Library search for [`Library`] (split out of db.rs).

use anyhow::Result;

use super::{like_escape, Library};
use crate::model::{AlbumHit, SearchResults, SongHit};

impl Library {
    /// Library search for the title-bar search field. Matches artists, albums
    /// and songs against `query` (case-insensitive substring); a numeric query
    /// additionally matches an album's release year (from the online metadata,
    /// `album_meta` – the "date" dimension lives at album/meta level, not on the
    /// files). Each group is capped at `limit` rows.
    pub fn search_library(&self, query: &str, limit: usize) -> Result<SearchResults> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(SearchResults::default());
        }
        let like = format!("%{}%", like_escape(q));
        // A purely numeric query is also treated as a year for the album match.
        let year: Option<i64> = q.parse::<i64>().ok().filter(|y| (1000..=9999).contains(y));
        let lim = limit as i64;
        // Over-fetch so that dropping hidden items below still leaves a full page
        // of visible results in the common case (few/no hidden matches).
        let fetch = lim.saturating_mul(4).max(lim);

        // --- Artists (Interpreten) ---
        let mut artists = Vec::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT artist FROM track
                 WHERE artist IS NOT NULL AND TRIM(artist) <> ''
                   AND artist LIKE ?1 ESCAPE '\\'
                 ORDER BY artist COLLATE NOCASE
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![like, fetch], |r| r.get::<_, String>(0))?;
            for a in rows {
                artists.push(a?);
            }
        }

        // --- Albums (name match, or year match for a numeric query) ---
        let mut albums = Vec::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT t.album, MIN(t.artist), MAX(m.year)
                 FROM track t
                 LEFT JOIN album_meta m ON m.album = t.album
                 WHERE t.album IS NOT NULL AND TRIM(t.album) <> ''
                   AND (t.album LIKE ?1 ESCAPE '\\'
                        OR (?2 IS NOT NULL AND m.year = ?2))
                 GROUP BY t.album COLLATE NOCASE
                 ORDER BY t.album COLLATE NOCASE
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(rusqlite::params![like, year, fetch], |r| {
                Ok(AlbumHit {
                    album: r.get(0)?,
                    artist: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    year: r.get::<_, Option<i64>>(2)?.map(|y| y as i32),
                })
            })?;
            for a in rows {
                albums.push(a?);
            }
        }

        // --- Songs (title match) ---
        let mut songs = Vec::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT path, title, artist, album
                 FROM track
                 WHERE title LIKE ?1 ESCAPE '\\'
                 ORDER BY title COLLATE NOCASE
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![like, fetch], |r| {
                Ok(SongHit {
                    path: r.get(0)?,
                    title: r.get(1)?,
                    artist: r.get(2)?,
                    album: r.get(3)?,
                })
            })?;
            for s in rows {
                songs.push(s?);
            }
        }

        // Hidden content (empty effective areas) must not surface in search — only
        // the Settings "hidden content" manager lists it. Resolve artists/albums
        // through one shared snapshot (kind-aware, O(1) per item) so dropping
        // hidden hits doesn't run a classification scan per result; tracks use the
        // cheap per-row resolution. Then trim back to the page size.
        //
        // Album hits are also *split* by their most specific resolved area into the
        // same categories the navigation uses, so a single/compilation/concert/
        // audiobook surfaces (and later opens) under its own heading instead of
        // pooling into a generic "Albums" list. The snapshot already does the
        // kind-aware + folder-inherited resolution, so this is free here.
        use crate::core::category::Area;
        let mut singles = Vec::new();
        let mut compilations = Vec::new();
        let mut concerts = Vec::new();
        let mut audiobooks = Vec::new();
        if let Ok(snap) = self.category_snapshot() {
            artists.retain(|a| !snap.artist_areas(a).is_empty());
            let mut plain = Vec::new();
            for a in albums {
                let areas = snap.album_areas(&a.artist, &a.album);
                if areas.is_empty() {
                    continue; // hidden
                }
                // Most specific wins: a concert that is *also* filed under Albums
                // belongs in Concerts here, never in both.
                let bucket = if areas.contains(&Area::Audiobooks) {
                    &mut audiobooks
                } else if areas.contains(&Area::Concerts) {
                    &mut concerts
                } else if areas.contains(&Area::Compilations) {
                    &mut compilations
                } else if areas.contains(&Area::Singles) {
                    &mut singles
                } else {
                    &mut plain
                };
                bucket.push(a);
            }
            albums = plain;
        }
        songs.retain(|s| {
            !self
                .resolve_areas(s.artist.as_deref(), s.album.as_deref(), &s.path)
                .is_empty()
        });
        artists.truncate(limit);
        albums.truncate(limit);
        singles.truncate(limit);
        compilations.truncate(limit);
        concerts.truncate(limit);
        audiobooks.truncate(limit);
        songs.truncate(limit);

        // --- The user's own local collections: timeshift recordings and voice
        //     memos. These lists are personal and small, so they are filtered in
        //     memory (case-insensitive substring) rather than via separate SQL.
        //     Streaming stations and YouTube content are intentionally not part of
        //     the library search (they have their own dedicated sections). ---
        let ql = q.to_lowercase();
        let hit = |s: &str| s.to_lowercase().contains(&ql);
        let ohit =
            |s: &Option<String>| s.as_deref().is_some_and(|x| x.to_lowercase().contains(&ql));

        let recordings: Vec<_> = self
            .recordings()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| hit(&r.title) || ohit(&r.artist) || ohit(&r.station))
            .take(limit)
            .collect();
        let memos: Vec<_> = self
            .memos()
            .unwrap_or_default()
            .into_iter()
            .filter(|m| hit(&m.title))
            .take(limit)
            .collect();

        Ok(SearchResults {
            artists,
            albums,
            singles,
            compilations,
            concerts,
            audiobooks,
            songs,
            recordings,
            memos,
        })
    }
}
