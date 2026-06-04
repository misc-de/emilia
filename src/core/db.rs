//! SQLite library index (rusqlite).

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use std::path::PathBuf;
use std::time::Duration;

use crate::model::{
    AlbumMeta, ArtistMeta, Episode, Source, StatEntry, StatTotals, Track, TrackMeta,
};

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

/// Shared upsert for the `track` table, used by both the single-row
/// [`Library::upsert_track`] and the batched [`Library::upsert_tracks`].
const UPSERT_TRACK_SQL: &str = r#"
    INSERT INTO track (path, title, artist, album, track_no, disc_no, duration_ms, genre)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
    ON CONFLICT(path) DO UPDATE SET
        title       = excluded.title,
        artist      = excluded.artist,
        album       = excluded.album,
        track_no    = excluded.track_no,
        disc_no     = excluded.disc_no,
        duration_ms = excluded.duration_ms,
        genre       = excluded.genre
"#;

/// Binds a `Track` to [`UPSERT_TRACK_SQL`]'s placeholders. A macro (not a fn)
/// because `rusqlite::params!` borrows from `t` and cannot be returned.
macro_rules! track_upsert_params {
    ($t:expr) => {
        rusqlite::params![
            $t.path,
            $t.title,
            $t.artist,
            $t.album,
            $t.track_no,
            $t.disc_no,
            $t.duration_ms,
            $t.genre,
        ]
    };
}

/// Escapes the LIKE metacharacters `\ % _` so an arbitrary (user-chosen) path
/// can be used as a literal prefix in a `LIKE … ESCAPE '\'` pattern.
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// In-memory snapshot of the `category` table (+ one sample track path per
/// `(artist, album)`) for resolving the areas of many items at once. Built by
/// [`Library::category_snapshot`]. Resolution mirrors the per-item
/// [`Library::album_areas`] / [`Library::artist_areas`].
struct CategorySnapshot {
    map: std::collections::HashMap<(String, String), Vec<crate::core::category::Area>>,
    sample: std::collections::HashMap<(String, String), String>,
}

impl CategorySnapshot {
    fn get(&self, scope: &str, key: &str) -> Option<&Vec<crate::core::category::Area>> {
        self.map.get(&(scope.to_string(), key.to_string()))
    }

    /// Album → artist → parent-folder chain (of a sample track) → default.
    fn album_areas(&self, artist: &str, album: &str) -> Vec<crate::core::category::Area> {
        use crate::core::category::{album_key, Area};
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
        Area::DEFAULT.to_vec()
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
        lib.migrate()?;
        Ok(lib)
    }

    /// In-memory DB (for tests).
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let lib = Self { conn };
        lib.migrate()?;
        Ok(lib)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS track (
                id          INTEGER PRIMARY KEY,
                path        TEXT UNIQUE NOT NULL,
                title       TEXT NOT NULL,
                artist      TEXT,
                album       TEXT,
                track_no    INTEGER,
                disc_no     INTEGER,
                duration_ms INTEGER,
                resume_ms   INTEGER NOT NULL DEFAULT 0,
                last_played INTEGER,
                genre       TEXT
            );
            -- Fast lookup of a sample track per album (folder inheritance).
            CREATE INDEX IF NOT EXISTS idx_track_album ON track(album);
            -- Artist-scoped lookups and the (artist, album) grouping of the
            -- album/artist overviews.
            CREATE INDEX IF NOT EXISTS idx_track_artist_album ON track(artist, album);

            CREATE TABLE IF NOT EXISTS eq_preset (
                id     INTEGER PRIMARY KEY,
                preamp REAL NOT NULL DEFAULT 0,
                bands  TEXT NOT NULL          -- JSON [g0..g9] in dB
            );

            CREATE TABLE IF NOT EXISTS eq_binding (
                scope     TEXT NOT NULL CHECK(scope IN ('global','artist','album','track')),
                target_id INTEGER,
                preset_id INTEGER NOT NULL REFERENCES eq_preset(id),
                PRIMARY KEY(scope, target_id)
            );

            CREATE TABLE IF NOT EXISTS setting (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            -- Online-enriched album data (MusicBrainz / Cover Art Archive).
            -- Deliberately kept separate from the audio files: none of this is ever
            -- written back into the tags.
            CREATE TABLE IF NOT EXISTS album_meta (
                artist     TEXT NOT NULL,
                album      TEXT NOT NULL,
                mbid       TEXT,
                cover_path TEXT,
                year       INTEGER,
                status     TEXT NOT NULL DEFAULT 'pending',
                fetched_at INTEGER,
                attempts   INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (artist, album)
            );
            -- `album_cover()` looks an album cover up by album name alone (the
            -- composite primary key can't serve that), called once per single track.
            CREATE INDEX IF NOT EXISTS idx_album_meta_album ON album_meta(album);

            -- Artist photos (Deezer). Also kept separate from the files.
            CREATE TABLE IF NOT EXISTS artist_meta (
                name       TEXT PRIMARY KEY,
                image_path TEXT,
                status     TEXT NOT NULL DEFAULT 'pending',
                fetched_at INTEGER,
                attempts   INTEGER NOT NULL DEFAULT 0
            );

            -- Track data identified by fingerprint (AcoustID) -- pure suggestions,
            -- never written back into the file's tags.
            CREATE TABLE IF NOT EXISTS track_meta (
                path           TEXT PRIMARY KEY,
                recording_mbid TEXT,
                title          TEXT,
                artist         TEXT,
                album          TEXT,
                status         TEXT NOT NULL DEFAULT 'pending',
                fetched_at     INTEGER,
                attempts       INTEGER NOT NULL DEFAULT 0
            );

            -- Folders/files marked as a concert by the user.
            CREATE TABLE IF NOT EXISTS concert (
                path     TEXT PRIMARY KEY,
                title    TEXT NOT NULL,
                is_dir   INTEGER NOT NULL DEFAULT 0,
                added_at INTEGER
            );

            -- Favorites (star in "More info"). scope ∈ {track,folder,album,artist};
            -- key = path | artist\1album | artist name. title = display name.
            CREATE TABLE IF NOT EXISTS favorite (
                scope    TEXT NOT NULL,
                key      TEXT NOT NULL,
                title    TEXT NOT NULL,
                is_dir   INTEGER NOT NULL DEFAULT 0,
                added_at INTEGER,
                pos      INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (scope, key)
            );

            -- Content attribute (music/concert/podcast/audiobook) per level.
            -- Inheritance track → album → artist → default; only deviations
            -- are stored. key = path | artist\1album | artist name.
            CREATE TABLE IF NOT EXISTS category (
                scope TEXT NOT NULL,
                key   TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (scope, key)
            );

            -- Equalizer settings per output and level (10 bands as JSON).
            -- Inheritance track → album → artist → global; additionally a
            -- device-specific output falls back to the default output ('').
            -- output: '' (all/default) | sink name.  key: '' (global) |
            -- artist name | artist\1album | path.
            CREATE TABLE IF NOT EXISTS eq_setting (
                output TEXT NOT NULL DEFAULT '',
                scope  TEXT NOT NULL CHECK(scope IN ('global','artist','album','track')),
                key    TEXT NOT NULL,
                bands  TEXT NOT NULL,
                PRIMARY KEY (output, scope, key)
            );

            -- Multiple images per album or artist (gallery). The single image
            -- stored in album_meta/artist_meta remains the one shown primarily;
            -- these tables hold the full set.
            CREATE TABLE IF NOT EXISTS album_image (
                artist TEXT NOT NULL,
                album  TEXT NOT NULL,
                idx    INTEGER NOT NULL,
                path   TEXT NOT NULL,
                kind   TEXT,
                source TEXT,
                PRIMARY KEY (artist, album, idx)
            );

            CREATE TABLE IF NOT EXISTS artist_image (
                name   TEXT NOT NULL,
                idx    INTEGER NOT NULL,
                path   TEXT NOT NULL,
                kind   TEXT,
                source TEXT,
                PRIMARY KEY (name, idx)
            );

            -- User-created playlists and their entries (ordered).
            -- Entries are paths (like the queue); duplicates allowed.
            CREATE TABLE IF NOT EXISTS playlist (
                id         INTEGER PRIMARY KEY,
                name       TEXT NOT NULL,
                created_at INTEGER
            );
            CREATE TABLE IF NOT EXISTS playlist_item (
                playlist_id INTEGER NOT NULL,
                position    INTEGER NOT NULL,
                path        TEXT NOT NULL,
                PRIMARY KEY (playlist_id, position)
            );

            -- Subscribed podcasts and their episodes (from RSS feeds; audio is
            -- streamed, nothing is downloaded).
            CREATE TABLE IF NOT EXISTS podcast (
                id        INTEGER PRIMARY KEY,
                title     TEXT NOT NULL,
                feed_url  TEXT NOT NULL UNIQUE,
                image_url TEXT,
                added_at  INTEGER
            );
            CREATE TABLE IF NOT EXISTS episode (
                podcast_id  INTEGER NOT NULL,
                position    INTEGER NOT NULL,
                guid        TEXT,
                title       TEXT NOT NULL,
                audio_url   TEXT NOT NULL,
                published   TEXT,
                duration    TEXT,
                description TEXT,
                PRIMARY KEY (podcast_id, position)
            );

            -- Resume position per episode, keyed by audio URL --
            -- deliberately separate from `episode`, so that a feed refresh (which
            -- replaces the episode rows) does not delete the resume position.
            CREATE TABLE IF NOT EXISTS episode_progress (
                url         TEXT PRIMARY KEY,
                position_ms INTEGER NOT NULL DEFAULT 0,
                updated_at  INTEGER NOT NULL DEFAULT 0
            );

            -- Downloaded episodes (offline playback), keyed by audio URL like
            -- `episode_progress` so a feed refresh keeps the download. The audio
            -- file lives at `path`; playback prefers it over the network URL.
            CREATE TABLE IF NOT EXISTS episode_download (
                url           TEXT PRIMARY KEY,
                path          TEXT NOT NULL,
                downloaded_at INTEGER NOT NULL DEFAULT 0
            );

            -- Saved streaming stations (internet radio). Playback directly
            -- via the stream URL; nothing is downloaded.
            CREATE TABLE IF NOT EXISTS stream (
                id        INTEGER PRIMARY KEY,
                name      TEXT NOT NULL,
                url       TEXT NOT NULL UNIQUE,
                favicon   TEXT,
                tags      TEXT,
                country   TEXT,
                codec     TEXT,
                bitrate   INTEGER,
                favorite  INTEGER NOT NULL DEFAULT 0,
                added_at  INTEGER
            );

            -- Timeshift recordings (songs saved from stations). The
            -- audio file lives at `path`; here only the metadata/management.
            CREATE TABLE IF NOT EXISTS recording (
                id          INTEGER PRIMARY KEY,
                path        TEXT NOT NULL,
                artist      TEXT,
                title       TEXT NOT NULL,
                station     TEXT,
                recorded_at INTEGER,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                incomplete  INTEGER NOT NULL DEFAULT 0
            );

            -- Listening statistics: one event per played track (raw; nothing is
            -- precomputed). Stays purely local -- never leaves the device. Artist/
            -- album/genre are joined to `track` via `path` for analysis,
            -- not duplicated here (same principle as the online metadata).
            CREATE TABLE IF NOT EXISTS play_event (
                id          INTEGER PRIMARY KEY,
                path        TEXT NOT NULL,
                started_at  INTEGER NOT NULL,           -- Unix seconds (start)
                played_ms   INTEGER NOT NULL,           -- actually heard (only while "Playing")
                duration_ms INTEGER,                    -- snapshot (file may disappear)
                completed   INTEGER NOT NULL DEFAULT 0, -- 1 = listened through to EOS, 0 = skip/switch
                source      TEXT                        -- 'queue'|'album'|'artist'|… | NULL
            );
            CREATE INDEX IF NOT EXISTS idx_play_event_path ON play_event(path);
            CREATE INDEX IF NOT EXISTS idx_play_event_time ON play_event(started_at);

            -- Additional music sources besides the primary `music_dir` folder.
            -- Each source appears as its own tab in the file view. The
            -- primary directory stays the `music_dir` setting and is deliberately
            -- NOT listed here (no entry), so that scan/library are untouched.
            -- kind = 'local' (second folder, e.g. SD card) | 'webdav'
            -- (Nextcloud share). The username and app password are stored as
            -- Secret Service references (`secret-tool:<id>`) when available;
            -- older/fallback rows may contain the values directly.
            CREATE TABLE IF NOT EXISTS source (
                id         INTEGER PRIMARY KEY,
                kind       TEXT NOT NULL CHECK(kind IN ('local','webdav')),
                name       TEXT NOT NULL,
                position   INTEGER NOT NULL DEFAULT 0,
                path       TEXT,   -- local:  root path in the filesystem
                base_url   TEXT,   -- webdav: e.g. https://cloud.example.com
                username   TEXT,   -- webdav: username (or secret-tool reference)
                password   TEXT,   -- webdav: app password/token (or secret-tool ref)
                music_path TEXT    -- webdav: subpath to the music, e.g. /Music
            );
            "#,
        )?;

        // Migration: upgrade an earlier eq_setting version without an `output` column.
        let has_output = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('eq_setting') WHERE name = 'output'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_output {
            self.conn.execute_batch(
                r#"
                ALTER TABLE eq_setting RENAME TO eq_setting_old;
                CREATE TABLE eq_setting (
                    output TEXT NOT NULL DEFAULT '',
                    scope  TEXT NOT NULL CHECK(scope IN ('global','artist','album','track')),
                    key    TEXT NOT NULL,
                    bands  TEXT NOT NULL,
                    PRIMARY KEY (output, scope, key)
                );
                INSERT INTO eq_setting (output, scope, key, bands)
                    SELECT '', scope, key, bands FROM eq_setting_old;
                DROP TABLE eq_setting_old;
                "#,
            )?;
        }

        // Migration: add disc_no (disc number for multi-CD albums).
        let has_disc = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('track') WHERE name = 'disc_no'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_disc {
            self.conn
                .execute_batch("ALTER TABLE track ADD COLUMN disc_no INTEGER;")?;
        }

        // Migration: add the genre column (for the genre statistics). It is only
        // populated by re-scanning the library.
        let has_genre = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('track') WHERE name = 'genre'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_genre {
            self.conn
                .execute_batch("ALTER TABLE track ADD COLUMN genre TEXT;")?;
        }

        // Migration: add the attempts counter to the meta tables (limits the
        // repeated retrying of online fetches that kept failing).
        for table in ["album_meta", "artist_meta", "track_meta"] {
            let has = self
                .conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = 'attempts'"
                    ),
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0;
            if !has {
                self.conn.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;"
                ))?;
            }
        }

        // Migration: sort column for favorites (for manual reordering).
        let has_pos = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('favorite') WHERE name = 'pos'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_pos {
            self.conn
                .execute_batch("ALTER TABLE favorite ADD COLUMN pos INTEGER NOT NULL DEFAULT 0;")?;
            // Number the existing favorites in their previous order.
            self.conn.execute_batch(
                "UPDATE favorite SET pos = (
                     SELECT COUNT(*) FROM favorite f2
                     WHERE COALESCE(f2.added_at,0) < COALESCE(favorite.added_at,0)
                        OR (COALESCE(f2.added_at,0) = COALESCE(favorite.added_at,0) AND f2.key <= favorite.key)
                 );",
            )?;
        }

        // Migration: add show notes/description for episodes.
        let has_descr = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('episode') WHERE name = 'description'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_descr {
            self.conn
                .execute_batch("ALTER TABLE episode ADD COLUMN description TEXT;")?;
        }

        // Migration: map the old single attributes (music/concert/…) onto the new
        // area list (properties). Idempotent.
        self.conn.execute_batch(
            "UPDATE category SET value = CASE value
                 WHEN 'music'     THEN 'filesystem,artists,albums'
                 WHEN 'concert'   THEN 'concerts'
                 WHEN 'audiobook' THEN 'audiobooks'
                 WHEN 'podcast'   THEN 'filesystem,artists,albums'
                 ELSE value END
             WHERE value IN ('music','concert','audiobook','podcast');",
        )?;

        // Migration: remove the old CHECK constraint on scope, so that the
        // folder level ('folder') can be stored too.
        let has_old_check = self
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='category'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .map(|s| s.contains("CHECK(scope"))
            .unwrap_or(false);
        if has_old_check {
            self.conn.execute_batch(
                "ALTER TABLE category RENAME TO category_old;
                 CREATE TABLE category (
                     scope TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
                     PRIMARY KEY (scope, key)
                 );
                 INSERT INTO category SELECT * FROM category_old;
                 DROP TABLE category_old;",
            )?;
        }
        Ok(())
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
                    let album = key.splitn(2, '\u{1}').nth(1).unwrap_or("").to_string();
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
        out.sort_by(|a, b| a.2.to_lowercase().cmp(&b.2.to_lowercase()));
        out
    }

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
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
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
            if let Ok(Some(v)) = self.get_category("album", &album_key(artist.unwrap_or(""), album)) {
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
        use crate::core::category::{album_key, parse_areas, Area};
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
        Area::DEFAULT.to_vec()
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
    fn category_snapshot(&self) -> Result<CategorySnapshot> {
        use crate::core::category::parse_areas;
        use std::collections::HashMap;

        let mut map: HashMap<(String, String), Vec<crate::core::category::Area>> = HashMap::new();
        {
            let mut stmt = self.conn.prepare("SELECT scope, key, value FROM category")?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
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
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?;
            for (artist, album, path) in rows.flatten() {
                sample.insert((artist, album), path);
            }
        }
        Ok(CategorySnapshot { map, sample })
    }

    // ---- Equalizer (10 bands, with inheritance) ----

    /// Stores the 10 band gains (dB) for an output + a level.
    pub fn set_eq(&self, output: &str, scope: &str, key: &str, bands: &[f64; 10]) -> Result<()> {
        let json = serde_json::to_string(bands)?;
        self.conn.execute(
            "INSERT INTO eq_setting (output, scope, key, bands) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(output, scope, key) DO UPDATE SET bands = excluded.bands",
            rusqlite::params![output, scope, key, json],
        )?;
        Ok(())
    }

    /// Reads the bands of a single output/level combination (without inheritance).
    pub fn get_eq(&self, output: &str, scope: &str, key: &str) -> Result<Option<[f64; 10]>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT bands FROM eq_setting WHERE output = ?1 AND scope = ?2 AND key = ?3",
                rusqlite::params![output, scope, key],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(json.and_then(|j| serde_json::from_str::<[f64; 10]>(&j).ok()))
    }

    /// All stored equalizer settings (for the device synchronization).
    pub fn all_eq_settings(&self) -> Result<Vec<(String, String, String, [f64; 10])>> {
        let mut stmt = self
            .conn
            .prepare("SELECT output, scope, key, bands FROM eq_setting")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows.filter_map(|r| r.ok()) {
            let (output, scope, key, json) = row;
            if let Ok(bands) = serde_json::from_str::<[f64; 10]>(&json) {
                out.push((output, scope, key, bands));
            }
        }
        Ok(out)
    }

    /// Removes the setting (falls back to the inherited/default output).
    pub fn clear_eq(&self, output: &str, scope: &str, key: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM eq_setting WHERE output = ?1 AND scope = ?2 AND key = ?3",
            rusqlite::params![output, scope, key],
        )?;
        Ok(())
    }

    /// Effective equalizer for track + output. Order: first the concrete
    /// output (track→album→artist→global), then the default output ('')
    /// as the basis. `None` if nothing is set anywhere (→ neutral).
    pub fn resolve_eq(
        &self,
        output: &str,
        artist: Option<&str>,
        album: Option<&str>,
        path: &str,
    ) -> Option<[f64; 10]> {
        let album_key = album.map(|al| crate::core::category::album_key(artist.unwrap_or(""), al));

        // Concrete output first, then the default output as the basis.
        let mut outputs: Vec<&str> = Vec::new();
        if !output.is_empty() {
            outputs.push(output);
        }
        outputs.push("");

        for out in outputs {
            if let Ok(Some(b)) = self.get_eq(out, "track", path) {
                return Some(b);
            }
            if let Some(key) = &album_key {
                if let Ok(Some(b)) = self.get_eq(out, "album", key) {
                    return Some(b);
                }
            }
            if let Some(artist) = artist {
                if let Ok(Some(b)) = self.get_eq(out, "artist", artist) {
                    return Some(b);
                }
            }
            if let Ok(Some(b)) = self.get_eq(out, "global", "") {
                return Some(b);
            }
        }
        None
    }

    /// Reads a setting value (e.g. the music folder).
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let value = self
            .conn
            .query_row("SELECT value FROM setting WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        Ok(value)
    }

    /// Stores a setting value.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO setting (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Reads a security-sensitive setting (API key/token). A `secret-tool:`
    /// sentinel resolves to the Secret Service; a legacy plaintext value is
    /// returned verbatim.
    pub fn get_secret_setting(&self, key: &str) -> Result<Option<String>> {
        match self.get_setting(key)? {
            Some(v) if v == crate::core::secrets::SECRET_PREFIX => {
                Ok(crate::core::secrets::lookup_named(key))
            }
            Some(v) if v.is_empty() => Ok(None),
            other => Ok(other),
        }
    }

    /// Stores a security-sensitive setting in the Secret Service when available
    /// (only a `secret-tool:` sentinel is kept in the DB); otherwise falls back
    /// to a plaintext setting. An empty value clears both.
    pub fn set_secret_setting(&self, key: &str, value: &str) -> Result<()> {
        let value = value.trim();
        if value.is_empty() {
            crate::core::secrets::clear_named(key);
            self.conn.execute("DELETE FROM setting WHERE key = ?1", [key])?;
            return Ok(());
        }
        let label = format!("Emilia {key}");
        if crate::core::secrets::store_named(key, &label, value) {
            self.set_setting(key, crate::core::secrets::SECRET_PREFIX)
        } else {
            self.set_setting(key, value)
        }
    }

    /// Lists all additional music sources (by position, then ID).
    pub fn list_sources(&self) -> Result<Vec<Source>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, position, path, base_url, username, password, music_path
             FROM source ORDER BY position, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Source {
                id: r.get(0)?,
                kind: r.get(1)?,
                name: r.get(2)?,
                position: r.get(3)?,
                path: r.get(4)?,
                base_url: r.get(5)?,
                username: r.get(6)?,
                password: r.get(7)?,
                music_path: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Adds a source and returns its new ID. `position` is
    /// automatically set to the end (max + 1).
    pub fn add_source(&self, s: &Source) -> Result<i64> {
        let position: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(position), -1) + 1 FROM source", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        self.conn.execute(
            "INSERT INTO source (kind, name, position, path, base_url, username, password, music_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                s.kind,
                s.name,
                position,
                s.path,
                s.base_url,
                s.username,
                s.password,
                s.music_path,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Replaces the stored password field of a source. Used after creating a
    /// WebDAV source when its app password was moved to the Secret Service.
    pub fn set_source_password(&self, id: i64, password: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE source SET password = ?1 WHERE id = ?2",
            rusqlite::params![password, id],
        )?;
        Ok(())
    }

    /// Replaces the stored username field of a source. Used after creating a
    /// WebDAV source when its username was moved to the Secret Service.
    pub fn set_source_username(&self, id: i64, username: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE source SET username = ?1 WHERE id = ?2",
            rusqlite::params![username, id],
        )?;
        Ok(())
    }

    /// Best-effort migration of existing **plaintext** secrets into the Secret
    /// Service (run once at startup). Each value is only replaced by its
    /// `secret-tool:` reference after a verifying lookup confirms the keyring
    /// copy — so a missing/unavailable keyring never loses a credential, and the
    /// app keeps working with the plaintext fallback. Once everything is
    /// referenced this is a couple of cheap DB reads.
    pub fn migrate_secrets(&self) {
        use crate::core::secrets;
        // API keys/tokens stored as settings.
        for key in ["acoustid_key", "fanart_key"] {
            if let Ok(Some(v)) = self.get_setting(key) {
                if !v.is_empty()
                    && v != secrets::SECRET_PREFIX
                    && secrets::store_named(key, &format!("Emilia {key}"), &v)
                    && secrets::lookup_named(key).as_deref() == Some(v.as_str())
                {
                    let _ = self.set_setting(key, secrets::SECRET_PREFIX);
                }
            }
        }
        // Nextcloud/WebDAV credentials (username + app password).
        for s in self.list_sources().unwrap_or_default() {
            if s.kind != "webdav" {
                continue;
            }
            let label = format!("Emilia Nextcloud {}", s.name);
            if let Some(pw) = s.password.as_deref() {
                if !pw.is_empty()
                    && !pw.starts_with(secrets::SECRET_PREFIX)
                    && secrets::store_source_password(s.id, &label, pw)
                    && secrets::lookup_source_password(s.id).as_deref() == Some(pw)
                {
                    let _ = self.set_source_password(s.id, Some(&secrets::source_password_ref(s.id)));
                }
            }
            if let Some(user) = s.username.as_deref() {
                if !user.is_empty()
                    && !user.starts_with(secrets::SECRET_PREFIX)
                    && secrets::store_source_username(s.id, &label, user)
                    && secrets::lookup_source_username(s.id).as_deref() == Some(user)
                {
                    let _ = self.set_source_username(s.id, Some(&secrets::source_username_ref(s.id)));
                }
            }
        }
    }

    /// Removes a source by its ID.
    pub fn delete_source(&self, id: i64) -> Result<()> {
        crate::core::secrets::clear_source_password(id);
        self.conn
            .execute("DELETE FROM source WHERE id = ?1", [id])?;
        // Remove indexed cloud tracks of this source (synthetic path
        // `nc:<id>:…`). For local sources the pattern matches nothing.
        self.conn
            .execute("DELETE FROM track WHERE path LIKE ?1", [format!("nc:{id}:%")])?;
        Ok(())
    }

    /// (artist, album) pairs of a source's indexed tracks -- for the
    /// red "Disconnected" hint on the covers when the source is offline.
    pub fn remote_album_keys(&self, source_id: i64) -> Result<Vec<(String, String)>> {
        let like = format!("nc:{source_id}:%");
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT COALESCE(artist,''), COALESCE(album,'') FROM track \
             WHERE path LIKE ?1 AND album IS NOT NULL AND album <> ''",
        )?;
        let rows =
            stmt.query_map([like], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Artist names of a source's indexed tracks (for the "Disconnected"
    /// hint on the photos).
    pub fn remote_artists(&self, source_id: i64) -> Result<Vec<String>> {
        let like = format!("nc:{source_id}:%");
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT artist FROM track \
             WHERE path LIKE ?1 AND artist IS NOT NULL AND artist <> ''",
        )?;
        let rows = stmt.query_map([like], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Inserts a track or updates its metadata (key: path).
    pub fn upsert_track(&self, t: &Track) -> Result<()> {
        self.conn.execute(UPSERT_TRACK_SQL, track_upsert_params!(t))?;
        Ok(())
    }

    /// Upserts many tracks in a single transaction. Atomic (a crash mid-scan
    /// leaves the previous state, not a half-written batch) and dramatically
    /// faster than one implicit transaction per row (one fsync per batch instead
    /// of per track). Used by the directory scan.
    pub fn upsert_tracks(&self, tracks: &[Track]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut count = 0;
        {
            let mut stmt = tx.prepare_cached(UPSERT_TRACK_SQL)?;
            for t in tracks {
                stmt.execute(track_upsert_params!(t))?;
                count += 1;
            }
        }
        tx.commit()?;
        Ok(count)
    }

    /// Removes tracks under `root` whose files no longer exist on disk (orphans
    /// left behind by deletions/moves). Strictly scoped to `root`: remote
    /// (`nc:…`) tracks and other sources keep their own path prefixes and are
    /// never touched. `present` is the set of paths found during the scan; if it
    /// is empty nothing is pruned, so a transiently unreadable/unmounted folder
    /// cannot wipe the library. Returns the number of rows removed.
    pub fn prune_tracks_under(&self, root: &std::path::Path, present: &[String]) -> Result<usize> {
        if present.is_empty() {
            return Ok(0);
        }
        // `root/%`, escaping LIKE metacharacters in the (user-chosen) path.
        let prefix = like_escape(&root.to_string_lossy());
        let pattern = format!("{prefix}{}%", std::path::MAIN_SEPARATOR);
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _present(path TEXT PRIMARY KEY);
             DELETE FROM _present;",
        )?;
        {
            let mut stmt = tx.prepare("INSERT OR IGNORE INTO _present(path) VALUES (?1)")?;
            for p in present {
                stmt.execute([p])?;
            }
        }
        let removed = tx.execute(
            "DELETE FROM track
             WHERE path LIKE ?1 ESCAPE '\\'
               AND path NOT IN (SELECT path FROM _present)",
            rusqlite::params![pattern],
        )?;
        tx.commit()?;
        if removed > 0 {
            tracing::info!("Scan: pruned {removed} orphaned track(s) under {}", root.display());
        }
        Ok(removed)
    }

    /// Stores the resume position by path. The
    /// queue is path-based; nothing happens for an unknown path.
    pub fn set_resume_path(&self, path: &str, resume_ms: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE track SET resume_ms = ?1 WHERE path = ?2",
            rusqlite::params![resume_ms, path],
        )?;
        Ok(())
    }

    /// Reads a single track by its path (incl. resume position).
    pub fn track_by_path(&self, path: &str) -> Result<Option<Track>> {
        let track = self
            .conn
            .query_row(
                "SELECT id, path, title, artist, album, track_no, duration_ms, resume_ms, disc_no
                 FROM track WHERE path = ?1",
                [path],
                |r| {
                    Ok(Track {
                        id: r.get(0)?,
                        path: r.get(1)?,
                        title: r.get(2)?,
                        artist: r.get(3)?,
                        album: r.get(4)?,
                        genre: None,
                        track_no: r.get::<_, Option<i64>>(5)?.map(|n| n as u32),
                        duration_ms: r.get(6)?,
                        resume_ms: r.get(7)?,
                        disc_no: r.get::<_, Option<i64>>(8)?.map(|n| n as u32),
                    })
                },
            )
            .optional()?;
        Ok(track)
    }

    /// All tracks, sorted by album and track number.
    pub fn all_tracks(&self) -> Result<Vec<Track>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, title, artist, album, track_no, duration_ms, resume_ms, disc_no
             FROM track
             ORDER BY album, COALESCE(disc_no, 1), track_no, title",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Track {
                id: r.get(0)?,
                path: r.get(1)?,
                title: r.get(2)?,
                artist: r.get(3)?,
                album: r.get(4)?,
                genre: None,
                track_no: r.get::<_, Option<i64>>(5)?.map(|n| n as u32),
                duration_ms: r.get(6)?,
                resume_ms: r.get(7)?,
                disc_no: r.get::<_, Option<i64>>(8)?.map(|n| n as u32),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- Failure counters (limit the repeated online retrying) ----

    /// Previous unsuccessful online attempts for an album (0 if unknown).
    pub fn album_attempts(&self, artist: &str, album: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT attempts FROM album_meta WHERE artist = ?1 AND album = ?2",
                rusqlite::params![artist, album],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    /// Previous unsuccessful online attempts for an artist.
    pub fn artist_attempts(&self, name: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT attempts FROM artist_meta WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    /// Previous unsuccessful fingerprint attempts for a track (path).
    pub fn track_attempts(&self, path: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT attempts FROM track_meta WHERE path = ?1",
                [path],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

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
                    CASE WHEN ?6 IN ('matched','local') THEN 0 ELSE 1 END)
            ON CONFLICT(artist, album) DO UPDATE SET
                mbid       = excluded.mbid,
                cover_path = excluded.cover_path,
                year       = excluded.year,
                status     = excluded.status,
                fetched_at = excluded.fetched_at,
                attempts   = CASE WHEN excluded.status IN ('matched','local') THEN 0
                                  ELSE album_meta.attempts + 1 END
            "#,
            rusqlite::params![m.artist, m.album, m.mbid, m.cover_path, m.year, m.status],
        )?;
        Ok(())
    }

    /// Album overview for the UI: all unique albums from the library,
    /// enriched with (any available) online metadata and the track count.
    /// Sorted by album name (like the file view -- without artist groups).
    pub fn albums_overview(&self) -> Result<Vec<AlbumMeta>> {
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
            let Some(per) = by_artist.get(key) else { continue };
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
            // Cover ONLY if all (primary) artists provide the same cover.
            // For compilations / pseudo-albums ("[non-album tracks]",
            // "Dancemix 2009" …) a single member's cover would otherwise have
            // given a misleading image for the whole card → then neutral.
            let mut covers: Vec<&String> =
                artists.iter().filter_map(|(_, i)| i.1.as_ref()).collect();
            covers.sort();
            covers.dedup();
            meta.cover_path = match covers.as_slice() {
                [one] => Some((*one).clone()),
                _ => None,
            };
        }
        let mut out: Vec<AlbumMeta> = order.into_iter().filter_map(|k| map.remove(&k)).collect();
        // Properties: only show albums that are visible in the "Albums" area.
        // Resolve from one in-memory snapshot instead of querying per album.
        let cats = self.category_snapshot()?;
        out.retain(|a| {
            cats.album_areas(&a.artist, &a.album)
                .contains(&crate::core::category::Area::Albums)
        });
        out.sort_by(|a, b| a.album.to_lowercase().cmp(&b.album.to_lowercase()));
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
    pub fn artists_overview(&self) -> Result<Vec<ArtistMeta>> {
        let names = self.distinct_artists()?;
        // Resolve areas from one snapshot and pull all artist metadata in one
        // query, instead of two queries per artist (was a clear N+1).
        let cats = self.category_snapshot()?;
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
            rusqlite::params![m.path, m.recording_mbid, m.title, m.artist, m.album, m.status],
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

    /// Overall metrics from `since` (Unix seconds; 0 = since the beginning). The
    /// `distinct_*` numbers count only what actually counts as a play, and
    /// are -- like the rankings -- folded over feat./album names.
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
            distinct_artists: self.stats_top_artists(since, usize::MAX)?.len() as i64,
            distinct_albums: self.stats_top_albums(since, usize::MAX)?.len() as i64,
        })
    }

    /// Top tracks from `since`, sorted by plays (then time heard).
    pub fn stats_top_tracks(&self, since: i64, limit: usize) -> Result<Vec<StatEntry>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT t.title, e.path, COALESCE(t.artist, '') AS artist,
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
            if let Some((name, _)) = votes.get(key).and_then(|v| v.iter().max_by_key(|(_, c)| **c)) {
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
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
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

    // ---- Multiple images per album / artist (gallery) ----

    /// Replaces the stored album images (order = idx).
    /// `images`: each (path, kind, source).
    pub fn set_album_images(
        &self,
        artist: &str,
        album: &str,
        images: &[(String, String, String)],
    ) -> Result<()> {
        // One transaction so the gallery is never seen half-deleted/half-filled.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM album_image WHERE artist = ?1 AND album = ?2",
            rusqlite::params![artist, album],
        )?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO album_image (artist, album, idx, path, kind, source)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (i, (path, kind, source)) in images.iter().enumerate() {
                stmt.execute(rusqlite::params![artist, album, i as i64, path, kind, source])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// All stored image paths of an album (in order).
    pub fn album_images(&self, artist: &str, album: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT path FROM album_image WHERE artist = ?1 AND album = ?2 ORDER BY idx",
        )?;
        let rows =
            stmt.query_map(rusqlite::params![artist, album], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Replaces the stored artist images (order = idx).
    pub fn set_artist_images(
        &self,
        name: &str,
        images: &[(String, String, String)],
    ) -> Result<()> {
        // One transaction so the gallery is never seen half-deleted/half-filled.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM artist_image WHERE name = ?1", [name])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO artist_image (name, idx, path, kind, source)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (i, (path, kind, source)) in images.iter().enumerate() {
                stmt.execute(rusqlite::params![name, i as i64, path, kind, source])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// All stored image paths of an artist (in order).
    pub fn artist_images(&self, name: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM artist_image WHERE name = ?1 ORDER BY idx")?;
        let rows = stmt.query_map([name], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- Playlists ----

    /// Creates a playlist and returns its ID.
    pub fn create_playlist(&self, name: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO playlist (name, created_at) VALUES (?1, strftime('%s','now'))",
            [name],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Renames a playlist.
    pub fn rename_playlist(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE playlist SET name = ?1 WHERE id = ?2",
            rusqlite::params![name, id],
        )?;
        Ok(())
    }

    /// Deletes a playlist along with its entries.
    pub fn delete_playlist(&self, id: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM playlist_item WHERE playlist_id = ?1", [id])?;
        tx.execute("DELETE FROM playlist WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// All playlists as (id, name, track count), sorted alphabetically.
    pub fn playlists(&self) -> Result<Vec<(i64, String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, COUNT(i.path)
             FROM playlist p
             LEFT JOIN playlist_item i ON i.playlist_id = p.id
             GROUP BY p.id
             ORDER BY p.name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Appends paths to the end of a playlist (duplicates allowed).
    pub fn add_to_playlist(&self, id: i64, paths: &[String]) -> Result<()> {
        // Compute the start position and insert in one transaction so two
        // concurrent appenders cannot read the same MAX(position) and collide.
        let tx = self.conn.unchecked_transaction()?;
        let start: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM playlist_item WHERE playlist_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO playlist_item (playlist_id, position, path) VALUES (?1, ?2, ?3)",
            )?;
            for (i, path) in paths.iter().enumerate() {
                stmt.execute(rusqlite::params![id, start + i as i64, path])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Paths of a playlist in their order.
    pub fn playlist_paths(&self, id: i64) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM playlist_item WHERE playlist_id = ?1 ORDER BY position")?;
        let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Removes all occurrences of a path from a playlist.
    pub fn remove_from_playlist(&self, id: i64, path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM playlist_item WHERE playlist_id = ?1 AND path = ?2",
            rusqlite::params![id, path],
        )?;
        Ok(())
    }

    // ---- Podcasts ----

    /// Subscribes to a feed (or updates title/image for a known feed)
    /// and returns the podcast ID.
    pub fn subscribe_podcast(
        &self,
        title: &str,
        feed_url: &str,
        image_url: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO podcast (title, feed_url, image_url, added_at)
             VALUES (?1, ?2, ?3, strftime('%s','now'))
             ON CONFLICT(feed_url) DO UPDATE SET
                title = excluded.title, image_url = excluded.image_url",
            rusqlite::params![title, feed_url, image_url],
        )?;
        Ok(self
            .conn
            .query_row("SELECT id FROM podcast WHERE feed_url = ?1", [feed_url], |r| {
                r.get(0)
            })?)
    }

    /// All podcasts as (id, title, image URL, episode count), newest first.
    pub fn podcasts(&self) -> Result<Vec<(i64, String, Option<String>, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.title, p.image_url, COUNT(e.audio_url)
             FROM podcast p LEFT JOIN episode e ON e.podcast_id = p.id
             GROUP BY p.id ORDER BY p.added_at DESC, p.title COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Feed URL of a podcast (for the update).
    pub fn podcast_feed_url(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT feed_url FROM podcast WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Removes a podcast along with its episodes.
    pub fn delete_podcast(&self, id: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM episode WHERE podcast_id = ?1", [id])?;
        tx.execute("DELETE FROM podcast WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces the episodes of a podcast (order = feed order).
    pub fn set_episodes(&self, podcast_id: i64, episodes: &[Episode]) -> Result<()> {
        // One transaction: a refresh interrupted mid-way must not leave the feed
        // with its old episodes deleted and only some of the new ones inserted.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM episode WHERE podcast_id = ?1", [podcast_id])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO episode
                    (podcast_id, position, guid, title, audio_url, published, duration, description)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (i, ep) in episodes.iter().enumerate() {
                stmt.execute(rusqlite::params![
                    podcast_id,
                    i as i64,
                    ep.guid,
                    ep.title,
                    ep.audio_url,
                    ep.published,
                    ep.duration,
                    ep.description
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Stores/updates the resume position of an episode (by URL).
    /// `position_ms <= 0` deletes the entry (counts as "from the start / finished").
    pub fn set_episode_progress(&self, url: &str, position_ms: i64) -> Result<()> {
        if position_ms <= 0 {
            self.conn
                .execute("DELETE FROM episode_progress WHERE url = ?1", [url])?;
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO episode_progress (url, position_ms, updated_at)
             VALUES (?1, ?2, strftime('%s','now'))
             ON CONFLICT(url) DO UPDATE SET
                position_ms = excluded.position_ms, updated_at = excluded.updated_at",
            rusqlite::params![url, position_ms],
        )?;
        Ok(())
    }

    /// Remembered resume position of an episode (0 = none/from the start).
    pub fn episode_progress(&self, url: &str) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                "SELECT position_ms FROM episode_progress WHERE url = ?1",
                [url],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// All remembered episode positions (for the device sync).
    pub fn all_episode_progress(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT url, position_ms FROM episode_progress")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Records a downloaded episode (audio URL → local file path) for offline
    /// playback.
    pub fn set_episode_download(&self, url: &str, path: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO episode_download (url, path, downloaded_at)
             VALUES (?1, ?2, strftime('%s','now'))
             ON CONFLICT(url) DO UPDATE SET
                path = excluded.path, downloaded_at = excluded.downloaded_at",
            rusqlite::params![url, path],
        )?;
        Ok(())
    }

    /// Local path of a downloaded episode (offline copy), or `None` if the
    /// episode is not downloaded.
    pub fn episode_download(&self, url: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT path FROM episode_download WHERE url = ?1",
                [url],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Removes the download record for an episode and returns the stored file
    /// path (so the caller can delete the file). `None` if it wasn't downloaded.
    pub fn delete_episode_download(&self, url: &str) -> Result<Option<String>> {
        let path = self.episode_download(url)?;
        if path.is_some() {
            self.conn
                .execute("DELETE FROM episode_download WHERE url = ?1", [url])?;
        }
        Ok(path)
    }

    /// Show notes/description of an episode by its audio URL (for the
    /// chapter marks on the seekbar). `None` if unknown or empty.
    pub fn episode_description_by_url(&self, url: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT description FROM episode WHERE audio_url = ?1 LIMIT 1",
                [url],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .filter(|s| !s.trim().is_empty()))
    }

    /// Episodes of a podcast in feed order.
    pub fn episodes(&self, podcast_id: i64) -> Result<Vec<Episode>> {
        let mut stmt = self.conn.prepare(
            "SELECT guid, title, audio_url, published, duration, description FROM episode
             WHERE podcast_id = ?1 ORDER BY position",
        )?;
        let rows = stmt.query_map([podcast_id], |r| {
            Ok(Episode {
                guid: r.get(0)?,
                title: r.get(1)?,
                audio_url: r.get(2)?,
                published: r.get(3)?,
                duration: r.get(4)?,
                description: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- Streaming (internet radio) ----

    /// Stores a station (or updates its fields for a known URL)
    /// and returns the station ID.
    #[allow(clippy::too_many_arguments)]
    pub fn add_stream(
        &self,
        name: &str,
        url: &str,
        favicon: Option<&str>,
        tags: Option<&str>,
        country: Option<&str>,
        codec: Option<&str>,
        bitrate: Option<i64>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO stream (name, url, favicon, tags, country, codec, bitrate, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%s','now'))
             ON CONFLICT(url) DO UPDATE SET
                name = excluded.name, favicon = excluded.favicon, tags = excluded.tags,
                country = excluded.country, codec = excluded.codec, bitrate = excluded.bitrate",
            rusqlite::params![name, url, favicon, tags, country, codec, bitrate],
        )?;
        Ok(self
            .conn
            .query_row("SELECT id FROM stream WHERE url = ?1", [url], |r| r.get(0))?)
    }

    /// All stored stations -- favorites first, then the most recently
    /// added.
    pub fn streams(&self) -> Result<Vec<crate::model::StreamItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, url, favicon, tags, country FROM stream
             ORDER BY added_at DESC, name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::model::StreamItem {
                id: r.get(0)?,
                name: r.get(1)?,
                url: r.get(2)?,
                favicon: r.get(3)?,
                tags: r.get(4)?,
                country: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Removes a station.
    pub fn delete_stream(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM stream WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- Recordings (timeshift recordings) ----

    /// Stores a recording entry and returns its ID.
    pub fn add_recording(
        &self,
        path: &str,
        artist: Option<&str>,
        title: &str,
        station: Option<&str>,
        incomplete: bool,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO recording (path, artist, title, station, recorded_at, incomplete)
             VALUES (?1, ?2, ?3, ?4, strftime('%s','now'), ?5)",
            rusqlite::params![path, artist, title, station, incomplete as i64],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// All recordings, newest first.
    pub fn recordings(&self) -> Result<Vec<crate::model::RecordingItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, artist, title, station, recorded_at, incomplete
             FROM recording ORDER BY recorded_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::model::RecordingItem {
                id: r.get(0)?,
                path: r.get(1)?,
                artist: r.get(2)?,
                title: r.get(3)?,
                station: r.get(4)?,
                recorded_at: r.get(5)?,
                incomplete: r.get::<_, i64>(6)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Removes a recording from management and returns its file path
    /// (so that the caller can delete the file).
    pub fn delete_recording(&self, id: i64) -> Result<Option<String>> {
        let path: Option<String> = self
            .conn
            .query_row("SELECT path FROM recording WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        self.conn.execute("DELETE FROM recording WHERE id = ?1", [id])?;
        Ok(path)
    }

    /// All episodes along with podcast info (for the "Newest" view). The
    /// chronological sorting by publication date is handled by the UI
    /// (the stored date is only text).
    pub fn all_episodes(&self) -> Result<Vec<crate::model::EpisodeRef>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.title, p.image_url, e.title, e.audio_url, e.published, e.duration, e.description
             FROM episode e JOIN podcast p ON p.id = e.podcast_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::model::EpisodeRef {
                podcast_title: r.get(0)?,
                podcast_image: r.get(1)?,
                title: r.get(2)?,
                audio_url: r.get(3)?,
                published: r.get(4)?,
                duration: r.get(5)?,
                description: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(path: &str, artist: Option<&str>, album: Option<&str>) -> Track {
        Track {
            id: 0,
            path: path.to_string(),
            title: "T".to_string(),
            artist: artist.map(String::from),
            album: album.map(String::from),
            genre: None,
            track_no: None,
            disc_no: None,
            duration_ms: Some(60_000),
            resume_ms: 0,
        }
    }

    #[test]
    fn play_events_aggregate_into_stats() {
        let lib = Library::open_in_memory().unwrap();
        lib.upsert_track(&track("/m/a1.mp3", Some("Alice"), Some("Album X")))
            .unwrap();
        lib.upsert_track(&track("/m/a2.mp3", Some("Alice feat. Bob"), Some("Album X")))
            .unwrap();
        lib.upsert_track(&track("/m/c1.mp3", Some("Carol"), Some("Album Y")))
            .unwrap();

        // Duration of the test tracks is 60 s → threshold effectively 30 s.
        let t0: i64 = 1_700_000_000;
        lib.log_play("/m/a1.mp3", t0, 45_000, 60_000, true, Some("queue")).unwrap();
        lib.log_play("/m/a1.mp3", t0 + 100, 50_000, 60_000, true, None).unwrap();
        lib.log_play("/m/a2.mp3", t0 + 200, 40_000, 60_000, false, None).unwrap();
        lib.log_play("/m/c1.mp3", t0 + 300, 5_000, 60_000, false, None).unwrap(); // skip

        let tot = lib.stats_totals(0).unwrap();
        assert_eq!(tot.plays, 3);
        assert_eq!(tot.skips, 1);
        assert_eq!(tot.total_played_ms, 45_000 + 50_000 + 40_000 + 5_000);
        assert_eq!(tot.distinct_tracks, 2); // a1, a2 (c1 only a skip)
        assert_eq!(tot.distinct_artists, 1); // Alice (a2 falls onto Alice via primary)
        assert_eq!(tot.distinct_albums, 1); // Album X (Album Y only a skip)

        let tracks = lib.stats_top_tracks(0, 10).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].plays, 2); // a1 twice
        assert_eq!(tracks[0].detail, "Alice");

        let artists = lib.stats_top_artists(0, 10).unwrap();
        assert_eq!(artists.len(), 1);
        assert_eq!(artists[0].name, "Alice");
        assert_eq!(artists[0].plays, 3); // a1×2 + a2×1, folded

        let albums = lib.stats_top_albums(0, 10).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].name, "Album X");
        assert_eq!(albums[0].plays, 3);
        assert_eq!(albums[0].detail, "Alice");

        // last_played is tracked (forward: the later event wins).
        let lp: Option<i64> = lib
            .conn
            .query_row("SELECT last_played FROM track WHERE path = ?1", ["/m/a1.mp3"], |r| r.get(0))
            .unwrap();
        assert_eq!(lp, Some(t0 + 100));

        // Distributions preserve the total time (checked timezone-independently).
        assert_eq!(lib.stats_by_hour(0).unwrap().iter().sum::<i64>(), tot.total_played_ms);
        assert_eq!(lib.stats_by_weekday(0).unwrap().iter().sum::<i64>(), tot.total_played_ms);

        // since filter: from t0+250 only the skip (c1) remains.
        let recent = lib.stats_totals(t0 + 250).unwrap();
        assert_eq!(recent.plays, 0);
        assert_eq!(recent.skips, 1);
    }

    #[test]
    fn meta_attempts_count_failures_and_reset_on_success() {
        let lib = Library::open_in_memory().unwrap();
        let mut m = AlbumMeta::pending("A", "B");

        // Every unsuccessful fetch counts up.
        m.status = "notfound".to_string();
        lib.upsert_album_meta(&m).unwrap();
        assert_eq!(lib.album_attempts("A", "B"), 1);
        m.status = "error".to_string();
        lib.upsert_album_meta(&m).unwrap();
        assert_eq!(lib.album_attempts("A", "B"), 2);

        // Success ('matched' or a locally found cover) resets it.
        m.status = "matched".to_string();
        lib.upsert_album_meta(&m).unwrap();
        assert_eq!(lib.album_attempts("A", "B"), 0);

        m.status = "notfound".to_string();
        lib.upsert_album_meta(&m).unwrap();
        m.status = "local".to_string();
        lib.upsert_album_meta(&m).unwrap();
        assert_eq!(lib.album_attempts("A", "B"), 0);
    }

    #[test]
    fn podcast_subscribe_and_episodes() {
        let lib = Library::open_in_memory().unwrap();
        let id = lib
            .subscribe_podcast("Mein Podcast", "https://feed.example/rss", Some("https://img"))
            .unwrap();
        // Re-subscribing to the same feed → same ID (upsert), no duplicate.
        let id2 = lib
            .subscribe_podcast("Mein Podcast (neu)", "https://feed.example/rss", None)
            .unwrap();
        assert_eq!(id, id2);

        let eps = vec![
            Episode {
                guid: Some("g1".into()),
                title: "E1".into(),
                audio_url: "https://a/1.mp3".into(),
                published: Some("Mon, 01 Jan 2024".into()),
                duration: Some("10:00".into()),
                description: Some("Shownotes 1".into()),
            },
            Episode {
                guid: None,
                title: "E2".into(),
                audio_url: "https://a/2.mp3".into(),
                published: None,
                duration: None,
                description: None,
            },
        ];
        lib.set_episodes(id, &eps).unwrap();

        let got = lib.episodes(id).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].title, "E1");
        assert_eq!(got[0].description.as_deref(), Some("Shownotes 1"));
        assert_eq!(got[1].audio_url, "https://a/2.mp3");

        let list = lib.podcasts().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!((list[0].0, list[0].1.as_str(), list[0].3), (id, "Mein Podcast (neu)", 2));

        lib.delete_podcast(id).unwrap();
        assert!(lib.podcasts().unwrap().is_empty());
        assert!(lib.episodes(id).unwrap().is_empty());
    }

    #[test]
    fn playlist_crud_and_items() {
        let lib = Library::open_in_memory().unwrap();
        let id = lib.create_playlist("Roadtrip").unwrap();
        assert_eq!(lib.playlists().unwrap(), vec![(id, "Roadtrip".to_string(), 0)]);

        // Appending preserves the order (across two calls).
        lib.add_to_playlist(id, &["/a.mp3".into(), "/b.mp3".into()])
            .unwrap();
        lib.add_to_playlist(id, &["/c.mp3".into()]).unwrap();
        assert_eq!(
            lib.playlist_paths(id).unwrap(),
            vec!["/a.mp3", "/b.mp3", "/c.mp3"]
        );
        assert_eq!(lib.playlists().unwrap()[0].2, 3); // track count

        lib.rename_playlist(id, "Tour").unwrap();
        assert_eq!(lib.playlists().unwrap()[0].1, "Tour");

        lib.remove_from_playlist(id, "/b.mp3").unwrap();
        assert_eq!(lib.playlist_paths(id).unwrap(), vec!["/a.mp3", "/c.mp3"]);

        lib.delete_playlist(id).unwrap();
        assert!(lib.playlists().unwrap().is_empty());
        assert!(lib.playlist_paths(id).unwrap().is_empty());
    }

    #[test]
    fn area_filtering_hides_from_listings() {
        use crate::core::category::{album_key, areas_value, Area};
        let lib = Library::open_in_memory().unwrap();
        lib.upsert_track(&track("/x/1.mp3", Some("X"), Some("Y")))
            .unwrap();
        // Default: visible in albums and artists.
        assert_eq!(lib.albums_overview().unwrap().len(), 1);
        assert_eq!(lib.artists_overview().unwrap().len(), 1);

        // Take the album out of "Albums" (now only filesystem + artists).
        lib.set_category(
            "album",
            &album_key("X", "Y"),
            Some(&areas_value(&[Area::Filesystem, Area::Artists])),
        )
        .unwrap();
        assert!(lib.albums_overview().unwrap().is_empty());
        assert_eq!(lib.artists_overview().unwrap().len(), 1);

        // Hide the artist completely.
        lib.set_category("artist", "X", Some("")).unwrap();
        assert!(lib.artists_overview().unwrap().is_empty());
    }

    #[test]
    fn album_inherits_parent_folder_area() {
        use crate::core::category::Area;
        let lib = Library::open_in_memory().unwrap();
        lib.upsert_track(&track("/musik/Live/1.mp3", Some("X"), Some("Konzert")))
            .unwrap();
        // Default: the album is visible in the "Albums" area.
        assert!(lib.album_areas("X", "Konzert").contains(&Area::Albums));
        // Hide the parent folder (empty area list).
        lib.set_category("folder", "/musik/Live", Some("")).unwrap();
        // The album without its own setting now inherits the folder → hidden.
        assert!(lib.album_areas("X", "Konzert").is_empty());
        // Its own album setting still wins (non-destructive).
        lib.set_category("album", &crate::core::category::album_key("X", "Konzert"), Some("albums"))
            .unwrap();
        assert!(lib.album_areas("X", "Konzert").contains(&Area::Albums));
    }

    #[test]
    fn albums_overview_merges_feat_variants() {
        let lib = Library::open_in_memory().unwrap();
        for (path, artist) in [
            ("/1.mp3", "Beginner"),
            ("/2.mp3", "Beginner feat. Megaloh"),
            ("/3.mp3", "Beginner feat. Gzuz & Gentleman"),
        ] {
            lib.upsert_track(&track(path, Some(artist), Some("Advanced Chemistry")))
                .unwrap();
        }
        let albums = lib.albums_overview().unwrap();
        let ac: Vec<_> = albums
            .iter()
            .filter(|a| a.album == "Advanced Chemistry")
            .collect();
        // feat. variants of the same primary artist → exactly ONE card.
        assert_eq!(ac.len(), 1);
        assert_eq!(ac[0].artist, "Beginner");
        assert_eq!(ac[0].track_count, 3);
    }

    #[test]
    fn albums_overview_drops_ambiguous_cover_for_compilations() {
        let lib = Library::open_in_memory().unwrap();
        // Compilation: two artists with different covers → neutral.
        for (path, artist, cover) in [
            ("/c1.mp3", "DJ A", "/covers/a.jpg"),
            ("/c2.mp3", "DJ B", "/covers/b.jpg"),
        ] {
            lib.upsert_track(&track(path, Some(artist), Some("Dancemix 2009")))
                .unwrap();
            let mut m = crate::model::AlbumMeta::pending(artist, "Dancemix 2009");
            m.cover_path = Some(cover.to_string());
            m.status = "local".to_string();
            lib.upsert_album_meta(&m).unwrap();
        }
        let dm = lib
            .albums_overview()
            .unwrap()
            .into_iter()
            .find(|a| a.album == "Dancemix 2009")
            .unwrap();
        assert_eq!(dm.cover_path, None);

        // Real album by one artist → cover is retained.
        lib.upsert_track(&track("/d1.mp3", Some("Solo"), Some("Werk")))
            .unwrap();
        let mut m = crate::model::AlbumMeta::pending("Solo", "Werk");
        m.cover_path = Some("/covers/werk.jpg".to_string());
        m.status = "local".to_string();
        lib.upsert_album_meta(&m).unwrap();
        let werk = lib
            .albums_overview()
            .unwrap()
            .into_iter()
            .find(|a| a.album == "Werk")
            .unwrap();
        assert_eq!(werk.cover_path.as_deref(), Some("/covers/werk.jpg"));
    }

    #[test]
    fn albums_overview_groups_by_name_ignoring_artist() {
        let lib = Library::open_in_memory().unwrap();
        // Same album name, different artists → exactly ONE card.
        for (path, artist) in [
            ("/a1.mp3", "Artist A"),
            ("/a2.mp3", "Artist A"),
            ("/b1.mp3", "Artist B"),
        ] {
            lib.upsert_track(&track(path, Some(artist), Some("Live")))
                .unwrap();
        }
        let live: Vec<_> = lib
            .albums_overview()
            .unwrap()
            .into_iter()
            .filter(|a| a.album == "Live")
            .collect();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].track_count, 3);
        // Display artist = the one with the most tracks (A: 2 > B: 1).
        assert_eq!(live[0].artist, "Artist A");
    }

    #[test]
    fn multi_disc_tracks_ordered_by_disc_then_track() {
        let lib = Library::open_in_memory().unwrap();
        // Two CDs, deliberately inserted "the wrong way round".
        let rows = [
            ("/al/d2t2.mp3", 2u32, 2u32),
            ("/al/d1t1.mp3", 1, 1),
            ("/al/d2t1.mp3", 2, 1),
            ("/al/d1t2.mp3", 1, 2),
        ];
        for (path, disc, no) in rows {
            let mut t = track(path, Some("X"), Some("Doppelalbum"));
            t.disc_no = Some(disc);
            t.track_no = Some(no);
            lib.upsert_track(&t).unwrap();
        }
        let got: Vec<(Option<u32>, Option<u32>)> = lib
            .all_tracks()
            .unwrap()
            .into_iter()
            .map(|t| (t.disc_no, t.track_no))
            .collect();
        // First disc 1 (track 1,2), then disc 2 (track 1,2).
        assert_eq!(
            got,
            vec![
                (Some(1), Some(1)),
                (Some(1), Some(2)),
                (Some(2), Some(1)),
                (Some(2), Some(2)),
            ]
        );
    }

    #[test]
    fn resume_roundtrip_by_path() {
        let lib = Library::open_in_memory().unwrap();
        lib.upsert_track(&track("/a/hoerspiel.mp3", Some("X"), Some("Y")))
            .unwrap();

        // A freshly scanned track has no resume position.
        let t = lib.track_by_path("/a/hoerspiel.mp3").unwrap().unwrap();
        assert_eq!(t.resume_ms, 0);

        // Store the position and read it back.
        lib.set_resume_path("/a/hoerspiel.mp3", 123_456).unwrap();
        let t = lib.track_by_path("/a/hoerspiel.mp3").unwrap().unwrap();
        assert_eq!(t.resume_ms, 123_456);

        // Reset (track listened to the end).
        lib.set_resume_path("/a/hoerspiel.mp3", 0).unwrap();
        assert_eq!(lib.track_by_path("/a/hoerspiel.mp3").unwrap().unwrap().resume_ms, 0);
    }

    #[test]
    fn track_by_path_unknown_is_none_and_setresume_noop() {
        let lib = Library::open_in_memory().unwrap();
        assert!(lib.track_by_path("/nicht/da.mp3").unwrap().is_none());
        // Unknown path: no error, no effect.
        lib.set_resume_path("/nicht/da.mp3", 5000).unwrap();
        assert!(lib.track_by_path("/nicht/da.mp3").unwrap().is_none());
    }

    #[test]
    fn area_cascade_resolution() {
        use crate::core::category::Area;
        let lib = Library::open_in_memory().unwrap();
        // Without a setting: default = filesystem/artists/albums.
        assert_eq!(
            lib.resolve_areas(Some("X"), Some("Y"), "/a/1.mp3"),
            Area::DEFAULT.to_vec()
        );

        // Artist level = audiobooks only → inherited by album and track.
        lib.set_category("artist", "X", Some("audiobooks")).unwrap();
        assert_eq!(
            lib.resolve_areas(Some("X"), Some("Y"), "/a/1.mp3"),
            vec![Area::Audiobooks]
        );
        assert_eq!(lib.album_areas("X", "Y"), vec![Area::Audiobooks]);

        // Track level wins: empty list = hidden.
        lib.set_category("track", "/a/1.mp3", Some("")).unwrap();
        assert!(lib
            .resolve_areas(Some("X"), Some("Y"), "/a/1.mp3")
            .is_empty());
        // album_areas/artist_areas ignore the track level.
        assert_eq!(lib.album_areas("X", "Y"), vec![Area::Audiobooks]);
    }

    // ---- Equalizer cascade ----

    fn bands(v: f64) -> [f64; 10] {
        [v; 10]
    }

    #[test]
    fn eq_none_when_unset() {
        let lib = Library::open_in_memory().unwrap();
        assert_eq!(lib.resolve_eq("", Some("X"), Some("Y"), "/a/1.mp3"), None);
        assert_eq!(lib.resolve_eq("sink1", Some("X"), Some("Y"), "/a/1.mp3"), None);
    }

    #[test]
    fn eq_specificity_track_over_album_over_artist_over_global() {
        let lib = Library::open_in_memory().unwrap();
        let ak = crate::core::category::album_key("X", "Y");
        lib.set_eq("", "global", "", &bands(1.0)).unwrap();
        lib.set_eq("", "artist", "X", &bands(2.0)).unwrap();
        lib.set_eq("", "album", &ak, &bands(3.0)).unwrap();
        lib.set_eq("", "track", "/a/1.mp3", &bands(4.0)).unwrap();

        // The most specific level wins; after removal the next-higher one takes effect.
        let r = |l: &Library| l.resolve_eq("", Some("X"), Some("Y"), "/a/1.mp3");
        assert_eq!(r(&lib), Some(bands(4.0)));
        lib.clear_eq("", "track", "/a/1.mp3").unwrap();
        assert_eq!(r(&lib), Some(bands(3.0)));
        lib.clear_eq("", "album", &ak).unwrap();
        assert_eq!(r(&lib), Some(bands(2.0)));
        lib.clear_eq("", "artist", "X").unwrap();
        assert_eq!(r(&lib), Some(bands(1.0)));
        lib.clear_eq("", "global", "").unwrap();
        assert_eq!(r(&lib), None);
    }

    #[test]
    fn eq_concrete_output_cascade_beats_default_output() {
        let lib = Library::open_in_memory().unwrap();
        // Default output: specific track setting.
        lib.set_eq("", "track", "/a/1.mp3", &bands(4.0)).unwrap();
        // Concrete output: only a global setting.
        lib.set_eq("sink1", "global", "", &bands(9.0)).unwrap();
        // Documented behavior: the concrete output is resolved completely first
        // -- its global beats the track of the default output.
        assert_eq!(
            lib.resolve_eq("sink1", Some("X"), Some("Y"), "/a/1.mp3"),
            Some(bands(9.0))
        );
        // For the default output itself the track setting still applies.
        assert_eq!(
            lib.resolve_eq("", Some("X"), Some("Y"), "/a/1.mp3"),
            Some(bands(4.0))
        );
    }

    #[test]
    fn eq_falls_back_to_default_output() {
        let lib = Library::open_in_memory().unwrap();
        lib.set_eq("", "global", "", &bands(1.0)).unwrap();
        // Concrete output has nothing → fall back to the default output.
        assert_eq!(
            lib.resolve_eq("sink1", Some("X"), Some("Y"), "/a/1.mp3"),
            Some(bands(1.0))
        );
    }

    #[test]
    fn eq_album_key_avoids_cross_artist_collision() {
        let lib = Library::open_in_memory().unwrap();
        let ak = crate::core::category::album_key("X", "Y");
        lib.set_eq("", "album", &ak, &bands(3.0)).unwrap();
        // Same album name, different artist → no match at the album level.
        assert_eq!(lib.resolve_eq("", Some("Z"), Some("Y"), "/a/1.mp3"), None);
        // Correct artist → match.
        assert_eq!(
            lib.resolve_eq("", Some("X"), Some("Y"), "/a/1.mp3"),
            Some(bands(3.0))
        );
    }

    #[test]
    fn prune_removes_only_missing_files_under_root() {
        let lib = Library::open_in_memory().unwrap();
        lib.upsert_track(&track("/music/a.mp3", Some("A"), Some("X"))).unwrap();
        lib.upsert_track(&track("/music/gone.mp3", Some("A"), Some("X"))).unwrap();
        // A remote (Nextcloud) track and a track from another folder must survive.
        lib.upsert_track(&track("nc:7:Album/r.mp3", Some("A"), Some("X"))).unwrap();
        lib.upsert_track(&track("/other/b.mp3", Some("B"), Some("Y"))).unwrap();

        // Scan of /music found only a.mp3 (gone.mp3 was deleted on disk).
        let present = vec!["/music/a.mp3".to_string()];
        let removed = lib
            .prune_tracks_under(std::path::Path::new("/music"), &present)
            .unwrap();
        assert_eq!(removed, 1);
        assert!(lib.track_by_path("/music/a.mp3").unwrap().is_some());
        assert!(lib.track_by_path("/music/gone.mp3").unwrap().is_none());
        assert!(lib.track_by_path("nc:7:Album/r.mp3").unwrap().is_some());
        assert!(lib.track_by_path("/other/b.mp3").unwrap().is_some());
    }

    #[test]
    fn prune_with_empty_scan_keeps_everything() {
        // Guards against a transiently unreadable/unmounted folder wiping the DB.
        let lib = Library::open_in_memory().unwrap();
        lib.upsert_track(&track("/music/a.mp3", Some("A"), Some("X"))).unwrap();
        let removed = lib
            .prune_tracks_under(std::path::Path::new("/music"), &[])
            .unwrap();
        assert_eq!(removed, 0);
        assert!(lib.track_by_path("/music/a.mp3").unwrap().is_some());
    }

    #[test]
    fn prune_escapes_like_metacharacters_in_root() {
        // A root containing `%`/`_` must match literally, not as LIKE wildcards.
        let lib = Library::open_in_memory().unwrap();
        lib.upsert_track(&track("/m%/keep.mp3", Some("A"), Some("X"))).unwrap();
        lib.upsert_track(&track("/mX/other.mp3", Some("A"), Some("X"))).unwrap();
        // Scan of "/m%" found nothing under it → keep.mp3 is an orphan there,
        // but "/mX/other.mp3" must NOT be touched (would match if `%` were a
        // wildcard).
        let removed = lib
            .prune_tracks_under(std::path::Path::new("/m%"), &["/m%/x.mp3".to_string()])
            .unwrap();
        assert_eq!(removed, 1);
        assert!(lib.track_by_path("/m%/keep.mp3").unwrap().is_none());
        assert!(lib.track_by_path("/mX/other.mp3").unwrap().is_some());
    }

    #[test]
    fn upsert_tracks_batch_inserts_all() {
        let lib = Library::open_in_memory().unwrap();
        let batch = vec![
            track("/m/1.mp3", Some("A"), Some("X")),
            track("/m/2.mp3", Some("A"), Some("X")),
            track("/m/3.mp3", Some("B"), Some("Y")),
        ];
        assert_eq!(lib.upsert_tracks(&batch).unwrap(), 3);
        assert!(lib.track_by_path("/m/2.mp3").unwrap().is_some());
        // Re-running upserts (no duplicates, ON CONFLICT path).
        assert_eq!(lib.upsert_tracks(&batch).unwrap(), 3);
        assert_eq!(lib.all_tracks().unwrap().len(), 3);
    }
}
