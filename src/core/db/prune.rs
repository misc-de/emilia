//! Pruning of orphaned tracks and lost image pointers for [`Library`] (split out of db.rs).

use anyhow::Result;

use super::{like_escape, Library};

impl Library {
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

    /// Drops image pointers whose file has vanished: album covers, artist photos
    /// and the gallery entries of both. The image cache is disposable
    /// (`~/.cache/emilia`, and a Flatpak install has its own separate one) and
    /// may be cleared behind the app's back, while the rows pointing into it
    /// survive in the database. Since [`Self::albums_missing_cover`] and the
    /// artist phase of the enrichment only ask whether a pointer is *set*, those
    /// albums/artists would count as done and stay blank forever. Clearing the
    /// stale pointers puts them back in the queue: the next enrichment run
    /// refills them from the tags or online, galleries are refetched when a
    /// detail view opens. Returns the number of pointers dropped.
    pub fn prune_lost_images(&self) -> Result<usize> {
        let lost = |path: &str| crate::core::online::image_file_lost(path);
        let tx = self.conn.unchecked_transaction()?;
        let mut dropped = 0usize;

        // Album covers.
        let stale: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT artist, album, cover_path FROM album_meta
                 WHERE cover_path IS NOT NULL AND cover_path <> ''",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(_, _, path)| lost(path))
                .map(|(artist, album, _)| (artist, album))
                .collect()
        };
        {
            let mut upd = tx.prepare(
                "UPDATE album_meta SET cover_path = NULL WHERE artist = ?1 AND album = ?2",
            )?;
            for (artist, album) in &stale {
                dropped += upd.execute(rusqlite::params![artist, album])?;
            }
        }

        // Artist photos. Back to `pending` as well, because the enrichment skips
        // artists that are already `matched` regardless of the path.
        let stale: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT name, image_path FROM artist_meta
                 WHERE image_path IS NOT NULL AND image_path <> ''",
            )?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(_, path)| lost(path))
                .map(|(name, _)| name)
                .collect()
        };
        {
            let mut upd = tx.prepare(
                "UPDATE artist_meta SET image_path = NULL, status = 'pending' WHERE name = ?1",
            )?;
            for name in &stale {
                dropped += upd.execute([name])?;
            }
        }

        // Galleries. Both tables are keyed differently but only ever hold cached
        // files, so they are pruned by `rowid` (the table names are constants
        // below, never user input).
        {
            let prune_gallery = |table: &str| -> Result<usize> {
                let stale: Vec<i64> = {
                    let mut stmt = tx.prepare(&format!("SELECT rowid, path FROM {table}"))?;
                    let rows =
                        stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                        .into_iter()
                        .filter(|(_, path)| lost(path))
                        .map(|(id, _)| id)
                        .collect()
                };
                let mut del = tx.prepare(&format!("DELETE FROM {table} WHERE rowid = ?1"))?;
                let mut n = 0usize;
                for id in &stale {
                    n += del.execute([id])?;
                }
                Ok(n)
            };
            dropped += prune_gallery("album_image")?;
            dropped += prune_gallery("artist_image")?;
        }

        tx.commit()?;
        if dropped > 0 {
            tracing::info!("Dropped {dropped} image pointer(s) whose file is gone");
        }
        Ok(dropped)
    }
}
