//! Transport message handlers (`on_*`): play a folder/artist/album track,
//! track end, the per-second tick, resume persisting, queue jumps and
//! play/pause. Split out of [`crate::ui::app_playback`] – pure reordering, the
//! methods remain inherent `impl App` methods.

use std::path::PathBuf;

use relm4::ComponentController;

use crate::ui::app::App;

impl App {
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
        self.youtube.keep_recent_order = false;
        self.transport.queue = vec![PathBuf::from(&path)];
        self.transport.queue_pos = 0;
        // A single tapped song is not an album play (see `PlaySession::source`).
        self.transport.next_source = Some("single");
        self.play_current();
        self.refresh_queue_icons();
        if close {
            self.nav.nav_view.pop_to_tag("main");
        }
    }

    /// Header of an album page: its tracks as the queue, in the order shown
    /// or shuffled. While one of them is the loaded track, "Play" pauses or
    /// resumes instead of starting over.
    pub(crate) fn on_play_tracks(&mut self, paths: Vec<String>, shuffle: bool) {
        let files: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
        if files.is_empty() {
            return;
        }
        if !shuffle
            && self
                .transport
                .playing_path
                .as_ref()
                .is_some_and(|p| files.contains(p))
        {
            if self.mini.playing {
                self.save_resume();
            }
            self.flip_playing();
            return;
        }
        self.youtube.playing_playlist = false;
        self.youtube.keep_recent_order = false;
        let len = files.len();
        self.transport.queue = files;
        self.transport.shuffle = shuffle;
        // Shuffled: a random first track, then a fresh order over the rest.
        self.transport.queue_pos = if shuffle {
            gtk::glib::random_int_range(0, len as i32) as usize
        } else {
            0
        };
        if shuffle {
            self.rebuild_shuffle_order();
        }
        self.mpris.set_shuffle(shuffle);
        self.play_current();
        self.refresh_queue_icons();
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
        if self.youtube.playing_live.is_some() {
            self.yt_live_ended();
            return;
        }
        if self.files.playing_remote {
            // Remote queue: advance to the next track (or stop at the
            // end). Runs separately from the local queue.
            self.remote_next(false);
        } else if self.podcasts.playing_episode_url.is_some() && self.transport.queue.is_empty() {
            // A streamed episode has ended (no queue
            // behind it): finalize its statistics session as "fully
            // listened", then reset the playback state and marking.
            self.finalize_play_session(true);
            // Mark it heard so it stays in Recently/Newest as "Listened" even
            // though the resume position is cleared at the very end.
            if let Some(url) = self.podcasts.playing_episode_url.clone() {
                let _ = self.library.mark_episode_finished(&url);
                // Flip its rows to "Listened" now, instead of only after the
                // next rebuild (which used to need a tab switch).
                self.podcasts_page
                    .emit(crate::ui::podcasts_page::PodcastsInput::EpisodeFinished { url });
            }
            // Tear the pipeline down, the way the end of a queue does (see
            // `play_next`). EOS does not change a pipeline's state: without
            // this the deck stays in PLAYING with its PulseAudio stream open,
            // while the app already believes nothing is playing. Any later
            // change of the audio route - earbuds running flat, a call
            // ending, headphones plugged in - makes that leftover pipeline
            // preroll again and play the finished episode into the room. And
            // because the app's state, and with it MPRIS, still says
            // "paused", nothing has anything to stop: not the player bar, not
            // the lock screen, not a desktop service watching MPRIS. Seen
            // twice on 2026-09-20 with an episode that had ended at 02:37:
            // once at 06:44 when the earbuds ran out of battery, once at
            // 10:18 at the end of a phone call.
            self.player.stop();
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
                let path_str = path.to_string_lossy().into_owned();
                let _ = self.library.set_resume_path(&path_str, 0);
                // The playback state has to be cleared here as well — with
                // `playing_path` gone, `save_resume` no longer reaches it, and a
                // stale "a few seconds before the end" would otherwise be waiting
                // for the next start of the app.
                self.save_current_position(&path_str, 0, 0);
                // Remember it for "previous": a finished track that nothing
                // followed is played again rather than stepped past.
                self.transport.last_finished = Some(path);
            }
            // A long-form YouTube item that ran out counts as watched: keep the
            // mark (instead of a resume point) and show it in its rows at once.
            if let Some(vid) = self.youtube.playing_video_id.clone()
                && crate::core::youtube::is_longform(
                    (self.mini.track_duration_ms > 0).then_some(self.mini.track_duration_ms / 1000),
                )
            {
                let _ = self.library.mark_yt_finished(&vid);
                self.yt_page
                    .emit(crate::ui::yt_page::YtInput::VideoFinished { video_id: vid });
            }
            *self.transport.close_resume.borrow_mut() = None;
            // If a single song was slipped in between, now resume the interrupted
            // queue at its spot.
            if self.transport.queue.len() == 1 && self.transport.interrupted_queue.is_some() {
                if let Some((q, pos, at_ms)) = self.transport.interrupted_queue.take() {
                    self.transport.queue = q;
                    self.transport.queue_pos = pos;
                    // Pick the interrupted track up where it was cut off — the
                    // user never asked to restart it, and a song carries no
                    // resume position of its own any more.
                    if at_ms > 0 {
                        self.transport.forced_start_ms = Some(at_ms);
                    }
                    self.play_current();
                }
            } else {
                // A new (multi-part) playback discards a possibly
                // remembered interruption.
                self.transport.interrupted_queue = None;
                self.play_next(false);
            }
        }
    }

    /// Pushes the running item's position to the list pages, so their progress
    /// lines advance live (podcast episodes always; YouTube only for long-form
    /// items, see [`Self::push_yt_listen_progress`]).
    fn push_listen_progress(&self) {
        let pos = self.mini.position_ms;
        let dur = self.mini.track_duration_ms;
        if let Some(url) = self.podcasts.playing_episode_url.clone() {
            self.podcasts_page.emit(
                crate::ui::podcasts_page::PodcastsInput::EpisodeProgressTick {
                    url,
                    position_ms: pos,
                    duration_ms: dur,
                },
            );
        }
        // YouTube only for long-form items — a song's row stays plain.
        if let Some(video_id) = self.youtube.playing_video_id.clone()
            && crate::core::youtube::is_longform((dur > 0).then_some(dur / 1000))
        {
            self.yt_page
                .emit(crate::ui::yt_page::YtInput::VideoProgressTick {
                    video_id,
                    position_ms: pos,
                    duration_ms: dur,
                });
        }
    }

    /// 5 s timer: persist the resume point of the running track/episode.
    pub(crate) fn on_persist_resume(&mut self) {
        if self.mini.playing {
            // Persist resume points on this 5 s timer (not every Tick):
            // a hard crash loses at most ~5 s of position, while normal
            // pause/seek/track-switch/close still save immediately.
            self.save_resume();
            if let Some(url) = self.podcasts.playing_episode_url.clone() {
                self.save_episode_progress();
                // Now that a position exists, a freshly started episode belongs
                // in "Recently" — the page pulls it in if it is missing.
                self.podcasts_page.emit(
                    crate::ui::podcasts_page::PodcastsInput::EpisodeProgressPersisted { url },
                );
            }
            self.save_yt_progress();
            if let Some(pos) = self.player.position_ms() {
                self.mpris.set_position(pos);
            }
        }
    }

    /// 1 s timer: drive timeshift recording, refresh station icons and update
    /// the seek bar / chapter / statistics counters.
    pub(crate) fn on_tick(&mut self) {
        // Advance the running timeshift recording at the song boundaries.
        if self.streaming.record_state.is_some() {
            self.drive_recording();
        }
        // Sync the play/pause and record icons of the station rows.
        self.sync_stream_page_icons();
        if self.mini.playing {
            // A broken network stream is being re-opened: show the spinner
            // until it has prerolled again (`PlaybackReady` clears it).
            if self.player.is_reconnecting() {
                self.mini.loading = true;
            }
            // Advance the sleep-timer countdown / fade-out (only while playing).
            self.sleep_tick();
            // Only read the pipeline once the new source is actually on it.
            // While `loading` is set, the deck still holds the **previous**
            // track: a YouTube stream is resolved by a worker (yt-dlp takes
            // seconds), and the optimistic now-playing state is shown meanwhile.
            // Reading the position then drags the old track's playhead — usually
            // sitting at its very end — into the bar of the new one, which looks
            // exactly like a jump to a few seconds before the end.
            // A YouTube live stream reports its place in the ~1 h DVR window
            // (with no duration), which reads as a full bar at "59:51" — keep it
            // at 0:00 like a radio station.
            if !self.mini.loading && self.youtube.playing_live.is_none() {
                if let Some(pos) = self.player.position_ms() {
                    self.mini.position_ms = pos;
                }
                if let Some(dur) = self.player.duration_ms() {
                    self.mini.track_duration_ms = dur;
                    // The pipeline only knows the length a moment after the start,
                    // and stations / YouTube / podcasts / remote files never carry
                    // one up front – hand it to the lock screen once it is known.
                    self.mpris.set_length(dur);
                }
            }
            // Keep the MCP now-playing snapshot fresh (position + state).
            self.publish_now_playing();
            // Carry the close snapshot along.
            if let Some(entry) = self.transport.close_resume.borrow_mut().as_mut() {
                entry.1 = self.mini.position_ms;
                entry.2 = self.mini.track_duration_ms;
            }
            // (Episode resume is persisted on the 5 s PersistResume timer,
            // not here — no per-second DB write on the UI thread.)
            // The list rows do follow every second though: the pages update the
            // running item's progress line in place (no DB, no rebuild).
            self.push_listen_progress();
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
            if let Some(cs) = self.transport.close_session.borrow_mut().as_mut()
                && let Some(s) = self.transport.play_session.as_ref()
            {
                cs.2 = s.played_ms;
                cs.3 = s.duration_ms;
            }
            // Crossfade into the next track once we're inside the fade window
            // (no-op when crossfade is off or the next entry isn't eligible).
            self.maybe_crossfade();
        }
    }

    /// Play a user-queue entry now: move its block to the front, then advance.
    pub(crate) fn on_play_queue_at(&mut self, start: usize, len: usize) {
        // The row of the entry that is already running shows a pause icon (as
        // that entry does in every other list), so pressing it has to toggle
        // pause/resume rather than re-queue the block. An album row asks about
        // the running album, a single row about the file — the same two
        // questions its icon was drawn from.
        if let Some(first) = self.transport.user_queue.get(start).cloned() {
            let running = if len >= 2 {
                self.library
                    .track_by_path(&first.to_string_lossy())
                    .ok()
                    .flatten()
                    .and_then(|t| t.album)
                    .filter(|a| !a.trim().is_empty())
                    .is_some_and(|album| self.toggle_if_active_album_name(&album))
            } else {
                self.toggle_if_active_file(&first)
            };
            if running {
                return;
            }
        }
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
            self.play_next(false);
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
        } else if let Some(vid) = self.youtube.playing_live.clone() {
            // A paused live stream rejoins the broadcast where it is *now*
            // (resuming the old buffer would lag behind, or fail once its HLS
            // segments are gone).
            let title = self.mini.now_playing.clone().unwrap_or_default();
            self.play_yt_live(vid, title);
            return;
        } else if (self.transport.playing_path.is_some()
            || self.streaming.playing_stream.is_some()
            || self.podcasts.playing_episode_url.is_some())
            && self.player.resume()
        {
            // Paused (file, station or episode) → resume.
            self.mini.playing = true;
        } else if !self.transport.queue.is_empty() {
            // Playback had ended → restart from the current position (rewound
            // to 0 after the end). play_current sets
            // playing/MPRIS/icons itself.
            self.play_current();
            return;
        } else if let Some(id) = self.streaming.playing_stream {
            // The station is still the current context but the pipeline is gone
            // (torn down by a desktop Stop) → start it over.
            self.play_stream(id);
            return;
        } else if let Some(url) = self.podcasts.playing_episode_url.clone() {
            // Same for an episode; it resumes at its stored position.
            let title = self.mini.now_playing.clone().unwrap_or_default();
            self.play_episode(&url, &title);
            return;
        } else if !self.transport.user_queue.is_empty() {
            // Nothing loaded, but the user queued tracks → start the queue
            // (play_next splices the first queued track into the context).
            self.play_next(true);
            return;
        } else {
            return;
        }
        self.mpris.set_playing(self.mini.playing);
        // Adjust the play/pause icon of the active track in the list.
        self.refresh_queue_icons();
        self.sync_stream_page_icons();
        // Keep the tray's Play/Pause label in sync.
        self.refresh_tray_state();
        // Mirror the pause/resume into the MCP now-playing snapshot.
        self.publish_now_playing();
    }
}
