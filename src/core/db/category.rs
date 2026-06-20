//! Category / attribute queries for [`Library`] (split out of db.rs).

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::{CategorySnapshot, Library};

impl Library {
    // ---- Attributes (category with inheritance) ----

    /// Sets (or, with `None`, deletes) the setting of a level.
    /// `scope` ∈ {`artist`,`album`,`track`}.
    pub fn set_category(&self, scope: &str, key: &str, value: Option<&str>) -> Result<()> {
        match value {
            Some(v) => self.conn.execute(
                "INSERT INTO category (scope, key, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
                rusqlite::params![scope, key, v],
            )?,
            None => self.conn.execute(
                "DELETE FROM category WHERE scope = ?1 AND key = ?2",
                rusqlite::params![scope, key],
            )?,
        };
        Ok(())
    }

    /// Reads the setting of a single level (without inheritance).
    pub fn get_category(&self, scope: &str, key: &str) -> Result<Option<String>> {
        let v = self
            .conn
            .query_row(
                "SELECT value FROM category WHERE scope = ?1 AND key = ?2",
                rusqlite::params![scope, key],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(v)
    }

    /// All stored category settings (for the device synchronization).
    pub fn all_categories(&self) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT scope, key, value FROM category")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Effective **areas** of a track (most specific level wins:
    /// track → album → artist → default). Empty list = hidden.
    pub fn resolve_areas(
        &self,
        artist: Option<&str>,
        album: Option<&str>,
        path: &str,
    ) -> Vec<crate::core::category::Area> {
        use crate::core::category::{album_key, parse_areas, Area};
        if let Ok(Some(v)) = self.get_category("track", path) {
            return parse_areas(&v);
        }
        if let Some(album) = album {
            if let Ok(Some(v)) = self.get_category("album", &album_key(artist.unwrap_or(""), album))
            {
                return parse_areas(&v);
            }
        }
        if let Some(artist) = artist {
            if let Ok(Some(v)) = self.get_category("artist", artist) {
                return parse_areas(&v);
            }
        }
        // Folder chain: from the file's directory upwards (deepest setting wins).
        let mut dir = std::path::Path::new(path).parent();
        while let Some(d) = dir {
            if let Ok(Some(v)) = self.get_category("folder", &d.to_string_lossy()) {
                return parse_areas(&v);
            }
            dir = d.parent();
        }
        Area::DEFAULT.to_vec()
    }

    /// Effective areas of a folder (this folder upwards → default).
    pub fn folder_areas(&self, folder: &str) -> Vec<crate::core::category::Area> {
        use crate::core::category::{parse_areas, Area};
        let mut dir = Some(std::path::Path::new(folder));
        while let Some(d) = dir {
            if let Ok(Some(v)) = self.get_category("folder", &d.to_string_lossy()) {
                return parse_areas(&v);
            }
            dir = d.parent();
        }
        Area::DEFAULT.to_vec()
    }

    /// Effective areas of an album: album → artist → **parent
    /// folder** (of a sample track, upwards) → default. This way an album
    /// without its own setting inherits the setting of a parent folder
    /// (non-destructive -- its own setting still wins).
    pub fn album_areas(&self, artist: &str, album: &str) -> Vec<crate::core::category::Area> {
        use crate::core::category::{album_key, parse_areas};
        if let Ok(Some(v)) = self.get_category("album", &album_key(artist, album)) {
            return parse_areas(&v);
        }
        if let Ok(Some(v)) = self.get_category("artist", artist) {
            return parse_areas(&v);
        }
        if let Some(path) = self.album_sample_path(artist, album) {
            let mut dir = std::path::Path::new(&path).parent();
            while let Some(d) = dir {
                if let Ok(Some(v)) = self.get_category("folder", &d.to_string_lossy()) {
                    return parse_areas(&v);
                }
                dir = d.parent();
            }
        }
        self.album_kind_default(album)
    }

    /// Default areas for an album with no explicit/inherited setting, augmented
    /// by its [`AlbumKind`] classification so an (auto-classified) single or
    /// compilation appears in those areas without a stored override. Mirrors
    /// [`CategorySnapshot`]'s kind-aware default; used for single lookups (e.g.
    /// the "Available in" detail group) where no snapshot is at hand.
    fn album_kind_default(&self, album: &str) -> Vec<crate::core::category::Area> {
        use crate::core::category::Area;
        use crate::model::AlbumKind;
        let lc = album.to_lowercase();
        let in_kind = |kind| {
            self.albums_classified(kind)
                .map(|v| v.iter().any(|c| c.album.to_lowercase() == lc))
                .unwrap_or(false)
        };
        let mut areas = Area::DEFAULT.to_vec();
        if in_kind(AlbumKind::Compilation) {
            areas.push(Area::Compilations);
        } else if in_kind(AlbumKind::Single) {
            areas.push(Area::Singles);
        }
        areas
    }

    /// Path of a track of *this* artist's album (for folder inheritance).
    /// Filtering by artist matters: two artists can share an album name but live
    /// in different folders, and the wrong folder would inherit the wrong areas.
    fn album_sample_path(&self, artist: &str, album: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT path FROM track WHERE COALESCE(artist, '') = ?1 AND album = ?2 LIMIT 1",
                rusqlite::params![artist, album],
                |r| r.get::<_, String>(0),
            )
            .ok()
    }

    /// Effective areas of an artist (artist → default).
    pub fn artist_areas(&self, name: &str) -> Vec<crate::core::category::Area> {
        use crate::core::category::{parse_areas, Area};
        if let Ok(Some(v)) = self.get_category("artist", name) {
            return parse_areas(&v);
        }
        Area::DEFAULT.to_vec()
    }

    /// Loads the whole (tiny) `category` table plus one sample track path per
    /// `(artist, album)` into memory, so the overviews can resolve the areas of
    /// thousands of albums/artists without a query per item (was a clear N+1).
    /// The resolution in [`CategorySnapshot`] mirrors [`Self::album_areas`] /
    /// [`Self::artist_areas`] exactly.
    pub(crate) fn category_snapshot(&self) -> Result<CategorySnapshot> {
        use crate::core::category::parse_areas;
        use std::collections::HashMap;

        let mut map: HashMap<(String, String), Vec<crate::core::category::Area>> = HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT scope, key, value FROM category")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for (scope, key, value) in rows.flatten() {
                map.insert((scope, key), parse_areas(&value));
            }
        }

        let mut sample: HashMap<(String, String), String> = HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT COALESCE(artist, ''), album, MIN(path) FROM track
                 WHERE album IS NOT NULL AND album <> ''
                 GROUP BY COALESCE(artist, ''), album",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for (artist, album, path) in rows.flatten() {
                sample.insert((artist, album), path);
            }
        }

        // Album-name sets for the kind-aware default: an album with no explicit
        // category setting still surfaces in the Singles/Compilations areas based
        // on its classification (same source of truth as those tabs).
        use crate::model::AlbumKind;
        let names = |kind| -> std::collections::HashSet<String> {
            self.albums_classified(kind)
                .map(|v| v.into_iter().map(|c| c.album.to_lowercase()).collect())
                .unwrap_or_default()
        };
        let single_names = names(AlbumKind::Single);
        let comp_names = names(AlbumKind::Compilation);

        Ok(CategorySnapshot {
            map,
            sample,
            single_names,
            comp_names,
        })
    }
}
