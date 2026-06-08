//! Streaming (internet radio): station list, station detail page, and the
//! add dialog (manual stream URL **and** worldwide search via the
//! Radio-Browser API). Stations are streamed directly – nothing is downloaded.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, gettext_f};
use crate::model::StreamItem;
use crate::ui::app::{App, Cmd, Msg};


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
                    ))
                } else {
                    None
                };
            }
            Err(e) => tracing::error!("Failed to play stream: {e}"),
        }
    }

    /// Stops the timeshift buffer and running recording (on stop/switch to music).
    pub(crate) fn stop_recorder(&mut self) {
        self.streaming.recorder = None;
        self.streaming.record_state = None;
    }

    /// Pushes the current playback state to the StreamPage component so it can
    /// refresh the station + recording row play/pause icons. Replaces the former
    /// `refresh_stream_icons`/`refresh_recording_icons` (now in the component).
    pub(crate) fn sync_stream_page_icons(&self) {
        self.stream_page
            .emit(crate::ui::stream_page::StreamInput::PlaybackStateChanged {
                playing_stream: self.streaming.playing_stream,
                playing_path: self
                    .transport
                    .playing_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                playing: self.mini.playing,
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
    pub(crate) fn record_arm(&mut self, sender: &ComponentSender<Self>, id: i64) {
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
        self.maybe_fetch_live_cover(sender);
    }

    /// Kicks off – once per title – an online cover lookup for the song
    /// currently being recorded, so the live entry at the top of the recordings
    /// list gets a cover. Reuses the cache; on success the list is reloaded.
    fn maybe_fetch_live_cover(&mut self, sender: &ComponentSender<Self>) {
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
        sender.spawn_command(move |out| {
            let _ = crate::core::online::recording_cover(&raw, station.as_deref());
            let _ = out.send(Cmd::ReloadRecordings);
        });
    }

    /// Driven by the 1 s tick: saves songs of the running recording that have
    /// finished (at the song boundaries) and advances.
    pub(crate) fn drive_recording(&mut self, sender: &ComponentSender<Self>) {
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
            self.finalize_recording(sender);
            self.streaming.record_state = None;
            self.toast(&gettext("Recording stopped (stream ended)"));
            self.sync_live_recording();
            return;
        }
        let station = self.stream_name(stream_id);

        // Collect finished segments (read-only data; no self-mutation): start,
        // end, the raw ICY title, and whether the beginning was missing.
        let mut segs: Vec<(u64, u64, String, bool)> = Vec::new();
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
            segs.push((next_start, end, song.title.clone(), incomplete));
            next_start = end;
        }

        let mut saved = 0;
        for (start, end, raw_title, incomplete) in &segs {
            let ok = if raw_title.trim().is_empty() {
                // Untitled gap (talk/ads between songs): save to its own file, but
                // without any song recognition/enrichment.
                self.store_plain_segment(*start, *end, station.as_deref(), *incomplete)
            } else {
                self.store_segment(
                    sender,
                    *start,
                    *end,
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
            self.maybe_fetch_live_cover(sender);
        }
    }

    /// Cuts `[start, end)` out of the buffer, stores it as a recording (with the
    /// station-aware best guess for artist/title), and queues an online lookup
    /// that embeds clean tags + cover and refreshes the list. Returns `true` on
    /// success. Shared by the continuous recording, the stop-finalize and the
    /// replay "save".
    pub(crate) fn store_segment(
        &mut self,
        sender: &ComponentSender<Self>,
        start: u64,
        end: u64,
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
        let path = match rec.save_song(start, end, artist.as_deref(), &title, &dest) {
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
        sender.spawn_command(move |out| {
            if let Some((bytes, album)) = crate::core::online::recording_cover(&raw, st.as_deref())
            {
                crate::core::recorder::embed_cover(
                    &path,
                    artist.as_deref(),
                    &title,
                    album.as_deref(),
                    &bytes,
                );
                let _ = out.send(Cmd::ReloadRecordings);
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
        let path = match rec.save_song(start, end, None, &title, &recordings_dir()) {
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
    pub(crate) fn finalize_recording(&mut self, sender: &ComponentSender<Self>) {
        let Some(rs) = self.streaming.record_state.as_ref() else {
            return;
        };
        let (stream_id, next_start) = (rs.stream_id, rs.next_start);
        let live_title = rs.current_title.clone();
        let Some(rec) = self.streaming.recorder.as_ref() else {
            return;
        };
        let snap = rec.snapshot();
        let end = snap.total;
        if end <= next_start {
            return;
        }
        // The running song (for its title + completeness).
        let song = snap
            .songs
            .iter()
            .find(|s| s.start <= next_start && s.end.is_none());
        let raw_title = live_title.or_else(|| song.map(|s| s.title.clone()));
        let incomplete = song.is_none_or(|s| !s.complete || next_start > s.start);
        let station = self.stream_name(stream_id);
        match raw_title {
            // A real song still running → save with recognition.
            Some(t) if !t.trim().is_empty() => {
                self.store_segment(sender, next_start, end, &t, station.as_deref(), incomplete);
            }
            // A trailing untitled gap (talk/ads) → save plainly.
            Some(_) => {
                self.store_plain_segment(next_start, end, station.as_deref(), incomplete);
            }
            // Nothing identified yet → don't save an untitled blob.
            None => {}
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
            self.record_arm(sender, id);
            // Jump to the recordings view so the new captures are visible as they
            // are saved at the song boundaries.
            self.stream_page
                .emit(crate::ui::stream_page::StreamInput::SetView(
                    crate::ui::app::StreamView::Recordings,
                ));
            self.sync_stream_page_icons();
        }
    }

    /// ICY `StreamTitle` update while a station is running → mini player + MPRIS.
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
            }
        }
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
        self.stream_page.emit(crate::ui::stream_page::StreamInput::Reload);
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
    pub(crate) fn replay_save(
        &mut self,
        sender: &ComponentSender<Self>,
        start: u64,
        end: u64,
        title: String,
    ) {
        let station = self
            .streaming
            .playing_stream
            .and_then(|id| self.stream_name(id));
        if self.store_segment(sender, start, end, &title, station.as_deref(), false) {
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
}
