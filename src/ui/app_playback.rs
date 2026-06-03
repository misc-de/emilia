//! Playback: queue, play/pause/next/prev, resume logic and the
//! running equalizer. Extracted from app.rs – pure reordering, no
//! change in behavior; the methods remain inherent `impl App` methods.

use std::path::PathBuf;

use relm4::gtk;

use crate::core::scanner;
use crate::core::webdav::{self, Creds};
use crate::model::Track;
use crate::ui::app::{guarded_resume, ActiveSource, App, Msg, PlaySession, RemoteTrack};
use crate::ui::fs_row::{FsEntry, FsInput};

impl App {
    /// Refreshes the queue marker of all visible file rows.
    pub(crate) fn refresh_queue_icons(&mut self) {
        let queue = self.queue.clone();
        // Currently playing track (for the play marker).
        let active_path = self.queue.get(self.queue_pos).cloned();
        // Remote playback: the active entry is marked via the rel path.
        let active_rel = if self.playing_remote {
            self.remote_queue.get(self.remote_pos).map(|t| t.rel_path.clone())
        } else {
            None
        };
        let states: Vec<(usize, bool, bool)> = {
            let guard = self.entries.guard();
            (0..guard.len())
                .filter_map(|i| {
                    guard.get(i).map(|r| {
                        let is_file = !r.entry.is_dir();
                        match r.entry.path() {
                            Some(p) => {
                                let q = is_file && queue.iter().any(|x| x == p);
                                let a = is_file
                                    && active_path.as_deref() == Some(p.as_path());
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
        let playing = self.playing;
        for (i, q, a) in states {
            self.entries.send(i, FsInput::SetQueued(q));
            self.entries.send(i, FsInput::SetActive { active: a, playing });
        }
        // Sync the play row of an open detail dialog with the playback state.
        self.refresh_ctx_play();
        // Play/pause icons of the podcast episodes (and the detail "Play" row).
        self.refresh_episode_icons();
    }

    /// Credentials of the currently active WebDAV source (if one is active).
    pub(crate) fn active_webdav_creds(&self) -> Option<Creds> {
        let ActiveSource::Source(id) = self.active_source else {
            return None;
        };
        let s = self.sources.iter().find(|s| s.id == id)?;
        if s.kind != "webdav" {
            return None;
        }
        Creds::from_source(s)
    }

    /// Local cache path of a remote file of the active source (or `None`).
    pub(crate) fn remote_cache_path(&self, rel: &str) -> Option<PathBuf> {
        let ActiveSource::Source(id) = self.active_source else {
            return None;
        };
        Some(webdav::cache_path(id, rel))
    }

    /// Tap a remote file: tapping the running track again
    /// toggles pause/resume; otherwise the folder row is set as the remote queue
    /// and played from the chosen track.
    pub(crate) fn activate_remote(&mut self, rel: &str) {
        let is_active = self.playing_remote
            && self
                .remote_queue
                .get(self.remote_pos)
                .is_some_and(|t| t.rel_path == rel);
        if is_active {
            if self.playing {
                self.save_resume();
                self.player.pause();
            } else {
                self.player.resume();
            }
            self.playing = !self.playing;
            self.mpris.set_playing(self.playing);
            self.refresh_queue_icons();
            return;
        }
        // Build the remote row from the visible file rows (folder sequence).
        let mut queue = Vec::new();
        let mut start = 0;
        {
            let guard = self.entries.guard();
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
        self.remote_queue = queue;
        self.remote_pos = start;
        self.play_remote_current();
    }

    /// Plays the current track of the remote row – locally (if already
    /// downloaded) or streamed. Self-contained like podcast/station; the
    /// local `PathBuf` queue stays empty in the process.
    pub(crate) fn play_remote_current(&mut self) {
        let Some(creds) = self.active_webdav_creds() else {
            return;
        };
        let Some(track) = self.remote_queue.get(self.remote_pos).cloned() else {
            return;
        };
        self.save_resume();
        self.save_episode_progress();
        self.finalize_play_session(false);
        let cached = self.remote_cache_path(&track.rel_path);
        let result = match &cached {
            Some(p) if p.exists() => self.player.play_file(&p.to_string_lossy(), 0),
            _ => self.player.play_uri(&webdav::stream_uri(&creds, &track.rel_path), 0),
        };
        match result {
            Ok(()) => {
                self.now_playing = Some(track.title.clone());
                self.playing = true;
                self.playing_path = None;
                self.playing_episode_url = None;
                self.playing_stream = None;
                self.playing_remote = true;
                self.stop_recorder();
                self.queue.clear();
                self.queue_pos = 0;
                self.position_ms = 0;
                self.track_duration_ms = 0;
                *self.close_resume.borrow_mut() = None;
                self.mpris.set_metadata(0, &track.title, None, None, None, None);
                self.mpris.set_playing(true);
                self.set_chapters(Vec::new());
                self.refresh_queue_icons();
            }
            Err(e) => {
                tracing::error!("Failed to play remote file: {e}");
                self.toast(&crate::i18n::gettext("Could not play this file"));
            }
        }
    }

    /// Next track of the remote row (for the next button and EOS advancing).
    pub(crate) fn remote_next(&mut self) {
        if self.remote_pos + 1 < self.remote_queue.len() {
            self.remote_pos += 1;
            self.play_remote_current();
        } else if self.repeat && !self.remote_queue.is_empty() {
            self.remote_pos = 0;
            self.play_remote_current();
        } else {
            // End of the row – stop playback (like at the end of an episode).
            self.player.stop();
            self.playing = false;
            self.mpris.set_playing(false);
            self.refresh_queue_icons();
        }
    }

    /// Previous track of the remote row.
    pub(crate) fn remote_prev(&mut self) {
        if self.remote_pos > 0 {
            self.remote_pos -= 1;
            self.play_remote_current();
        }
    }

    /// Rebuilds the shuffle order (Fisher-Yates), with the currently
    /// running track in first place. This way every track of the queue plays
    /// exactly once, in random order.
    pub(crate) fn rebuild_shuffle_order(&mut self) {
        let len = self.queue.len();
        let mut order: Vec<usize> = (0..len).collect();
        for i in (1..len).rev() {
            let j = gtk::glib::random_int_range(0, (i + 1) as i32) as usize;
            order.swap(i, j);
        }
        // Move the running track to the front so it isn't skipped immediately.
        if let Some(p) = order.iter().position(|&x| x == self.queue_pos) {
            order.swap(0, p);
        }
        self.shuffle_order = order;
        self.shuffle_idx = 0;
    }

    /// Next track: when shuffling, the next of the shuffle order, otherwise the
    /// following one. At the end (all played) playback stops.
    pub(crate) fn play_next(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let len = self.queue.len();
        let next = if self.shuffle {
            // Reshuffle if the queue has changed or the running
            // track is no longer the expected one of the order (e.g. after
            // manual selection) – then keep shuffling from the current track.
            if self.shuffle_order.len() != len
                || self.shuffle_order.get(self.shuffle_idx) != Some(&self.queue_pos)
            {
                self.rebuild_shuffle_order();
            }
            if self.shuffle_idx + 1 < self.shuffle_order.len() {
                self.shuffle_idx += 1;
                Some(self.shuffle_order[self.shuffle_idx])
            } else {
                None
            }
        } else if self.queue_pos + 1 < len {
            Some(self.queue_pos + 1)
        } else {
            None
        };
        match next {
            Some(n) => {
                self.queue_pos = n;
                self.play_current();
            }
            None if self.repeat && !self.queue.is_empty() => {
                // Repeat: start over at the end (single track likewise, since
                // the queue then has only one entry). Reshuffle when shuffling.
                if self.shuffle {
                    self.rebuild_shuffle_order();
                    self.queue_pos = self.shuffle_order.first().copied().unwrap_or(0);
                } else {
                    self.queue_pos = 0;
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
                self.playing = false;
                self.playing_path = None;
                self.queue_pos = 0;
                self.position_ms = 0;
                self.track_duration_ms = 0;
                self.shuffle_order.clear();
                self.shuffle_idx = 0;
                *self.close_resume.borrow_mut() = None;
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
            .last_prev
            .is_some_and(|t| now.duration_since(t) <= std::time::Duration::from_secs(1));
        self.last_prev = Some(now);

        if double {
            if let Some(prev) = self.play_history.pop() {
                // Previous song: preferably play it at its queue position,
                // otherwise the path directly (without another history entry).
                self.skip_history_push = true;
                if let Some(pos) = self.queue.iter().position(|p| *p == prev) {
                    self.queue_pos = pos;
                    self.play_current();
                } else {
                    self.queue = vec![prev];
                    self.queue_pos = 0;
                    self.play_current();
                }
                return;
            }
            // No history → sequentially one song back.
            if !self.queue.is_empty() && self.queue_pos > 0 {
                self.skip_history_push = true;
                self.queue_pos -= 1;
                self.play_current();
            }
            return;
        }

        // First press: if a new context was just started (track
        // has been running for < 5 s) and a displaced context lies on the stack,
        // restore it **including its playlist** and keep listening to the song
        // (resume from the DB). "Back" right after an accidentally
        // started song thus returns to the previous one.
        if !self.nav_stack.is_empty() && self.player.position_ms().unwrap_or(0) < 5000 {
            if let Some((q, pos)) = self.nav_stack.pop() {
                self.skip_history_push = true;
                self.queue = q;
                self.queue_pos = pos.min(self.queue.len().saturating_sub(1));
                self.play_current();
                self.refresh_queue_icons();
                return;
            }
        }

        // Otherwise: running track from the beginning.
        if self.playing_path.is_some() {
            self.skip_history_push = true;
            self.play_current();
        }
    }

    /// Plays the current entry of the queue.
    /// Display name of a track for the bar: "Artist - Title" from the tags,
    /// failing that the file name.
    /// Starts playback of a track path. Local paths go through
    /// `play_file`; **remote** tracks (synthetic path `nc:<id>:<rel>`) are
    /// played from the local cache or streamed directly from Nextcloud.
    pub(crate) fn start_track_playback(&self, path_str: &str, resume_ms: i64) -> anyhow::Result<()> {
        if let Some((sid, rel)) = crate::core::webdav::parse_nc_path(path_str) {
            let cache = crate::core::webdav::cache_path(sid, &rel);
            if cache.exists() {
                return self.player.play_file(&cache.to_string_lossy(), resume_ms);
            }
            if let Some(creds) = self
                .sources
                .iter()
                .find(|s| s.id == sid)
                .and_then(crate::core::webdav::Creds::from_source)
            {
                return self
                    .player
                    .play_uri(&crate::core::webdav::stream_uri(&creds, &rel), resume_ms);
            }
            return Err(anyhow::anyhow!("Nextcloud source unavailable"));
        }
        self.player.play_file(path_str, resume_ms)
    }

    /// Display name of a track for the bar/queue: preferably from the
    /// database (also works for remote tracks), otherwise from the file.
    pub(crate) fn display_name(&self, path: &std::path::Path) -> String {
        let path_str = path.to_string_lossy();
        if let Ok(Some(t)) = self.library.track_by_path(&path_str) {
            let title = if t.title.trim().is_empty() {
                path.file_stem().and_then(|n| n.to_str()).unwrap_or("").to_string()
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
            .queue
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = self.library.set_setting("queue_paths", &paths);
        let _ = self
            .library
            .set_setting("queue_pos", &self.queue_pos.to_string());
    }

    /// Saves the current playback position of the loaded track as a
    /// resume point. Near the start or end it is reset to 0 so that a
    /// nearly finished track starts over from the beginning next time.
    pub(crate) fn save_resume(&self) {
        let Some(path) = self.playing_path.clone() else {
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
        let Some(url) = self.playing_episode_url.clone() else {
            return;
        };
        let Some(pos) = self.player.position_ms() else {
            return;
        };
        let dur = self.player.duration_ms().unwrap_or(self.track_duration_ms);
        let _ = self
            .library
            .set_episode_progress(&url, guarded_resume(pos, dur));
    }

    /// Finalizes the running listening session and writes it as one
    /// `play_event` into the statistics. `completed` = listened to the end (EOS).
    /// Without a session nothing happens (idempotent).
    pub(crate) fn finalize_play_session(&mut self, completed: bool) {
        if let Some(s) = self.play_session.take() {
            let dur = if s.duration_ms > 0 {
                s.duration_ms
            } else {
                self.track_duration_ms
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
        *self.close_session.borrow_mut() = None;
    }

    pub(crate) fn play_current(&mut self) {
        // Save the position of the previously running track before a new one is loaded.
        self.save_resume();
        // If a podcast episode was playing before, save its resume position.
        self.save_episode_progress();
        // Finalize the previous listening session as a switch/skip (if the call came
        // from an EOS, it is already finalized → no-op).
        self.finalize_play_session(false);
        let Some(path) = self.queue.get(self.queue_pos).cloned() else {
            return;
        };
        // Detect context switch: if a new selection replaces the running
        // queue, push the old context (queue + position) onto the back
        // stack – this allows "keep listening to the previous song **including
        // its playlist**". When jumping back itself, don't stack again.
        if !self.skip_history_push {
            if let Some((pq, pp)) = self.prev_ctx.clone() {
                if !pq.is_empty() && pq != self.queue {
                    self.nav_stack.push((pq, pp));
                    if self.nav_stack.len() > 50 {
                        self.nav_stack.remove(0);
                    }
                }
            }
        }
        // Maintain history: remember the previously running track (for "previous song").
        // When jumping back from the history itself, don't add it again.
        if self.skip_history_push {
            self.skip_history_push = false;
        } else if let Some(prev) = self.playing_path.clone() {
            if prev != path {
                self.play_history.push(prev);
                if self.play_history.len() > 200 {
                    self.play_history.remove(0);
                }
            }
        }
        let path_str = path.to_string_lossy().to_string();
        // Saved resume position (for all tracks; see should_resume).
        let track = self.library.track_by_path(&path_str).ok().flatten();
        let resume_ms = match &track {
            Some(t) if self.should_resume(t) => t.resume_ms,
            _ => 0,
        };
        match self.start_track_playback(&path_str, resume_ms) {
            Ok(()) => {
                self.playing_path = Some(path.clone());
                // Music is playing again – no podcast episode/station/
                // remote file active anymore.
                self.playing_episode_url = None;
                self.playing_stream = None;
                self.playing_remote = false;
                self.stop_recorder();
                self.now_playing = Some(self.display_name(&path));
                self.playing = true;
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
                self.position_ms = start;
                self.track_duration_ms = self
                    .player
                    .duration_ms()
                    .or_else(|| track.as_ref().and_then(|t| t.duration_ms))
                    .unwrap_or(0);
                // Snapshot for saving on close (resume tracks only).
                let resumable = matches!(&track, Some(t) if self.should_resume(t));
                *self.close_resume.borrow_mut() = resumable
                    .then(|| (path_str.clone(), start, self.track_duration_ms));
                // Start a new listening session for the statistics.
                let now = crate::ui::app::unix_now();
                self.play_session = Some(PlaySession {
                    path: path.clone(),
                    started_at: now,
                    played_ms: 0,
                    duration_ms: self.track_duration_ms,
                });
                *self.close_session.borrow_mut() =
                    Some((path_str.clone(), now, 0, self.track_duration_ms));
                // Adjust the play/queue markers in the list to the new track.
                self.refresh_queue_icons();
                // Save the queue + position for the next start.
                self.save_queue();
                // Remember the current context (detection of future queue switches).
                self.prev_ctx = Some((self.queue.clone(), self.queue_pos));
                // Tracks have no chapters → clear markers/hover list.
                self.set_chapters(Vec::new());
                // If usable tags are missing (artist/album), let the track be
                // identified in the background via fingerprint – instead of a bulk
                // run, only what is actually played. The actual gating checks (key,
                // fpcalc, network, attempt limit) are done by fetch_focus_track.
                let needs_id = track.as_ref().map_or(true, |t| {
                    t.artist.as_deref().unwrap_or("").trim().is_empty()
                        || t.album.as_deref().unwrap_or("").trim().is_empty()
                });
                if needs_id && self.acoustid_key.as_deref().is_some_and(|k| !k.is_empty()) {
                    let _ = self.input.send(Msg::FingerprintCurrent(path.clone()));
                }
            }
            Err(e) => tracing::error!("Playback failed: {e}"),
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
            self.queue_pos,
            &title,
            artist.as_deref(),
            album.as_deref(),
            length,
            art.as_deref(),
        );
    }

    /// Resolves the equalizer for the running track + active output
    /// (track→album→artist→global, then default output) and applies it live.
    /// Without any setting: neutral (all bands 0).
    pub(crate) fn apply_current_eq(&self) {
        let Some(path) = self.queue.get(self.queue_pos) else {
            return;
        };
        let (artist, album) = match scanner::read_track(path) {
            Ok(t) => (t.artist, t.album),
            Err(_) => (None, None),
        };
        let bands = self
            .library
            .resolve_eq(
                &self.settings.active_output,
                artist.as_deref(),
                album.as_deref(),
                &path.to_string_lossy(),
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
        let files = if is_dir {
            let mut fs = scanner::collect_audio_files(&p);
            // Like the display (`folder_tracks_ordered`): **natural** path
            // sorting so that playback and display order match
            // (CD folders + file names dictate the order, robust against
            // wrong/missing disc/track tags).
            fs.sort_by_cached_key(|f| {
                crate::ui::app_views::natural_key(&f.to_string_lossy())
            });
            fs
        } else {
            vec![p]
        };
        if !files.is_empty() {
            self.queue = files;
            self.queue_pos = 0;
            self.play_current();
            self.refresh_queue_icons();
        }
    }
}
