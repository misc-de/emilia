//! Schema migration for [`Library`] (split out of db.rs).

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::{Library, SCHEMA_VERSION};

impl Library {
    pub(super) fn migrate(&self) -> Result<()> {
        // Downgrade guard: refuse a DB written by a newer build instead of
        // letting a later schema change surface as a cryptic SQL error mid-run.
        // Fresh DBs report 0; the version is stamped at the end of this method.
        let found: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0);
        if found > SCHEMA_VERSION as i64 {
            anyhow::bail!(
                "library database schema version {found} is newer than this build supports \
                 (max {SCHEMA_VERSION}); please update Emilia"
            );
        }

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
                genre       TEXT,
                year        INTEGER
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

            -- Canonical tracklist of an album (MusicBrainz), cached so the album
            -- detail can flag tracks that are missing locally. Keyed by
            -- (artist, album) like album_meta; one row per (disc, position).
            CREATE TABLE IF NOT EXISTS album_tracklist (
                artist    TEXT NOT NULL,
                album     TEXT NOT NULL,
                disc      INTEGER NOT NULL DEFAULT 1,
                position  INTEGER NOT NULL,
                title     TEXT NOT NULL,
                length_ms INTEGER,
                PRIMARY KEY (artist, album, disc, position)
            );
            -- Records that a tracklist fetch was attempted, so an album that has
            -- no online match isn't re-queried on every open. status: 'ok'
            -- (tracks stored) or 'none' (no match / empty result).
            CREATE TABLE IF NOT EXISTS album_tracklist_fetch (
                artist     TEXT NOT NULL,
                album      TEXT NOT NULL,
                status     TEXT NOT NULL,
                fetched_at INTEGER,
                PRIMARY KEY (artist, album)
            );

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

            -- Manual overrides for the Singles/Compilations classification
            -- (the heuristic in `albums_classified` is the default). Keyed by the
            -- lowercased album name; kind ∈ 'album' | 'single' | 'compilation'.
            CREATE TABLE IF NOT EXISTS album_kind (
                album TEXT PRIMARY KEY,
                kind  TEXT NOT NULL
            );

            -- Equalizer settings per output and level (10 bands as JSON).
            -- Inheritance track → album → artist → global (and station /
            -- episode → podcast as the queue-less cascades); additionally a
            -- device-specific output falls back to the default output ('').
            -- output: '' (all/default) | sink name.  key: '' (global) |
            -- artist name | artist\1album | path | station id | podcast id |
            -- episode audio URL.
            CREATE TABLE IF NOT EXISTS eq_setting (
                output TEXT NOT NULL DEFAULT '',
                scope  TEXT NOT NULL CHECK(scope IN ('global','artist','album','track','stream','podcast','episode')),
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
            -- The primary key is (podcast_id, position), but the listening stats
            -- resolve an episode the other way round: from its audio URL, once
            -- per distinct played path. Without this index that scalar subquery
            -- degrades to a full scan of `episode` per group -- measurably so
            -- (roughly 2.7x on a 50k-track library with 6k episodes).
            CREATE INDEX IF NOT EXISTS idx_episode_audio_url ON episode(audio_url);

            -- Resume position per episode, keyed by audio URL --
            -- deliberately separate from `episode`, so that a feed refresh (which
            -- replaces the episode rows) does not delete the resume position.
            CREATE TABLE IF NOT EXISTS episode_progress (
                url         TEXT PRIMARY KEY,
                position_ms INTEGER NOT NULL DEFAULT 0,
                updated_at  INTEGER NOT NULL DEFAULT 0,
                -- 1 = listened to the end. Distinct from "no row" (never played),
                -- so a finished episode still shows up as heard even though its
                -- resume position is cleared.
                finished    INTEGER NOT NULL DEFAULT 0
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

            -- Log of songs *recognized* (from a station's ICY title) while
            -- streaming — the "Recently heard" history. Unlike `recording`,
            -- nothing is captured to disk: this is just metadata about what
            -- played. One row per song (deduped on artist+title, case-folded);
            -- hearing it again only bumps `heard_at`/`station` and `count`.
            -- Purely local, like the play statistics.
            CREATE TABLE IF NOT EXISTS heard (
                id       INTEGER PRIMARY KEY,
                artist   TEXT,
                title    TEXT NOT NULL,
                station  TEXT,
                heard_at INTEGER NOT NULL DEFAULT 0,
                count    INTEGER NOT NULL DEFAULT 1
            );
            CREATE INDEX IF NOT EXISTS idx_heard_time ON heard(heard_at);

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
            -- Watch progress of **long-form** YouTube items (talks, streams,
            -- podcasts — see `youtube::LONGFORM_SECS`), the mirror of
            -- `episode_progress`. Songs deliberately get no row: resuming a
            -- 3-minute track mid-way is not what anyone expects. Kept separate
            -- from `episode_progress` so device sync (which ships podcast
            -- progress) is unaffected.
            CREATE TABLE IF NOT EXISTS yt_progress (
                video_id    TEXT PRIMARY KEY,
                position_ms INTEGER NOT NULL DEFAULT 0,
                updated_at  INTEGER NOT NULL DEFAULT 0,
                -- 1 = watched to the end (kept when the resume point is cleared).
                finished    INTEGER NOT NULL DEFAULT 0
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
                count     INTEGER NOT NULL DEFAULT 0,
                -- For 'playlist' entries: summed runtime (seconds) of all songs,
                -- so the row can show a total. NULL when unknown.
                total_duration INTEGER
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
            -- Cache of a browsed (not "saved") YouTube playlist's song list, so
            -- reopening it is instant instead of re-querying YouTube every time.
            -- `songs` is the JSON-serialized result list; `fetched_at` (Unix
            -- seconds) drives a staleness-gated background refresh. Saved
            -- playlists use the `playlist` mirror instead (origin = url).
            CREATE TABLE IF NOT EXISTS yt_detail (
                video_id    TEXT PRIMARY KEY,
                description TEXT,
                -- jump marks as JSON [[ms, "label"], …]
                chapters    TEXT,
                fetched_at  INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS yt_playlist_cache (
                url        TEXT PRIMARY KEY,
                title      TEXT NOT NULL,
                songs      TEXT NOT NULL,
                fetched_at INTEGER NOT NULL
            );

            -- Lyrics cache, keyed by track path. `source` distinguishes embedded
            -- tags from an online (LRCLIB) fetch; a row with source='none' is a
            -- negative result (don't refetch for a while). `synced` holds the raw
            -- LRC text. Like all online metadata, never written back to tags.
            CREATE TABLE IF NOT EXISTS lyrics_cache (
                path      TEXT PRIMARY KEY,
                plain     TEXT,
                synced    TEXT,
                source    TEXT NOT NULL,
                cached_at INTEGER NOT NULL
            );

            -- Per-track lyric preferences. Kept separate from `lyrics_cache` so a
            -- lyrics re-fetch (which replaces the cache row) does not reset them:
            -- whether the timed karaoke highlighting is off, and a manual timing
            -- offset in milliseconds (+ = lyrics shown later).
            CREATE TABLE IF NOT EXISTS lyrics_pref (
                path        TEXT PRIMARY KEY,
                karaoke_off INTEGER NOT NULL DEFAULT 0,
                delay_ms    INTEGER NOT NULL DEFAULT 0
            );

            -- Voice memos (microphone recordings) and the user-created
            -- categories that organise them. Unrelated to the `category`
            -- *areas* table above (same word, different concept) and to
            -- `recording` (radio timeshift): a memo's audio file lives at
            -- `path`, here only the metadata/management.
            CREATE TABLE IF NOT EXISTS memo_category (
                id         INTEGER PRIMARY KEY,
                name       TEXT NOT NULL,
                position   INTEGER NOT NULL DEFAULT 0,  -- manual sort order
                created_at INTEGER
            );
            -- category_id NULL = unassigned ("General"). Foreign keys are not
            -- enforced (no PRAGMA foreign_keys), so deleting a category resets
            -- its memos to NULL explicitly in `delete_memo_category` rather than
            -- via ON DELETE — the REFERENCES clause is documentation only.
            CREATE TABLE IF NOT EXISTS memo (
                id          INTEGER PRIMARY KEY,
                path        TEXT NOT NULL,
                title       TEXT NOT NULL,
                category_id INTEGER REFERENCES memo_category(id),
                recorded_at INTEGER,                    -- Unix seconds (newest first)
                duration_ms INTEGER NOT NULL DEFAULT 0
            );
            -- The default "Recent" view sorts by recording time; the category
            -- filter is the second axis.
            CREATE INDEX IF NOT EXISTS idx_memo_recorded ON memo(recorded_at);
            CREATE INDEX IF NOT EXISTS idx_memo_category ON memo(category_id);
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
            // Atomic table rebuild: a crash mid-way must not leave the renamed
            // `_old` table without its replacement (which would break the next
            // start). BEGIN/COMMIT make it all-or-nothing.
            self.conn.execute_batch(
                r#"
                BEGIN;
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
                COMMIT;
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

        // Migration: allow the 'stream' (per-station) and 'podcast'/'episode'
        // equalizer scopes. Earlier CHECK constraints listed fewer scopes;
        // SQLite can't alter a CHECK in place, so rebuild the table when it's
        // still an old shape (detected by the stored schema SQL). Atomic, like
        // above.
        let eq_schema: String = self
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'eq_setting'",
                [],
                |r| r.get(0),
            )
            .unwrap_or_default();
        if !eq_schema.contains("'episode'") {
            self.conn.execute_batch(
                r#"
                BEGIN;
                ALTER TABLE eq_setting RENAME TO eq_setting_old;
                CREATE TABLE eq_setting (
                    output TEXT NOT NULL DEFAULT '',
                    scope  TEXT NOT NULL CHECK(scope IN ('global','artist','album','track','stream','podcast','episode')),
                    key    TEXT NOT NULL,
                    bands  TEXT NOT NULL,
                    enabled INTEGER NOT NULL DEFAULT 1,
                    PRIMARY KEY (output, scope, key)
                );
                INSERT INTO eq_setting (output, scope, key, bands, enabled)
                    SELECT output, scope, key, bands, enabled FROM eq_setting_old;
                DROP TABLE eq_setting_old;
                COMMIT;
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

        // Migration: per-track release year (from the file's date tag). Lets the
        // album/song date sort work from the embedded metadata stored in the DB,
        // never from the file's modification timestamp. Existing rows stay NULL
        // until the next library scan re-reads their tags.
        let has_year = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('track') WHERE name = 'year'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_year {
            self.conn
                .execute_batch("ALTER TABLE track ADD COLUMN year INTEGER;")?;
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

        // Migration: recent playlists gained a `total_duration` (summed runtime).
        let has_yt_total = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('yt_recent') WHERE name = 'total_duration'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_yt_total {
            self.conn
                .execute_batch("ALTER TABLE yt_recent ADD COLUMN total_duration INTEGER;")?;
        }

        // Migration: `yt_progress` gained the `finished` flag (watched to the
        // end, kept after the resume point is cleared). Databases that already
        // carry a three-column `yt_progress` from an earlier build keep their
        // stored positions — `CREATE TABLE IF NOT EXISTS` skips them, so without
        // this the writes would fail on the missing column.
        let has_yt_finished = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('yt_progress') WHERE name = 'finished'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_yt_finished {
            self.conn.execute_batch(
                "ALTER TABLE yt_progress ADD COLUMN finished INTEGER NOT NULL DEFAULT 0;",
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

        // Migration: mark episodes listened to the end (distinct from "no row").
        let has_finished = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('episode_progress') WHERE name = 'finished'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_finished {
            self.conn.execute_batch(
                "ALTER TABLE episode_progress ADD COLUMN finished INTEGER NOT NULL DEFAULT 0;",
            )?;
        }

        // One-time backfill: episodes already heard to the end before the
        // `finished` flag existed left no progress row (the end guard cleared
        // it), so they'd never show as heard. The listening stats still know
        // them — a play_event whose path is an episode's audio URL that reached
        // within 30 s of the end. Gated by its own setting flag (not by the
        // column check above), so it still runs when the column was already
        // added by an earlier build. INSERT OR IGNORE leaves in-progress rows be.
        let backfill_done = self
            .conn
            .query_row(
                "SELECT 1 FROM setting WHERE key = 'episode_finished_backfill'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !backfill_done {
            // Stamp updated_at with the *actual* last listen time (from the
            // stats), not "now" — otherwise the "Recently" list, which sorts by
            // updated_at, would show every backfilled episode at the same time
            // and fall back to insertion order (grouped by podcast).
            self.conn.execute_batch(
                "INSERT OR IGNORE INTO episode_progress (url, position_ms, updated_at, finished)
                 SELECT e.audio_url, 0,
                        (SELECT MAX(pe.started_at) FROM play_event pe WHERE pe.path = e.audio_url),
                        1
                 FROM episode e
                 WHERE EXISTS (
                     SELECT 1 FROM play_event pe
                     WHERE pe.path = e.audio_url
                       AND pe.duration_ms > 0
                       AND pe.played_ms >= pe.duration_ms - 30000
                 );
                 INSERT OR REPLACE INTO setting (key, value)
                 VALUES ('episode_finished_backfill', '1');",
            )?;
        }

        // Repair: an earlier backfill (0.8.12) stamped every backfilled row with
        // the same "now", so "Recently" couldn't order them by when they were
        // actually heard. Reset those timestamps from the stats, once. Only rows
        // that carry no resume position (finished-only) and have a play_event.
        let backfill_ts_fixed = self
            .conn
            .query_row(
                "SELECT 1 FROM setting WHERE key = 'episode_finished_backfill_ts'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !backfill_ts_fixed {
            self.conn.execute_batch(
                "UPDATE episode_progress
                 SET updated_at = (
                     SELECT MAX(pe.started_at) FROM play_event pe
                     WHERE pe.path = episode_progress.url
                 )
                 WHERE finished = 1 AND position_ms = 0
                   AND EXISTS (
                     SELECT 1 FROM play_event pe WHERE pe.path = episode_progress.url
                   );
                 INSERT OR REPLACE INTO setting (key, value)
                 VALUES ('episode_finished_backfill_ts', '1');",
            )?;
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

        // Migration: separate retry counter for the release-year backfill. The
        // cover `attempts` counter resets to 0 whenever a cover is present, so it
        // can't bound year lookups for albums that already have (local) artwork.
        let has_year_attempts = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('album_meta') WHERE name = 'year_attempts'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_year_attempts {
            self.conn.execute_batch(
                "ALTER TABLE album_meta ADD COLUMN year_attempts INTEGER NOT NULL DEFAULT 0;",
            )?;
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
            // Atomic rebuild (see the eq_setting migration above).
            self.conn.execute_batch(
                "BEGIN;
                 ALTER TABLE category RENAME TO category_old;
                 CREATE TABLE category (
                     scope TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
                     PRIMARY KEY (scope, key)
                 );
                 INSERT INTO category SELECT * FROM category_old;
                 DROP TABLE category_old;
                 COMMIT;",
            )?;
        }

        // All migrations applied → stamp the schema version (read back by the
        // downgrade guard at the top). PRAGMA takes no bind parameters.
        self.conn
            .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        Ok(())
    }
}
