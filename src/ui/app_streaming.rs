//! Streaming (internet radio): station list, station detail page, and the
//! add dialog (manual stream URL **and** worldwide search via the
//! Radio-Browser API). Stations are streamed directly – nothing is downloaded.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, gettext_f};
use crate::model::StreamItem;
use crate::ui::app::{App, Msg};

/// State of a running continuous recording (state machine, driven by the 1 s
/// tick; saves complete songs until manually stopped).
pub(crate) struct RecordState {
    pub stream_id: i64,
    /// Absolute byte offset at which the next song to be saved begins.
    pub next_start: u64,
    /// ICY title of the song currently being recorded. Drives the **live entry**
    /// at the top of the recordings list. `None` until a title is firmly
    /// assigned (a committed marker) – then the entry shows "Current recording".
    pub current_title: Option<String>,
    /// Title we already kicked off an online cover lookup for (dedupes the
    /// per-tick fetch so a not-found title is not searched again and again).
    pub cover_fetch_for: Option<String>,
}

/// Storage folder for saved recordings: `<Music>/Streaming`.
pub(crate) fn recordings_dir() -> std::path::PathBuf {
    let mut dir = dirs::audio_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    dir.push("Streaming");
    dir
}

impl App {
    /// Starts a saved station (replaces the current playback).
    /// Live stream → no resume, no duration.
    pub(crate) fn play_stream(&mut self, id: i64) {
        let Some(st) = self.stream_item(id) else {
            return;
        };
        // Save the position/session of a previously playing track.
        self.save_resume();
        self.save_episode_progress();
        self.finalize_play_session(false);
        match self.player.play_uri(&st.url, 0) {
            Ok(()) => {
                self.mini.now_playing = Some(st.name.clone());
                self.mini.current_album = None; // station — no album page
                self.mini.playing = true;
                self.transport.playing_path = None;
                self.podcasts.playing_episode_url = None;
                self.streaming.playing_stream = Some(id);
                self.youtube.playing_video_id = None;
                self.files.playing_remote = false;
                self.streaming.stream_title = None;
                self.transport.queue.clear();
                self.transport.queue_pos = 0;
                self.mini.position_ms = 0;
                self.mini.track_duration_ms = 0;
                *self.transport.close_resume.borrow_mut() = None;
                self.mpris.set_metadata(0, &st.name, None, None, None, None);
                self.mpris.set_playing(true);
                self.refresh_queue_icons();
                self.set_chapters(Vec::new());
                // End any previous recording …
                self.streaming.record_state = None;
                // … and start the timeshift buffer for this station (provided
                // the buffer is enabled). Dropping the old recorder cleans up.
                self.streaming.recorder = if self.streaming.recording_buffer_minutes > 0 {
                    Some(crate::core::recorder::Recorder::start(
                        &st.url,
                        self.streaming.recording_buffer_minutes,
                        Some(&st.name),
                    ))
                } else {
                    None
                };
                // Apply the per-station equalizer (station → global) on the
                // current output, like a normal track start does.
                self.settings.active_output =
                    crate::core::output::default_output().unwrap_or_default();
                self.apply_current_eq();
            }
            Err(e) => tracing::error!("Failed to play stream: {e}"),
        }
    }

    /// Stops the timeshift buffer and running recording (on stop/switch to music).
    ///
    /// If a continuous recording is active, the song still in progress is
    /// **finalized** (saved) and the live "Current recording" entry cleared
    /// before the buffer is torn down — otherwise switching away (tapping
    /// another recording, starting a track/podcast/YouTube/Nextcloud item, or
    /// deleting the running station) would silently drop the in-progress song
    /// and leave a stale live row in the list. Mirrors the explicit
    /// `Msg::RecordStop` path.
    pub(crate) fn stop_recorder(&mut self) {
        if self.streaming.record_state.is_some() {
            self.finalize_recording();
            self.streaming.record_state = None;
            self.sync_live_recording();
        }
        self.streaming.recorder = None;
    }

    /// Pushes the current playback state to the StreamPage component so it can
    /// refresh the station + recording row play/pause icons. Replaces the former
    /// `refresh_stream_icons`/`refresh_recording_icons` (now in the component).
    pub(crate) fn sync_stream_page_icons(&self) {
        let state = (
            self.streaming.playing_stream,
            self.transport
                .playing_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            self.mini.playing,
        );
        // Called on every tick while playing; only push to the StreamPage (which
        // walks all station/recording rows) when the playback state actually
        // changed, so an unchanged tick costs nothing.
        if self.last_stream_icon_state.borrow().as_ref() == Some(&state) {
            return;
        }
        *self.last_stream_icon_state.borrow_mut() = Some(state.clone());
        self.stream_page
            .emit(crate::ui::stream_page::StreamInput::PlaybackStateChanged {
                playing_stream: state.0,
                playing_path: state.1,
                playing: state.2,
            });
    }

    /// Pushes the running-recording snapshot to the StreamPage component (drives
    /// the live entry at the top of the recordings list); the component then
    /// rebuilds the recordings list. Replaces the former direct `reload_recordings`.
    pub(crate) fn sync_live_recording(&self) {
        let snap = self
            .streaming
            .record_state
            .as_ref()
            .map(|r| (r.stream_id, r.current_title.clone()));
        self.stream_page
            .emit(crate::ui::stream_page::StreamInput::SetLiveRecording(snap));
    }

    /// Looks up a saved station by id (the station list now lives in the
    /// StreamPage component, so the transport reads it straight from the DB).
    fn stream_item(&self, id: i64) -> Option<StreamItem> {
        self.library
            .streams()
            .unwrap_or_default()
            .into_iter()
            .find(|s| s.id == id)
    }

    /// Name of a saved station by id (for the now-playing label / recordings).
    fn stream_name(&self, id: i64) -> Option<String> {
        self.stream_item(id).map(|s| s.name)
    }

    /// Arms the continuous recording: start offset = beginning of the current song.
    pub(crate) fn record_arm(&mut self, id: i64) {
        let Some(rec) = self.streaming.recorder.as_ref() else {
            return;
        };
        let snap = rec.snapshot();
        let next_start = snap.current_start.unwrap_or(0);
        // Title of the song currently running (firmly assigned), if any.
        let current_title = snap
            .songs
            .last()
            .filter(|s| s.end.is_none())
            .map(|s| s.title.clone());
        self.streaming.record_state = Some(RecordState {
            stream_id: id,
            next_start,
            current_title,
            cover_fetch_for: None,
        });
        self.toast(&gettext("Recording …"));
        // Show the live entry ("Current recording" / the song title) immediately …
        self.sync_live_recording();
        // … and fetch its cover from the online DB in the background.
        self.maybe_fetch_live_cover();
    }

    /// Kicks off – once per title – an online cover lookup for the song
    /// currently being recorded, so the live entry at the top of the recordings
    /// list gets a cover. Reuses the cache; on success the list is reloaded.
    fn maybe_fetch_live_cover(&mut self) {
        let (raw, stream_id) = match self.streaming.record_state.as_mut() {
            Some(rs) => {
                let Some(raw) = rs.current_title.clone() else {
                    return;
                };
                if rs.cover_fetch_for.as_deref() == Some(raw.as_str()) {
                    return;
                }
                rs.cover_fetch_for = Some(raw.clone());
                (raw, rs.stream_id)
            }
            None => return,
        };
        let station = self.stream_name(stream_id);
        // Already cached (under the best-guess key) → the pending reload shows it.
        if let Some((a, t)) =
            crate::core::online::recording_query_candidates(&raw, station.as_deref()).first()
        {
            if crate::core::online::recording_cover_path(a.as_deref().unwrap_or(""), t).is_some() {
                return;
            }
        }
        let input = self.input.clone();
        std::thread::spawn(move || {
            let _ = crate::core::online::recording_cover(&raw, station.as_deref());
            let _ = input.send(Msg::ReloadRecordings);
        });
    }

    /// Driven by the 1 s tick: saves songs of the running recording that have
    /// finished (at the song boundaries) and advances.
    pub(crate) fn drive_recording(&mut self) {
        let snap = match self.streaming.recorder.as_ref() {
            Some(r) => r.snapshot(),
            None => return,
        };
        let (stream_id, mut next_start) = match self.streaming.record_state.as_ref() {
            Some(rs) => (rs.stream_id, rs.next_start),
            None => return,
        };
        if snap.ended {
            // The stream ended: finalize the song still in progress so it isn't
            // lost, then end the recording.
            self.finalize_recording();
            self.streaming.record_state = None;
            self.toast(&gettext("Recording stopped (stream ended)"));
            self.sync_live_recording();
            return;
        }
        let station = self.stream_name(stream_id);

        // Collect finished segments (read-only data; no self-mutation): start,
        // end, the raw ICY title, and whether the beginning was missing.
        // (start, end, raw title, incomplete, lead_pad, tail_pad)
        let mut segs: Vec<(u64, u64, String, bool, u64, u64)> = Vec::new();
        loop {
            // The song that contains `next_start` …
            let song = match snap
                .songs
                .iter()
                .find(|s| s.start <= next_start && s.end.is_none_or(|e| e > next_start))
            {
                Some(s) => s,
                // … otherwise advance to the next known song start (skipping an
                // untracked beginning, e.g. after a fresh start).
                None => match snap.songs.iter().find(|s| s.start > next_start) {
                    Some(first) => {
                        next_start = first.start;
                        first
                    }
                    None => break,
                },
            };
            let Some(end) = song.end else {
                break; // still running
            };
            let incomplete = !song.complete || next_start > song.start;
            segs.push((
                next_start,
                end,
                song.title.clone(),
                incomplete,
                song.lead_pad,
                song.tail_pad,
            ));
            next_start = end;
        }

        let mut saved = 0;
        for (start, end, raw_title, incomplete, lead_pad, tail_pad) in &segs {
            let ok = if raw_title.trim().is_empty() {
                // Untitled gap (talk/ads between songs): save to its own file, but
                // without any song recognition/enrichment.
                self.store_plain_segment(*start, *end, station.as_deref(), *incomplete)
            } else {
                self.store_segment(
                    *start,
                    *end,
                    *lead_pad,
                    *tail_pad,
                    raw_title,
                    station.as_deref(),
                    *incomplete,
                )
            };
            if ok {
                saved += 1;
            }
        }

        // Title of the song now running (firmly assigned). When it changes, the
        // live entry at the top of the list must follow it.
        let live_title = snap
            .songs
            .last()
            .filter(|s| s.end.is_none())
            .map(|s| s.title.clone());
        let title_changed = self
            .streaming
            .record_state
            .as_ref()
            .is_some_and(|rs| rs.current_title != live_title);
        if let Some(rs) = self.streaming.record_state.as_mut() {
            rs.next_start = next_start;
            if title_changed {
                rs.current_title = live_title;
            }
        }
        if saved > 0 || title_changed {
            self.sync_live_recording();
        }
        if title_changed {
            self.maybe_fetch_live_cover();
        }
    }

    /// Cuts `[start, end)` out of the buffer, stores it as a recording (with the
    /// station-aware best guess for artist/title), and queues an online lookup
    /// that embeds clean tags + cover and refreshes the list. Returns `true` on
    /// success. Shared by the continuous recording, the stop-finalize and the
    /// replay "save".
    pub(crate) fn store_segment(
        &mut self,
        start: u64,
        end: u64,
        lead_pad: u64,
        tail_pad: u64,
        raw_title: &str,
        station: Option<&str>,
        incomplete: bool,
    ) -> bool {
        // Best guess for storage/display = the first search candidate (station
        // name stripped); falls back to the raw title.
        let (artist, title) = crate::core::online::recording_query_candidates(raw_title, station)
            .into_iter()
            .next()
            .filter(|(_, t)| !t.trim().is_empty())
            .unwrap_or((None, gettext("Recording")));
        let Some(rec) = self.streaming.recorder.as_ref() else {
            return false;
        };
        let dest = recordings_dir();
        let path = match rec.save_song(
            start,
            end,
            lead_pad,
            tail_pad,
            artist.as_deref(),
            &title,
            &dest,
        ) {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!("Could not save recording: {e}");
                return false;
            }
        };
        match self.library.add_recording(
            &path.to_string_lossy(),
            artist.as_deref(),
            &title,
            station,
            incomplete,
        ) {
            // Reused an existing recording (same song detected again): drop the
            // redundant new file, or – when it upgraded an incomplete copy –
            // delete the superseded old file instead. Either way the existing
            // entry already carries cover/metadata, so skip the online lookup.
            Ok((_, false, superseded)) => {
                let stale = superseded.unwrap_or_else(|| path.to_string_lossy().into_owned());
                let _ = std::fs::remove_file(&stale);
                return true;
            }
            Ok((_, true, _)) => {}
            Err(e) => {
                tracing::warn!("Could not store recording: {e}");
                let _ = std::fs::remove_file(&path);
                return false;
            }
        }
        // Look up cover + album online (trying several combinations) and embed
        // them into the file (best effort); refresh the list once cached.
        let (raw, st) = (raw_title.to_string(), station.map(str::to_string));
        let input = self.input.clone();
        std::thread::spawn(move || {
            if let Some((bytes, album)) = crate::core::online::recording_cover(&raw, st.as_deref())
            {
                crate::core::recorder::embed_cover(
                    &path,
                    artist.as_deref(),
                    &title,
                    album.as_deref(),
                    &bytes,
                );
                let _ = input.send(Msg::ReloadRecordings);
            }
        });
        true
    }

    /// Saves a non-song segment (the talk/ads gap between songs) to its own file
    /// with a neutral label and **no** song recognition or online lookup. Returns
    /// `true` on success.
    pub(crate) fn store_plain_segment(
        &mut self,
        start: u64,
        end: u64,
        station: Option<&str>,
        incomplete: bool,
    ) -> bool {
        let title = gettext("Talk");
        let Some(rec) = self.streaming.recorder.as_ref() else {
            return false;
        };
        let path = match rec.save_song(start, end, 0, 0, None, &title, &recordings_dir()) {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!("Could not save gap recording: {e}");
                return false;
            }
        };
        match self
            .library
            .add_plain_recording(&path.to_string_lossy(), &title, station, incomplete)
        {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!("Could not store gap recording: {e}");
                let _ = std::fs::remove_file(&path);
                false
            }
        }
    }

    /// On stop: saves the song currently being recorded (from `next_start` up to
    /// the current buffer end) so the in-progress song is not lost. Best effort;
    /// does nothing if nothing has been buffered/identified yet.
    pub(crate) fn finalize_recording(&mut self) {
        let Some(rs) = self.streaming.record_state.as_ref() else {
            return;
        };
        let (stream_id, next_start) = (rs.stream_id, rs.next_start);
        let live_title = rs.current_title.clone();
        let Some(rec) = self.streaming.recorder.as_ref() else {
            return;
        };
        let snap = rec.snapshot();
        let station = self.stream_name(stream_id);
        match plan_finalize(&snap, next_start, live_title.as_deref()) {
            // A real song still running → save with recognition.
            FinalizePlan::Song {
                end,
                lead_pad,
                raw_title,
                incomplete,
            } => {
                self.store_segment(
                    next_start,
                    end,
                    lead_pad,
                    0,
                    &raw_title,
                    station.as_deref(),
                    incomplete,
                );
            }
            // A trailing untitled gap (talk/ads) → save plainly.
            FinalizePlan::Plain { end, incomplete } => {
                self.store_plain_segment(next_start, end, station.as_deref(), incomplete);
            }
            // Nothing buffered/identified beyond `next_start` → save nothing.
            FinalizePlan::Nothing => {}
        }
    }

    /// Replay subpage of a station: the songs detected in the buffer for
    /// previewing or saving after the fact. Reachable from the detail page.
    pub(crate) fn open_stream_replay(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some(st) = self.stream_item(id) else {
            return;
        };
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        let snap = self.streaming.recorder.as_ref().map(|r| r.snapshot());
        // Only finished songs (with a known end), newest first.
        let mut songs: Vec<crate::core::recorder::BufferedSong> = snap
            .map(|s| s.songs)
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.end.is_some())
            .collect();
        songs.reverse();

        let group = adw::PreferencesGroup::builder()
            .title(gettext("Recently detected"))
            .build();
        if songs.is_empty() {
            group.add(
                &adw::ActionRow::builder()
                    .title(gettext("Nothing buffered yet"))
                    .build(),
            );
        }
        for song in songs {
            let end = song.end.unwrap_or(song.start);
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&song.title))
                .build();
            if !song.complete {
                row.set_subtitle(&gettext("Beginning missing"));
            }
            let save = gtk::Button::builder()
                .icon_name("document-save-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(gettext("Save"))
                .build();
            save.add_css_class("flat");
            {
                let sender = sender.clone();
                let (start, e, title) = (song.start, end, song.title.clone());
                save.connect_clicked(move |_| {
                    sender.input(Msg::ReplaySave {
                        start,
                        end: e,
                        title: title.clone(),
                    });
                });
            }
            let play = gtk::Button::builder()
                .icon_name("media-playback-start-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(gettext("Play"))
                .build();
            play.add_css_class("flat");
            {
                let sender = sender.clone();
                let (start, e) = (song.start, end);
                play.connect_clicked(move |_| {
                    sender.input(Msg::ReplayPlay { start, end: e });
                });
            }
            row.add_suffix(&play);
            row.add_suffix(&save);
            group.add(&row);
        }
        content.append(&group);
        self.push_subpage(
            &gettext_f("Replay – {name}", &[("name", &st.name)]),
            &content,
        );
    }
    /// Toggle pause/resume on the running station, or start this one.
    pub(crate) fn toggle_stream(&mut self, id: i64) {
        if self.streaming.playing_stream == Some(id) {
            // Already running → toggle pause/resume (buffer keeps running).
            if self.mini.playing {
                self.player.pause();
                self.mini.playing = false;
            } else {
                self.player.resume();
                self.mini.playing = true;
            }
            self.mpris.set_playing(self.mini.playing);
        } else {
            self.play_stream(id);
        }
        self.sync_stream_page_icons();
    }

    /// Start/stop the timeshift recording for a station.
    pub(crate) fn stream_record_toggle(&mut self, sender: &ComponentSender<Self>, id: i64) {
        if self.streaming.record_state.as_ref().map(|r| r.stream_id) == Some(id) {
            // Running → stop.
            sender.input(Msg::RecordStop);
        } else if self.streaming.recording_buffer_minutes == 0 {
            self.toast(&gettext(
                "Enable the recording buffer in the settings first",
            ));
        } else {
            // Ensure the station (with buffer), then start the continuous recording.
            if self.streaming.playing_stream != Some(id) {
                self.play_stream(id);
            }
            self.record_arm(id);
            // Jump to the recordings view so the new captures are visible as they
            // are saved at the song boundaries.
            self.stream_page
                .emit(crate::ui::stream_page::StreamInput::SetView(
                    crate::ui::app::StreamView::Recordings,
                ));
            self.sync_stream_page_icons();
        }
    }

    /// ICY `StreamTitle` update while a station is running → mini player + MPRIS,
    /// and (for real songs) the "Recently heard" history.
    pub(crate) fn stream_title(&mut self, title: String) {
        let title = title.trim().to_string();
        if let Some(id) = self.streaming.playing_stream {
            if !title.is_empty() && self.streaming.stream_title.as_deref() != Some(title.as_str()) {
                self.streaming.stream_title = Some(title.clone());
                let station = self.stream_name(id);
                self.mini.now_playing = Some(match &station {
                    Some(name) => format!("{name} — {title}"),
                    None => title.clone(),
                });
                self.mpris
                    .set_metadata(0, &title, station.as_deref(), None, None, None);
                self.note_heard_song(&title, station.as_deref());
            }
        }
    }

    /// Logs an ICY title to the "Recently heard" history — but only when it
    /// parses into an `Artist – Title` song, which filters out station
    /// idents/jingles (those lack that structure). Refreshes the page and, once
    /// per new song, fetches a cover in the background (cached by artist+title,
    /// shared with the recordings cover cache).
    fn note_heard_song(&self, raw_title: &str, station: Option<&str>) {
        let Some((artist, title)) =
            crate::core::online::recording_query_candidates(raw_title, station)
                .into_iter()
                .find(|(a, t)| a.is_some() && !t.trim().is_empty())
        else {
            return;
        };
        if self
            .library
            .note_heard(artist.as_deref(), &title, station)
            .is_err()
        {
            return;
        }
        self.stream_page
            .emit(crate::ui::stream_page::StreamInput::ReloadHeard);
        // Already have a cover (e.g. the song was recorded earlier) → done.
        if crate::core::online::recording_cover_path(artist.as_deref().unwrap_or(""), &title)
            .is_some()
        {
            return;
        }
        let (raw, st) = (raw_title.to_string(), station.map(str::to_string));
        let input = self.input.clone();
        std::thread::spawn(move || {
            if crate::core::online::recording_cover(&raw, st.as_deref()).is_some() {
                let _ = input.send(Msg::ReloadHeard);
            }
        });
    }

    /// Remove a station (stopping it first if it is the running one).
    pub(crate) fn stream_delete_confirmed(&mut self, id: i64) {
        if self.streaming.playing_stream == Some(id) {
            self.player.stop();
            self.mini.playing = false;
            self.streaming.playing_stream = None;
            self.mini.now_playing = None;
            self.mpris.set_playing(false);
            self.stop_recorder();
        }
        let _ = self.library.delete_stream(id);
        self.stream_page
            .emit(crate::ui::stream_page::StreamInput::Reload);
    }

    /// Play a buffer segment `[start, end)` as a temporary "replay".
    pub(crate) fn replay_play(&mut self, start: u64, end: u64) {
        let temp = self
            .streaming
            .recorder
            .as_ref()
            .and_then(|r| r.extract_temp(start, end).ok());
        match temp {
            Some(path) => {
                let p = path.to_string_lossy().to_string();
                self.player.stop();
                match self.player.play_file(&p, 0) {
                    Ok(()) => {
                        self.mini.now_playing = Some(gettext("Replay"));
                        self.mini.current_album = None; // replay clip — no album page
                        self.mini.playing = true;
                        self.transport.playing_path = Some(path);
                        self.podcasts.playing_episode_url = None;
                        self.streaming.playing_stream = None;
                        self.mpris.set_playing(true);
                    }
                    Err(e) => tracing::error!("Replay failed: {e}"),
                }
            }
            None => self.toast(&gettext("Could not extract from buffer")),
        }
    }

    /// Save a buffer segment `[start, end)` as a recording.
    pub(crate) fn replay_save(&mut self, start: u64, end: u64, title: String) {
        let station = self
            .streaming
            .playing_stream
            .and_then(|id| self.stream_name(id));
        // Replay "save" is an exact, user-picked range → no generous guard.
        if self.store_segment(start, end, 0, 0, &title, station.as_deref(), false) {
            self.sync_live_recording();
        } else {
            self.toast(&gettext("Could not extract from buffer"));
        }
    }

    /// Play a saved recording file.
    pub(crate) fn play_recording(&mut self, path: String) {
        let p = std::path::PathBuf::from(&path);
        // Re-tapping the recording that is already playing toggles
        // pause/resume instead of restarting it.
        if self.toggle_if_active_file(&p) {
            return;
        }
        if p.exists() {
            self.stop_recorder();
            self.transport.queue = vec![p];
            self.transport.queue_pos = 0;
            self.play_current();
        } else {
            self.toast(&gettext("File not found"));
        }
    }

    /// Play a recognized song from the "Recently heard" list. Prefers a locally
    /// saved variant — a timeshift recording of the same song, then a matching
    /// library track — and only streams it via YouTube if neither exists.
    pub(crate) fn play_heard(
        &mut self,
        sender: &ComponentSender<Self>,
        root: &adw::ApplicationWindow,
        artist: Option<String>,
        title: String,
    ) {
        // 1) A saved timeshift recording of this song.
        // 2) Otherwise a matching library track (real local file only — a
        //    cloud/`nc:` path won't pass the existence check and falls through).
        for found in [
            self.library.find_recording(artist.as_deref(), &title),
            self.library.find_track(artist.as_deref(), &title),
        ] {
            if let Ok(Some(path)) = found {
                if std::path::Path::new(&path).exists() {
                    self.play_recording(path);
                    return;
                }
            }
        }
        // 3) Nothing saved → resolve and stream it via YouTube.
        self.resolve_heard_youtube(sender, root, artist, title, false);
    }

    /// Download a recognized song via YouTube into the music library.
    pub(crate) fn download_heard(
        &mut self,
        sender: &ComponentSender<Self>,
        root: &adw::ApplicationWindow,
        artist: Option<String>,
        title: String,
    ) {
        self.resolve_heard_youtube(sender, root, artist, title, true);
    }

    /// Searches YouTube for `artist title` in the background; the first hit comes
    /// back as [`Cmd::HeardResolved`] and is then played or imported.
    fn resolve_heard_youtube(
        &mut self,
        sender: &ComponentSender<Self>,
        root: &adw::ApplicationWindow,
        artist: Option<String>,
        title: String,
        download: bool,
    ) {
        if !self.youtube.enabled || !crate::core::youtube::available() {
            self.toast(&gettext("Enable YouTube in the settings to use this"));
            return;
        }
        let query = match artist.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(a) => format!("{a} {title}"),
            None => title.clone(),
        };
        // No local copy → show a spinner while the online lookup runs; it is
        // closed again in `on_heard_resolved` once the search returns.
        self.show_resolve_busy(root, &gettext("Looking online …"));
        sender.spawn_command(move |out| {
            let video_id =
                crate::core::youtube::search(&query, crate::core::youtube::YtKind::Video, 1)
                    .ok()
                    .and_then(|mut v| v.drain(..).next())
                    .map(|r| r.id);
            let _ = out.send(crate::ui::app::Cmd::HeardResolved {
                video_id,
                title,
                download,
            });
        });
    }

    /// A "Recently heard" song was resolved to a YouTube video → play it or hand
    /// it to the YouTube library import.
    pub(crate) fn on_heard_resolved(
        &mut self,
        video_id: Option<String>,
        title: String,
        download: bool,
    ) {
        // The online lookup returned → take down the spinner.
        self.close_resolve_busy();
        let Some(video_id) = video_id else {
            self.toast(&gettext("Not found on YouTube"));
            return;
        };
        if download {
            self.yt_page
                .emit(crate::ui::yt_page::YtInput::AddToLibrary { video_id, title });
        } else {
            self.yt_play_video(video_id, title);
        }
    }

    /// Show a small modal spinner with `text` while a "Recently heard" song is
    /// being resolved online. Any previous one is closed first; the dialog is
    /// taken down in [`Self::close_resolve_busy`] when the lookup returns.
    fn show_resolve_busy(&mut self, root: &adw::ApplicationWindow, text: &str) {
        if let Some(d) = self.streaming.resolve_busy.take() {
            d.close();
        }
        let dialog = adw::Dialog::builder().content_width(280).build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .margin_top(28)
            .margin_bottom(28)
            .margin_start(28)
            .margin_end(28)
            .halign(gtk::Align::Center)
            .build();
        let spinner = gtk::Spinner::builder()
            .width_request(32)
            .height_request(32)
            .build();
        spinner.set_spinning(true);
        let label = gtk::Label::builder()
            .label(text)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        content.append(&spinner);
        content.append(&label);
        dialog.set_child(Some(&content));
        dialog.present(Some(root));
        self.streaming.resolve_busy = Some(dialog);
    }

    /// Close the online-resolution spinner (if showing).
    fn close_resolve_busy(&mut self) {
        if let Some(d) = self.streaming.resolve_busy.take() {
            d.close();
        }
    }
}

/// What `finalize_recording` should persist for the song still in progress when
/// a continuous recording is stopped (manually, on switch-away, or because the
/// stream ended). All start offsets are `next_start`; the plan only varies the
/// end/title/guard/completeness. Kept as a pure decision over the buffer
/// `Snapshot` so it can be unit-tested without the GStreamer/DB/UI stack.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FinalizePlan {
    /// Nothing buffered/identified beyond `next_start` → save nothing.
    Nothing,
    /// A real, titled song is still running → save with recognition.
    Song {
        end: u64,
        lead_pad: u64,
        raw_title: String,
        incomplete: bool,
    },
    /// A trailing untitled gap (talk/ads) → save plainly, no recognition.
    Plain { end: u64, incomplete: bool },
}

/// Decide what the in-progress song should become on stop. The live title (the
/// firmly-assigned ICY title tracked by `RecordState`) wins over the snapshot's
/// running-song title; absent both, an unidentified blob is not saved.
pub(crate) fn plan_finalize(
    snap: &crate::core::recorder::Snapshot,
    next_start: u64,
    live_title: Option<&str>,
) -> FinalizePlan {
    let end = snap.total;
    // The live buffer edge hasn't advanced past where we'd resume saving → there
    // is nothing new to write.
    if end <= next_start {
        return FinalizePlan::Nothing;
    }
    // The song that is still running (the one containing `next_start`).
    let song = snap
        .songs
        .iter()
        .find(|s| s.start <= next_start && s.end.is_none());
    let raw_title = live_title
        .map(str::to_string)
        .or_else(|| song.map(|s| s.title.clone()));
    // Incomplete if the start boundary was lost from the buffer, or we resume
    // mid-song (past the song's start), or there is no tracked song at all.
    let incomplete = song.is_none_or(|s| !s.complete || next_start > s.start);
    // Lead guard from the song's start boundary; no tail guard — `end` is the
    // live buffer edge, there is nothing buffered beyond it.
    let lead_pad = song.map_or(0, |s| s.lead_pad);
    match raw_title {
        Some(t) if !t.trim().is_empty() => FinalizePlan::Song {
            end,
            lead_pad,
            raw_title: t,
            incomplete,
        },
        Some(_) => FinalizePlan::Plain { end, incomplete },
        None => FinalizePlan::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::{plan_finalize, FinalizePlan};
    use crate::core::recorder::{BufferedSong, Snapshot};

    fn running_song(start: u64, title: &str, complete: bool, lead_pad: u64) -> BufferedSong {
        BufferedSong {
            start,
            end: None,
            title: title.to_string(),
            complete,
            lead_pad,
            tail_pad: 0,
        }
    }

    fn snap(total: u64, songs: Vec<BufferedSong>) -> Snapshot {
        Snapshot {
            current_start: songs.last().map(|s| s.start),
            songs,
            total,
            ended: false,
        }
    }

    #[test]
    fn nothing_when_buffer_has_not_advanced() {
        // end (total) == next_start → no new bytes to save.
        let s = snap(1000, vec![running_song(0, "A - B", true, 0)]);
        assert_eq!(plan_finalize(&s, 1000, None), FinalizePlan::Nothing);
        // total < next_start is likewise nothing.
        assert_eq!(plan_finalize(&s, 1500, None), FinalizePlan::Nothing);
    }

    #[test]
    fn nothing_when_unidentified() {
        // Data buffered but no running song and no live title → don't save a blob.
        let s = snap(2000, vec![]);
        assert_eq!(plan_finalize(&s, 500, None), FinalizePlan::Nothing);
    }

    #[test]
    fn complete_running_song_saved_with_recognition() {
        let s = snap(2000, vec![running_song(500, "Artist - Title", true, 64)]);
        assert_eq!(
            plan_finalize(&s, 500, None),
            FinalizePlan::Song {
                end: 2000,
                lead_pad: 64,
                raw_title: "Artist - Title".to_string(),
                incomplete: false,
            }
        );
    }

    #[test]
    fn resuming_mid_song_is_incomplete() {
        // next_start past the song's own start → the beginning is missing.
        let s = snap(2000, vec![running_song(500, "Artist - Title", true, 64)]);
        match plan_finalize(&s, 900, None) {
            FinalizePlan::Song { incomplete, .. } => assert!(incomplete),
            other => panic!("expected Song, got {other:?}"),
        }
    }

    #[test]
    fn lost_start_boundary_is_incomplete() {
        let s = snap(2000, vec![running_song(500, "Artist - Title", false, 0)]);
        match plan_finalize(&s, 500, None) {
            FinalizePlan::Song { incomplete, .. } => assert!(incomplete),
            other => panic!("expected Song, got {other:?}"),
        }
    }

    #[test]
    fn live_title_overrides_song_title() {
        let s = snap(2000, vec![running_song(500, "Stale ICY", true, 0)]);
        match plan_finalize(&s, 500, Some("Fresh Live Title")) {
            FinalizePlan::Song { raw_title, .. } => assert_eq!(raw_title, "Fresh Live Title"),
            other => panic!("expected Song, got {other:?}"),
        }
    }

    #[test]
    fn live_title_without_tracked_song_saves_incomplete_song() {
        // The recorder lost the song boundary but the live entry still knows the
        // title → save it (flagged incomplete, no lead guard).
        let s = snap(2000, vec![]);
        assert_eq!(
            plan_finalize(&s, 0, Some("Artist - Title")),
            FinalizePlan::Song {
                end: 2000,
                lead_pad: 0,
                raw_title: "Artist - Title".to_string(),
                incomplete: true,
            }
        );
    }

    #[test]
    fn empty_running_title_is_a_plain_gap() {
        // A running but untitled segment (talk/ads) → plain save, no recognition.
        let s = snap(2000, vec![running_song(500, "   ", true, 0)]);
        assert_eq!(
            plan_finalize(&s, 500, None),
            FinalizePlan::Plain {
                end: 2000,
                incomplete: false,
            }
        );
    }

    #[test]
    fn whitespace_live_title_is_a_plain_gap() {
        let s = snap(2000, vec![]);
        assert_eq!(
            plan_finalize(&s, 0, Some("   ")),
            FinalizePlan::Plain {
                end: 2000,
                incomplete: true,
            }
        );
    }
}
