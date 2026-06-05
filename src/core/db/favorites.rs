//! Favorites & area-entry queries for [`Library`] (split out of db.rs).

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::{file_name_of, file_stem_of, Library};

impl Library {
    // ---- Favorites ----

    /// Sets/removes a favorite (star). `scope` ∈ {track,folder,album,artist}.
    pub fn set_favorite(
        &self,
        scope: &str,
        key: &str,
        title: &str,
        is_dir: bool,
        on: bool,
    ) -> Result<()> {
        if on {
            // Sort new favorites to the end (max pos + 1).
            let next_pos: i64 = self
                .conn
                .query_row("SELECT COALESCE(MAX(pos), -1) + 1 FROM favorite", [], |r| {
                    r.get(0)
                })
                .unwrap_or(0);
            self.conn.execute(
                "INSERT INTO favorite (scope, key, title, is_dir, added_at, pos)
                 VALUES (?1, ?2, ?3, ?4, strftime('%s','now'), ?5)
                 ON CONFLICT(scope, key) DO UPDATE SET title = excluded.title",
                rusqlite::params![scope, key, title, is_dir as i64, next_pos],
            )?;
        } else {
            self.conn.execute(
                "DELETE FROM favorite WHERE scope = ?1 AND key = ?2",
                rusqlite::params![scope, key],
            )?;
        }
        Ok(())
    }

    /// Whether a level is marked as a favorite.
    pub fn is_favorite(&self, scope: &str, key: &str) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM favorite WHERE scope = ?1 AND key = ?2",
                rusqlite::params![scope, key],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    /// All favorites (scope, key, title, is_dir) in stored order.
    pub fn favorites(&self) -> Result<Vec<(String, String, String, bool)>> {
        let mut stmt = self.conn.prepare(
            "SELECT scope, key, title, is_dir FROM favorite ORDER BY pos, added_at, title",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)? != 0,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Stores the order of the favorites (pos = index in `ordered`).
    pub fn set_favorite_order(&self, ordered: &[(String, String)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt =
                tx.prepare_cached("UPDATE favorite SET pos = ?1 WHERE scope = ?2 AND key = ?3")?;
            for (i, (scope, key)) in ordered.iter().enumerate() {
                stmt.execute(rusqlite::params![i as i64, scope, key])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---- Area entries (concerts / audiobooks from the properties) ----

    /// Content whose properties contain the area `area` -- derived **live** from
    /// the settings (category). Returns per entry
    /// `(scope, key, title, is_dir)` (the same form as favorites), so that
    /// playback/detail/cover can be resolved uniformly.
    ///
    /// `include_folders`/`include_artists` control whether folder or artist
    /// settings are included (e.g. audiobooks: without folders, with
    /// artists/composers).
    pub fn area_entries(
        &self,
        area: crate::core::category::Area,
        include_folders: bool,
        include_artists: bool,
    ) -> Vec<(String, String, String, bool)> {
        self.category_entries(
            |areas| areas.contains(&area),
            include_folders,
            include_artists,
        )
    }

    /// All **hidden** content (empty area list) -- each the
    /// object that carries the setting (artist/album/track/folder). Basis
    /// for the "Hidden" overview.
    pub fn hidden_entries(&self) -> Vec<(String, String, String, bool)> {
        self.category_entries(|areas| areas.is_empty(), true, true)
    }

    /// Returns `(scope, key, title, is_dir)` for each setting whose area list
    /// satisfies the predicate. `include_folders`/`include_artists` control whether
    /// folder or artist levels are included.
    fn category_entries(
        &self,
        keep: impl Fn(&[crate::core::category::Area]) -> bool,
        include_folders: bool,
        include_artists: bool,
    ) -> Vec<(String, String, String, bool)> {
        use crate::core::category::parse_areas;
        let Ok(mut stmt) = self.conn.prepare("SELECT scope, key, value FROM category") else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        }) else {
            return Vec::new();
        };

        let mut seen = std::collections::HashSet::new();
        let mut out: Vec<(String, String, String, bool)> = Vec::new();
        for row in rows.flatten() {
            let (scope, key, value) = row;
            if !keep(&parse_areas(&value)) {
                continue;
            }
            let entry = match scope.as_str() {
                "track" => {
                    let title = self
                        .track_by_path(&key)
                        .ok()
                        .flatten()
                        .map(|t| t.title)
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| file_stem_of(&key));
                    Some(("track", title, false))
                }
                "album" => {
                    // key = "artist\1album" → title = album name.
                    let album = key
                        .split_once('\u{1}')
                        .map(|x| x.1)
                        .unwrap_or("")
                        .to_string();
                    Some(("album", album, false))
                }
                "folder" if include_folders => Some(("folder", file_name_of(&key), true)),
                "artist" if include_artists => Some(("artist", key.clone(), false)),
                _ => None,
            };
            if let Some((scope, title, is_dir)) = entry {
                if seen.insert((scope, key.clone())) {
                    out.push((scope.to_string(), key, title, is_dir));
                }
            }
        }
        out.sort_by_key(|a| a.2.to_lowercase());
        out
    }
}
