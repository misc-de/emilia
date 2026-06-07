//! Playback: queue, play/pause/next/prev, resume logic and the
//! running equalizer. Extracted from app.rs – pure reordering, no
//! change in behavior; the methods remain inherent `impl App` methods.

use std::path::{Path, PathBuf};

use gtk::prelude::GtkWindowExt;
use relm4::{adw, gtk, ComponentSender};

use crate::core::scanner;
use crate::core::webdav::{self, Creds};
use crate::model::Track;
use crate::ui::app::{guarded_resume, ActiveSource, App, Msg, PlaySession, RemoteTrack};
use crate::ui::fs_row::{FsEntry, FsInput};

impl App {
    /// Refreshes the queue marker of all visible file rows.
    pub(crate) fn refresh_queue_icons(&mut self) {
        // The "in queue" marker reflects the explicit user queue, not the active
        // context (the album currently playing through).
        let queued: std::collections::HashSet<PathBuf> =
            self.transport.user_queue.iter().cloned().collect();
        // Currently playing track (for the play marker).
        let active_path = self.transport.queue.get(self.transport.queue_pos).cloned();
        // Remote playback: the active entry is marked via the rel path.
        let active_rel = if self.files.playing_remote {
            self.files
                .remote_queue
                .get(self.files.remote_pos)
                .map(|t| t.rel_path.clone())
        } else {
            None
        };
        let states: Vec<(usize, bool, bool)> = {
            let guard = self.libview.entries.guard();
            (0..guard.len())
                .filter_map(|i| {
                    guard.get(i).map(|r| {
                        let is_file = !r.entry.is_dir();
                        match r.entry.path() {
                            Some(p) => {
                                let q = is_file && queued.contains(p);
                                let a = is_file && active_path.as_deref() == Some(p.as_path());
                                (i, q, a)
                            }
                            None => {
                                // Remote entry: active marker via rel_path.
                                let a = is_file
                                    && active_rel.is_some()
                                    && r.entry.rel_path() == active_rel.as_deref();
                                (i, false, a)
                            }
                        }
                    })
                })
                .collect()
        };
        let playing = self.mini.playing;
        for (i, q, a) in states {
            self.libview.entries.send(i, FsInput::SetQueued(q));
            self.libview
                .entries
                .send(i, FsInput::SetActive { active: a, playing });
        }
        // Sync the play row of an open detail dialog with the playback state.
        self.refresh_ctx_play();
        // Play/pause icons of the podcast episodes (and the detail "Play" row).
        self.refresh_episode_icons();
        // …and of the YouTube video rows.
        self.refresh_yt_icons();
        // …and of the saved-recording rows.
        self.refresh_recording_icons();
    }

    /// Credentials of the currently active WebDAV source (if one is active).
    pub(crate) fn active_webdav_creds(&self) -> Option<Creds> {
        let ActiveSource::Source(id) = self.files.active_source else {
            return None;
        };
        let s = self.files.sources.iter().find(|s| s.id == id)?;
        if s.kind != "webdav" {
            return None;
        }
        Creds::from_source(s)
    }

    /// Local cache path of a remote file of the active source (or `None`).
    pub(crate) fn remote_cache_path(&self, rel: &str) -> Option<PathBuf> {
        let ActiveSource::Source(id) = self.files.active_source else {
            return None;
        };
        Some(webdav::cache_path(id, rel))
    }

    /// Tap a remote file: tapping the running track again
    /// toggles pause/resume; otherwise the folder row is set as the remote queue
    /// and played from the chosen track.
    pub(crate) fn activate_remote(&mut self, rel: &str) {
        let is_active = self.files.playing_remote
            && self
                .files
                .remote_queue
                .get(self.files.remote_pos)
                .is_some_and(|t| t.rel_path == rel);
        if is_active {
            if self.mini.playing {
                self.save_resume();
                self.player.pause();
            } else {
                self.player.resume();
            }
            self.mini.playing = !self.mini.playing;
            self.mpris.set_playing(self.mini.playing);
            self.refresh_queue_icons();
            return;
        }
        // Build the remote row from the visible file rows (folder sequence).
        let mut queue = Vec::new();
        let mut start = 0;
        {
            let guard = self.libview.entries.guard();
            for i in 0..guard.len() {
                if let Some(row) = guard.get(i) {
                    if let FsEntry::RemoteFile { rel_path, .. } = &row.entry {
                        if rel_path == rel {
                            start = queue.len();
                        }
                        queue.push(RemoteTrack {
                            rel_path: rel_path.clone(),
                            title: row.entry.display_title(),
                        });
                    }
                }
            }
        }
        if queue.is_empty() {
            return;
        }
        self.files.remote_queue = queue;
        self.files.remote_pos = start;
        self.play_remote_current();
    }

    /// Plays the current track of the remote row – locally (if already
    /// downloaded) or streamed. Self-contained like podcast/station; the
    /// local `PathBuf` queue stays empty in the process.
    pub(crate) fn play_remote_current(&mut self) {
        let Some(creds) = self.active_webdav_creds() else {
            return;
        };
        let Some(track) = self.files.remote_queue.get(self.files.remote_pos).cloned() else {
            return;
        };
        self.save_resume();
        self.save_episode_progress();
        self.finalize_play_session(false);
        // Mark the remote context up front so a failure routes the skip to the
        // remote row (not the main queue).
        self.files.playing_remote = true;
        let cached = self.remote_cache_path(&track.rel_path);
        let is_stream = !matches!(&cached, Some(p) if p.exists());
        let result = match &cached {
            Some(p) if p.exists() => self.player.play_file(&p.to_string_lossy(), 0),
            _ => self
                .player
                .play_uri(&webdav::stream_uri(&creds, &track.rel_path), 0),
        };
        match result {
            Ok(()) => {
                self.transport.skip_count = 0;
                self.mini.now_playing = Some(track.title.clone());
                self.mini.current_album = None; // cloud track — no local album page
                self.mini.playing = true;
                // Streaming from Nextcloud buffers first → spinner until ready
                // (a cached copy plays instantly, so no spinner there).
                self.mini.loading = is_stream;
                self.transport.playing_path = None;
                self.podcasts.playing_episode_url = None;
                self.streaming.playing_stream = None;
                self.youtube.playing_video_id = None;
                self.files.playing_remote = true;
                self.stop_recorder();
                self.transport.queue.clear();
                self.transport.queue_pos = 0;
                self.mini.position_ms = 0;
                self.mini.track_duration_ms = 0;
                *self.transport.close_resume.borrow_mut() = None;
                self.mpris
                    .set_metadata(0, &track.title, None, None, None, None);
                self.mpris.set_playing(true);
                self.set_chapters(Vec::new());
                self.refresh_queue_icons();
            }
            Err(e) => {
                // Unreachable Nextcloud → skip to the next remote entry
                // (message-driven, so no recursion here).
                tracing::warn!("Remote playback failed, skipping: {e}");
                let _ = self.input.send(Msg::PlaybackError);
            }
        }
    }

    /// Next track of the remote row (for the next button and EOS advancing).
    pub(crate) fn remote_next(&mut self) {
        if self.files.remote_pos + 1 < self.files.remote_queue.len() {
            self.files.remote_pos += 1;
            self.play_remote_current();
        } else if self.transport.repeat && !self.files.remote_queue.is_empty() {
            self.files.remote_pos = 0;
            self.play_remote_current();
        } else {
            // End of the row – stop playback (like at the end of an episode).
            self.player.stop();
            self.mini.playing = false;
            self.transport.skip_count = 0;
            self.mpris.set_playing(false);
            self.refresh_queue_icons();
        }
    }

    /// Previous track of the remote row.
    pub(crate) fn remote_prev(&mut self) {
        if self.files.remote_pos > 0 {
            self.files.remote_pos -= 1;
            self.play_remote_current();
        }
    }

    /// Rebuilds the shuffle order (Fisher-Yates), with the currently
    /// running track in first place. This way every track of the queue plays
    /// exactly once, in random order.
    pub(crate) fn rebuild_shuffle_order(&mut self) {
        let len = self.transport.queue.len();
        let mut order: Vec<usize> = (0..len).collect();
        for i in (1..len).rev() {
            let j = gtk::glib::random_int_range(0, (i + 1) as i32) as usize;
            order.swap(i, j);
        }
        // Move the running track to the front so it isn't skipped immediately.
        if let Some(p) = order.iter().position(|&x| x == self.transport.queue_pos) {
            order.swap(0, p);
        }
        self.transport.shuffle_order = order;
        self.transport.shuffle_idx = 0;
    }

    /// Next track: when shuffling, the next of the shuffle order, otherwise the
    /// following one. At the end (all played) playback stops.
    pub(crate) fn play_next(&mut self) {
        // The explicit user queue jumps ahead of the rest of the context: take
        // the first queued track, splice it into the context right after the
        // current track and play it. Splicing (instead of a separate list) keeps
        // play_current/play_prev/save_queue working unchanged; the queue entry is
        // consumed as it starts playing.
        if !self.transport.user_queue.is_empty() {
            let path = self.transport.user_queue.remove(0);
            let at = if self.transport.queue.is_empty() {
                0
            } else {
                (self.transport.queue_pos + 1).min(self.transport.queue.len())
            };
            self.transport.queue.insert(at, path);
            self.transport.queue_pos = at;
            // The context length changed → let the shuffle order rebuild.
            self.transport.shuffle_order.clear();
            self.play_current();
            self.refresh_queue_icons();
            self.reload_queue_list();
            self.save_queue();
            return;
        }
        if self.transport.queue.is_empty() {
            return;
        }
        let len = self.transport.queue.len();
        let next = if self.transport.shuffle {
            // Reshuffle if the queue has changed or the running
            // track is no longer the expected one of the order (e.g. after
            // manual selection) – then keep shuffling from the current track.
            if self.transport.shuffle_order.len() != len
                || self.transport.shuffle_order.get(self.transport.shuffle_idx)
                    != Some(&self.transport.queue_pos)
            {
                self.rebuild_shuffle_order();
            }
            if self.transport.shuffle_idx + 1 < self.transport.shuffle_order.len() {
                self.transport.shuffle_idx += 1;
                Some(self.transport.shuffle_order[self.transport.shuffle_idx])
            } else {
                None
            }
        } else if self.transport.queue_pos + 1 < len {
            Some(self.transport.queue_pos + 1)
        } else {
            None
        };
        match next {
            Some(n) => {
                self.transport.queue_pos = n;
                self.play_current();
            }
            None if self.transport.repeat && !self.transport.queue.is_empty() => {
                // Repeat: start over at the end (single track likewise, since
                // the queue then has only one entry). Reshuffle when shuffling.
                if self.transport.shuffle {
                    self.rebuild_shuffle_order();
                    self.transport.queue_pos =
                        self.transport.shuffle_order.first().copied().unwrap_or(0);
                } else {
                    self.transport.queue_pos = 0;
                }
                self.play_current();
            }
            None => {
                // End of playback: stop and rewind to the start of the queue
                // so that the play button shows "Play" again and
                // pressing it again starts from the beginning (see TogglePlay).
                self.save_resume();
                // Finalize the running session (no-op if already done via EOS).
                self.finalize_play_session(false);
                self.player.stop();
                self.mini.playing = false;
                self.mini.loading = false;
                self.transport.playing_path = None;
                self.transport.queue_pos = 0;
                self.mini.position_ms = 0;
                self.mini.track_duration_ms = 0;
                self.transport.shuffle_order.clear();
                self.transport.shuffle_idx = 0;
                self.transport.skip_count = 0;
                *self.transport.close_resume.borrow_mut() = None;
                self.mpris.set_stopped();
                self.refresh_queue_icons();
                self.save_queue();
            }
        }
    }

    /// Back button: pressing once restarts the running track from the beginning,
    /// a second press **within one second** jumps to the previously
    /// played track (history).
    pub(crate) fn play_prev(&mut self) {
        let now = std::time::Instant::now();
        let double = self
            .transport
            .last_prev
            .is_some_and(|t| now.duration_since(t) <= std::time::Duration::from_secs(1));
        self.transport.last_prev = Some(now);

        if double {
            if let Some(prev) = self.transport.play_history.pop() {
                // Previous song: preferably play it at its queue position,
                // otherwise the path directly (without another history entry).
                self.transport.skip_history_push = true;
                if let Some(pos) = self.transport.queue.iter().position(|p| *p == prev) {
                    self.transport.queue_pos = pos;
                    self.play_current();
                } else {
                    self.transport.queue = vec![prev];
                    self.transport.queue_pos = 0;
                    self.play_current();
                }
                return;
            }
            // No history → sequentially one song back.
            if !self.transport.queue.is_empty() && self.transport.queue_pos > 0 {
                self.transport.skip_history_push = true;
                self.transport.queue_pos -= 1;
                self.play_current();
            }
            return;
        }

        // First press: if a new context was just started (track
        // has been running for < 5 s) and a displaced context lies on the stack,
        // restore it **including its playlist** and keep listening to the song
        // (resume from the DB). "Back" right after an accidentally
        // started song thus returns to the previous one.
        if !self.transport.nav_stack.is_empty() && self.player.position_ms().unwrap_or(0) < 5000 {
            if let Some((q, pos)) = self.transport.nav_stack.pop() {
                self.transport.skip_history_push = true;
                self.transport.queue = q;
                self.transport.queue_pos = pos.min(self.transport.queue.len().saturating_sub(1));
                self.play_current();
                self.refresh_queue_icons();
                return;
            }
        }

        // Otherwise: running track from the beginning.
        if self.transport.playing_path.is_some() {
            self.transport.skip_history_push = true;
            self.play_current();
        }
    }

    /// Plays the current entry of the queue.
    /// Display name of a track for the bar: "Artist - Title" from the tags,
    /// failing that the file name.
    /// Starts playback of a track path. Local paths go through
    /// `play_file`; **remote** tracks (synthetic path `nc:<id>:<rel>`) are
    /// played from the local cache or streamed directly from Nextcloud.
    /// Starts playback of `path_str`. Returns `Ok(true)` when a **network
    /// stream** (Nextcloud over the network) was started – it still has to
    /// buffer/preroll, so the caller shows a loading spinner until the player
    /// reports ready. `Ok(false)` for local files / cached copies, which start
    /// fast enough that a spinner would only flicker.
    pub(crate) fn start_track_playback(
        &self,
        path_str: &str,
        resume_ms: i64,
    ) -> anyhow::Result<bool> {
        // YouTube: an offline copy plays directly; a stream must be resolved
        // asynchronously (done in `play_current`), so reaching here without a
        // local file is an error – we never block the UI thread on `yt-dlp -g`.
        if let Some(video_id) = crate::core::youtube::parse_yt_path(path_str) {
            if let Some(local) = self
                .library
                .yt_download(&video_id)
                .ok()
                .flatten()
                .filter(|p| std::path::Path::new(p).exists())
            {
                return self.player.play_file(&local, resume_ms).map(|_| false);
            }
            return Err(anyhow::anyhow!(
                "YouTube stream must be resolved asynchronously"
            ));
        }
        if let Some((sid, rel)) = crate::core::webdav::parse_nc_path(path_str) {
            let cache = crate::core::webdav::cache_path(sid, &rel);
            if cache.exists() {
                return self
                    .player
                    .play_file(&cache.to_string_lossy(), resume_ms)
                    .map(|_| false);
            }
            if let Some(creds) = self
                .files
                .sources
                .iter()
                .find(|s| s.id == sid)
                .and_then(crate::core::webdav::Creds::from_source)
            {
                return self
                    .player
                    .play_uri(&crate::core::webdav::stream_uri(&creds, &rel), resume_ms)
                    .map(|_| true);
            }
            return Err(anyhow::anyhow!("Nextcloud source unavailable"));
        }
        // Local file: a missing path usually means an unmounted SD card / mount
        // point. Fail fast so the caller skips it (instead of a GStreamer round-trip).
        if !std::path::Path::new(path_str).exists() {
            return Err(anyhow::anyhow!(
                "file unavailable (mount missing?): {path_str}"
            ));
        }
        self.player.play_file(path_str, resume_ms).map(|_| false)
    }

    /// Display name of a track for the bar/queue: preferably from the
    /// database (also works for remote tracks), otherwise from the file.
    pub(crate) fn display_name(&self, path: &std::path::Path) -> String {
        let path_str = path.to_string_lossy();
        // YouTube tracks have no library row; use the cached title.
        if let Some(vid) = crate::core::youtube::parse_yt_path(&path_str) {
            if let Ok(Some(t)) = self.library.yt_title(&vid) {
                return t;
            }
        }
        if let Ok(Some(t)) = self.library.track_by_path(&path_str) {
            let title = if t.title.trim().is_empty() {
                path.file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                t.title
            };
            return match t.artist {
                Some(a) if !a.trim().is_empty() => format!("{a} - {title}"),
                _ => title,
            };
        }
        Self::track_display_name(path)
    }

    pub(crate) fn track_display_name(path: &std::path::Path) -> String {
        let stem = || {
            path.file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string()
        };
        match scanner::read_track(path) {
            Ok(t) => {
                let title = if t.title.trim().is_empty() {
                    stem()
                } else {
                    t.title
                };
                match t.artist {
                    Some(a) if !a.trim().is_empty() => format!("{a} - {title}"),
                    _ => title,
                }
            }
            Err(_) => stem(),
        }
    }

    /// A resume position is kept for **all** tracks: on the next start
    /// the track continues where it was stopped. The `guarded_resume`
    /// guards ensure that a nearly finished or just-started
    /// track starts over from the beginning.
    pub(crate) fn should_resume(&self, _t: &Track) -> bool {
        true
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

    /// Saves the current playback position of the loaded track as a
    /// resume point. Near the start or end it is reset to 0 so that a
    /// nearly finished track starts over from the beginning next time.
    pub(crate) fn save_resume(&self) {
        let Some(path) = self.transport.playing_path.clone() else {
            return;
        };
        let path_str = path.to_string_lossy();
        let Some(track) = self.library.track_by_path(&path_str).ok().flatten() else {
            return;
        };
        if !self.should_resume(&track) {
            return;
        }
        let Some(pos) = self.player.position_ms() else {
            return;
        };
        let dur = self.player.duration_ms().or(track.duration_ms).unwrap_or(0);
        let _ = self
            .library
            .set_resume_path(&path_str, guarded_resume(pos, dur));
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
                None, // source (queue/album/…) stays unused in v1, column reserved.
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
        self.transport.play_session = Some(PlaySession {
            path,
            started_at: now,
            played_ms: 0,
            duration_ms,
        });
        *self.transport.close_session.borrow_mut() = Some((path_str, now, 0, duration_ms));
    }

    /// Tapping the entry of the file that is *already loaded* must not restart
    /// it. If `path` is the currently playing/paused file, this toggles
    /// pause/resume (like the mini player) and returns `true` so the caller
    /// skips re-queuing it. Returns `false` for any other file, so the caller
    /// proceeds to start it normally.
    pub(crate) fn toggle_if_active_file(&mut self, path: &Path) -> bool {
        if self.transport.playing_path.as_deref() != Some(path) {
            return false;
        }
        if self.mini.playing {
            self.save_resume();
            self.player.pause();
        } else {
            self.player.resume();
        }
        self.mini.playing = !self.mini.playing;
        self.mpris.set_playing(self.mini.playing);
        self.refresh_queue_icons();
        true
    }

    pub(crate) fn play_current(&mut self) {
        // Save the position of the previously running track before a new one is loaded.
        self.save_resume();
        // If a podcast episode was playing before, save its resume position.
        self.save_episode_progress();
        // Finalize the previous listening session as a switch/skip (if the call came
        // from an EOS, it is already finalized → no-op).
        self.finalize_play_session(false);
        let Some(path) = self.transport.queue.get(self.transport.queue_pos).cloned() else {
            return;
        };
        // Detect context switch: if a new selection replaces the running
        // queue, push the old context (queue + position) onto the back
        // stack – this allows "keep listening to the previous song **including
        // its playlist**". When jumping back itself, don't stack again.
        if !self.transport.skip_history_push {
            if let Some((pq, pp)) = self.transport.prev_ctx.clone() {
                if !pq.is_empty() && pq != self.transport.queue {
                    self.transport.nav_stack.push((pq, pp));
                    if self.transport.nav_stack.len() > 50 {
                        self.transport.nav_stack.remove(0);
                    }
                }
            }
        }
        // Maintain history: remember the previously running track (for "previous song").
        // When jumping back from the history itself, don't add it again.
        if self.transport.skip_history_push {
            self.transport.skip_history_push = false;
        } else if let Some(prev) = self.transport.playing_path.clone() {
            if prev != path {
                self.transport.play_history.push(prev);
                if self.transport.play_history.len() > 200 {
                    self.transport.play_history.remove(0);
                }
            }
        }
        let path_str = path.to_string_lossy().to_string();
        // YouTube tracks resolve asynchronously (yt-dlp -g takes seconds). A
        // local offline copy plays synchronously below; otherwise resolve in a
        // worker thread and start streaming when `YtStreamResolved` arrives.
        let yt_video = crate::core::youtube::parse_yt_path(&path_str);
        // Title from the current play context (single video or playlist queue),
        // so a `yt:` track shows a name rather than its id.
        let yt_name = yt_video
            .as_ref()
            .and_then(|vid| self.youtube.video_titles.get(vid).cloned())
            .filter(|t| !t.trim().is_empty());
        if let Some(video_id) = &yt_video {
            let name = yt_name.clone().unwrap_or_else(|| self.display_name(&path));
            // Log to the "Recent" history and enrich (cover/artist) in the background.
            self.note_youtube_play(video_id, &name);
            let has_local = self
                .library
                .yt_download(video_id)
                .ok()
                .flatten()
                .map(|p| std::path::Path::new(&p).exists())
                .unwrap_or(false);
            if !has_local {
                // A freshly selected video always starts from the beginning.
                let resume = 0;
                // Optimistic now-playing state; the worker resolves the stream.
                self.transport.skip_count = 0;
                self.transport.playing_path = Some(path.clone());
                self.podcasts.playing_episode_url = None;
                self.streaming.playing_stream = None;
                self.files.playing_remote = false;
                self.youtube.playing_video_id = Some(video_id.clone());
                self.stop_recorder();
                self.mini.now_playing = Some(name.clone());
                self.mini.current_album = None; // YouTube — no local album page
                self.mini.playing = true;
                // Resolving the stream URL (yt-dlp) and buffering takes a moment
                // → spinner until `YtStreamResolved` plays and the player is ready.
                self.mini.loading = true;
                self.mini.position_ms = resume.max(0);
                self.mini.track_duration_ms = 0;
                *self.transport.close_resume.borrow_mut() = None;
                self.set_chapters(Vec::new());
                // No lyrics for a (just-resolving) stream; drop the old track's.
                self.close_lyrics_view();
                self.lyrics.current = None;
                self.lyrics.for_path = None;
                self.mpris.set_metadata(0, &name, None, None, None, None);
                self.mpris.set_playing(true);
                self.refresh_queue_icons();
                let input = self.input.clone();
                let vid = video_id.clone();
                std::thread::spawn(move || {
                    let result =
                        crate::core::youtube::resolve_audio_url(&vid).map_err(|e| e.to_string());
                    let _ = input.send(crate::ui::app::Msg::YtStreamResolved {
                        video_id: vid,
                        resume,
                        result,
                    });
                });
                return;
            }
        }
        // Saved resume position (for all tracks; see should_resume). A one-shot
        // forced start (recording editor preview) overrides it for this start.
        let track = self.library.track_by_path(&path_str).ok().flatten();
        let resume_ms = match self.transport.forced_start_ms.take() {
            Some(ms) => ms.max(0),
            None => match &track {
                Some(t) if self.should_resume(t) => t.resume_ms,
                _ => 0,
            },
        };
        match self.start_track_playback(&path_str, resume_ms) {
            Ok(is_network_stream) => {
                // A track started → reset the unplayable-skip guard.
                self.transport.skip_count = 0;
                // Network streams (Nextcloud) buffer before playing → spinner
                // until ready; local/cached files start instantly (no spinner).
                self.mini.loading = is_network_stream;
                self.transport.playing_path = Some(path.clone());
                // Music is playing again – no podcast episode/station/
                // remote file active anymore.
                self.podcasts.playing_episode_url = None;
                self.streaming.playing_stream = None;
                self.files.playing_remote = false;
                // For a YouTube track this is its id (marks the row); None resets it.
                self.youtube.playing_video_id = yt_video.clone();
                self.stop_recorder();
                self.mini.now_playing = Some(match &yt_video {
                    Some(_) => yt_name.clone().unwrap_or_else(|| self.display_name(&path)),
                    None => self.display_name(&path),
                });
                // Album shortcut in the player bar: only for a local track with an
                // album (not for YouTube tracks) …
                let album = match &yt_video {
                    Some(_) => None,
                    None => track
                        .as_ref()
                        .and_then(|t| t.album.clone())
                        .filter(|a| !a.trim().is_empty()),
                };
                // … and only when the album actually has more than this one track
                // (a single-track album has no meaningful song page to open).
                self.mini.current_album = album.filter(|a| {
                    self.library
                        .album_track_paths_by_name(a)
                        .map(|p| p.len() > 1)
                        .unwrap_or(false)
                });
                self.mini.playing = true;
                // Refresh the active output (may have changed).
                self.settings.active_output =
                    crate::core::output::default_output().unwrap_or_default();
                self.apply_current_eq();
                // Inform the lock screen/media keys about the new track.
                self.update_mpris_metadata(&path, track.as_ref());
                self.mpris.set_playing(true);
                let start = self.player.position_ms().unwrap_or(resume_ms.max(0));
                self.mpris.set_position(start);
                self.mpris.seeked(start);
                // Set the seek bar to the new track (the tick refines the duration).
                self.mini.position_ms = start;
                self.mini.track_duration_ms = self
                    .player
                    .duration_ms()
                    .or_else(|| track.as_ref().and_then(|t| t.duration_ms))
                    .unwrap_or(0);
                // Snapshot for saving on close (resume tracks only).
                let resumable = matches!(&track, Some(t) if self.should_resume(t));
                *self.transport.close_resume.borrow_mut() =
                    resumable.then(|| (path_str.clone(), start, self.mini.track_duration_ms));
                // Start a new listening session for the statistics.
                self.start_play_session(path.clone(), self.mini.track_duration_ms);
                // Adjust the play/queue markers in the list to the new track.
                self.refresh_queue_icons();
                // Save the queue + position for the next start.
                self.save_queue();
                // Remember the current context (detection of future queue switches).
                self.transport.prev_ctx =
                    Some((self.transport.queue.clone(), self.transport.queue_pos));
                // Tracks have no chapters → clear markers/hover list.
                self.set_chapters(Vec::new());
                // Load lyrics for the new track (embedded/cache instantly, then
                // LRCLIB in the background) – shows the karaoke button when synced
                // lyrics exist.
                let _ = self.input.send(Msg::LoadLyrics(path.clone()));
                // If usable tags are missing (artist/album), let the track be
                // identified in the background via fingerprint – instead of a bulk
                // run, only what is actually played. The actual gating checks (key,
                // fpcalc, network, attempt limit) are done by fetch_focus_track.
                let needs_id = track.as_ref().is_none_or(|t| {
                    t.artist.as_deref().unwrap_or("").trim().is_empty()
                        || t.album.as_deref().unwrap_or("").trim().is_empty()
                });
                if needs_id
                    && self
                        .enrich_state
                        .acoustid_key
                        .as_deref()
                        .is_some_and(|k| !k.is_empty())
                {
                    let _ = self.input.send(Msg::FingerprintCurrent(path.clone()));
                }
            }
            Err(e) => {
                // Synchronous failure (e.g. Nextcloud source without credentials)
                // → skip this entry (message-driven, so no recursion here).
                tracing::warn!("Playback failed, skipping: {e}");
                let _ = self.input.send(Msg::PlaybackError);
            }
        }
    }

    /// Skips the current (unplayable) track and advances to the next queue
    /// entry. Bounded by [`TransportState::skip_count`] so an entirely
    /// unplayable queue (e.g. an unmounted SD card / offline Nextcloud) stops
    /// instead of looping forever.
    pub(crate) fn skip_current_track(&mut self) {
        let limit = self
            .transport
            .queue
            .len()
            .max(self.files.remote_queue.len())
            .max(1);
        self.transport.skip_count += 1;
        if self.transport.skip_count > limit as u32 {
            // Whole queue unplayable → give up and stop.
            self.transport.skip_count = 0;
            self.player.stop();
            self.mini.playing = false;
            self.mini.now_playing = None;
            self.mini.current_album = None;
            self.transport.playing_path = None;
            self.files.playing_remote = false;
            *self.transport.close_resume.borrow_mut() = None;
            self.mpris.set_stopped();
            self.refresh_queue_icons();
            self.toast(&crate::i18n::gettext("No playable tracks"));
            return;
        }
        // Brief, non-spammy hint on the first skip of a run.
        if self.transport.skip_count == 1 {
            self.toast(&crate::i18n::gettext("Skipping unavailable track"));
        }
        if self.files.playing_remote {
            self.remote_next();
        } else {
            self.play_next();
        }
    }

    /// Sends the metadata of the running track to the MPRIS service
    /// (lock screen). The cover – if present – is added best effort.
    pub(crate) fn update_mpris_metadata(&self, path: &std::path::Path, track: Option<&Track>) {
        let (title, artist, album, length) = match track {
            Some(t) => (
                t.title.clone(),
                t.artist.clone(),
                t.album.clone(),
                t.duration_ms,
            ),
            None => (Self::track_display_name(path), None, None, None),
        };
        let art = album
            .as_deref()
            .and_then(|al| self.library.album_cover(al).ok().flatten());
        self.mpris.set_metadata(
            self.transport.queue_pos,
            &title,
            artist.as_deref(),
            album.as_deref(),
            length,
            art.as_deref(),
        );
        // Keep the lock-screen shuffle/repeat in sync (e.g. repeat restored from
        // settings at startup, which predates the async MPRIS player being ready).
        self.mpris.set_shuffle(self.transport.shuffle);
        self.mpris.set_repeat(self.transport.repeat);
    }

    /// Resolves the equalizer for the running track + active output
    /// (track→album→artist→global, then default output) and applies it live.
    /// Without any setting: neutral (all bands 0).
    pub(crate) fn apply_current_eq(&self) {
        let Some(path) = self.transport.queue.get(self.transport.queue_pos) else {
            return;
        };
        let path_str = path.to_string_lossy();
        let track = self
            .library
            .track_by_path(&path_str)
            .ok()
            .flatten()
            .or_else(|| scanner::read_track(path).ok());
        let (artist, album) = match track {
            Some(t) => (t.artist, t.album),
            None => (None, None),
        };
        let bands = self
            .library
            .resolve_eq(
                &self.settings.active_output,
                artist.as_deref(),
                album.as_deref(),
                &path_str,
            )
            .unwrap_or([0.0; 10]);
        self.player.set_eq_bands(&bands);
    }

    /// Plays a path (folder recursively or single file) as **one**
    /// queue. For multi-CD content (e.g. live concerts) the CDs are
    /// played together: first CD1, then CD2 … – sorted by subfolder
    /// (CD folder), then disc and track number from the tags, otherwise file name.
    pub(crate) fn play_path(&mut self, path: &str, is_dir: bool) {
        let p = PathBuf::from(path);
        // Re-tapping the song that is already playing toggles pause/resume
        // instead of restarting it (folders always (re)start the whole set).
        if !is_dir && self.toggle_if_active_file(&p) {
            return;
        }
        let files = if is_dir {
            let mut fs = scanner::collect_audio_files(&p);
            // Like the display (`folder_tracks_ordered`): **natural** path
            // sorting so that playback and display order match
            // (CD folders + file names dictate the order, robust against
            // wrong/missing disc/track tags).
            fs.sort_by_cached_key(|f| crate::ui::app_views::natural_key(&f.to_string_lossy()));
            fs
        } else {
            vec![p]
        };
        if !files.is_empty() {
            self.transport.queue = files;
            self.transport.queue_pos = 0;
            self.play_current();
            self.refresh_queue_icons();
        }
    }

    /// Handle a desktop / lock-screen MPRIS command (media keys, etc.).
    pub(crate) fn handle_mpris(
        &mut self,
        root: &adw::ApplicationWindow,
        cmd: crate::core::mpris::MprisCommand,
    ) {
        use crate::core::mpris::MprisCommand as M;
        match cmd {
            M::PlayPause => {
                if self.mini.now_playing.is_some() {
                    if self.mini.playing {
                        self.save_resume();
                        self.player.pause();
                    } else {
                        self.player.resume();
                    }
                    self.mini.playing = !self.mini.playing;
                    self.mpris.set_playing(self.mini.playing);
                    self.refresh_queue_icons();
                }
            }
            M::Play => {
                if self.mini.now_playing.is_some() && !self.mini.playing {
                    self.player.resume();
                    self.mini.playing = true;
                    self.mpris.set_playing(true);
                    self.refresh_queue_icons();
                }
            }
            M::Pause => {
                if self.mini.now_playing.is_some() && self.mini.playing {
                    self.save_resume();
                    self.player.pause();
                    self.mini.playing = false;
                    self.mpris.set_playing(false);
                    self.refresh_queue_icons();
                }
            }
            M::Next => {
                if self.files.playing_remote {
                    self.remote_next();
                } else {
                    self.play_next();
                }
            }
            M::Prev => {
                if self.files.playing_remote {
                    self.remote_prev();
                } else {
                    self.play_prev();
                }
            }
            M::Stop => {
                self.save_resume();
                self.finalize_play_session(false);
                self.player.stop();
                self.mini.playing = false;
                self.transport.playing_path = None;
                self.mini.position_ms = 0;
                self.mini.track_duration_ms = 0;
                *self.transport.close_resume.borrow_mut() = None;
                self.mpris.set_stopped();
                self.refresh_queue_icons();
            }
            M::Raise => root.present(),
            M::SeekBy(offset_us) => {
                let cur = self.player.position_ms().unwrap_or(0);
                let target = (cur + offset_us / 1000).max(0);
                if self.player.seek_ms(target).is_ok() {
                    self.mpris.seeked(target);
                }
            }
            M::SetPosition(pos_us) => {
                let target = (pos_us / 1000).max(0);
                if self.player.seek_ms(target).is_ok() {
                    self.mpris.seeked(target);
                }
            }
            M::SetShuffle(on) => {
                if self.transport.shuffle != on {
                    self.transport.shuffle = on;
                    if on {
                        self.rebuild_shuffle_order();
                    }
                }
                self.mpris.set_shuffle(self.transport.shuffle);
            }
            M::SetRepeat(on) => {
                if self.transport.repeat != on {
                    self.transport.repeat = on;
                    let _ = self
                        .library
                        .set_setting("repeat", if on { "1" } else { "0" });
                }
                self.mpris.set_repeat(self.transport.repeat);
            }
        }
    }

    /// Play a track of a folder audiobook/concert (queue = folder in order,
    /// start at the tapped one). `close` pops the subpage back to the main view.
    pub(crate) fn on_play_folder_track(&mut self, folder: String, path: String, close: bool) {
        // Re-tapping the song that is already playing toggles
        // pause/resume instead of restarting it.
        if self.toggle_if_active_file(&PathBuf::from(&path)) {
            return;
        }
        let files: Vec<PathBuf> = self
            .folder_tracks_ordered(&folder)
            .into_iter()
            .map(|t| PathBuf::from(t.path))
            .collect();
        let target = PathBuf::from(&path);
        if let Some(pos) = files.iter().position(|p| *p == target) {
            self.transport.queue = files;
            self.transport.queue_pos = pos;
            self.play_current();
            self.refresh_queue_icons();
            if close {
                self.nav.nav_view.pop_to_tag("main");
            }
        }
    }

    /// Play a track from the artist overview (queue = all tracks of the artist,
    /// start at the tapped one). `close` pops the subpage back to the main view.
    pub(crate) fn on_play_artist_track(&mut self, name: String, path: String, close: bool) {
        // Re-tapping the song that is already playing toggles
        // pause/resume instead of restarting it.
        if self.toggle_if_active_file(&PathBuf::from(&path)) {
            return;
        }
        // Queue = all tracks of the artist (across albums),
        // start at the tapped track.
        let files: Vec<PathBuf> = self
            .artist_albums(&name)
            .into_iter()
            .flat_map(|(_, tracks)| tracks)
            .map(|t| PathBuf::from(t.path))
            .collect();
        let target = PathBuf::from(&path);
        if let Some(pos) = files.iter().position(|p| *p == target) {
            self.transport.queue = files;
            self.transport.queue_pos = pos;
            self.play_current();
            self.refresh_queue_icons();
            // Back to the main page, so that the mini player is visible.
            if close {
                self.nav.nav_view.pop_to_tag("main");
            }
        }
    }

    /// Play a **single** selected track (from an album or playlist): only this
    /// track is enqueued, not its siblings. `close` pops the subpage back.
    pub(crate) fn on_play_one_track(&mut self, path: String, close: bool) {
        // Re-tapping the song that is already playing toggles
        // pause/resume instead of restarting it.
        if self.toggle_if_active_file(&PathBuf::from(&path)) {
            return;
        }
        // Selecting a single track (album or playlist) plays *only* that
        // track – its siblings are not enqueued. Use the album/playlist
        // play button for the whole thing. A single play is logged to
        // "Recent" like any other standalone track.
        self.youtube.playing_playlist = false;
        self.transport.queue = vec![PathBuf::from(&path)];
        self.transport.queue_pos = 0;
        self.play_current();
        self.refresh_queue_icons();
        if close {
            self.nav.nav_view.pop_to_tag("main");
        }
    }

    /// Play the whole album in track order (shuffle off).
    pub(crate) fn on_play_album(&mut self, artist: String, album: String) {
        // Whole album from track 1 in track order (shuffle off).
        let files: Vec<PathBuf> = self
            .album_tracks_for_artist(&artist, &album)
            .into_iter()
            .map(|t| PathBuf::from(t.path))
            .collect();
        if !files.is_empty() {
            self.transport.shuffle = false;
            self.transport.queue = files;
            self.transport.queue_pos = 0;
            self.play_current();
            self.refresh_queue_icons();
            self.nav.nav_view.pop_to_tag("main");
        }
    }

    /// A track finished: advance the queue (local / remote / streamed episode),
    /// finalizing the listening session and clearing the resume point.
    pub(crate) fn on_track_finished(&mut self) {
        // "Sleep after this track": stop here instead of advancing the queue.
        if self.sleep_stop_at_track_end() {
            return;
        }
        if self.files.playing_remote {
            // Remote queue: advance to the next track (or stop at the
            // end). Runs separately from the local queue.
            self.remote_next();
        } else if self.podcasts.playing_episode_url.is_some() && self.transport.queue.is_empty() {
            // A streamed episode has ended (no queue
            // behind it): finalize its statistics session as "fully
            // listened", then reset the playback state and marking.
            self.finalize_play_session(true);
            self.mini.playing = false;
            self.podcasts.playing_episode_url = None;
            self.mpris.set_playing(false);
            self.refresh_queue_icons();
        } else {
            // Listened to the end → finalize the listening session as "fully listened",
            // before the subsequent play_current starts a new session.
            self.finalize_play_session(true);
            // Track finished → forget resume, next time from the start.
            // `take()` prevents play_current from saving the (end) position again
            // as a resume point.
            if let Some(path) = self.transport.playing_path.take() {
                let _ = self.library.set_resume_path(&path.to_string_lossy(), 0);
            }
            *self.transport.close_resume.borrow_mut() = None;
            // If a single song was slipped in between, now resume the interrupted
            // queue at its spot.
            if self.transport.queue.len() == 1 && self.transport.interrupted_queue.is_some() {
                if let Some((q, pos)) = self.transport.interrupted_queue.take() {
                    self.transport.queue = q;
                    self.transport.queue_pos = pos;
                    self.play_current();
                }
            } else {
                // A new (multi-part) playback discards a possibly
                // remembered interruption.
                self.transport.interrupted_queue = None;
                self.play_next();
            }
        }
    }

    /// 5 s timer: persist the resume point of the running track/episode.
    pub(crate) fn on_persist_resume(&mut self) {
        if self.mini.playing {
            // Persist resume points on this 5 s timer (not every Tick):
            // a hard crash loses at most ~5 s of position, while normal
            // pause/seek/track-switch/close still save immediately.
            self.save_resume();
            if self.podcasts.playing_episode_url.is_some() {
                self.save_episode_progress();
            }
            if let Some(pos) = self.player.position_ms() {
                self.mpris.set_position(pos);
            }
        }
    }

    /// 1 s timer: drive timeshift recording, refresh station icons and update
    /// the seek bar / chapter / statistics counters.
    pub(crate) fn on_tick(&mut self, sender: &ComponentSender<Self>) {
        // Advance the running timeshift recording at the song boundaries.
        if self.streaming.record_state.is_some() {
            self.drive_recording(sender);
        }
        // Sync the play/pause and record icons of the station rows.
        self.refresh_stream_icons();
        if self.mini.playing {
            // Advance the sleep-timer countdown / fade-out (only while playing).
            self.sleep_tick();
            if let Some(pos) = self.player.position_ms() {
                self.mini.position_ms = pos;
            }
            if let Some(dur) = self.player.duration_ms() {
                self.mini.track_duration_ms = dur;
            }
            // Carry the close snapshot along.
            if let Some(entry) = self.transport.close_resume.borrow_mut().as_mut() {
                entry.1 = self.mini.position_ms;
                entry.2 = self.mini.track_duration_ms;
            }
            // (Episode resume is persisted on the 5 s PersistResume timer,
            // not here — no per-second DB write on the UI thread.)
            // Track the current chapter below the title (except while hovering).
            self.update_current_chapter();
            // Keep counting the listened time of the statistics session (wall clock, only
            // during "Playing"; ~1 s per tick). Backfill the duration if needed,
            // in case it was not yet known at the start.
            let dur = self.mini.track_duration_ms;
            if let Some(s) = self.transport.play_session.as_mut() {
                s.played_ms += 1000;
                if s.duration_ms == 0 {
                    s.duration_ms = dur;
                }
            }
            if let Some(cs) = self.transport.close_session.borrow_mut().as_mut() {
                if let Some(s) = self.transport.play_session.as_ref() {
                    cs.2 = s.played_ms;
                    cs.3 = s.duration_ms;
                }
            }
        }
    }

    /// Play a user-queue entry now: move its block to the front, then advance.
    pub(crate) fn on_play_queue_at(&mut self, start: usize, len: usize) {
        // Play this queue entry now: move its block to the front of the
        // user queue, then advance – `play_next` splices the first track
        // into the context and the rest follow track by track. Entries
        // before it stay queued and play afterwards.
        let n = self.transport.user_queue.len();
        if start < n {
            let len = len.clamp(1, n - start);
            let block: Vec<PathBuf> = self
                .transport
                .user_queue
                .drain(start..start + len)
                .collect();
            for (i, p) in block.into_iter().enumerate() {
                self.transport.user_queue.insert(i, p);
            }
            self.play_next();
        }
    }

    /// Player-bar play/pause: pause/resume the running file/station/episode,
    /// restart finished playback, or start the user queue.
    pub(crate) fn on_toggle_play(&mut self) {
        if self.mini.playing {
            self.save_resume();
            self.player.pause();
            self.mini.playing = false;
            // Pausing during buffering stops the spinner (no longer "loading").
            self.mini.loading = false;
        } else if self.transport.playing_path.is_some()
            || self.streaming.playing_stream.is_some()
            || self.podcasts.playing_episode_url.is_some()
        {
            // Paused (file, station or episode) → resume.
            self.player.resume();
            self.mini.playing = true;
        } else if !self.transport.queue.is_empty() {
            // Playback had ended → restart from the current position (rewound
            // to 0 after the end). play_current sets
            // playing/MPRIS/icons itself.
            self.play_current();
            return;
        } else if !self.transport.user_queue.is_empty() {
            // Nothing loaded, but the user queued tracks → start the queue
            // (play_next splices the first queued track into the context).
            self.play_next();
            return;
        } else {
            return;
        }
        self.mpris.set_playing(self.mini.playing);
        // Adjust the play/pause icon of the active track in the list.
        self.refresh_queue_icons();
        self.refresh_stream_icons();
    }
}
