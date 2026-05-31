//! SQLite-Bibliotheksindex (rusqlite).

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use std::path::PathBuf;
use std::time::Duration;

use crate::model::{AlbumMeta, ArtistMeta, Episode, Track, TrackMeta};

/// Speicherort der Datenbank: `$XDG_DATA_HOME/emilia/library.db`.
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

impl Library {
    pub fn open() -> Result<Self> {
        let conn = Connection::open(db_path())?;
        // Mehrere Verbindungen (UI-Thread + Online-Worker) greifen parallel zu:
        // kurz warten statt sofort mit „database is locked“ abzubrechen.
        conn.busy_timeout(Duration::from_secs(10))?;
        let lib = Self { conn };
        lib.migrate()?;
        Ok(lib)
    }

    /// In-Memory-DB (für Tests).
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
                last_played INTEGER
            );

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

            -- Online angereicherte Albumdaten (MusicBrainz / Cover Art Archive).
            -- Bewusst getrennt von den Audiodateien: nichts hiervon wird je in
            -- die Tags zurückgeschrieben.
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

            -- Künstlerfotos (Deezer). Ebenfalls getrennt von den Dateien.
            CREATE TABLE IF NOT EXISTS artist_meta (
                name       TEXT PRIMARY KEY,
                image_path TEXT,
                status     TEXT NOT NULL DEFAULT 'pending',
                fetched_at INTEGER,
                attempts   INTEGER NOT NULL DEFAULT 0
            );

            -- Per Fingerprint (AcoustID) erkannte Titeldaten – reine Vorschläge,
            -- werden nie in die Tags der Datei zurückgeschrieben.
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

            -- Vom Nutzer als Konzert markierte Ordner/Dateien.
            CREATE TABLE IF NOT EXISTS concert (
                path     TEXT PRIMARY KEY,
                title    TEXT NOT NULL,
                is_dir   INTEGER NOT NULL DEFAULT 0,
                added_at INTEGER
            );

            -- Inhalts-Merkmal (Musik/Konzert/Podcast/Hörbuch) je Ebene.
            -- Vererbung Titel → Album → Interpret → Standard; nur Abweichungen
            -- werden gespeichert. key = Pfad | Interpret\1Album | Interpretname.
            CREATE TABLE IF NOT EXISTS category (
                scope TEXT NOT NULL,
                key   TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (scope, key)
            );

            -- Equalizer-Einstellungen je Ausgang und Ebene (10 Bänder als JSON).
            -- Vererbung Titel → Album → Interpret → Global; zusätzlich fällt ein
            -- gerätespezifischer Ausgang auf den Standard-Ausgang ('') zurück.
            -- output: '' (alle/Standard) | Sink-Name.  key: '' (global) |
            -- Interpretname | Interpret\1Album | Pfad.
            CREATE TABLE IF NOT EXISTS eq_setting (
                output TEXT NOT NULL DEFAULT '',
                scope  TEXT NOT NULL CHECK(scope IN ('global','artist','album','track')),
                key    TEXT NOT NULL,
                bands  TEXT NOT NULL,
                PRIMARY KEY (output, scope, key)
            );

            -- Mehrere Bilder je Album bzw. Interpret (Galerie). Das in
            -- album_meta/artist_meta gespeicherte Einzelbild bleibt das
            -- primaer angezeigte; diese Tabellen halten den vollen Vorrat.
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

            -- Vom Nutzer angelegte Playlisten und ihre Einträge (geordnet).
            -- Einträge sind Pfade (wie die Warteschlange); Duplikate erlaubt.
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

            -- Abonnierte Podcasts und ihre Episoden (aus RSS-Feeds; Audio wird
            -- gestreamt, nichts heruntergeladen).
            CREATE TABLE IF NOT EXISTS podcast (
                id        INTEGER PRIMARY KEY,
                title     TEXT NOT NULL,
                feed_url  TEXT NOT NULL UNIQUE,
                image_url TEXT,
                added_at  INTEGER
            );
            CREATE TABLE IF NOT EXISTS episode (
                podcast_id INTEGER NOT NULL,
                position   INTEGER NOT NULL,
                guid       TEXT,
                title      TEXT NOT NULL,
                audio_url  TEXT NOT NULL,
                published  TEXT,
                duration   TEXT,
                PRIMARY KEY (podcast_id, position)
            );
            "#,
        )?;

        // Migration: frühere eq_setting-Version ohne `output`-Spalte nachrüsten.
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

        // Migration: disc_no (Disc-Nummer für Mehr-CD-Alben) nachrüsten.
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

        // Migration: attempts-Zähler in den Meta-Tabellen nachrüsten (begrenzt das
        // wiederholte Anfragen erfolglos gebliebener Online-Abrufe).
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

        // Migration: alte Einzel-Merkmale (music/concert/…) auf die neue
        // Bereichsliste (Eigenschaften) abbilden. Idempotent.
        self.conn.execute_batch(
            "UPDATE category SET value = CASE value
                 WHEN 'music'     THEN 'filesystem,artists,albums'
                 WHEN 'concert'   THEN 'concerts'
                 WHEN 'audiobook' THEN 'audiobooks'
                 WHEN 'podcast'   THEN 'filesystem,artists,albums'
                 ELSE value END
             WHERE value IN ('music','concert','audiobook','podcast');",
        )?;

        // Migration: alte CHECK-Beschränkung auf scope entfernen, damit auch die
        // Ordner-Ebene ('folder') gespeichert werden kann.
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

    /// Markiert einen Ordner/eine Datei als Konzert.
    pub fn add_concert(&self, path: &str, title: &str, is_dir: bool) -> Result<()> {
        self.conn.execute(
            "INSERT INTO concert (path, title, is_dir, added_at)
             VALUES (?1, ?2, ?3, strftime('%s','now'))
             ON CONFLICT(path) DO UPDATE SET title = excluded.title",
            rusqlite::params![path, title, is_dir as i64],
        )?;
        Ok(())
    }

    /// Entfernt eine Konzert-Markierung.
    pub fn remove_concert(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM concert WHERE path = ?1", [path])?;
        Ok(())
    }

    /// Alle Konzerte (Pfad, Titel, is_dir), neueste zuerst.
    pub fn concerts(&self) -> Result<Vec<(String, String, bool)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, title, is_dir FROM concert ORDER BY added_at DESC, title",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? != 0,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Pfade aller markierten Konzerte (für die Kandidaten-Filterung).
    pub fn concert_paths(&self) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM concert")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<std::collections::HashSet<_>>>()?)
    }

    // ---- Merkmale (Kategorie mit Vererbung) ----

    /// Setzt (oder löscht bei `None`) die Festlegung einer Ebene.
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

    /// Liest die Festlegung einer einzelnen Ebene (ohne Vererbung).
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

    /// Effektive **Bereiche** eines Titels (spezifischste Ebene gewinnt:
    /// Titel → Album → Interpret → Standard). Leere Liste = ausgeblendet.
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
        // Ordner-Kette: vom Verzeichnis der Datei aufwärts (tiefste Festlegung gewinnt).
        let mut dir = std::path::Path::new(path).parent();
        while let Some(d) = dir {
            if let Ok(Some(v)) = self.get_category("folder", &d.to_string_lossy()) {
                return parse_areas(&v);
            }
            dir = d.parent();
        }
        Area::DEFAULT.to_vec()
    }

    /// Effektive Bereiche eines Ordners (dieser Ordner aufwärts → Standard).
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

    /// Effektive Bereiche eines Albums (Album → Interpret → Standard).
    pub fn album_areas(&self, artist: &str, album: &str) -> Vec<crate::core::category::Area> {
        use crate::core::category::{album_key, parse_areas, Area};
        if let Ok(Some(v)) = self.get_category("album", &album_key(artist, album)) {
            return parse_areas(&v);
        }
        if let Ok(Some(v)) = self.get_category("artist", artist) {
            return parse_areas(&v);
        }
        Area::DEFAULT.to_vec()
    }

    /// Effektive Bereiche eines Interpreten (Interpret → Standard).
    pub fn artist_areas(&self, name: &str) -> Vec<crate::core::category::Area> {
        use crate::core::category::{parse_areas, Area};
        if let Ok(Some(v)) = self.get_category("artist", name) {
            return parse_areas(&v);
        }
        Area::DEFAULT.to_vec()
    }

    // ---- Equalizer (10 Bänder, mit Vererbung) ----

    /// Speichert die 10 Band-Verstärkungen (dB) für einen Ausgang + eine Ebene.
    pub fn set_eq(&self, output: &str, scope: &str, key: &str, bands: &[f64; 10]) -> Result<()> {
        let json = serde_json::to_string(bands)?;
        self.conn.execute(
            "INSERT INTO eq_setting (output, scope, key, bands) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(output, scope, key) DO UPDATE SET bands = excluded.bands",
            rusqlite::params![output, scope, key, json],
        )?;
        Ok(())
    }

    /// Liest die Bänder einer einzelnen Ausgang/Ebene-Kombination (ohne Vererbung).
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

    /// Entfernt die Festlegung (fällt auf die geerbte/den Standard-Ausgang zurück).
    pub fn clear_eq(&self, output: &str, scope: &str, key: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM eq_setting WHERE output = ?1 AND scope = ?2 AND key = ?3",
            rusqlite::params![output, scope, key],
        )?;
        Ok(())
    }

    /// Effektiver Equalizer für Titel + Ausgang. Reihenfolge: erst der konkrete
    /// Ausgang (Titel→Album→Interpret→Global), dann der Standard-Ausgang ('')
    /// als Basis. `None`, wenn nirgends etwas gesetzt ist (→ neutral).
    pub fn resolve_eq(
        &self,
        output: &str,
        artist: Option<&str>,
        album: Option<&str>,
        path: &str,
    ) -> Option<[f64; 10]> {
        let album_key = album.map(|al| crate::core::category::album_key(artist.unwrap_or(""), al));

        // Konkreter Ausgang zuerst, dann der Standard-Ausgang als Basis.
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

    /// Liest einen Einstellungswert (z. B. den Musikordner).
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let value = self
            .conn
            .query_row("SELECT value FROM setting WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        Ok(value)
    }

    /// Speichert einen Einstellungswert.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO setting (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Fügt einen Track ein oder aktualisiert dessen Metadaten (Schlüssel: Pfad).
    pub fn upsert_track(&self, t: &Track) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO track (path, title, artist, album, track_no, disc_no, duration_ms)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(path) DO UPDATE SET
                title       = excluded.title,
                artist      = excluded.artist,
                album       = excluded.album,
                track_no    = excluded.track_no,
                disc_no     = excluded.disc_no,
                duration_ms = excluded.duration_ms
            "#,
            rusqlite::params![
                t.path,
                t.title,
                t.artist,
                t.album,
                t.track_no,
                t.disc_no,
                t.duration_ms,
            ],
        )?;
        Ok(())
    }

    /// Speichert die Wiedergabeposition (Resume) anhand des Pfads. Die
    /// Warteschlange ist pfadbasiert; bei unbekanntem Pfad passiert nichts.
    pub fn set_resume_path(&self, path: &str, resume_ms: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE track SET resume_ms = ?1 WHERE path = ?2",
            rusqlite::params![resume_ms, path],
        )?;
        Ok(())
    }

    /// Liest einen einzelnen Track anhand seines Pfads (inkl. Resume-Position).
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

    /// Alle Tracks, nach Album und Tracknummer sortiert.
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
                track_no: r.get::<_, Option<i64>>(5)?.map(|n| n as u32),
                duration_ms: r.get(6)?,
                resume_ms: r.get(7)?,
                disc_no: r.get::<_, Option<i64>>(8)?.map(|n| n as u32),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn track_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM track", [], |r| r.get(0))?)
    }

    // ---- Fehlversuchs-Zähler (begrenzen das wiederholte Online-Anfragen) ----

    /// Bisherige erfolglose Online-Versuche für ein Album (0, wenn unbekannt).
    pub fn album_attempts(&self, artist: &str, album: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT attempts FROM album_meta WHERE artist = ?1 AND album = ?2",
                rusqlite::params![artist, album],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    /// Bisherige erfolglose Online-Versuche für einen Interpreten.
    pub fn artist_attempts(&self, name: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT attempts FROM artist_meta WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    /// Bisherige erfolglose Fingerprint-Versuche für einen Titel (Pfad).
    pub fn track_attempts(&self, path: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT attempts FROM track_meta WHERE path = ?1",
                [path],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    /// Anzahl roher (Interpret, Album)-Gruppen – **gleiche Gruppierung** wie
    /// [`Self::albums_missing_cover`], als Gesamtsumme für die Fortschrittsanzeige
    /// des Cover-Abrufs. (Die *Anzeige* in [`Self::albums_overview`] fasst
    /// feat.-Varianten zusätzlich nach Haupt-Interpret zusammen und kann daher
    /// weniger Karten zeigen.)
    pub fn album_count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM (
                 SELECT 1 FROM track
                 WHERE album IS NOT NULL AND album <> ''
                 GROUP BY COALESCE(artist, ''), album
             )",
            [],
            |r| r.get(0),
        )?)
    }

    /// Liest die Online-Metadaten zu einem Album (falls bereits gesucht).
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

    /// Irgendein vorhandenes Cover zu einem Albumnamen (interpretenübergreifend) –
    /// nützlich für Einzeltitel, deren Album zwar bekannt ist, aber unter einem
    /// anderen Interpreten-Credit gespeichert wurde.
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

    /// Speichert/aktualisiert die Online-Metadaten eines Albums.
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

    /// Album-Übersicht für die UI: alle eindeutigen Alben aus der Bibliothek,
    /// angereichert mit (ggf. vorhandenen) Online-Metadaten und der Titelanzahl.
    /// Nach Albumname sortiert (wie die Dateiansicht – ohne Interpreten-Gruppen).
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

        // Alben werden **allein über den Albumnamen** zusammengefasst – der
        // Interpret spielt keine Rolle. Gleichnamige Titel verschiedener
        // Interpreten (auch „feat."-Varianten) bilden damit genau eine Karte.
        // Anzeige-Interpret + Cover stammen vom Interpreten mit den meisten
        // Titeln des Albums (Lücken werden aus den übrigen gefüllt).
        use std::collections::HashMap;
        // Je Album-Schlüssel: Statistik pro Haupt-Interpret (Titelzahl, Cover,
        // Jahr, MBID) für die Wahl von Anzeige-Interpret/Cover.
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
        // Anzeige-Interpret = der mit den meisten Titeln; dessen Cover/Jahr/MBID
        // bevorzugen, fehlende Felder aus den übrigen Interpreten ergänzen.
        for (key, meta) in map.iter_mut() {
            let Some(per) = by_artist.get(key) else { continue };
            let mut artists: Vec<(&String, &ArtistInfo)> = per.iter().collect();
            artists.sort_by(|a, b| {
                b.1 .0
                    .cmp(&a.1 .0)
                    .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
            });
            for (i, (name, info)) in artists.iter().enumerate() {
                if i == 0 {
                    meta.artist = (*name).clone();
                }
                if meta.cover_path.is_none() {
                    meta.cover_path = info.1.clone();
                }
                if meta.year.is_none() {
                    meta.year = info.2;
                }
                if meta.mbid.is_none() {
                    meta.mbid = info.3.clone();
                }
            }
        }
        let mut out: Vec<AlbumMeta> = order.into_iter().filter_map(|k| map.remove(&k)).collect();
        // Eigenschaften: nur Alben zeigen, die im Bereich „Alben" sichtbar sind.
        out.retain(|a| {
            self.album_areas(&a.artist, &a.album)
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

    /// Alben **ohne** Cover, je mit einem Beispiel-Track-Pfad. Grundlage für die
    /// lokale Cover-Extraktion (eingebettetes Bild) und die Online-Lückenfüllung.
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

    // ---- Interpreten ----

    /// Eindeutige **Einzel**-Interpreten aus der Bibliothek. Zusammengesetzte
    /// Angaben („A feat. B & C") werden in ihre Künstler zerlegt
    /// (siehe [`crate::core::artist::split_artists`]) und case-insensitiv
    /// dedupliziert. Alphabetisch sortiert.
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

    /// Interpreten-Übersicht für die UI: jede(r) Einzelkünstler(in) – auch aus
    /// „feat."-Angaben – mit (ggf. vorhandenem) Foto.
    pub fn artists_overview(&self) -> Result<Vec<ArtistMeta>> {
        let names = self.distinct_artists()?;
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            // Eigenschaften: nur im Bereich „Interpreten" sichtbare zeigen.
            if !self
                .artist_areas(&name)
                .contains(&crate::core::category::Area::Artists)
            {
                continue;
            }
            let meta = self
                .get_artist_meta(&name)?
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

    // ---- Fingerprint-Erkennung (AcoustID) ----

    /// Tracks mit lückenhaften Tags (Interpret oder Album fehlt) – Kandidaten
    /// für die Fingerprint-Erkennung.
    pub fn tracks_needing_id(&self) -> Result<Vec<Track>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, title, artist, album, track_no, duration_ms, resume_ms, disc_no
             FROM track
             WHERE artist IS NULL OR artist = '' OR album IS NULL OR album = ''
             ORDER BY path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Track {
                id: r.get(0)?,
                path: r.get(1)?,
                title: r.get(2)?,
                artist: r.get(3)?,
                album: r.get(4)?,
                track_no: r.get::<_, Option<i64>>(5)?.map(|n| n as u32),
                duration_ms: r.get(6)?,
                resume_ms: r.get(7)?,
                disc_no: r.get::<_, Option<i64>>(8)?.map(|n| n as u32),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

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

    // ---- Mehrere Bilder je Album / Interpret (Galerie) ----

    /// Ersetzt die gespeicherten Album-Bilder (Reihenfolge = idx).
    /// `images`: je (Pfad, Art, Quelle).
    pub fn set_album_images(
        &self,
        artist: &str,
        album: &str,
        images: &[(String, String, String)],
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM album_image WHERE artist = ?1 AND album = ?2",
            rusqlite::params![artist, album],
        )?;
        for (i, (path, kind, source)) in images.iter().enumerate() {
            self.conn.execute(
                "INSERT INTO album_image (artist, album, idx, path, kind, source)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![artist, album, i as i64, path, kind, source],
            )?;
        }
        Ok(())
    }

    /// Alle gespeicherten Bildpfade eines Albums (in Reihenfolge).
    pub fn album_images(&self, artist: &str, album: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT path FROM album_image WHERE artist = ?1 AND album = ?2 ORDER BY idx",
        )?;
        let rows =
            stmt.query_map(rusqlite::params![artist, album], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Ersetzt die gespeicherten Interpreten-Bilder (Reihenfolge = idx).
    pub fn set_artist_images(
        &self,
        name: &str,
        images: &[(String, String, String)],
    ) -> Result<()> {
        self.conn
            .execute("DELETE FROM artist_image WHERE name = ?1", [name])?;
        for (i, (path, kind, source)) in images.iter().enumerate() {
            self.conn.execute(
                "INSERT INTO artist_image (name, idx, path, kind, source)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![name, i as i64, path, kind, source],
            )?;
        }
        Ok(())
    }

    /// Alle gespeicherten Bildpfade eines Interpreten (in Reihenfolge).
    pub fn artist_images(&self, name: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM artist_image WHERE name = ?1 ORDER BY idx")?;
        let rows = stmt.query_map([name], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- Playlisten ----

    /// Legt eine Playlist an und gibt ihre ID zurück.
    pub fn create_playlist(&self, name: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO playlist (name, created_at) VALUES (?1, strftime('%s','now'))",
            [name],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Benennt eine Playlist um.
    pub fn rename_playlist(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE playlist SET name = ?1 WHERE id = ?2",
            rusqlite::params![name, id],
        )?;
        Ok(())
    }

    /// Löscht eine Playlist samt ihrer Einträge.
    pub fn delete_playlist(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM playlist_item WHERE playlist_id = ?1", [id])?;
        self.conn.execute("DELETE FROM playlist WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Alle Playlisten als (id, Name, Titelanzahl), alphabetisch sortiert.
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

    /// Hängt Pfade ans Ende einer Playlist an (Duplikate erlaubt).
    pub fn add_to_playlist(&self, id: i64, paths: &[String]) -> Result<()> {
        let start: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM playlist_item WHERE playlist_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        for (i, path) in paths.iter().enumerate() {
            self.conn.execute(
                "INSERT INTO playlist_item (playlist_id, position, path) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, start + i as i64, path],
            )?;
        }
        Ok(())
    }

    /// Pfade einer Playlist in ihrer Reihenfolge.
    pub fn playlist_paths(&self, id: i64) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM playlist_item WHERE playlist_id = ?1 ORDER BY position")?;
        let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Entfernt alle Vorkommen eines Pfads aus einer Playlist.
    pub fn remove_from_playlist(&self, id: i64, path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM playlist_item WHERE playlist_id = ?1 AND path = ?2",
            rusqlite::params![id, path],
        )?;
        Ok(())
    }

    // ---- Podcasts ----

    /// Abonniert einen Feed (oder aktualisiert Titel/Bild bei bekanntem Feed)
    /// und gibt die Podcast-ID zurück.
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

    /// Alle Podcasts als (id, Titel, Bild-URL, Episodenzahl), neueste zuerst.
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

    /// Feed-URL eines Podcasts (für die Aktualisierung).
    pub fn podcast_feed_url(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT feed_url FROM podcast WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Entfernt einen Podcast samt Episoden.
    pub fn delete_podcast(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM episode WHERE podcast_id = ?1", [id])?;
        self.conn.execute("DELETE FROM podcast WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Ersetzt die Episoden eines Podcasts (Reihenfolge = Feed-Reihenfolge).
    pub fn set_episodes(&self, podcast_id: i64, episodes: &[Episode]) -> Result<()> {
        self.conn
            .execute("DELETE FROM episode WHERE podcast_id = ?1", [podcast_id])?;
        for (i, ep) in episodes.iter().enumerate() {
            self.conn.execute(
                "INSERT INTO episode
                    (podcast_id, position, guid, title, audio_url, published, duration)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    podcast_id,
                    i as i64,
                    ep.guid,
                    ep.title,
                    ep.audio_url,
                    ep.published,
                    ep.duration
                ],
            )?;
        }
        Ok(())
    }

    /// Episoden eines Podcasts in Feed-Reihenfolge.
    pub fn episodes(&self, podcast_id: i64) -> Result<Vec<Episode>> {
        let mut stmt = self.conn.prepare(
            "SELECT guid, title, audio_url, published, duration FROM episode
             WHERE podcast_id = ?1 ORDER BY position",
        )?;
        let rows = stmt.query_map([podcast_id], |r| {
            Ok(Episode {
                guid: r.get(0)?,
                title: r.get(1)?,
                audio_url: r.get(2)?,
                published: r.get(3)?,
                duration: r.get(4)?,
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
            track_no: None,
            disc_no: None,
            duration_ms: Some(60_000),
            resume_ms: 0,
        }
    }

    #[test]
    fn meta_attempts_count_failures_and_reset_on_success() {
        let lib = Library::open_in_memory().unwrap();
        let mut m = AlbumMeta::pending("A", "B");

        // Jeder erfolglose Abruf zählt hoch.
        m.status = "notfound".to_string();
        lib.upsert_album_meta(&m).unwrap();
        assert_eq!(lib.album_attempts("A", "B"), 1);
        m.status = "error".to_string();
        lib.upsert_album_meta(&m).unwrap();
        assert_eq!(lib.album_attempts("A", "B"), 2);

        // Erfolg ('matched' oder lokal gefundenes Cover) setzt zurück.
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
        // Erneutes Abo desselben Feeds → gleiche ID (Upsert), kein Duplikat.
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
            },
            Episode {
                guid: None,
                title: "E2".into(),
                audio_url: "https://a/2.mp3".into(),
                published: None,
                duration: None,
            },
        ];
        lib.set_episodes(id, &eps).unwrap();

        let got = lib.episodes(id).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].title, "E1");
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

        // Anhängen erhält die Reihenfolge (über zwei Aufrufe hinweg).
        lib.add_to_playlist(id, &["/a.mp3".into(), "/b.mp3".into()])
            .unwrap();
        lib.add_to_playlist(id, &["/c.mp3".into()]).unwrap();
        assert_eq!(
            lib.playlist_paths(id).unwrap(),
            vec!["/a.mp3", "/b.mp3", "/c.mp3"]
        );
        assert_eq!(lib.playlists().unwrap()[0].2, 3); // Titelanzahl

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
        // Standard: in Alben und Interpreten sichtbar.
        assert_eq!(lib.albums_overview().unwrap().len(), 1);
        assert_eq!(lib.artists_overview().unwrap().len(), 1);

        // Album aus „Alben" nehmen (nur noch Dateisystem + Interpreten).
        lib.set_category(
            "album",
            &album_key("X", "Y"),
            Some(&areas_value(&[Area::Filesystem, Area::Artists])),
        )
        .unwrap();
        assert!(lib.albums_overview().unwrap().is_empty());
        assert_eq!(lib.artists_overview().unwrap().len(), 1);

        // Interpret komplett ausblenden.
        lib.set_category("artist", "X", Some("")).unwrap();
        assert!(lib.artists_overview().unwrap().is_empty());
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
        // feat.-Varianten desselben Haupt-Interpreten → genau EINE Karte.
        assert_eq!(ac.len(), 1);
        assert_eq!(ac[0].artist, "Beginner");
        assert_eq!(ac[0].track_count, 3);
    }

    #[test]
    fn albums_overview_groups_by_name_ignoring_artist() {
        let lib = Library::open_in_memory().unwrap();
        // Gleicher Albumname, verschiedene Interpreten → genau EINE Karte.
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
        // Anzeige-Interpret = der mit den meisten Titeln (A: 2 > B: 1).
        assert_eq!(live[0].artist, "Artist A");
    }

    #[test]
    fn multi_disc_tracks_ordered_by_disc_then_track() {
        let lib = Library::open_in_memory().unwrap();
        // Zwei CDs, absichtlich „verkehrt herum" eingefügt.
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
        // Erst Disc 1 (Track 1,2), dann Disc 2 (Track 1,2).
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

        // Frisch eingelesener Track hat keine Resume-Position.
        let t = lib.track_by_path("/a/hoerspiel.mp3").unwrap().unwrap();
        assert_eq!(t.resume_ms, 0);

        // Position speichern und wieder auslesen.
        lib.set_resume_path("/a/hoerspiel.mp3", 123_456).unwrap();
        let t = lib.track_by_path("/a/hoerspiel.mp3").unwrap().unwrap();
        assert_eq!(t.resume_ms, 123_456);

        // Zurücksetzen (Titel zu Ende gehört).
        lib.set_resume_path("/a/hoerspiel.mp3", 0).unwrap();
        assert_eq!(lib.track_by_path("/a/hoerspiel.mp3").unwrap().unwrap().resume_ms, 0);
    }

    #[test]
    fn track_by_path_unknown_is_none_and_setresume_noop() {
        let lib = Library::open_in_memory().unwrap();
        assert!(lib.track_by_path("/nicht/da.mp3").unwrap().is_none());
        // Unbekannter Pfad: kein Fehler, kein Effekt.
        lib.set_resume_path("/nicht/da.mp3", 5000).unwrap();
        assert!(lib.track_by_path("/nicht/da.mp3").unwrap().is_none());
    }

    #[test]
    fn area_cascade_resolution() {
        use crate::core::category::Area;
        let lib = Library::open_in_memory().unwrap();
        // Ohne Festlegung: Standard = Dateisystem/Interpreten/Alben.
        assert_eq!(
            lib.resolve_areas(Some("X"), Some("Y"), "/a/1.mp3"),
            Area::DEFAULT.to_vec()
        );

        // Interpret-Ebene = nur Hörbücher → vererbt auf Album und Titel.
        lib.set_category("artist", "X", Some("audiobooks")).unwrap();
        assert_eq!(
            lib.resolve_areas(Some("X"), Some("Y"), "/a/1.mp3"),
            vec![Area::Audiobooks]
        );
        assert_eq!(lib.album_areas("X", "Y"), vec![Area::Audiobooks]);

        // Titel-Ebene gewinnt: leere Liste = ausgeblendet.
        lib.set_category("track", "/a/1.mp3", Some("")).unwrap();
        assert!(lib
            .resolve_areas(Some("X"), Some("Y"), "/a/1.mp3")
            .is_empty());
        // album_areas/artist_areas ignorieren die Titel-Ebene.
        assert_eq!(lib.album_areas("X", "Y"), vec![Area::Audiobooks]);
    }

    // ---- Equalizer-Kaskade ----

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

        // Spezifischste Ebene gewinnt; nach dem Entfernen greift die nächsthöhere.
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
        // Standard-Ausgang: spezifische Titel-Einstellung.
        lib.set_eq("", "track", "/a/1.mp3", &bands(4.0)).unwrap();
        // Konkreter Ausgang: nur eine globale Einstellung.
        lib.set_eq("sink1", "global", "", &bands(9.0)).unwrap();
        // Dokumentiertes Verhalten: der konkrete Ausgang wird komplett zuerst
        // aufgelöst – dessen Global schlägt den Titel des Standard-Ausgangs.
        assert_eq!(
            lib.resolve_eq("sink1", Some("X"), Some("Y"), "/a/1.mp3"),
            Some(bands(9.0))
        );
        // Für den Standard-Ausgang selbst gilt weiter die Titel-Einstellung.
        assert_eq!(
            lib.resolve_eq("", Some("X"), Some("Y"), "/a/1.mp3"),
            Some(bands(4.0))
        );
    }

    #[test]
    fn eq_falls_back_to_default_output() {
        let lib = Library::open_in_memory().unwrap();
        lib.set_eq("", "global", "", &bands(1.0)).unwrap();
        // Konkreter Ausgang hat nichts → Rückfall auf den Standard-Ausgang.
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
        // Gleicher Albumname, anderer Interpret → kein Treffer auf Album-Ebene.
        assert_eq!(lib.resolve_eq("", Some("Z"), Some("Y"), "/a/1.mp3"), None);
        // Richtiger Interpret → Treffer.
        assert_eq!(
            lib.resolve_eq("", Some("X"), Some("Y"), "/a/1.mp3"),
            Some(bands(3.0))
        );
    }
}
