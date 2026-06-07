//! Gapless + crossfade integration on the app side.
//!
//! Both only engage for **sequential local** queues (no shuffle, no pending
//! user-queue, plain local files): albums, concerts, audiobooks. Anything else
//! (streams, podcasts, YouTube, Nextcloud, shuffle) keeps the normal
//! end-of-track path in [`crate::ui::app_playback`].
//!
//! * Gapless arms the next track's URI on the player; `playbin3` continues into
//!   it and the resulting `STREAM_START` drives [`App::on_gapless_advanced`].
//! * Crossfade is triggered from the 1 s tick ([`App::maybe_crossfade`]) once the
//!   running track is within the fade window of its end.

use crate::ui::app::{App, Msg};

impl App {
    /// Pushes the gapless / crossfade preferences to the player and re-arms the
    /// next gapless track. Call after the settings change.
    pub(crate) fn apply_playback_prefs(&self) {
        self.player.set_gapless(self.settings.gapless);
        self.player.set_crossfade_secs(self.settings.crossfade_secs);
        self.arm_gapless();
    }

    /// The next queue entry **if** it is eligible for gapless/crossfade: a plain,
    /// existing local file in a sequential (non-shuffle) context with no pending
    /// user-queue and no active stream/remote playback. Returns `(index, file
    /// URI)`.
    pub(crate) fn next_seq_local(&self) -> Option<(usize, String)> {
        if self.transport.shuffle || !self.transport.user_queue.is_empty() {
            return None;
        }
        if self.files.playing_remote
            || self.podcasts.playing_episode_url.is_some()
            || self.streaming.playing_stream.is_some()
        {
            return None;
        }
        let next = self.transport.queue_pos.checked_add(1)?;
        let path = self.transport.queue.get(next)?;
        let s = path.to_string_lossy();
        // Synthetic remote paths (yt:/nc:) resolve asynchronously → not gapless.
        if crate::core::youtube::parse_yt_path(&s).is_some() {
            return None;
        }
        if crate::core::webdav::parse_nc_path(&s).is_some() {
            return None;
        }
        if !std::path::Path::new(path).exists() {
            return None;
        }
        let uri = crate::core::player::file_uri(&s)?;
        Some((next, uri))
    }

    /// Arms (or clears) the player's next gapless URI for the current context.
    /// Cleared when gapless is off or crossfade is on (crossfade owns the
    /// transition then).
    pub(crate) fn arm_gapless(&self) {
        if !self.settings.gapless || self.settings.crossfade_secs > 0.0 {
            self.player.arm_next_gapless(None);
            return;
        }
        let uri = self.next_seq_local().map(|(_, u)| u);
        self.player.arm_next_gapless(uri);
    }

    /// Advances the **logical** playback state to queue index `next` without
    /// loading audio (the deck already moved there gaplessly, or is crossfading).
    /// Mirrors the success branch of [`App::play_current`] minus the load.
    fn advance_logical_to(&mut self, next: usize) {
        // The just-finished track counts as fully listened.
        self.finalize_play_session(true);
        // Forget the previous track's resume, remember it in the back history.
        if let Some(prev) = self.transport.playing_path.take() {
            let _ = self.library.set_resume_path(&prev.to_string_lossy(), 0);
            if self.transport.queue.get(next) != Some(&prev) {
                self.transport.play_history.push(prev);
                if self.transport.play_history.len() > 200 {
                    self.transport.play_history.remove(0);
                }
            }
        }
        *self.transport.close_resume.borrow_mut() = None;
        self.transport.queue_pos = next;
        self.transport.skip_count = 0;

        let Some(path) = self.transport.queue.get(next).cloned() else {
            return;
        };
        let path_str = path.to_string_lossy().to_string();
        let track = self.library.track_by_path(&path_str).ok().flatten();
        self.transport.playing_path = Some(path.clone());
        self.podcasts.playing_episode_url = None;
        self.streaming.playing_stream = None;
        self.youtube.playing_video_id = None;
        self.files.playing_remote = false;
        self.mini.now_playing = Some(self.display_name(&path));
        let album = track
            .as_ref()
            .and_then(|t| t.album.clone())
            .filter(|a| !a.trim().is_empty());
        self.mini.current_album = album.filter(|a| {
            self.library
                .album_track_paths_by_name(a)
                .map(|p| p.len() > 1)
                .unwrap_or(false)
        });
        self.mini.playing = true;
        self.mini.loading = false;
        self.settings.active_output = crate::core::output::default_output().unwrap_or_default();
        self.apply_current_eq();
        self.update_mpris_metadata(&path, track.as_ref());
        self.mpris.set_playing(true);
        self.mini.position_ms = 0;
        self.mini.track_duration_ms = self
            .player
            .duration_ms()
            .or_else(|| track.as_ref().and_then(|t| t.duration_ms))
            .unwrap_or(0);
        let resumable = matches!(&track, Some(t) if self.should_resume(t));
        *self.transport.close_resume.borrow_mut() =
            resumable.then(|| (path_str.clone(), 0, self.mini.track_duration_ms));
        self.start_play_session(path.clone(), self.mini.track_duration_ms);
        self.refresh_queue_icons();
        self.save_queue();
        self.transport.prev_ctx = Some((self.transport.queue.clone(), self.transport.queue_pos));
        self.set_chapters(Vec::new());
        let _ = self.input.send(Msg::LoadLyrics(path));
    }

    /// `STREAM_START` arrived: the active deck continued gaplessly into the next
    /// track. Advance the app state to match and arm the following track.
    pub(crate) fn on_gapless_advanced(&mut self) {
        let next = self.transport.queue_pos + 1;
        if next >= self.transport.queue.len() {
            return;
        }
        self.advance_logical_to(next);
        // A gapless continuation starts a fresh segment at rate 1.0.
        self.player.reapply_rate();
        self.arm_gapless();
    }

    /// 1 s tick hook: if crossfade is enabled and the running track is within the
    /// fade window of its end, start the crossfade into the next sequential local
    /// track and advance the logical state to it.
    pub(crate) fn maybe_crossfade(&mut self) {
        let secs = self.settings.crossfade_secs;
        if secs <= 0.0 || !self.mini.playing {
            return;
        }
        let dur = self.mini.track_duration_ms;
        let window = (secs * 1000.0) as i64;
        // Skip very short tracks (jingles) and only trigger inside the window.
        if dur < window + 2000 {
            return;
        }
        let remaining = dur - self.mini.position_ms;
        if remaining > window || remaining <= 200 {
            return;
        }
        let Some((next, uri)) = self.next_seq_local() else {
            return;
        };
        if self.player.crossfade_to(&uri, 0).is_err() {
            return;
        }
        self.advance_logical_to(next);
        self.arm_gapless();
    }
}
