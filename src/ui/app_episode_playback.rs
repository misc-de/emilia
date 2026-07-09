//! Episode playback on the shared transport. These methods used to live next to
//! the podcast *page*, but they are really transport logic: starting/pausing an
//! episode mutates the single player, mini-player, MPRIS, statistics and queue
//! and resets the other sources' "now playing" markers. They stay on `App` (the
//! transport owner); the podcast page itself lives in
//! [`crate::ui::podcasts_page`] and reaches them through its `Output`. The
//! canonical "an episode is playing" flag remains `self.podcasts.playing_episode_url`.

use adw::prelude::*;
use relm4::gtk;

use crate::ui::app::App;

impl App {
    /// Streams a podcast episode (replaces the current playback). Starts at
    /// the remembered position (resume) and first saves the position of a
    /// previously playing episode.
    pub(crate) fn play_episode(&mut self, url: &str, title: &str) {
        let resume = self.library.episode_progress(url).unwrap_or(0);
        self.play_episode_from(url, title, resume);
    }

    /// Like `play_episode`, but starts at a specific position (for the
    /// clickable jump markers in the shownotes).
    pub(crate) fn play_episode_at(&mut self, url: &str, title: &str, ms: i64) {
        self.play_episode_from(url, title, ms.max(0));
    }

    /// Sets the chapters of the current playback: seekbar markers **and** the
    /// shared chapter list for the hover display. Empty list = clear (e.g. for
    /// tracks without chapters). The markers reposition automatically once the
    /// duration is known (the tick updates the value range).
    pub(crate) fn set_chapters(&self, chapters: Vec<(i64, String)>) {
        self.mini.seek_scale.clear_marks();
        for (ms, _) in &chapters {
            if *ms > 0 {
                self.mini
                    .seek_scale
                    .add_mark(*ms as f64, gtk::PositionType::Top, None);
            }
        }
        self.mini.chapter_label.set_visible(false);
        *self.mini.chapters.borrow_mut() = chapters;
    }

    /// Updates the chapter label to the chapter at the current playback
    /// position. No-op during a hover (then the mouse position takes
    /// precedence) and without chapters (the label stays hidden).
    pub(crate) fn update_current_chapter(&self) {
        if self.mini.hovering_seek.get() {
            return;
        }
        let name = {
            let chaps = self.mini.chapters.borrow();
            chaps
                .iter()
                .rev()
                .find(|(ms, _)| *ms <= self.mini.position_ms)
                .map(|(_, n)| n.clone())
                .filter(|n| !n.is_empty())
        };
        match name {
            Some(n) => {
                self.mini.chapter_label.set_text(&n);
                self.mini.chapter_label.set_visible(true);
            }
            None => self.mini.chapter_label.set_visible(false),
        }
    }

    fn play_episode_from(&mut self, url: &str, title: &str, resume: i64) {
        self.save_episode_progress();
        // Close the previous statistics session (a track or another episode)
        // as a skip before this one starts; its own session opens below.
        self.finalize_play_session(false);
        // Offline copy present → play the local file (works without a
        // connection and starts instantly); otherwise stream the network URL.
        // Playback state stays keyed by `url` (resume position, play/pause
        // marker, chapters), only the actual source differs.
        let local = self
            .library
            .episode_download(url)
            .ok()
            .flatten()
            .filter(|p| std::path::Path::new(p).exists());
        let started = match &local {
            Some(path) => self.player.play_file(path, resume),
            None => self.player.play_uri(url, resume),
        };
        match started {
            Ok(()) => {
                self.mini.now_playing = Some(title.to_string());
                self.mini.current_album = None; // podcast episode — no album page
                self.mini.playing = true;
                self.transport.playing_path = None;
                self.podcasts.playing_episode_url = Some(url.to_string());
                self.streaming.playing_stream = None;
                self.youtube.playing_video_id = None;
                self.files.playing_remote = false;
                self.stop_recorder();
                self.transport.queue.clear();
                self.transport.queue_pos = 0;
                self.mini.position_ms = resume.max(0);
                self.mini.track_duration_ms = 0;
                *self.transport.close_resume.borrow_mut() = None;
                self.mpris.set_metadata(0, title, None, None, None, None);
                self.mpris.set_playing(true);
                self.refresh_queue_icons();
                // Chapters (time + label) from the shownotes: set seekbar
                // markers and remember them for the hover display.
                let chapters = self
                    .library
                    .episode_description_by_url(url)
                    .ok()
                    .flatten()
                    .map(|d| crate::core::podcast::parse_chapters(&d))
                    .unwrap_or_default();
                self.set_chapters(chapters);
                // Show the current chapter (at the resume/start position) immediately.
                self.update_current_chapter();
                // Apply the per-episode equalizer (episode → podcast → global)
                // on the current output, like a normal track start does.
                self.settings.active_output =
                    crate::core::output::default_output().unwrap_or_default();
                self.apply_current_eq();
                // Count the episode in the statistics: a session keyed by the
                // audio URL (the tick accumulates listened time, finalize on
                // end/switch writes the play_event). Duration backfills on tick.
                self.start_play_session(std::path::PathBuf::from(url), 0);
            }
            Err(e) => tracing::error!("Failed to play episode: {e}"),
        }
    }

    /// Toggle pause/resume on the running episode, or start this one.
    pub(crate) fn toggle_episode(&mut self, url: String, title: String) {
        if self.podcasts.playing_episode_url.as_deref() == Some(url.as_str()) {
            // Already loaded episode → toggle pause/resume.
            if self.mini.playing {
                self.player.pause();
            } else {
                self.player.resume();
            }
            self.mini.playing = !self.mini.playing;
            self.mpris.set_playing(self.mini.playing);
            self.refresh_queue_icons();
        } else {
            // Other/no episode → start this one.
            self.play_episode(&url, &title);
        }
    }

    /// Seek the running episode to `ms`, or start it at that mark.
    pub(crate) fn episode_seek_to(&mut self, url: String, title: String, ms: i64) {
        if self.podcasts.playing_episode_url.as_deref() == Some(url.as_str()) {
            // Already running → jump directly to the spot.
            if self.player.seek_ms(ms).is_ok() {
                self.mini.position_ms = ms;
                self.save_episode_progress();
            }
        } else {
            // Otherwise start the episode at the jump mark.
            self.play_episode_at(&url, &title, ms);
        }
    }
}
