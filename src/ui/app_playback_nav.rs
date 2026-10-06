//! Playback navigation: remote-source (Nextcloud/SMB/Drive) playback,
//! next/previous/skip, chapter/episode/station stepping and the shuffle
//! order. Split out of [`crate::ui::app_playback`] – pure reordering, the
//! methods remain inherent `impl App` methods.

use std::path::PathBuf;

use relm4::gtk;

use crate::core::remote::{self, Backend};
use crate::ui::app::{ActiveSource, App, Msg, RemoteTrack};
use crate::ui::app_playback::{chapter_target, TransportMsg, PREV_RESTART_MS};
use crate::ui::fs_row::FsEntry;

impl App {
    /// Backend of the currently active remote source (if one is active).
    pub(crate) fn active_backend(&self) -> Option<Backend> {
        let ActiveSource::Source(id) = self.files.active_source else {
            return None;
        };
        let s = self.files.sources.iter().find(|s| s.id == id)?;
        if !s.is_remote() {
            return None;
        }
        Backend::from_source(s)
    }

    /// Local cache path of a remote file of the active source (or `None`).
    pub(crate) fn remote_cache_path(&self, rel: &str) -> Option<PathBuf> {
        let ActiveSource::Source(id) = self.files.active_source else {
            return None;
        };
        Some(remote::cache_path(id, rel))
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
            }
            self.flip_playing();
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
        let ActiveSource::Source(source_id) = self.files.active_source else {
            return;
        };
        let Some(backend) = self.active_backend() else {
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
            _ => backend
                .stream_uri(source_id, &track.rel_path)
                .and_then(|uri| self.player.play_uri(&uri, 0)),
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
                self.youtube.playing_live = None;
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
                let _ = self.input.send(Msg::Transport(TransportMsg::PlaybackError));
            }
        }
    }

    /// Next track of the remote row (for the next button and EOS advancing).
    /// `wrap` (an explicit user "next") restarts the row from the top at its end.
    pub(crate) fn remote_next(&mut self, wrap: bool) {
        if self.files.remote_pos + 1 < self.files.remote_queue.len() {
            self.files.remote_pos += 1;
            self.play_remote_current();
        } else if (self.transport.repeat || wrap) && !self.files.remote_queue.is_empty() {
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

    /// Unified "next" for the headphone / MPRIS / UI next button. Something
    /// with jump marks (audiobook chapters, podcast shownotes, a YouTube video's
    /// chapters) first steps through them; only past the last mark does it move
    /// on to the next item (see [`Self::skip_next_item`]).
    pub(crate) fn skip_next(&mut self) {
        if !self.chapter_step(1) {
            self.skip_next_item();
        }
    }

    /// Unified "previous" counterpart to [`Self::skip_next`]: back to the start
    /// of the running chapter, pressed again to the one before it, and from the
    /// first chapter on to the previous item.
    pub(crate) fn skip_prev(&mut self) {
        if !self.chapter_step(-1) {
            self.skip_prev_item();
        }
    }

    /// Seeks to the neighbouring jump mark (`dir` = +1 / −1) of what is
    /// playing. `false` when there is none in that direction (no marks, past
    /// the last one, within the first one), so the caller changes the item.
    fn chapter_step(&mut self, dir: i32) -> bool {
        let something_seekable = self.transport.playing_path.is_some()
            || self.podcasts.playing_episode_url.is_some()
            || self.youtube.playing_video_id.is_some();
        if !something_seekable || !self.player.has_content() {
            return false;
        }
        let starts: Vec<i64> = self
            .mini
            .chapters
            .borrow()
            .iter()
            .map(|(ms, _)| *ms)
            .collect();
        if starts.is_empty() {
            return false;
        }
        let pos = self.player.position_ms().unwrap_or(self.mini.position_ms);
        match chapter_target(&starts, pos, dir, self.playing_length_ms()) {
            Some(ms) => {
                self.seek_from_desktop(ms);
                self.update_current_chapter();
                true
            }
            None => false,
        }
    }

    /// Next item without looking at jump marks: routes to the active context so
    /// the command is never a silent no-op. A podcast steps to the neighbouring
    /// episode of its feed, radio to the neighbouring saved station, a remote row
    /// to its next track, everything else to the next queue track (wrapping to
    /// the start at the end).
    pub(crate) fn skip_next_item(&mut self) {
        if self.podcasts.playing_episode_url.is_some() {
            self.podcast_step(1);
        } else if self.streaming.playing_stream.is_some() {
            self.station_step(1);
        } else if self.youtube.playing_live.is_some() {
            self.yt_live_step(1);
        } else if self.files.playing_remote {
            self.remote_next(true);
        } else {
            self.play_next(true);
        }
    }

    /// Previous item, the counterpart to [`Self::skip_next_item`].
    pub(crate) fn skip_prev_item(&mut self) {
        if self.podcasts.playing_episode_url.is_some() {
            self.podcast_step(-1);
        } else if self.streaming.playing_stream.is_some() {
            self.station_step(-1);
        } else if self.youtube.playing_live.is_some() {
            self.yt_live_step(-1);
        } else if self.files.playing_remote {
            self.remote_prev();
        } else {
            self.play_prev();
        }
    }

    /// Step to another episode (`dir` = +1 next / −1 previous) of the podcast the
    /// playing episode belongs to, cycling at the ends. A single-episode feed (or
    /// wrapping onto the same episode) restarts it from the start.
    fn podcast_step(&mut self, dir: i32) {
        let Some(url) = self.podcasts.playing_episode_url.clone() else {
            return;
        };
        let Some(pid) = self.library.podcast_id_for_episode_url(&url).ok().flatten() else {
            return;
        };
        let eps = self.library.episodes(pid).unwrap_or_default();
        if eps.is_empty() {
            return;
        }
        let cur = eps.iter().position(|e| e.audio_url == url).unwrap_or(0);
        let n = eps.len() as i32;
        let next = (cur as i32 + dir).rem_euclid(n) as usize;
        let (next_url, next_title) = (eps[next].audio_url.clone(), eps[next].title.clone());
        if next_url == url {
            // Same episode (single-episode feed / wrap onto itself): from the start.
            self.play_episode_at(&next_url, &next_title, 0);
        } else {
            self.play_episode(&next_url, &next_title);
        }
    }

    /// Step to another saved station (`dir` = +1 next / −1 previous), cycling at
    /// the ends. A single station restarts it.
    fn station_step(&mut self, dir: i32) {
        let Some(cur_id) = self.streaming.playing_stream else {
            return;
        };
        let list = self.library.streams().unwrap_or_default();
        if list.is_empty() {
            return;
        }
        let cur = list.iter().position(|s| s.id == cur_id).unwrap_or(0);
        let n = list.len() as i32;
        let next = (cur as i32 + dir).rem_euclid(n) as usize;
        self.play_stream(list[next].id);
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

    /// [`Self::play_current`] for a move **within** the running queue: the piece
    /// that is stepped to starts at its beginning instead of at a resume
    /// position left over from an earlier, partial listen (see
    /// [`crate::ui::app_state::TransportState::fresh_start`]).
    fn play_current_fresh(&mut self) {
        self.transport.fresh_start = true;
        self.play_current();
    }

    /// Next track: when shuffling, the next of the shuffle order, otherwise the
    /// following one. At the end: with `wrap` (an explicit user "next") playback
    /// restarts from the start of the queue (a single track restarts itself);
    /// without it (automatic advance at end-of-stream) playback stops.
    pub(crate) fn play_next(&mut self, wrap: bool) {
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
            self.play_current_fresh();
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
                self.play_current_fresh();
            }
            None if (self.transport.repeat || wrap) && !self.transport.queue.is_empty() => {
                // Repeat or an explicit "next" at the end: start over from the
                // top (single track likewise, since the queue then has only one
                // entry). Reshuffle when shuffling.
                if self.transport.shuffle {
                    self.rebuild_shuffle_order();
                    self.transport.queue_pos =
                        self.transport.shuffle_order.first().copied().unwrap_or(0);
                } else {
                    self.transport.queue_pos = 0;
                }
                self.play_current_fresh();
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

    /// Back button, in the order the presses are meant:
    ///
    /// 1. Playback ran out and stopped → play that track again.
    /// 2. The running track is past [`PREV_RESTART_MS`] → send it back to its
    ///    start. A second press then falls through to the step below, which is
    ///    how "back, back" reaches the previous track.
    /// 3. Step to the **previous track** of the running queue.
    /// 4. At the very start of the queue (or a lone single-song context):
    ///    restore a context that a single-song tap displaced, then the most
    ///    recently played track, and finally — with nothing before it — a
    ///    restart of the current track.
    pub(crate) fn play_prev(&mut self) {
        // Playback ran out and stopped: "previous" means "that one again". The
        // track just heard is the one the press is about — stepping back into
        // the album it interrupted would be a jump the user did not ask for.
        if self.transport.playing_path.is_none() {
            if let Some(path) = self.transport.last_finished.clone() {
                match self.transport.queue.iter().position(|p| *p == path) {
                    Some(pos) => self.transport.queue_pos = pos,
                    None => {
                        self.transport.queue = vec![path];
                        self.transport.queue_pos = 0;
                    }
                }
                self.transport.skip_history_push = true;
                self.play_current_fresh();
                return;
            }
        }

        // Past the first few seconds, "previous" first sends the running track
        // back to its start; pressing again gets to the one before it. The
        // position used is the one shown in the bar, so a source that is still
        // loading (a YouTube stream resolves in a worker) counts as being at its
        // start and steps back rather than restarting nothing.
        if self.transport.playing_path.is_some() && self.mini.position_ms > PREV_RESTART_MS {
            self.transport.skip_history_push = true;
            self.play_current_fresh();
            return;
        }

        // One track back within the running queue (the common "list" case).
        if self.transport.queue_pos > 0 && self.transport.queue.len() > 1 {
            self.transport.skip_history_push = true;
            self.transport.queue_pos -= 1;
            self.play_current_fresh();
            return;
        }

        // At the start of a single-song context: restore a list that a quick
        // single-song tap displaced, keeping the previously playing song **and**
        // its playlist (resume from the DB).
        if let Some((q, pos)) = self.transport.nav_stack.pop() {
            self.transport.skip_history_push = true;
            self.transport.queue = q;
            self.transport.queue_pos = pos.min(self.transport.queue.len().saturating_sub(1));
            self.play_current();
            self.refresh_queue_icons();
            return;
        }

        // Else jump to the most recently played track (crosses contexts):
        // preferably at its queue position, otherwise by path directly.
        if let Some(prev) = self.transport.play_history.pop() {
            self.transport.skip_history_push = true;
            if let Some(pos) = self.transport.queue.iter().position(|p| *p == prev) {
                self.transport.queue_pos = pos;
            } else {
                self.transport.queue = vec![prev];
                self.transport.queue_pos = 0;
            }
            self.play_current();
            return;
        }

        // Nothing before this: restart the current track from the beginning.
        if self.transport.playing_path.is_some() {
            self.transport.skip_history_push = true;
            self.play_current_fresh();
        }
    }
}
