//! Playback: queue, play/pause/next/prev, resume logic and the
//! running equalizer. Extracted from app.rs – pure reordering, no
//! change in behavior; the methods remain inherent `impl App` methods.

use std::path::{Path, PathBuf};

use relm4::{ComponentController, ComponentSender, adw};

use crate::core::queue;
use crate::core::remote::{self, Backend};
use crate::core::scanner;
use crate::model::Track;
use crate::ui::app::{App, Msg};
use crate::ui::app_favorites::EntryMarks;
use crate::ui::app_lyrics::LyricsMsg;
use crate::ui::play_mark::{PlaybackSink, PlaybackState};

pub(crate) use crate::core::queue::PREV_RESTART_MS;

/// Settings key: path of the track the player currently has loaded. Together
/// with [`CURRENT_POS_KEY`] it forms the playback state that survives a
/// restart, alongside the saved queue (`queue_paths` / `queue_pos`).
pub(crate) const CURRENT_PATH_KEY: &str = "current_path";
/// Settings key: how far [`CURRENT_PATH_KEY`]'s track has played, in ms.
pub(crate) const CURRENT_POS_KEY: &str = "current_pos_ms";

impl App {
    /// Gathers what is playing right now, in the terms the lists ask in (see
    /// [`PlaybackState`]). Built once per change so no list has to dig through
    /// the transport itself — that digging is where the answers used to drift.
    pub(crate) fn playback_state(&self) -> PlaybackState {
        PlaybackState {
            playing: self.mini.playing,
            path: self.transport.queue.get(self.transport.queue_pos).cloned(),
            album: self.playing_album(),
            album_card: self.playing_album_card(),
            // Remote playback runs its own queue, separate from the local one.
            rel_path: self
                .files
                .playing_remote
                .then(|| {
                    self.files
                        .remote_queue
                        .get(self.files.remote_pos)
                        .map(|t| t.rel_path.clone())
                })
                .flatten(),
            episode_url: self.podcasts.playing_episode_url.clone(),
            // A live stream is marked by its video id as well (Live tab rows).
            video_id: self
                .youtube
                .playing_video_id
                .clone()
                .or_else(|| self.youtube.playing_live.clone()),
            // The "in queue" marker reflects the explicit user queue, not the
            // active context (the album currently playing through).
            queued: self.transport.user_queue.iter().cloned().collect(),
        }
    }

    /// Pushes the current playback state to every list that marks a row.
    ///
    /// The mechanism differs per list — a factory message, recycled rows, a
    /// control registry, a component's channel — but the state and the marker
    /// policy are shared (see [`crate::ui::play_mark`]), so no list can drift
    /// into answering "is this the one playing?" its own way again.
    pub(crate) fn refresh_queue_icons(&mut self) {
        let state = self.playback_state();
        let sinks: [&dyn PlaybackSink; 6] = [
            &self.libview.entries,
            &self.libview.albums,
            &self.libview.singles,
            &self.libview.compilations,
            &self.podcasts_page,
            &self.yt_page,
        ];
        for sink in sinks {
            sink.apply_playback(&state);
        }
        // Keep the MCP snapshot current while paused, too (the tick only
        // publishes during playback).
        self.publish_now_playing();
        for marks in [
            &self.favorites.favorite_marks,
            &self.favorites.audiobook_marks,
            &self.concerts.concert_marks,
            &self.libview.page_marks,
            &self.transport.queue_marks,
            &self.memo.marks,
        ] {
            EntryMarks(marks).apply_playback(&state);
        }
        // Sync the play row of an open detail dialog with the playback state.
        self.refresh_ctx_play();
        // Re-check the yt-dlp "broken" banner (a failed stream resolve flips it).
        self.yt_page
            .emit(crate::ui::yt_page::YtInput::RefreshBroken);
        // The recording/station rows have their own change guard (they are also
        // refreshed from the per-second tick), so they stay a separate push.
        self.sync_stream_page_icons();
        // The queue/next track may have changed (add/remove/reorder) → keep the
        // armed gapless follow in step with the new "next".
        self.arm_gapless();
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
        if let Some((sid, rel)) = remote::parse_nc_path(path_str) {
            let cache = remote::cache_path(sid, &rel);
            if cache.exists() {
                return self
                    .player
                    .play_file(&cache.to_string_lossy(), resume_ms)
                    .map(|_| false);
            }
            if let Some(backend) = self
                .files
                .sources
                .iter()
                .find(|s| s.id == sid)
                .and_then(Backend::from_source)
            {
                return backend
                    .stream_uri(sid, &rel)
                    .and_then(|uri| self.player.play_uri(&uri, resume_ms))
                    .map(|_| true);
            }
            return Err(anyhow::anyhow!("remote source unavailable"));
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
        if let Some(vid) = crate::core::youtube::parse_yt_path(&path_str)
            && let Ok(Some(t)) = self.library.yt_title(&vid)
        {
            return t;
        }
        // Voice memos aren't in the music library — show their list title
        // ("Memo <date>" or the user's name), not the file name.
        if let Some(t) = self.memo_title_for_path(&path_str) {
            return t;
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

    /// The display title of a voice memo at `path` (the loaded list first, a DB
    /// lookup as fallback for the startup-restore case), or `None` if `path`
    /// isn't a memo.
    fn memo_title_for_path(&self, path_str: &str) -> Option<String> {
        if let Some(m) = self
            .memo
            .memo_items
            .iter()
            .find(|m| m.path.as_str() == path_str)
        {
            return Some(m.title.clone());
        }
        let memos_dir = crate::core::mic::memos_dir();
        if std::path::Path::new(path_str).starts_with(&memos_dir) {
            return self.library.memo_title_by_path(path_str).ok().flatten();
        }
        None
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

    /// Flips play/pause on whatever is running and syncs everything that hangs
    /// off that state: the flag the UI watches, the MPRIS status and the queue
    /// icons. Saving the playback position is deliberately *not* part of it —
    /// local tracks, podcast episodes and YouTube items each have their own
    /// store ([`Self::save_resume`], [`Self::save_episode_progress`],
    /// [`Self::save_yt_progress`]), so the caller saves before calling.
    pub(crate) fn flip_playing(&mut self) {
        // Same path as the player bar's button: it also covers the case where
        // the app has a current track but the pipeline holds nothing (a queue
        // restored at startup, or after a desktop Stop), which a bare
        // `resume()` would turn into a silent, forever-"playing" state.
        self.on_toggle_play();
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
        }
        self.flip_playing();
        true
    }

    /// Album of the track currently loaded into the player. Unlike
    /// [`crate::ui::app::MiniState::current_album`] — which drives the player
    /// bar's album shortcut and is therefore blank for single-track albums —
    /// this is purely "which album is running", so the Singles rows can mark
    /// themselves too.
    pub(crate) fn playing_album(&self) -> Option<String> {
        let path = self.transport.playing_path.as_ref()?;
        self.library
            .track_by_path(&path.to_string_lossy())
            .ok()
            .flatten()
            .and_then(|t| t.album)
            .filter(|a| !a.trim().is_empty())
    }

    /// Card key (see [`crate::core::album_group::card_key`]) of the album card
    /// the loaded track belongs to — what the overview rows mark on, so of two
    /// same-titled albums by different artists only the running one lights up.
    pub(crate) fn playing_album_card(&self) -> Option<String> {
        let path = self.transport.playing_path.as_ref()?;
        let t = self
            .library
            .track_by_path(&path.to_string_lossy())
            .ok()
            .flatten()?;
        let album = t.album.filter(|a| !a.trim().is_empty())?;
        self.library
            .album_card_key(t.artist.as_deref().unwrap_or(""), &album)
            .ok()
    }

    /// Name-based [`Self::toggle_if_active_album`] for the lists that mark album
    /// blocks by album name (queue, playlists), so the toggle asks the same
    /// question their icon was drawn from.
    pub(crate) fn toggle_if_active_album_name(&mut self, album: &str) -> bool {
        if !self
            .playing_album()
            .is_some_and(|a| a.eq_ignore_ascii_case(album))
        {
            return false;
        }
        if self.mini.playing {
            self.save_resume();
        }
        self.flip_playing();
        true
    }

    /// Like [`Self::toggle_if_active_file`], but for a whole album card: the
    /// play button of an overview row shows a pause icon while that album runs,
    /// so pressing it must toggle pause/resume instead of restarting from track 1.
    pub(crate) fn toggle_if_active_album(&mut self, artist: &str, album: &str) -> bool {
        let key = self
            .library
            .album_card_key(artist, album)
            .unwrap_or_default();
        if self.playing_album_card().as_deref() != Some(key.as_str()) {
            return false;
        }
        if self.mini.playing {
            self.save_resume();
        }
        self.flip_playing();
        true
    }

    pub(crate) fn play_current(&mut self) {
        // Consume the one-shot start markers first, so none can leak into a
        // later start — not even when this call returns early below. The
        // restored session position is taken here and matched against the path
        // further down: whatever starts now, it is spent either way.
        let fresh_start = std::mem::take(&mut self.transport.fresh_start);
        let restored_session = self.transport.resume_current.take();
        let forced_ms = self.transport.forced_start_ms.take();
        let stepping_back = std::mem::take(&mut self.transport.skip_history_push);
        // Something is starting, so there is no finished track waiting to be
        // replayed any more (see `play_prev`).
        self.transport.last_finished = None;
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
        // Keep the previous context and track reachable for "previous".
        let t = &mut self.transport;
        queue::record_start(
            &mut t.nav_stack,
            &mut t.play_history,
            t.prev_ctx.as_ref(),
            &t.queue,
            t.playing_path.as_ref(),
            &path,
            stepping_back,
        );
        let path_str = path.to_string_lossy().to_string();
        let yt_video = crate::core::youtube::parse_yt_path(&path_str);
        let track = self.library.track_by_path(&path_str).ok().flatten();
        // A `yt:` track shows its title from the play context (single video or
        // playlist queue) rather than its id.
        let name = yt_video
            .as_ref()
            .and_then(|vid| self.youtube.video_titles.get(vid).cloned())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| self.display_name(&path));
        // A tapped jump mark of this very video (see `yt_play_video_at`) —
        // honoured for a downloaded copy just as for a stream.
        let mark_ms = yt_video.as_ref().and_then(|vid| {
            self.youtube
                .pending_seek
                .take()
                .filter(|(v, _)| v == vid)
                .map(|(_, ms)| ms)
        });
        // The restored session position counts for its own track only.
        let restored = restored_session
            .filter(|(p, _)| *p == path)
            .map(|(_, ms)| ms);
        // A YouTube item has no library row — its resume point lives in
        // `yt_progress`; of local tracks only long-form material and audiobooks
        // keep one (see `should_resume`).
        let stored_ms = match (&track, &yt_video) {
            (_, Some(vid)) => self.library.yt_progress(vid).unwrap_or(0).max(0),
            (Some(t), None) if self.should_resume(t) => t.resume_ms,
            _ => 0,
        };
        let start_ms = start_position(mark_ms, forced_ms, fresh_start, restored, stored_ms);
        if let Some(video_id) = &yt_video {
            // Log to the "Recent" history and enrich (cover/artist) in the background.
            self.note_youtube_play(video_id, &name);
            let has_local = self
                .library
                .yt_download(video_id)
                .ok()
                .flatten()
                .is_some_and(|p| Path::new(&p).exists());
            if !has_local {
                self.start_yt_stream(path, video_id.clone(), name, start_ms);
                return;
            }
        }
        match self.start_track_playback(&path_str, start_ms) {
            Ok(is_network_stream) => {
                // A track started → reset the unplayable-skip guard.
                self.transport.skip_count = 0;
                // Network streams (Nextcloud) buffer before playing → spinner
                // until ready; local/cached files start instantly (no spinner).
                self.mini.loading = is_network_stream;
                let start = self.player.position_ms().unwrap_or(start_ms);
                self.note_track_started(&path, track.as_ref(), yt_video, name, start);
                // Arm the next track for gapless continuation (no-op when the
                // next entry isn't a sequential local file or gapless is off).
                self.arm_gapless();
            }
            Err(e) => {
                // Synchronous failure (e.g. Nextcloud source without credentials)
                // → skip this entry (message-driven, so no recursion here).
                tracing::warn!("Playback failed, skipping: {e}");
                let _ = self.input.send(Msg::Transport(TransportMsg::PlaybackError));
            }
        }
    }

    /// Starts a YouTube queue track that has no offline copy. Resolving its
    /// stream (`yt-dlp -g`) takes seconds, so the now-playing state is set
    /// optimistically and a worker resolves the URL; playback begins when
    /// `YtStreamResolved` arrives.
    fn start_yt_stream(&mut self, path: PathBuf, video_id: String, name: String, resume: i64) {
        self.transport.skip_count = 0;
        self.transport.playing_path = Some(path);
        self.podcasts.playing_episode_url = None;
        self.streaming.playing_stream = None;
        self.youtube.playing_live = None;
        self.files.playing_remote = false;
        self.youtube.playing_video_id = Some(video_id.clone());
        self.stop_recorder();
        self.mpris.set_metadata(0, &name, None, None, None, None);
        self.mini.now_playing = Some(name);
        self.mini.current_album = None; // YouTube — no local album page
        self.mini.playing = true;
        // Resolving the stream URL (yt-dlp) and buffering takes a moment
        // → spinner until `YtStreamResolved` plays and the player is ready.
        self.mini.loading = true;
        self.mini.position_ms = resume;
        self.mini.track_duration_ms = 0;
        *self.transport.close_resume.borrow_mut() = None;
        self.set_chapters(Vec::new());
        // No lyrics for a (just-resolving) stream; drop the old track's.
        self.close_lyrics_view();
        self.lyrics.current = None;
        self.lyrics.for_path = None;
        self.mpris.set_playing(true);
        self.refresh_queue_icons();
        let input = self.input.clone();
        let ticket = crate::core::youtube::PLAY_RESOLVE.ticket();
        std::thread::spawn(move || {
            // A newer play superseded this one; its own resolve follows.
            if !crate::core::youtube::PLAY_RESOLVE
                .settled(ticket, crate::core::youtube::RESOLVE_SETTLE)
            {
                return;
            }
            let result =
                crate::core::youtube::resolve_audio_url(&video_id).map_err(|e| e.to_string());
            let _ = input.send(Msg::Yt(crate::ui::app_yt_glue::YtMsg::YtStreamResolved {
                video_id,
                resume,
                result,
            }));
        });
    }

    /// Everything that hangs off "this queue track plays now": which source is
    /// active, the player bar, MPRIS, cover background, statistics session,
    /// saved queue, chapters, lyrics and tag identification. Shared by
    /// [`Self::play_current`] and the gapless/crossfade hand-over, so the two
    /// cannot drift apart. Loading the audio and the spinner stay with the
    /// caller; `start` is where playback begins (ms).
    pub(crate) fn note_track_started(
        &mut self,
        path: &Path,
        track: Option<&Track>,
        yt_video: Option<String>,
        name: String,
        start: i64,
    ) {
        self.transport.playing_path = Some(path.to_path_buf());
        // Music is playing again – no podcast episode/station/
        // remote file active anymore.
        self.podcasts.playing_episode_url = None;
        self.streaming.playing_stream = None;
        self.youtube.playing_live = None;
        self.files.playing_remote = false;
        // Album shortcut in the player bar: only for a local track (not for a
        // YouTube one). For a YouTube track `yt_video` is its id (marks the
        // row); None resets it.
        self.mini.current_album = match yt_video {
            Some(_) => None,
            None => self.album_shortcut(track),
        };
        self.youtube.playing_video_id = yt_video;
        self.stop_recorder();
        self.mini.now_playing = Some(name);
        self.mini.playing = true;
        // Refresh the active output (may have changed).
        self.settings.active_output = crate::core::output::default_output().unwrap_or_default();
        self.apply_current_eq();
        // Inform the lock screen/media keys about the new track.
        self.update_mpris_metadata(path, track);
        self.mpris.set_playing(true);
        // Refresh the blurred cover background + tray menu for the new track.
        self.refresh_cover_background();
        self.refresh_tray_state();
        self.mpris.set_position(start);
        self.mpris.seeked(start);
        // Set the seek bar to the new track (the tick refines the duration).
        self.mini.position_ms = start;
        self.mini.track_duration_ms = self
            .player
            .duration_ms()
            .or_else(|| track.and_then(|t| t.duration_ms))
            .unwrap_or(0);
        let path_str = path.to_string_lossy().to_string();
        // Snapshot for saving on close — kept for every track: the close
        // handler writes the playback state from it always and the track's own
        // resume point only where one is wanted.
        *self.transport.close_resume.borrow_mut() =
            Some((path_str.clone(), start, self.mini.track_duration_ms));
        // Start a new listening session for the statistics.
        self.start_play_session(path.to_path_buf(), self.mini.track_duration_ms);
        // Adjust the play/queue markers in the list to the new track.
        self.refresh_queue_icons();
        // Save the queue + position for the next start.
        self.save_queue();
        // Remember the current context (detection of future queue switches).
        self.transport.prev_ctx = Some((self.transport.queue.clone(), self.transport.queue_pos));
        // Tracks have no chapters — except a downloaded YouTube video, whose
        // description carries the same jump marks the streamed version shows.
        self.set_chapters(self.local_yt_chapters(&path_str));
        self.update_current_chapter();
        // Load lyrics for the new track (embedded/cache instantly, then LRCLIB
        // in the background) – shows the karaoke button when synced lyrics exist.
        let _ = self
            .input
            .send(Msg::Lyrics(LyricsMsg::LoadLyrics(path.to_path_buf())));
        // If usable tags are missing (artist/album), let the track be identified
        // in the background via fingerprint – instead of a bulk run, only what is
        // actually played. The actual gating checks (key, fpcalc, network,
        // attempt limit) are done by fetch_focus_track.
        let needs_id = track.is_none_or(|t| {
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
            let _ = self.input.send(Msg::FingerprintCurrent(path.to_path_buf()));
        }
    }

    /// The player bar's album shortcut for `track`: its (artist, album), but
    /// only when that album has more than this one track — a single-track album
    /// has no meaningful song page to open.
    fn album_shortcut(&self, track: Option<&Track>) -> Option<(String, String)> {
        let t = track?;
        let album = t.album.clone().filter(|a| !a.trim().is_empty())?;
        let artist = t.artist.clone().unwrap_or_default();
        self.library
            .album_card_tracks(&artist, &album)
            .is_ok_and(|tracks| tracks.len() > 1)
            .then_some((artist, album))
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
            self.remote_next(false);
        } else {
            self.play_next(false);
        }
    }

    /// Resolves the equalizer for the running track + active output
    /// (track→album→artist→global, then default output) and applies it live.
    /// Without any setting: neutral (all bands 0).
    pub(crate) fn apply_current_eq(&self) {
        // Internet radio has no queue/track metadata: resolve the per-station EQ
        // (station → global) keyed by the running station id and apply it.
        if let Some(id) = self.streaming.playing_stream {
            let bands = self
                .library
                .resolve_eq_stream(&self.settings.active_output, &id.to_string())
                .unwrap_or([0.0; 10]);
            self.player.set_eq_bands(&bands);
            return;
        }
        // A YouTube live stream: its own `stream`-level EQ keyed by
        // `yt-live:<id>` (set from the player's EQ button), else the global one.
        if let Some(vid) = self.youtube.playing_live.as_deref() {
            let bands = self
                .library
                .resolve_eq_stream(&self.settings.active_output, &format!("yt-live:{vid}"))
                .unwrap_or([0.0; 10]);
            self.player.set_eq_bands(&bands);
            return;
        }
        // A podcast episode (also queue-less): resolve the per-episode EQ
        // (episode → podcast → global) keyed by the episode's audio URL.
        if let Some(url) = self.podcasts.playing_episode_url.clone() {
            self.apply_episode_eq(&url);
            return;
        }
        let Some(path) = self.transport.queue.get(self.transport.queue_pos) else {
            return;
        };
        let path_str = path.to_string_lossy();
        // An episode played from a playlist lands in the queue with its audio
        // URL as the path; route it to the podcast cascade, not the track one.
        if self
            .library
            .podcast_id_for_episode_url(&path_str)
            .ok()
            .flatten()
            .is_some()
        {
            self.apply_episode_eq(&path_str);
            return;
        }
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

    /// Resolves and applies the equalizer for a podcast episode (episode →
    /// podcast → global, then default output) on the active output.
    fn apply_episode_eq(&self, url: &str) {
        let podcast_id = self
            .library
            .podcast_id_for_episode_url(url)
            .ok()
            .flatten()
            .map(|id| id.to_string());
        let bands = self
            .library
            .resolve_eq_podcast(&self.settings.active_output, podcast_id.as_deref(), url)
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
            // Prefer the indexed tracks under the folder (same set as the
            // displayed `folder_tracks_ordered`) over a synchronous recursive
            // filesystem scan on the UI thread; fall back to the scan only when
            // the folder isn't indexed yet.
            let mut fs: Vec<PathBuf> = self
                .library
                .tracks_under_path(path)
                .unwrap_or_default()
                .into_iter()
                .map(|t| PathBuf::from(t.path))
                .collect();
            if fs.is_empty() {
                fs = scanner::collect_audio_files(&p);
            }
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
}

/// `Msg` sub-enum of the transport domain (split out of `App::update`).
#[derive(Debug)]
pub(crate) enum TransportMsg {
    TrackFinished,
    /// The active deck moved to the next queue track **gaplessly** (driven by
    /// `playbin3`'s `about-to-finish`); advance the app's state to match.
    GaplessAdvanced,
    /// Periodic tick: save the resume position of the running track.
    PersistResume,
    /// 1-s tick: update position/duration of the seek bar.
    Tick,
    /// Jump to a position (ms) by dragging/clicking the seek bar.
    Seek(i64),
    Next,
    Prev,
    ToggleShuffle,
    ToggleRepeat,
    TogglePlay,
    /// Open the detail view of the currently running track (click on the bar).
    OpenNowPlaying,
    /// Play a user-queue entry now (its index + length; album rows span `len`
    /// tracks). The entry jumps ahead of the rest of the queue.
    PlayQueueAt {
        start: usize,
        len: usize,
    },
    /// Set the playback speed (0.25–2.0, in 0.25 steps).
    SetPlaybackRate(f64),
    /// The current track failed to play (missing file/mount, unreachable
    /// Nextcloud, …) → skip to the next entry.
    PlaybackError,
    /// The freshly loaded pipeline prerolled (buffered enough to play) → clear
    /// the loading spinner of a slow source (Nextcloud/YouTube).
    PlaybackReady,
    /// The playing file carries its own chapters (a container TOC — an
    /// audiobook in one m4b/mp3). Used when nothing else supplied jump marks.
    EmbeddedChapters(Vec<(i64, String)>),
    /// Clear the user queue (after confirmation). Playback keeps running.
    QueueClear,
    /// Reorder the user queue: move the `len`-track block starting at `from` so
    /// it lands at index `to` (album rows move as one block).
    QueueMoveRange {
        from: usize,
        len: usize,
        to: usize,
    },
    /// Open the queue dialog.
    ShowQueue,
    /// Open the song page of the album currently playing (player-bar shortcut).
    ShowCurrentAlbum,
}

impl App {
    /// Dispatch for [`TransportMsg`] (the former `App::update` arms, moved verbatim).
    pub(crate) fn update_transport(
        &mut self,
        msg: TransportMsg,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        match msg {
            TransportMsg::TrackFinished => self.on_track_finished(),
            TransportMsg::GaplessAdvanced => self.on_gapless_advanced(),
            TransportMsg::PersistResume => self.on_persist_resume(),
            TransportMsg::Tick => self.on_tick(),
            // A live stream is not seekable (see `on_tick`).
            TransportMsg::Seek(_) if self.youtube.playing_live.is_some() => {}
            TransportMsg::Seek(ms) => {
                let ms = ms.max(0);
                self.mini.position_ms = ms;
                if self.player.seek_ms(ms).is_ok() {
                    self.mpris.seeked(ms);
                }
            }
            TransportMsg::Next => self.skip_next(),
            TransportMsg::Prev => self.skip_prev(),
            TransportMsg::ToggleShuffle => {
                self.transport.shuffle = !self.transport.shuffle;
                // When enabling, build a fresh random order of the whole
                // queue (running track first).
                if self.transport.shuffle {
                    self.rebuild_shuffle_order();
                }
                self.mpris.set_shuffle(self.transport.shuffle);
                // Shuffle changes the "next" track → re-arm (or clear) gapless.
                self.arm_gapless();
            }
            TransportMsg::ToggleRepeat => {
                self.transport.repeat = !self.transport.repeat;
                let _ = self
                    .library
                    .set_setting("repeat", if self.transport.repeat { "1" } else { "0" });
                self.mpris.set_repeat(self.transport.repeat);
            }
            TransportMsg::ShowQueue => self.open_queue_dialog(root, sender),
            TransportMsg::ShowCurrentAlbum => {
                if let Some((artist, album)) = self.mini.current_album.clone() {
                    self.open_album_card(sender, &artist, &album);
                }
            }
            TransportMsg::PlayQueueAt { start, len } => self.on_play_queue_at(start, len),
            TransportMsg::SetPlaybackRate(rate) => {
                let rate = (rate / 0.25).round() * 0.25;
                let rate = rate.clamp(0.25, 2.0);
                // Guard against the scale's #[watch] re-emitting the same value.
                if (rate - self.mini.playback_rate).abs() > 1e-3 {
                    self.mini.playback_rate = rate;
                    self.player.set_rate(rate);
                }
            }
            TransportMsg::EmbeddedChapters(chapters) => {
                // Shownotes / YouTube marks were set on purpose for this
                // playback; the file's own TOC only fills the gap.
                if self.mini.chapters.borrow().is_empty() {
                    self.set_chapters(chapters);
                    self.update_current_chapter();
                }
            }
            TransportMsg::PlaybackReady => {
                // Source finished buffering → stop the loading spinner.
                if self.mini.loading {
                    self.mini.loading = false;
                }
            }
            TransportMsg::PlaybackError => {
                // A failed start clears the loading spinner regardless of source.
                self.mini.loading = false;
                // A live stream whose address expired restarts (or stops).
                if self.youtube.playing_live.is_some() {
                    self.yt_live_ended();
                    return;
                }
                // Streams/episodes have no "next" → don't skip on their errors.
                // The player already tried to reconnect: stop for real, so the
                // bar and the lock screen don't go on claiming "playing".
                if self.streaming.playing_stream.is_some()
                    || self.podcasts.playing_episode_url.is_some()
                {
                    // One failure posts several errors: handle the first.
                    if !self.mini.playing {
                        return;
                    }
                    if self.podcasts.playing_episode_url.is_some() {
                        self.save_episode_progress();
                    }
                    self.finalize_play_session(false);
                    self.player.stop();
                    self.mini.playing = false;
                    self.mpris.set_playing(false);
                    self.refresh_queue_icons();
                    self.toast(&crate::i18n::gettext(
                        "Stream not reachable – playback stopped",
                    ));
                    return;
                }
                // Only skip when something is actually queued.
                if self.files.playing_remote || !self.transport.queue.is_empty() {
                    self.skip_current_track();
                }
            }
            TransportMsg::QueueClear => self.on_queue_clear(),
            TransportMsg::QueueMoveRange { from, len, to } => {
                self.on_queue_move_range(from, len, to)
            }
            TransportMsg::TogglePlay => self.on_toggle_play(),
            TransportMsg::OpenNowPlaying => self.on_open_now_playing(root, sender),
        }
    }
}

/// Tolerance around a jump mark: a key-unit seek lands a little before the
/// mark, and the position then must still count as "in" that chapter, or the
/// next press would target the same mark again.
const CHAPTER_SLACK_MS: i64 = 1_500;

/// Where a chapter skip (`dir` = +1 next / −1 previous) goes from `pos_ms`,
/// given the sorted chapter starts. The part before the first mark counts as a
/// chapter of its own. `None` = no mark in that direction: past the last
/// chapter going forward, within the first seconds of the first going back.
/// Previous inside a chapter first restarts it (as for tracks, after
/// [`PREV_RESTART_MS`]).
pub(crate) fn chapter_target(
    starts: &[i64],
    pos_ms: i64,
    dir: i32,
    length_ms: Option<i64>,
) -> Option<i64> {
    let mut marks: Vec<i64> = std::iter::once(0)
        .chain(starts.iter().copied().filter(|&ms| ms > 0))
        .filter(|&ms| length_ms.is_none_or(|len| ms < len))
        .collect();
    marks.sort_unstable();
    marks.dedup();
    let cur = marks
        .iter()
        .rposition(|&ms| ms <= pos_ms + CHAPTER_SLACK_MS)
        .unwrap_or(0);
    if dir > 0 {
        marks.get(cur + 1).copied()
    } else if pos_ms - marks[cur] > PREV_RESTART_MS {
        Some(marks[cur])
    } else {
        cur.checked_sub(1).map(|i| marks[i])
    }
}

/// What a seek request from the desktop should do, given the track length.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SeekTarget {
    /// Seek to this position (ms).
    To(i64),
    /// Skip to the next track.
    Next,
    /// Do nothing at all.
    Ignore,
}

/// `Seek` (relative) per the MPRIS spec: before the start clamps to 0, past the
/// end "acts like a call to Next". Seeking past the end for real would leave the
/// pipeline playing silence beyond the data — a network source never reaches an
/// EOS there, so it would run on (and hold the audio sink awake) forever.
/// A live stream has no length, so nothing can be past its end.
pub(crate) fn relative_seek(pos_ms: i64, offset_ms: i64, length_ms: Option<i64>) -> SeekTarget {
    let target = pos_ms.saturating_add(offset_ms);
    match length_ms {
        Some(len) if target >= len => SeekTarget::Next,
        _ => SeekTarget::To(target.max(0)),
    }
}

/// `SetPosition` per the MPRIS spec: a position outside the track (negative, or
/// beyond its length) is ignored rather than clamped.
pub(crate) fn absolute_seek(pos_ms: i64, length_ms: Option<i64>) -> SeekTarget {
    if pos_ms < 0 {
        return SeekTarget::Ignore;
    }
    match length_ms {
        Some(len) if pos_ms > len => SeekTarget::Ignore,
        _ => SeekTarget::To(pos_ms),
    }
}

/// Where a start of a track begins, in ms — the pure half of the decision in
/// [`App::play_current`]. In order of precedence:
///
/// 1. `mark_ms`: a tapped jump mark of the YouTube video being started.
/// 2. `forced_ms`: a one-shot start demanded by the caller (the recording
///    editor's "play from the playhead", an interrupted queue picked back up).
/// 3. `fresh_start`: moving on **within** the running queue always begins at
///    the beginning — an album's next song, an audiobook's next chapter.
/// 4. `restored_ms`: where this very track stood when listening stopped, from
///    the playback state restored at startup. A song has one of these too.
/// 5. `stored_ms`: the track's own resume point, which only long-form material
///    and audiobooks carry (see [`App::should_resume`]); 0 for everything else.
pub(crate) fn start_position(
    mark_ms: Option<i64>,
    forced_ms: Option<i64>,
    fresh_start: bool,
    restored_ms: Option<i64>,
    stored_ms: i64,
) -> i64 {
    match mark_ms.or(forced_ms) {
        Some(ms) => ms.max(0),
        None if fresh_start => 0,
        None => restored_ms.unwrap_or(stored_ms).max(0),
    }
}

#[cfg(test)]
mod tests {
    use super::chapter_target;

    #[test]
    fn chapter_next_steps_through_marks_then_gives_up() {
        let marks = [0, 60_000, 120_000];
        assert_eq!(
            chapter_target(&marks, 5_000, 1, Some(180_000)),
            Some(60_000)
        );
        // Landed slightly before the mark (key-unit seek) → still the next one.
        assert_eq!(
            chapter_target(&marks, 59_200, 1, Some(180_000)),
            Some(120_000)
        );
        assert_eq!(chapter_target(&marks, 130_000, 1, Some(180_000)), None);
        // A mark at/after the end does not count.
        assert_eq!(chapter_target(&[0, 200_000], 5_000, 1, Some(180_000)), None);
    }

    #[test]
    fn chapter_prev_restarts_then_steps_back() {
        let marks = [0, 60_000, 120_000];
        assert_eq!(chapter_target(&marks, 90_000, -1, None), Some(60_000));
        assert_eq!(chapter_target(&marks, 61_000, -1, None), Some(0));
        assert_eq!(chapter_target(&marks, 10_000, -1, None), Some(0));
        assert_eq!(chapter_target(&marks, 1_000, -1, None), None);
    }

    #[test]
    fn intro_before_first_mark_is_a_chapter() {
        let marks = [30_000, 90_000];
        assert_eq!(chapter_target(&marks, 5_000, 1, None), Some(30_000));
        assert_eq!(chapter_target(&marks, 31_000, -1, None), Some(0));
    }

    use super::{SeekTarget, absolute_seek, relative_seek, start_position};

    #[test]
    fn relative_seek_past_the_end_skips_instead_of_running_on() {
        let len = Some(180_000);
        assert_eq!(relative_seek(10_000, 20_000, len), SeekTarget::To(30_000));
        // The case that wedged the player: far beyond the end.
        assert_eq!(relative_seek(10_000, 600_000, len), SeekTarget::Next);
        // Exactly at the end is the end, too.
        assert_eq!(relative_seek(170_000, 10_000, len), SeekTarget::Next);
        // Backwards past the start clamps to 0 (spec).
        assert_eq!(relative_seek(5_000, -20_000, len), SeekTarget::To(0));
    }

    /// A station has no length; every seek stays a seek (the pipeline itself
    /// refuses what it cannot do).
    #[test]
    fn relative_seek_without_a_length_never_skips() {
        assert_eq!(relative_seek(1_000, 600_000, None), SeekTarget::To(601_000));
        assert_eq!(relative_seek(1_000, -600_000, None), SeekTarget::To(0));
    }

    #[test]
    fn absolute_seek_ignores_positions_outside_the_track() {
        let len = Some(180_000);
        assert_eq!(absolute_seek(120_000, len), SeekTarget::To(120_000));
        assert_eq!(absolute_seek(180_000, len), SeekTarget::To(180_000));
        assert_eq!(absolute_seek(600_000, len), SeekTarget::Ignore);
        assert_eq!(absolute_seek(-1, len), SeekTarget::Ignore);
        assert_eq!(absolute_seek(600_000, None), SeekTarget::To(600_000));
    }
    /// The whole point of the resume rework: moving on within a queue starts
    /// the next piece at its beginning, while a paused album picks the song it
    /// stood on back up — and a song that was merely sampled once does not
    /// drag its old position along.
    #[test]
    fn start_position_follows_its_order_of_precedence() {
        // Nothing to go on: the beginning.
        assert_eq!(start_position(None, None, false, None, 0), 0);

        // The track's own resume point (audiobook, long-form) is used…
        assert_eq!(start_position(None, None, false, None, 42_000), 42_000);
        // …but never when moving on within the queue: the next chapter of an
        // audiobook begins at its beginning.
        assert_eq!(start_position(None, None, true, None, 42_000), 0);

        // A paused album carries on mid-song, even though a song keeps no
        // resume point of its own (stored_ms = 0).
        assert_eq!(start_position(None, None, false, Some(75_000), 0), 75_000);
        // Advancing wins over that too — it is the *next* song starting.
        assert_eq!(start_position(None, None, true, Some(75_000), 0), 0);
        // And it takes precedence over a stored point for the same track.
        assert_eq!(
            start_position(None, None, false, Some(75_000), 42_000),
            75_000
        );

        // A forced start beats everything, advance included.
        assert_eq!(
            start_position(None, Some(9_000), false, Some(75_000), 42_000),
            9_000
        );
        assert_eq!(
            start_position(None, Some(9_000), true, Some(75_000), 42_000),
            9_000
        );
        // Even a forced start of 0 is honoured as "from the top".
        assert_eq!(start_position(None, Some(0), false, Some(75_000), 0), 0);

        // Negatives can't reach the player.
        // A tapped jump mark wins over all of it, forced start included.
        assert_eq!(
            start_position(Some(30_000), Some(9_000), true, Some(75_000), 42_000),
            30_000
        );

        assert_eq!(start_position(None, Some(-5), false, None, 0), 0);
        assert_eq!(start_position(None, None, false, Some(-5), 0), 0);
    }
}
