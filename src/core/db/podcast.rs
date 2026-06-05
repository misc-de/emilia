//! Podcast queries for [`Library`] (split out of db.rs).

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::Library;
use crate::model::*;

impl Library {
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
        Ok(self.conn.query_row(
            "SELECT id FROM podcast WHERE feed_url = ?1",
            [feed_url],
            |r| r.get(0),
        )?)
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

    /// Feed URLs of all subscribed podcasts (for refreshing every feed at once).
    pub fn podcast_feed_urls(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT feed_url FROM podcast")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
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
}
