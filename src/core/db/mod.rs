//! SQLite library index (rusqlite).

use anyhow::Result;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Once;
use std::time::Duration;

// Further `impl Library` blocks, split out of this file by concern.
mod album;
mod artist;
mod attempts;
mod category;
mod eq;
mod favorites;
mod gallery;
mod heard;
mod lyrics;
mod memo;
mod migrate;
mod playlist;
mod podcast;
mod prune;
mod search;
mod settings;
mod source;
mod stats;
mod stream;
mod track;
mod youtube;

/// Database location: `$XDG_DATA_HOME/emilia/library.db`.
pub fn db_path() -> PathBuf {
    let mut dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.push("emilia");
    let _ = std::fs::create_dir_all(&dir);
    dir.push("library.db");
    dir
}

pub struct Library {
    conn: Connection,
}

/// Escapes the LIKE metacharacters `\ % _` so an arbitrary (user-chosen) path
/// can be used as a literal prefix in a `LIKE … ESCAPE '\'` pattern.
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// In-memory snapshot of the `category` table (+ one sample track path per
/// `(artist, album)`) for resolving the areas of many items at once. Built by
/// [`Library::category_snapshot`]. Resolution mirrors the per-item
/// [`Library::album_areas`] / [`Library::artist_areas`].
pub(crate) struct CategorySnapshot {
    map: std::collections::HashMap<(String, String), Vec<crate::core::category::Area>>,
    sample: std::collections::HashMap<(String, String), String>,
    /// Lowercased album names classified (auto or overridden) as singles /
    /// compilations. Drives the kind-aware default below, so an album with no
    /// explicit category setting still surfaces in the Singles/Compilations
    /// areas exactly as it does in those tabs.
    single_names: std::collections::HashSet<String>,
    comp_names: std::collections::HashSet<String>,
}

impl CategorySnapshot {
    fn get(&self, scope: &str, key: &str) -> Option<&Vec<crate::core::category::Area>> {
        self.map.get(&(scope.to_string(), key.to_string()))
    }

    /// Default areas for an album with no explicit/inherited setting: the base
    /// default plus the area implied by its classification (a compilation also
    /// counts as Compilations; otherwise a single also counts as Singles).
    fn kind_default(&self, album: &str) -> Vec<crate::core::category::Area> {
        use crate::core::category::Area;
        let mut areas = Area::DEFAULT.to_vec();
        let lc = album.to_lowercase();
        if self.comp_names.contains(&lc) {
            areas.push(Area::Compilations);
        } else if self.single_names.contains(&lc) {
            areas.push(Area::Singles);
        }
        areas
    }

    /// Album → artist → parent-folder chain (of a sample track) → kind-aware
    /// default. An explicit setting at any level wins verbatim (so the user's
    /// own Singles/Compilations choices are honored).
    fn album_areas(&self, artist: &str, album: &str) -> Vec<crate::core::category::Area> {
        use crate::core::category::album_key;
        if let Some(v) = self.get("album", &album_key(artist, album)) {
            return v.clone();
        }
        if let Some(v) = self.get("artist", artist) {
            return v.clone();
        }
        if let Some(path) = self.sample.get(&(artist.to_string(), album.to_string())) {
            let mut dir = std::path::Path::new(path).parent();
            while let Some(d) = dir {
                if let Some(v) = self.get("folder", &d.to_string_lossy()) {
                    return v.clone();
                }
                dir = d.parent();
            }
        }
        self.kind_default(album)
    }

    /// Artist → default.
    fn artist_areas(&self, name: &str) -> Vec<crate::core::category::Area> {
        self.get("artist", name)
            .cloned()
            .unwrap_or_else(|| crate::core::category::Area::DEFAULT.to_vec())
    }
}

/// File name without extension (fallback: the whole key).
fn file_stem_of(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string()
}

/// Last path component (directory/file name; fallback: the whole key).
fn file_name_of(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string()
}

/// The on-disk schema only needs migrating **once per process**: later worker
/// connections (online enrichment, sync, stats, …) reuse the already-migrated
/// file. `Once` both skips the redundant work (each `migrate()` probes ~15
/// columns via `pragma_table_info`) and serialises the very first migration, so
/// concurrent first opens cannot race on the `ALTER TABLE` statements.
static FILE_DB_MIGRATED: Once = Once::new();

/// Highest schema version this build knows how to run. The idempotent column
/// probes in [`Library::migrate`] remain the source of truth for the actual
/// shape; this number is only a forward marker stamped via `PRAGMA user_version`
/// and a downgrade guard (refuse a DB written by a newer build). Bump it
/// whenever a future migration would break an older binary that opened the DB.
const SCHEMA_VERSION: i32 = 1;

impl Library {
    pub fn open() -> Result<Self> {
        let conn = Connection::open(db_path())?;
        // Multiple connections (UI thread + online worker) access in parallel:
        // wait briefly instead of aborting immediately with "database is locked".
        conn.busy_timeout(Duration::from_secs(10))?;
        // WAL lets readers (the UI) keep working while a writer (scan/enrichment)
        // is active, instead of every reader blocking on a single rollback-journal
        // lock for up to the busy-timeout. `synchronous=NORMAL` is the safe, fast
        // companion for WAL (one fsync per checkpoint, not per commit).
        // `execute_batch` is used because `PRAGMA journal_mode` returns a row.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        let lib = Self { conn };
        // Migrate the file schema once per process (see `FILE_DB_MIGRATED`). Only
        // the first caller runs it and observes its result; later opens reuse the
        // migrated file. The per-connection PRAGMAs above always run.
        let mut migrate_result: Result<()> = Ok(());
        FILE_DB_MIGRATED.call_once(|| migrate_result = lib.migrate());
        migrate_result?;
        Ok(lib)
    }

    /// A throwaway in-memory DB (tests, and the [`open_or_memory`] fallback).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let lib = Self { conn };
        lib.migrate()?;
        Ok(lib)
    }

    /// Opens the on-disk library, or—if that fails (corrupt DB, full/read-only
    /// disk)—logs and returns a throwaway in-memory DB. For **secondary** UI
    /// components (Stats/Sync pages) that must not panic the whole running app
    /// just because a second connection could not be opened. The main app still
    /// treats [`open`](Self::open) as required.
    pub fn open_or_memory() -> Self {
        Self::open().unwrap_or_else(|e| {
            tracing::error!("opening the library failed ({e}); using a temporary in-memory DB");
            // A fresh in-memory DB is deterministic and effectively infallible.
            Self::open_in_memory().expect("in-memory fallback library")
        })
    }

    /// Adds an area to the properties of a level without losing existing
    /// areas. If no setting exists, the default is assumed. Used by the concert
    /// import (marks the "Concerts" category), so that concerts are managed
    /// solely through the properties.
    pub fn add_category_area(
        &self,
        scope: &str,
        key: &str,
        area: crate::core::category::Area,
    ) -> Result<()> {
        use crate::core::category::{areas_value, parse_areas, Area};
        let mut areas = match self.get_category(scope, key)? {
            Some(v) => parse_areas(&v),
            None => Area::DEFAULT.to_vec(),
        };
        if !areas.contains(&area) {
            areas.push(area);
        }
        self.set_category(scope, key, Some(&areas_value(&areas)))
    }

    /// Records a folder/file in the concert table -- now only for the
    /// candidate filtering during import (so that already-added ones are not
    /// suggested again). Display happens via the properties.
    pub fn add_concert(&self, path: &str, title: &str, is_dir: bool) -> Result<()> {
        self.conn.execute(
            "INSERT INTO concert (path, title, is_dir, added_at)
             VALUES (?1, ?2, ?3, strftime('%s','now'))
             ON CONFLICT(path) DO UPDATE SET title = excluded.title",
            rusqlite::params![path, title, is_dir as i64],
        )?;
        Ok(())
    }

    /// Paths of all marked concerts (for the candidate filtering).
    pub fn concert_paths(&self) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM concert")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<std::collections::HashSet<_>>>()?)
    }
}

#[cfg(test)]
mod tests;
