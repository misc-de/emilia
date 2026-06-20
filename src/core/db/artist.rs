//! Artist & track metadata queries for [`Library`] (split out of db.rs).

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::{CategorySnapshot, Library};
use crate::model::*;

impl Library {
    // ---- Artists ----

    /// Unique **individual** artists from the library. Composite
    /// entries ("A feat. B & C") are split into their artists
    /// (see [`crate::core::artist::split_artists`]) and deduplicated
    /// case-insensitively. Sorted alphabetically.
    pub fn distinct_artists(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT artist FROM track
             WHERE artist IS NOT NULL AND artist <> ''",
        )?;
        let raws = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for raw in &raws {
            for name in crate::core::artist::split_artists(raw) {
                if seen.insert(crate::core::artist::norm_key(&name)) {
                    out.push(name);
                }
            }
        }
        out.sort_by_key(|s| s.to_lowercase());
        Ok(out)
    }

    pub fn get_artist_meta(&self, name: &str) -> Result<Option<ArtistMeta>> {
        let meta = self
            .conn
            .query_row(
                "SELECT name, image_path, status FROM artist_meta WHERE name = ?1",
                [name],
                Self::map_artist_meta,
            )
            .optional()?;
        Ok(meta)
    }

    /// Set of artist names that currently have a non-empty photo path. Used to
    /// expose a per-artist `has_image` flag (one query, no N+1) without pulling
    /// full metadata for every artist.
    pub fn artist_image_names(&self) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT name FROM artist_meta WHERE image_path IS NOT NULL AND image_path <> ''",
        )?;
        let names = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
        Ok(names)
    }

    pub fn upsert_artist_meta(&self, m: &ArtistMeta) -> Result<()> {
        self.conn.execute(
            "INSERT INTO artist_meta (name, image_path, status, fetched_at, attempts)
             VALUES (?1, ?2, ?3, strftime('%s','now'),
                     CASE WHEN ?3 = 'matched' THEN 0 ELSE 1 END)
             ON CONFLICT(name) DO UPDATE SET
                image_path = excluded.image_path,
                status     = excluded.status,
                fetched_at = excluded.fetched_at,
                attempts   = CASE WHEN excluded.status = 'matched' THEN 0
                                  ELSE artist_meta.attempts + 1 END",
            rusqlite::params![m.name, m.image_path, m.status],
        )?;
        Ok(())
    }

    /// Artist overview for the UI: every individual artist -- including from
    /// "feat." entries -- with (any available) photo.
    /// Reuses a pre-built category snapshot when one is passed (so a combined
    /// album+artist reload builds it only once), otherwise builds its own.
    pub(crate) fn artists_overview_with(
        &self,
        snap: Option<&CategorySnapshot>,
    ) -> Result<Vec<ArtistMeta>> {
        let names = self.distinct_artists()?;
        // Resolve areas from one snapshot and pull all artist metadata in one
        // query, instead of two queries per artist (was a clear N+1).
        let owned_snap;
        let cats = match snap {
            Some(s) => s,
            None => {
                owned_snap = self.category_snapshot()?;
                &owned_snap
            }
        };
        let mut meta_by_name: std::collections::HashMap<String, ArtistMeta> =
            std::collections::HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT name, image_path, status FROM artist_meta")?;
            let rows = stmt.query_map([], Self::map_artist_meta)?;
            for m in rows.flatten() {
                meta_by_name.insert(m.name.clone(), m);
            }
        }
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            // Properties: only show those visible in the "Artists" area.
            if !cats
                .artist_areas(&name)
                .contains(&crate::core::category::Area::Artists)
            {
                continue;
            }
            let meta = meta_by_name
                .remove(&name)
                .unwrap_or_else(|| ArtistMeta::pending(&name));
            out.push(meta);
        }
        Ok(out)
    }

    /// Per-artist counts of distinct albums and of songs (tracks), keyed by the
    /// normalized artist key ([`crate::core::artist::norm_key`]). Composite
    /// credits ("A feat. B") are split so each individual artist is counted,
    /// matching [`Self::distinct_artists`]. One pass over the track table, so
    /// the artist overview can show counts without a query per row.
    pub fn artist_counts(&self) -> Result<std::collections::HashMap<String, (u32, u32)>> {
        use crate::core::artist::{norm_key, split_artists};
        let mut stmt = self
            .conn
            .prepare("SELECT artist, album FROM track WHERE artist IS NOT NULL AND artist <> ''")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        // norm_key -> (distinct album names, song count)
        let mut acc: std::collections::HashMap<String, (std::collections::HashSet<String>, u32)> =
            std::collections::HashMap::new();
        for (artist, album) in rows.flatten() {
            for name in split_artists(&artist) {
                let entry = acc.entry(norm_key(&name)).or_default();
                entry.1 += 1;
                if let Some(al) = album.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    entry.0.insert(al.to_lowercase());
                }
            }
        }
        Ok(acc
            .into_iter()
            .map(|(k, (albums, songs))| (k, (albums.len() as u32, songs)))
            .collect())
    }

    /// One artist's tallies: distinct albums, song count, and total track
    /// runtime (ms). Counts composite credits ("A feat. B" contributes to both
    /// A and B), matching [`Self::distinct_artists`]/[`Self::artist_counts`], so
    /// a collaboration still counts toward the named artist. `name` is matched
    /// case-insensitively via [`crate::core::artist::norm_key`]. Returns zeros
    /// for an unknown artist.
    pub fn artist_summary(&self, name: &str) -> Result<(u32, u32, i64)> {
        use crate::core::artist::{norm_key, split_artists};
        let key = norm_key(name);
        let mut stmt = self.conn.prepare(
            "SELECT artist, album, duration_ms FROM track
             WHERE artist IS NOT NULL AND artist <> ''",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        })?;
        let mut albums = std::collections::HashSet::new();
        let mut songs = 0u32;
        let mut duration_ms = 0i64;
        for (artist, album, dur) in rows.flatten() {
            if split_artists(&artist).iter().any(|n| norm_key(n) == key) {
                songs += 1;
                duration_ms += dur.unwrap_or(0);
                if let Some(al) = album.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    albums.insert(al.to_lowercase());
                }
            }
        }
        Ok((albums.len() as u32, songs, duration_ms))
    }

    fn map_artist_meta(r: &rusqlite::Row<'_>) -> rusqlite::Result<ArtistMeta> {
        Ok(ArtistMeta {
            name: r.get(0)?,
            image_path: r.get(1)?,
            status: r.get(2)?,
        })
    }

    // ---- Fingerprint recognition (AcoustID) ----

    pub fn get_track_meta(&self, path: &str) -> Result<Option<TrackMeta>> {
        let meta = self
            .conn
            .query_row(
                "SELECT path, recording_mbid, title, artist, album, status
                 FROM track_meta WHERE path = ?1",
                [path],
                Self::map_track_meta,
            )
            .optional()?;
        Ok(meta)
    }

    pub fn upsert_track_meta(&self, m: &TrackMeta) -> Result<()> {
        self.conn.execute(
            "INSERT INTO track_meta
                (path, recording_mbid, title, artist, album, status, fetched_at, attempts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s','now'),
                     CASE WHEN ?6 = 'matched' THEN 0 ELSE 1 END)
             ON CONFLICT(path) DO UPDATE SET
                recording_mbid = excluded.recording_mbid,
                title          = excluded.title,
                artist         = excluded.artist,
                album          = excluded.album,
                status         = excluded.status,
                fetched_at     = excluded.fetched_at,
                attempts       = CASE WHEN excluded.status = 'matched' THEN 0
                                      ELSE track_meta.attempts + 1 END",
            rusqlite::params![
                m.path,
                m.recording_mbid,
                m.title,
                m.artist,
                m.album,
                m.status
            ],
        )?;
        Ok(())
    }

    fn map_track_meta(r: &rusqlite::Row<'_>) -> rusqlite::Result<TrackMeta> {
        Ok(TrackMeta {
            path: r.get(0)?,
            recording_mbid: r.get(1)?,
            title: r.get(2)?,
            artist: r.get(3)?,
            album: r.get(4)?,
            status: r.get(5)?,
        })
    }
}
