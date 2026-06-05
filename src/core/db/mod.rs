//! SQLite library index (rusqlite).

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use std::path::PathBuf;
use std::sync::Once;
use std::time::Duration;

use crate::model::{AlbumHit, SearchResults, SongHit, Source, Track};

// Further `impl Library` blocks, split out of this file by concern.
mod album;
mod artist;
mod category;
mod eq;
mod favorites;
mod gallery;
mod playlist;
mod podcast;
mod stats;
mod stream;
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

/// The on-disk schema only needs migrating **once per process**: later worker
/// connections (online enrichment, sync, stats, …) reuse the already-migrated
/// file. `Once` both skips the redundant work (each `migrate()` probes ~15
/// columns via `pragma_table_info`) and serialises the very first migration, so
/// concurrent first opens cannot race on the `ALTER TABLE` statements.
static FILE_DB_MIGRATED: Once = Once::new();

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
                enabled INTEGER NOT NULL DEFAULT 1,
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
                created_at INTEGER,
                origin     TEXT    -- NULL = user playlist; else the source key
                                   -- (e.g. a mirrored YouTube playlist URL)
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

            -- Subscribed YouTube channels (the "bell"): newest videos are
            -- refreshed on startup like podcast feeds. Optional feature; the
            -- extractor (yt-dlp) is downloaded at runtime, never bundled.
            CREATE TABLE IF NOT EXISTS yt_channel (
                id         INTEGER PRIMARY KEY,
                channel_id TEXT NOT NULL UNIQUE,  -- YouTube channel id/handle
                title      TEXT NOT NULL,
                url        TEXT NOT NULL,
                thumbnail  TEXT,
                added_at   INTEGER
            );
            -- Cached newest videos of a subscribed channel (replaced on refresh,
            -- like `episode`; nothing is downloaded).
            CREATE TABLE IF NOT EXISTS yt_video (
                channel_id INTEGER NOT NULL,
                position   INTEGER NOT NULL,
                video_id   TEXT NOT NULL,
                title      TEXT NOT NULL,
                url        TEXT NOT NULL,
                duration   INTEGER,
                published  TEXT,
                thumbnail  TEXT,
                PRIMARY KEY (channel_id, position)
            );
            -- Offline-downloaded YouTube audio, keyed by video id (mirror
            -- `episode_download`). Playback prefers `path` over re-resolving.
            CREATE TABLE IF NOT EXISTS yt_download (
                video_id      TEXT PRIMARY KEY,
                path          TEXT NOT NULL,
                downloaded_at INTEGER NOT NULL DEFAULT 0
            );
            -- Recently played YouTube items (history). `kind` = 'video' (keyed
            -- by video id) or 'playlist' (keyed by playlist URL, `count` = number
            -- of songs). `artist` is filled in by the on-play enrichment.
            CREATE TABLE IF NOT EXISTS yt_recent (
                video_id  TEXT PRIMARY KEY,
                title     TEXT NOT NULL,
                artist    TEXT,
                thumbnail TEXT,
                played_at INTEGER NOT NULL DEFAULT 0,
                kind      TEXT NOT NULL DEFAULT 'video',
                count     INTEGER NOT NULL DEFAULT 0
            );
            -- Title cache for `yt:<id>` tracks, so playlist/queue entries show a
            -- name instead of their id without polluting the library. `duration`
            -- (seconds) lets those rows show a runtime even though `yt:` tracks
            -- are not stored in `track`.
            CREATE TABLE IF NOT EXISTS yt_title (
                video_id TEXT PRIMARY KEY,
                title    TEXT NOT NULL,
                duration INTEGER
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

        // Migration: EQ bypass flag. Existing settings stay active; "Turn off"
        // only flips this flag and keeps the saved bands intact.
        let has_eq_enabled = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('eq_setting') WHERE name = 'enabled'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_eq_enabled {
            self.conn.execute_batch(
                "ALTER TABLE eq_setting ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;",
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

        // Migration: yt_recent gained `kind`/`count` columns (playlists in the
        // YouTube "Recent" history).
        let has_yt_kind = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('yt_recent') WHERE name = 'kind'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_yt_kind {
            self.conn.execute_batch(
                "ALTER TABLE yt_recent ADD COLUMN kind TEXT NOT NULL DEFAULT 'video';
                 ALTER TABLE yt_recent ADD COLUMN count INTEGER NOT NULL DEFAULT 0;",
            )?;
        }

        // Migration: playlists gained an `origin` marker so a mirrored YouTube
        // playlist can be replaced/looked up by its source URL instead of by
        // name – which used to clobber a user playlist of the same name.
        let has_origin = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('playlist') WHERE name = 'origin'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_origin {
            self.conn
                .execute_batch("ALTER TABLE playlist ADD COLUMN origin TEXT;")?;
        }

        // Migration: yt_title gained a `duration` (seconds) so queue/playlist
        // rows can show the runtime of `yt:` tracks (which are not in `track`).
        let has_yt_duration = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('yt_title') WHERE name = 'duration'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_yt_duration {
            self.conn
                .execute_batch("ALTER TABLE yt_title ADD COLUMN duration INTEGER;")?;
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

        // Migration: playlists gained a chosen cover (derived from their songs;
        // the user can pick one in the detail view when several covers exist).
        let has_pl_cover = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('playlist') WHERE name = 'cover_path'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_pl_cover {
            self.conn
                .execute_batch("ALTER TABLE playlist ADD COLUMN cover_path TEXT;")?;
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
            self.conn
                .execute("DELETE FROM setting WHERE key = ?1", [key])?;
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
            .query_row(
                "SELECT COALESCE(MAX(position), -1) + 1 FROM source",
                [],
                |r| r.get(0),
            )
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
                    let _ =
                        self.set_source_password(s.id, Some(&secrets::source_password_ref(s.id)));
                }
            }
            if let Some(user) = s.username.as_deref() {
                if !user.is_empty()
                    && !user.starts_with(secrets::SECRET_PREFIX)
                    && secrets::store_source_username(s.id, &label, user)
                    && secrets::lookup_source_username(s.id).as_deref() == Some(user)
                {
                    let _ =
                        self.set_source_username(s.id, Some(&secrets::source_username_ref(s.id)));
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
        self.conn.execute(
            "DELETE FROM track WHERE path LIKE ?1",
            [format!("nc:{id}:%")],
        )?;
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
        let rows = stmt.query_map([like], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
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
        self.conn
            .execute(UPSERT_TRACK_SQL, track_upsert_params!(t))?;
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

    /// Upserts a batch like [`upsert_tracks`], but if the batched transaction
    /// fails it falls back to per-track upserts so a single bad row cannot drop
    /// the whole chunk. Never returns an error (best effort) — used by the
    /// library scan and cloud indexing, where one odd file must not abort the
    /// entire run. Returns how many tracks were stored.
    pub fn upsert_tracks_resilient(&self, tracks: &[Track]) -> usize {
        if tracks.is_empty() {
            return 0;
        }
        match self.upsert_tracks(tracks) {
            Ok(c) => c,
            Err(_) => tracks
                .iter()
                .filter(|t| self.upsert_track(t).is_ok())
                .count(),
        }
    }

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
            let rows = stmt.query_map(rusqlite::params![like, lim], |r| r.get::<_, String>(0))?;
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
            let rows = stmt.query_map(rusqlite::params![like, year, lim], |r| {
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
            let rows = stmt.query_map(rusqlite::params![like, lim], |r| {
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

        Ok(SearchResults {
            artists,
            albums,
            songs,
        })
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
            tracing::info!(
                "Scan: pruned {removed} orphaned track(s) under {}",
                root.display()
            );
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

    /// Looks up many tracks by path in one (chunked) query, returning a
    /// `path -> Track` map. Avoids an N+1 of [`track_by_path`] when resolving a
    /// whole queue's or playlist's metadata at once.
    pub fn tracks_by_paths(
        &self,
        paths: &[String],
    ) -> Result<std::collections::HashMap<String, Track>> {
        let mut map = std::collections::HashMap::with_capacity(paths.len());
        // SQLite caps the number of bound parameters; chunk well under the limit.
        for chunk in paths.chunks(900) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT id, path, title, artist, album, track_no, duration_ms, resume_ms, disc_no
                 FROM track WHERE path IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk), |r| {
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
            for t in rows {
                let t = t?;
                map.insert(t.path.clone(), t);
            }
        }
        Ok(map)
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

    /// Tracks of one album name only, sorted for album playback/subpages. This
    /// avoids loading the whole library when opening a single album.
    pub fn tracks_by_album_name(&self, album: &str) -> Result<Vec<Track>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, title, artist, album, track_no, duration_ms, resume_ms, disc_no
             FROM track
             WHERE album = ?1 COLLATE NOCASE
             ORDER BY COALESCE(disc_no, 1), track_no, path",
        )?;
        let rows = stmt.query_map([album], |r| {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    // Types used only by tests (their production callers moved to submodules).
    use crate::model::{AlbumMeta, Episode};

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
        lib.upsert_track(&track(
            "/m/a2.mp3",
            Some("Alice feat. Bob"),
            Some("Album X"),
        ))
        .unwrap();
        lib.upsert_track(&track("/m/c1.mp3", Some("Carol"), Some("Album Y")))
            .unwrap();

        // Duration of the test tracks is 60 s → threshold effectively 30 s.
        let t0: i64 = 1_700_000_000;
        lib.log_play("/m/a1.mp3", t0, 45_000, 60_000, true, Some("queue"))
            .unwrap();
        lib.log_play("/m/a1.mp3", t0 + 100, 50_000, 60_000, true, None)
            .unwrap();
        lib.log_play("/m/a2.mp3", t0 + 200, 40_000, 60_000, false, None)
            .unwrap();
        lib.log_play("/m/c1.mp3", t0 + 300, 5_000, 60_000, false, None)
            .unwrap(); // skip

        let tot = lib.stats_totals(0).unwrap();
        assert_eq!(tot.plays, 3);
        assert_eq!(tot.skips, 1);
        assert_eq!(tot.total_played_ms, 45_000 + 50_000 + 40_000 + 5_000);
        assert_eq!(tot.distinct_tracks, 2); // a1, a2 (c1 only a skip)
                                            // stats_totals leaves distinct_artists/albums at 0 — the caller fills
                                            // them from the full top lists, whose lengths (1 and 1) are asserted
                                            // below: 1 artist (Alice, a2 folds onto her) and 1 album (Album X).
        assert_eq!(tot.distinct_artists, 0);
        assert_eq!(tot.distinct_albums, 0);

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
            .query_row(
                "SELECT last_played FROM track WHERE path = ?1",
                ["/m/a1.mp3"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(lp, Some(t0 + 100));

        // Distributions preserve the total time (checked timezone-independently).
        assert_eq!(
            lib.stats_by_hour(0).unwrap().iter().sum::<i64>(),
            tot.total_played_ms
        );
        assert_eq!(
            lib.stats_by_weekday(0).unwrap().iter().sum::<i64>(),
            tot.total_played_ms
        );

        // since filter: from t0+250 only the skip (c1) remains.
        let recent = lib.stats_totals(t0 + 250).unwrap();
        assert_eq!(recent.plays, 0);
        assert_eq!(recent.skips, 1);
    }

    #[test]
    fn meta_attempts_count_failures_and_reset_on_cover() {
        let lib = Library::open_in_memory().unwrap();
        let mut m = AlbumMeta::pending("A", "B");

        // Every unsuccessful fetch counts up.
        m.status = "notfound".to_string();
        lib.upsert_album_meta(&m).unwrap();
        assert_eq!(lib.album_attempts("A", "B"), 1);
        m.status = "error".to_string();
        lib.upsert_album_meta(&m).unwrap();
        assert_eq!(lib.album_attempts("A", "B"), 2);

        // A bare "matched" *without* a cover is still an unsuccessful cover
        // attempt – otherwise the cover-less album would be re-queried on every
        // sweep forever and never reach MAX_ATTEMPTS.
        m.status = "matched".to_string();
        lib.upsert_album_meta(&m).unwrap();
        assert_eq!(lib.album_attempts("A", "B"), 3);

        // Only an actual cover (matched online or extracted locally) resets it.
        m.cover_path = Some("/cache/cover.img".to_string());
        lib.upsert_album_meta(&m).unwrap();
        assert_eq!(lib.album_attempts("A", "B"), 0);

        // A fresh failure starts counting again.
        m.status = "notfound".to_string();
        m.cover_path = None;
        lib.upsert_album_meta(&m).unwrap();
        assert_eq!(lib.album_attempts("A", "B"), 1);

        // A locally found cover resets as well.
        m.status = "local".to_string();
        m.cover_path = Some("/cache/local.img".to_string());
        lib.upsert_album_meta(&m).unwrap();
        assert_eq!(lib.album_attempts("A", "B"), 0);
    }

    #[test]
    fn podcast_subscribe_and_episodes() {
        let lib = Library::open_in_memory().unwrap();
        let id = lib
            .subscribe_podcast(
                "Mein Podcast",
                "https://feed.example/rss",
                Some("https://img"),
            )
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
        assert_eq!(
            (list[0].0, list[0].1.as_str(), list[0].3),
            (id, "Mein Podcast (neu)", 2)
        );

        lib.delete_podcast(id).unwrap();
        assert!(lib.podcasts().unwrap().is_empty());
        assert!(lib.episodes(id).unwrap().is_empty());
    }

    #[test]
    fn playlist_crud_and_items() {
        let lib = Library::open_in_memory().unwrap();
        let id = lib.create_playlist("Roadtrip").unwrap();
        assert_eq!(
            lib.playlists().unwrap(),
            vec![(id, "Roadtrip".to_string(), 0)]
        );

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
    fn youtube_channels_videos_downloads_and_progress() {
        let lib = Library::open_in_memory().unwrap();
        // Subscribe (idempotent on channel_id) and list.
        let cid = lib
            .subscribe_channel("UC123", "Some Channel", "https://yt/UC123", Some("t.jpg"))
            .unwrap();
        assert_eq!(
            lib.subscribe_channel("UC123", "Renamed", "https://yt/UC123", None)
                .unwrap(),
            cid
        );
        let channels = lib.channels().unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].1, "Renamed");

        // Replace cached videos and read them back in order.
        let videos = vec![
            crate::model::YtVideo {
                video_id: "v1".into(),
                title: "First".into(),
                url: "https://yt/watch?v=v1".into(),
                duration: Some(200),
                published: None,
                thumbnail: None,
            },
            crate::model::YtVideo {
                video_id: "v2".into(),
                title: "Second".into(),
                url: "https://yt/watch?v=v2".into(),
                duration: None,
                published: None,
                thumbnail: None,
            },
        ];
        lib.set_channel_videos(cid, &videos).unwrap();
        let read = lib.channel_videos(cid).unwrap();
        assert_eq!(
            read.iter().map(|v| v.video_id.as_str()).collect::<Vec<_>>(),
            ["v1", "v2"]
        );
        assert_eq!(lib.channels().unwrap()[0].4, 2); // video count
        assert_eq!(lib.all_videos().unwrap().len(), 2);

        // Deleting the channel removes its cached videos too.
        lib.delete_channel(cid).unwrap();
        assert!(lib.channels().unwrap().is_empty());
        assert!(lib.all_videos().unwrap().is_empty());
    }

    #[test]
    fn youtube_recent_history_orders_and_enriches() {
        let lib = Library::open_in_memory().unwrap();
        lib.add_recent_video("a", "First", None).unwrap();
        lib.add_recent_video("b", "Second", Some("http://thumb/b.jpg"))
            .unwrap();
        // Re-playing "a" moves it back to the top.
        lib.add_recent_video("a", "First", None).unwrap();
        let recent = lib.recent_videos(10).unwrap();
        assert_eq!(
            recent
                .iter()
                .map(|r| r.video_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        // Enrichment fills the artist.
        lib.set_recent_meta("a", Some("The Artist"), Some("/cache/a.img"))
            .unwrap();
        let a = lib
            .recent_videos(10)
            .unwrap()
            .into_iter()
            .find(|r| r.video_id == "a")
            .unwrap();
        assert_eq!(a.artist.as_deref(), Some("The Artist"));
    }

    #[test]
    fn yt_playlist_mirror_keeps_same_named_user_playlist() {
        let lib = Library::open_in_memory().unwrap();
        // A user's own playlist that happens to share the YouTube playlist's name.
        let user = lib.create_playlist("Mix").unwrap();
        lib.add_to_playlist(user, &["song/mine.mp3".to_string()])
            .unwrap();

        // Mirror a YouTube playlist (different identity: an origin URL) under the
        // same name. The user playlist must survive untouched.
        let url = "https://www.youtube.com/playlist?list=PL123";
        let mirror = lib
            .replace_yt_playlist(url, "Mix", &["yt:v1".into(), "yt:v2".into()])
            .unwrap();
        assert_ne!(mirror, user, "mirror must be a distinct playlist");
        assert_eq!(
            lib.playlist_paths(user).unwrap(),
            vec!["song/mine.mp3".to_string()]
        );
        assert_eq!(lib.yt_playlist_id(url).unwrap(), Some(mirror));
        // The user playlist has no origin, so it is never matched as a mirror.
        assert_eq!(lib.playlists().unwrap().len(), 2);

        // Re-mirroring the same URL refreshes the SAME mirror in place (no
        // duplicate, contents replaced) and still leaves the user playlist alone.
        let mirror2 = lib
            .replace_yt_playlist(url, "Mix", &["yt:v3".into()])
            .unwrap();
        assert_eq!(mirror2, mirror);
        assert_eq!(
            lib.playlist_paths(mirror).unwrap(),
            vec!["yt:v3".to_string()]
        );
        assert_eq!(
            lib.playlist_paths(user).unwrap(),
            vec!["song/mine.mp3".to_string()]
        );
        assert_eq!(lib.playlists().unwrap().len(), 2);
    }

    #[test]
    fn area_filtering_hides_from_listings() {
        use crate::core::category::{album_key, areas_value, Area};
        let lib = Library::open_in_memory().unwrap();
        lib.upsert_track(&track("/x/1.mp3", Some("X"), Some("Y")))
            .unwrap();
        // Default: visible in albums and artists.
        assert_eq!(lib.albums_overview_with(None).unwrap().len(), 1);
        assert_eq!(lib.artists_overview_with(None).unwrap().len(), 1);

        // Take the album out of "Albums" (now only filesystem + artists).
        lib.set_category(
            "album",
            &album_key("X", "Y"),
            Some(&areas_value(&[Area::Filesystem, Area::Artists])),
        )
        .unwrap();
        assert!(lib.albums_overview_with(None).unwrap().is_empty());
        assert_eq!(lib.artists_overview_with(None).unwrap().len(), 1);

        // Hide the artist completely.
        lib.set_category("artist", "X", Some("")).unwrap();
        assert!(lib.artists_overview_with(None).unwrap().is_empty());
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
        lib.set_category(
            "album",
            &crate::core::category::album_key("X", "Konzert"),
            Some("albums"),
        )
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
        let albums = lib.albums_overview_with(None).unwrap();
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
    fn albums_overview_uses_representative_cover_for_compilations() {
        let lib = Library::open_in_memory().unwrap();
        // Compilation: several artists with different covers. The card shows the
        // cover of the dominant artist (most tracks) instead of dropping it — a
        // representative image beats an empty placeholder and matches the cover
        // shown on the album detail page.
        for (path, artist, cover) in [
            ("/c1.mp3", "DJ A", "/covers/a.jpg"),
            ("/c2.mp3", "DJ A", "/covers/a.jpg"),
            ("/c3.mp3", "DJ B", "/covers/b.jpg"),
        ] {
            lib.upsert_track(&track(path, Some(artist), Some("Dancemix 2009")))
                .unwrap();
            let mut m = crate::model::AlbumMeta::pending(artist, "Dancemix 2009");
            m.cover_path = Some(cover.to_string());
            m.status = "local".to_string();
            lib.upsert_album_meta(&m).unwrap();
        }
        let dm = lib
            .albums_overview_with(None)
            .unwrap()
            .into_iter()
            .find(|a| a.album == "Dancemix 2009")
            .unwrap();
        // DJ A has the most tracks → its cover represents the compilation.
        assert_eq!(dm.artist, "DJ A");
        assert_eq!(dm.cover_path.as_deref(), Some("/covers/a.jpg"));

        // Real album by one artist → cover is retained.
        lib.upsert_track(&track("/d1.mp3", Some("Solo"), Some("Werk")))
            .unwrap();
        let mut m = crate::model::AlbumMeta::pending("Solo", "Werk");
        m.cover_path = Some("/covers/werk.jpg".to_string());
        m.status = "local".to_string();
        lib.upsert_album_meta(&m).unwrap();
        let werk = lib
            .albums_overview_with(None)
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
            .albums_overview_with(None)
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
    fn tracks_by_album_name_loads_only_that_album_case_insensitive() {
        let lib = Library::open_in_memory().unwrap();
        lib.upsert_track(&track("/a/1.mp3", Some("A"), Some("Live")))
            .unwrap();
        lib.upsert_track(&track("/a/2.mp3", Some("A"), Some("Other")))
            .unwrap();

        let paths: Vec<String> = lib
            .tracks_by_album_name("live")
            .unwrap()
            .into_iter()
            .map(|t| t.path)
            .collect();
        assert_eq!(paths, vec!["/a/1.mp3".to_string()]);
    }

    #[test]
    fn album_track_paths_by_name_ignores_artist_credit() {
        let lib = Library::open_in_memory().unwrap();
        lib.upsert_track(&track("/b.mp3", Some("B"), Some("Shared")))
            .unwrap();
        lib.upsert_track(&track("/a.mp3", Some("A"), Some("Shared")))
            .unwrap();
        lib.upsert_track(&track("/x.mp3", Some("A"), Some("Other")))
            .unwrap();

        assert_eq!(
            lib.album_track_paths_by_name("Shared").unwrap(),
            vec!["/a.mp3".to_string(), "/b.mp3".to_string()]
        );
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
        assert_eq!(
            lib.track_by_path("/a/hoerspiel.mp3")
                .unwrap()
                .unwrap()
                .resume_ms,
            0
        );
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
        assert_eq!(
            lib.resolve_eq("sink1", Some("X"), Some("Y"), "/a/1.mp3"),
            None
        );
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
    fn eq_bypass_preserves_bands_and_resolves_flat() {
        let lib = Library::open_in_memory().unwrap();
        lib.set_eq("", "track", "/a/1.mp3", &bands(4.0)).unwrap();

        lib.set_eq_enabled("", "track", "/a/1.mp3", false).unwrap();
        assert_eq!(
            lib.get_eq("", "track", "/a/1.mp3").unwrap(),
            Some(bands(4.0))
        );
        assert_eq!(
            lib.resolve_eq("", Some("X"), Some("Y"), "/a/1.mp3"),
            Some(bands(0.0))
        );

        lib.set_eq_enabled("", "track", "/a/1.mp3", true).unwrap();
        assert_eq!(
            lib.resolve_eq("", Some("X"), Some("Y"), "/a/1.mp3"),
            Some(bands(4.0))
        );
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
        lib.upsert_track(&track("/music/a.mp3", Some("A"), Some("X")))
            .unwrap();
        lib.upsert_track(&track("/music/gone.mp3", Some("A"), Some("X")))
            .unwrap();
        // A remote (Nextcloud) track and a track from another folder must survive.
        lib.upsert_track(&track("nc:7:Album/r.mp3", Some("A"), Some("X")))
            .unwrap();
        lib.upsert_track(&track("/other/b.mp3", Some("B"), Some("Y")))
            .unwrap();

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
        lib.upsert_track(&track("/music/a.mp3", Some("A"), Some("X")))
            .unwrap();
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
        lib.upsert_track(&track("/m%/keep.mp3", Some("A"), Some("X")))
            .unwrap();
        lib.upsert_track(&track("/mX/other.mp3", Some("A"), Some("X")))
            .unwrap();
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
