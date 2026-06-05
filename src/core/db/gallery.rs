//! Album/artist gallery image queries for [`Library`] (split out of db.rs).

use anyhow::Result;

use super::Library;

impl Library {
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
                stmt.execute(rusqlite::params![
                    artist, album, i as i64, path, kind, source
                ])?;
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
        let rows = stmt.query_map(rusqlite::params![artist, album], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Replaces the stored artist images (order = idx).
    pub fn set_artist_images(&self, name: &str, images: &[(String, String, String)]) -> Result<()> {
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
}
