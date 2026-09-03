//! Music sources (local folders, Nextcloud/SMB/Google Drive) for [`Library`]
//! (split out of db.rs).

use anyhow::Result;

use super::Library;
use crate::model::Source;

impl Library {
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

    /// Renames a source (the tab label).
    pub fn set_source_name(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE source SET name = ?1 WHERE id = ?2",
            rusqlite::params![name, id],
        )?;
        Ok(())
    }

    /// Changes the root path of a local source (its "mount point").
    pub fn set_source_path(&self, id: i64, path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE source SET path = ?1 WHERE id = ?2",
            rusqlite::params![path, id],
        )?;
        Ok(())
    }

    /// Changes the indexed music subpath of a WebDAV source. The caller should
    /// re-index (see [`clear_source_tracks`](Self::clear_source_tracks)) since
    /// the synthetic track paths are relative to this root.
    pub fn set_source_music_path(&self, id: i64, music_path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE source SET music_path = ?1 WHERE id = ?2",
            rusqlite::params![music_path, id],
        )?;
        Ok(())
    }

    /// Removes the indexed cloud tracks (`nc:<id>:…`) of a source **without**
    /// deleting the source itself — used to re-index after its music path
    /// changed.
    pub fn clear_source_tracks(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM track WHERE path LIKE ?1",
            [format!("nc:{id}:%")],
        )?;
        Ok(())
    }

    /// Removes a source by its ID.
    pub fn delete_source(&self, id: i64) -> Result<()> {
        crate::core::secrets::clear_source_password(id);
        crate::core::gdrive::forget_source(id);
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
}
