//! YouTube queries for [`Library`] (split out of db.rs).

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::Library;
use crate::model::*;

impl Library {
    // ---- YouTube (subscribed channels, cached videos, offline copies) ----

    /// Subscribes to a channel (or updates title/url/thumbnail for a known one)
    /// and returns the channel's DB id.
    pub fn subscribe_channel(
        &self,
        channel_id: &str,
        title: &str,
        url: &str,
        thumbnail: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO yt_channel (channel_id, title, url, thumbnail, added_at)
             VALUES (?1, ?2, ?3, ?4, strftime('%s','now'))
             ON CONFLICT(channel_id) DO UPDATE SET
                title = excluded.title, url = excluded.url, thumbnail = excluded.thumbnail",
            rusqlite::params![channel_id, title, url, thumbnail],
        )?;
        Ok(self.conn.query_row(
            "SELECT id FROM yt_channel WHERE channel_id = ?1",
            [channel_id],
            |r| r.get(0),
        )?)
    }

    /// All subscribed channels as (id, title, url, thumbnail, video count),
    /// newest first.
    pub fn channels(&self) -> Result<Vec<(i64, String, String, Option<String>, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.title, c.url, c.thumbnail, COUNT(v.video_id)
             FROM yt_channel c LEFT JOIN yt_video v ON v.channel_id = c.id
             GROUP BY c.id ORDER BY c.added_at DESC, c.title COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Removes a channel subscription along with its cached videos.
    pub fn delete_channel(&self, id: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM yt_video WHERE channel_id = ?1", [id])?;
        tx.execute("DELETE FROM yt_channel WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces the cached videos of a channel (order = listing order).
    pub fn set_channel_videos(&self, channel_id: i64, videos: &[YtVideo]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM yt_video WHERE channel_id = ?1", [channel_id])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO yt_video
                    (channel_id, position, video_id, title, url, duration, published, thumbnail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (i, v) in videos.iter().enumerate() {
                stmt.execute(rusqlite::params![
                    channel_id,
                    i as i64,
                    v.video_id,
                    v.title,
                    v.url,
                    v.duration,
                    v.published,
                    v.thumbnail
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Cached videos of a channel in listing order.
    pub fn channel_videos(&self, channel_id: i64) -> Result<Vec<YtVideo>> {
        let mut stmt = self.conn.prepare(
            "SELECT video_id, title, url, duration, published, thumbnail FROM yt_video
             WHERE channel_id = ?1 ORDER BY position",
        )?;
        let rows = stmt.query_map([channel_id], |r| {
            Ok(YtVideo {
                video_id: r.get(0)?,
                title: r.get(1)?,
                url: r.get(2)?,
                duration: r.get(3)?,
                published: r.get(4)?,
                thumbnail: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Newest videos across all subscribed channels (for the cross-channel
    /// "Newest videos" view). Channel order, then listing order within.
    pub fn all_videos(&self) -> Result<Vec<YtVideoRef>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.title, c.thumbnail, v.video_id, v.title, v.duration, v.published
             FROM yt_video v JOIN yt_channel c ON c.id = v.channel_id
             ORDER BY c.added_at DESC, v.position",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(YtVideoRef {
                channel_title: r.get(0)?,
                channel_thumb: r.get(1)?,
                video_id: r.get(2)?,
                title: r.get(3)?,
                duration: r.get(4)?,
                published: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Stored info for a video that belongs to a subscribed channel:
    /// (channel title, duration, thumbnail URL). `None` if not stored – so the
    /// detail can show persisted data without re-fetching from the network.
    pub fn yt_video_info(
        &self,
        video_id: &str,
    ) -> Result<Option<(String, Option<i64>, Option<String>)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT c.title, v.duration, v.thumbnail
                 FROM yt_video v JOIN yt_channel c ON c.id = v.channel_id
                 WHERE v.video_id = ?1 LIMIT 1",
                [video_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?)
    }

    /// Local path of a downloaded video, or `None` if not downloaded.
    pub fn yt_download(&self, video_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT path FROM yt_download WHERE video_id = ?1",
                [video_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Records that `video_id` has a local copy at `path` (an offline download or
    /// a track filed into the library). This is what marks a `yt:<id>` track as
    /// "available offline": both playback (`start_track_playback`) and the detail
    /// dialog consult it to play the local file instead of streaming.
    pub fn set_yt_download(&self, video_id: &str, path: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO yt_download (video_id, path, downloaded_at)
             VALUES (?1, ?2, strftime('%s','now'))
             ON CONFLICT(video_id) DO UPDATE SET
                path = excluded.path, downloaded_at = excluded.downloaded_at",
            rusqlite::params![video_id, path],
        )?;
        Ok(())
    }

    const RECENT_CAP: i64 = 100;

    /// Records a video as "recently played" (moves it to the top).
    pub fn add_recent_video(
        &self,
        video_id: &str,
        title: &str,
        thumbnail: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO yt_recent (video_id, title, thumbnail, played_at, kind, count)
             VALUES (?1, ?2, ?3, strftime('%s','now'), 'video', 0)
             ON CONFLICT(video_id) DO UPDATE SET
                title = excluded.title, thumbnail = excluded.thumbnail,
                played_at = excluded.played_at, kind = 'video'",
            rusqlite::params![video_id, title, thumbnail],
        )?;
        self.trim_recent()
    }

    /// Records a playlist as "recently played" (keyed by its URL; `count` =
    /// number of songs, `total_duration` = summed runtime in seconds if known).
    /// Moves it to the top. A `None` total keeps any previously stored value.
    pub fn add_recent_playlist(
        &self,
        url: &str,
        title: &str,
        count: i64,
        total_duration: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO yt_recent (video_id, title, played_at, kind, count, total_duration)
             VALUES (?1, ?2, strftime('%s','now'), 'playlist', ?3, ?4)
             ON CONFLICT(video_id) DO UPDATE SET
                title = excluded.title, played_at = excluded.played_at,
                kind = 'playlist', count = excluded.count,
                total_duration = COALESCE(excluded.total_duration, yt_recent.total_duration)",
            rusqlite::params![url, title, count, total_duration],
        )?;
        self.trim_recent()
    }

    /// Caps the history at the newest `RECENT_CAP` entries.
    fn trim_recent(&self) -> Result<()> {
        self.conn.execute(
            "DELETE FROM yt_recent WHERE video_id NOT IN
                (SELECT video_id FROM yt_recent ORDER BY played_at DESC LIMIT ?1)",
            [Self::RECENT_CAP],
        )?;
        Ok(())
    }

    /// Stores the enriched artist + cover for a recent video (from the online
    /// lookup). No-op if the video is no longer in the history.
    pub fn set_recent_meta(
        &self,
        video_id: &str,
        artist: Option<&str>,
        cover: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE yt_recent SET artist = ?2, thumbnail = COALESCE(?3, thumbnail)
             WHERE video_id = ?1",
            rusqlite::params![video_id, artist, cover],
        )?;
        Ok(())
    }

    /// Recently played items (videos and playlists), newest first.
    pub fn recent_videos(&self, limit: usize) -> Result<Vec<YtRecent>> {
        let mut stmt = self.conn.prepare(
            // Duration: prefer the meta cache (`yt_title`, set on play), else the
            // channel-feed value (`yt_video`) so feed videos show a runtime even
            // when they were never resolved individually. Playlists match neither
            // (their key is the playlist URL) → NULL, as intended.
            "SELECT r.video_id, r.title, r.artist, r.kind, r.count, r.thumbnail,
                    COALESCE(t.duration, v.duration), r.total_duration
             FROM yt_recent r
             LEFT JOIN yt_title t ON t.video_id = r.video_id
             LEFT JOIN yt_video v ON v.video_id = r.video_id
             ORDER BY r.played_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(YtRecent {
                video_id: r.get(0)?,
                title: r.get(1)?,
                artist: r.get(2)?,
                kind: r.get(3)?,
                count: r.get(4)?,
                thumbnail: r.get(5)?,
                duration: r.get(6)?,
                total_duration: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Sets the representative thumbnail of a recent item (used for playlists,
    /// whose cover is derived from their first song). No-op if the item is not
    /// in the history.
    pub fn set_recent_thumb(&self, key: &str, thumb: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE yt_recent SET thumbnail = ?2 WHERE video_id = ?1",
            rusqlite::params![key, thumb],
        )?;
        Ok(())
    }

    /// Whether an item (video id or playlist URL) is in the "Recent" history.
    pub fn is_recent(&self, key: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM yt_recent WHERE video_id = ?1", [key], |_| {
                Ok(())
            })
            .optional()?
            .is_some())
    }

    /// Removes an item (video id or playlist URL) from the "Recent" history.
    pub fn delete_recent(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM yt_recent WHERE video_id = ?1", [key])?;
        Ok(())
    }

    /// Whether a path/URL is a known podcast episode (its audio enclosure URL).
    /// Used to label a playlist entry's source.
    pub fn is_podcast_episode(&self, url: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM episode WHERE audio_url = ?1 LIMIT 1",
                [url],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Caches the display title of a `yt:<id>` track.
    pub fn set_yt_title(&self, video_id: &str, title: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO yt_title (video_id, title) VALUES (?1, ?2)
             ON CONFLICT(video_id) DO UPDATE SET title = excluded.title",
            rusqlite::params![video_id, title],
        )?;
        Ok(())
    }

    /// Like [`set_yt_title`] but also caches the duration (seconds) when known,
    /// so queue/playlist rows can show a runtime for `yt:` tracks. A `None`
    /// duration keeps any previously cached value.
    pub fn set_yt_meta(&self, video_id: &str, title: &str, duration: Option<i64>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO yt_title (video_id, title, duration) VALUES (?1, ?2, ?3)
             ON CONFLICT(video_id) DO UPDATE SET
                 title = excluded.title,
                 duration = COALESCE(excluded.duration, yt_title.duration)",
            rusqlite::params![video_id, title, duration],
        )?;
        Ok(())
    }

    /// Cached duration (seconds) of a `yt:` video, if known.
    pub fn yt_duration(&self, video_id: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT duration FROM yt_title WHERE video_id = ?1",
                [video_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Cached display title of a `yt:<id>` track, if known.
    pub fn yt_title(&self, video_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT title FROM yt_title WHERE video_id = ?1",
                [video_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Caches a browsed YouTube playlist's song list (`songs` = JSON) so the next
    /// open is instant. `fetched_at` is stamped to now for staleness checks.
    pub fn set_yt_playlist_cache(&self, url: &str, title: &str, songs: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO yt_playlist_cache (url, title, songs, fetched_at)
             VALUES (?1, ?2, ?3, strftime('%s','now'))
             ON CONFLICT(url) DO UPDATE SET
                 title = excluded.title,
                 songs = excluded.songs,
                 fetched_at = excluded.fetched_at",
            rusqlite::params![url, title, songs],
        )?;
        Ok(())
    }

    /// Cached song list (JSON) of a browsed playlist plus its `fetched_at`
    /// (Unix seconds), or `None` if never cached.
    pub fn yt_playlist_cache(&self, url: &str) -> Result<Option<(String, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT songs, fetched_at FROM yt_playlist_cache WHERE url = ?1",
                [url],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?)
    }

    /// Id of the mirrored playlist for a given `origin` key (e.g. a YouTube
    /// playlist URL), if any. Never matches user playlists (`origin IS NULL`).
    pub fn yt_playlist_id(&self, origin: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM playlist WHERE origin = ?1 LIMIT 1",
                [origin],
                |r| r.get::<_, i64>(0),
            )
            .optional()?)
    }

    /// Mirrors a source playlist (identified by its `origin` key, e.g. a YouTube
    /// playlist URL) into the Playlists section: refreshes the existing mirror
    /// for that origin in place, or creates one. User playlists (`origin IS
    /// NULL`) are never touched – even one with the same `name`. Returns the
    /// mirror's playlist id.
    pub fn replace_yt_playlist(&self, origin: &str, name: &str, paths: &[String]) -> Result<i64> {
        let tx = self.conn.unchecked_transaction()?;
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM playlist WHERE origin = ?1 LIMIT 1",
                [origin],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        let id = match existing {
            Some(id) => {
                tx.execute("DELETE FROM playlist_item WHERE playlist_id = ?1", [id])?;
                tx.execute(
                    "UPDATE playlist SET name = ?1 WHERE id = ?2",
                    rusqlite::params![name, id],
                )?;
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO playlist (name, created_at, origin)
                     VALUES (?1, strftime('%s','now'), ?2)",
                    rusqlite::params![name, origin],
                )?;
                tx.last_insert_rowid()
            }
        };
        if !paths.is_empty() {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO playlist_item (playlist_id, position, path) VALUES (?1, ?2, ?3)",
            )?;
            for (i, path) in paths.iter().enumerate() {
                stmt.execute(rusqlite::params![id, i as i64, path])?;
            }
        }
        tx.commit()?;
        Ok(id)
    }
}
