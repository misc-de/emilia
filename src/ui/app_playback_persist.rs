//! Playback persistence: the saved queue, resume positions (tracks,
//! episodes, YouTube videos) and the listening-statistics play sessions.
//! Split out of [`crate::ui::app_playback`] – pure reordering, the methods
//! remain inherent `impl App` methods.

use std::path::PathBuf;

use crate::model::Track;
use crate::ui::app::{guarded_resume, App, PlaySession};
use crate::ui::app_playback::{CURRENT_PATH_KEY, CURRENT_POS_KEY};

impl App {
    /// Whether this track keeps a resume position: only long-form material and
    /// audiobooks do (see [`crate::core::db::Library::track_resumable`]) — a
    /// song is always started from the beginning. On top of that the
    /// `guarded_resume` guards drop a position that is very close to the start
    /// or the end of the track.
    pub(crate) fn should_resume(&self, t: &Track) -> bool {
        self.library.track_resumable(t)
    }

    /// Saves the current queue (paths + position) for
    /// restoration after a restart of the app.
    pub(crate) fn save_queue(&self) {
        let paths = self
            .transport
            .queue
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = self.library.set_setting("queue_paths", &paths);
        let _ = self
            .library
            .set_setting("queue_pos", &self.transport.queue_pos.to_string());
        // The user-curated queue (explicit "Add to queue") persists separately.
        let user_paths = self
            .transport
            .user_queue
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = self.library.set_setting("user_queue_paths", &user_paths);
    }

    /// Saves how far the loaded track has played — in two places, with two
    /// very different lifetimes:
    ///
    /// * As part of the **playback state** ([`Self::save_current_position`]),
    ///   for every track. This is what lets a paused or stopped album pick the
    ///   running song back up where it stood, a restart of the app included.
    ///   It belongs to *this* track being the loaded one and is left behind as
    ///   soon as another one starts.
    /// * As the track's **own resume point**, only for material that keeps one
    ///   (see [`Self::should_resume`]). That one outlives the session: an
    ///   audiobook tapped weeks later still continues where it was left off.
    ///
    /// Near the start or the end both are reset to 0, so a nearly finished
    /// track starts over rather than ending a moment after it began.
    pub(crate) fn save_resume(&self) {
        let Some(path) = self.transport.playing_path.clone() else {
            return;
        };
        let path_str = path.to_string_lossy();
        let Some(pos) = self.player.position_ms() else {
            return;
        };
        let track = self.library.track_by_path(&path_str).ok().flatten();
        let dur = self
            .player
            .duration_ms()
            .or_else(|| track.as_ref().and_then(|t| t.duration_ms))
            .unwrap_or(0);
        self.save_current_position(&path_str, pos, dur);
        if matches!(&track, Some(t) if self.should_resume(t)) {
            let _ = self
                .library
                .set_resume_path(&path_str, guarded_resume(pos, dur));
        }
    }

    /// Records which track is loaded and how far it has played, as **playback
    /// state** rather than as a property of the track (see [`Self::save_resume`]
    /// for why the two are kept apart). Read back on the next start, where it
    /// only applies if that same track is still the one the queue points at.
    pub(crate) fn save_current_position(&self, path: &str, pos_ms: i64, dur_ms: i64) {
        let _ = self.library.set_setting(CURRENT_PATH_KEY, path);
        let _ = self
            .library
            .set_setting(CURRENT_POS_KEY, &guarded_resume(pos_ms, dur_ms).to_string());
    }

    /// Saves the playback position of the running podcast episode (resume,
    /// by the audio URL). Near the start/end it is set to 0 (counts as
    /// new or finished). No-op when no episode is currently playing.
    pub(crate) fn save_episode_progress(&self) {
        let Some(url) = self.podcasts.playing_episode_url.clone() else {
            return;
        };
        let Some(pos) = self.player.position_ms() else {
            return;
        };
        let dur = self
            .player
            .duration_ms()
            .unwrap_or(self.mini.track_duration_ms);
        let _ = self
            .library
            .set_episode_progress(&url, guarded_resume(pos, dur));
    }

    /// Saves the watch position of the running YouTube item — but only for
    /// long-form ones (talks/streams/podcasts, see
    /// [`crate::core::youtube::LONGFORM_SECS`]); songs are meant to start from
    /// the beginning next time. No-op when no video is playing.
    pub(crate) fn save_yt_progress(&self) {
        let Some(vid) = self.youtube.playing_video_id.clone() else {
            return;
        };
        let Some(pos) = self.player.position_ms() else {
            return;
        };
        let dur = self
            .player
            .duration_ms()
            .unwrap_or(self.mini.track_duration_ms);
        if !crate::core::youtube::is_longform((dur > 0).then_some(dur / 1000)) {
            return;
        }
        let _ = self.library.set_yt_progress(&vid, guarded_resume(pos, dur));
    }

    /// Finalizes the running listening session and writes it as one
    /// `play_event` into the statistics. `completed` = listened to the end (EOS).
    /// Without a session nothing happens (idempotent).
    pub(crate) fn finalize_play_session(&mut self, completed: bool) {
        if let Some(s) = self.transport.play_session.take() {
            let dur = if s.duration_ms > 0 {
                s.duration_ms
            } else {
                self.mini.track_duration_ms
            };
            let _ = self.library.log_play(
                &s.path.to_string_lossy(),
                s.started_at,
                s.played_ms,
                dur,
                completed,
                s.source, // "single" for a tapped song → kept out of the album stats.
            );
        }
        *self.transport.close_session.borrow_mut() = None;
    }

    /// Opens a new statistics listening session for `path` and mirrors it into
    /// `close_session` (so a hard exit still logs it). Local tracks, podcast
    /// episodes and streamed YouTube all funnel through this into one
    /// `play_event` once [`Self::finalize_play_session`] runs. `duration_ms`
    /// may be 0 when not yet known – the tick backfills it.
    pub(crate) fn start_play_session(&mut self, path: PathBuf, duration_ms: i64) {
        let now = crate::ui::app::unix_now();
        let path_str = path.to_string_lossy().into_owned();
        // Consume the one-shot source tag set by the caller (e.g. "single");
        // a fresh session without one counts towards its album normally.
        let source = self.transport.next_source.take();
        self.transport.play_session = Some(PlaySession {
            path,
            started_at: now,
            played_ms: 0,
            duration_ms,
            source,
        });
        *self.transport.close_session.borrow_mut() = Some((path_str, now, 0, duration_ms));
    }

    /// Restores the saved queue and user queue at startup (called from
    /// `App::init` once the model exists).
    pub(crate) fn restore_saved_queue(&mut self) {
        // Restore the queue from last time (only still existing
        // files). It is **not** played automatically – the track sits
        // ready in the mini player and starts when "Play" is pressed.
        let saved_pos: usize = self
            .library
            .get_setting("queue_pos")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let raw_queue: Vec<PathBuf> = self
            .library
            .get_setting("queue_paths")
            .ok()
            .flatten()
            .map(|s| {
                s.split('\n')
                    .filter(|l| !l.is_empty())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        let mut q = Vec::new();
        let mut q_pos = 0usize;
        for (i, p) in raw_queue.iter().enumerate() {
            if p.exists() {
                if i <= saved_pos {
                    q_pos = q.len();
                }
                q.push(p.clone());
            }
        }
        if !q.is_empty() {
            q_pos = q_pos.min(q.len() - 1);
            self.mini.now_playing = Some(self.display_name(&q[q_pos]));
            // How far that track had played when Emilia was last closed. Kept
            // for every track (a song included), because it describes where
            // listening stopped rather than a property of the track — so a
            // paused album carries on mid-song instead of restarting it. It
            // only counts for this one track; `play_current` drops it as soon
            // as anything else starts.
            let at: i64 = self
                .library
                .get_setting(crate::ui::app_playback::CURRENT_POS_KEY)
                .ok()
                .flatten()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let same_track = self
                .library
                .get_setting(crate::ui::app_playback::CURRENT_PATH_KEY)
                .ok()
                .flatten()
                .is_some_and(|p| std::path::Path::new(&p) == q[q_pos]);
            if at > 0 && same_track {
                self.transport.resume_current = Some((q[q_pos].clone(), at));
                // Show the position in the player bar right away, so the bar
                // reflects where pressing play will pick things up.
                self.mini.position_ms = at;
                self.mini.track_duration_ms = self
                    .library
                    .track_by_path(&q[q_pos].to_string_lossy())
                    .ok()
                    .flatten()
                    .and_then(|t| t.duration_ms)
                    .unwrap_or(0);
            }
            self.transport.queue = q;
            self.transport.queue_pos = q_pos;
        }

        // Restore the explicit user queue ("Add to queue"). Streamable remote
        // entries (YouTube `yt:` / Nextcloud `nc:`) have no local file but are
        // still playable, so they are kept alongside existing local files.
        self.transport.user_queue = self
            .library
            .get_setting("user_queue_paths")
            .ok()
            .flatten()
            .map(|s| {
                s.split('\n')
                    .filter(|l| !l.is_empty())
                    .map(PathBuf::from)
                    .filter(|p| {
                        let s = p.to_string_lossy();
                        p.exists()
                            || crate::core::youtube::parse_yt_path(&s).is_some()
                            || crate::core::webdav::parse_nc_path(&s).is_some()
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
}
