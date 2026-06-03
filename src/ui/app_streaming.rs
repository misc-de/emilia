//! Streaming (internet radio): station list, station detail page, and the
//! add dialog (manual stream URL **and** worldwide search via the
//! Radio-Browser API). Stations are streamed directly – nothing is downloaded.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, gettext_f};
use crate::model::StreamItem;
use crate::ui::app::{App, Msg};

/// Placeholder icon when a station has no logo.
const STREAM_ICON: &str = "audio-x-generic-symbolic";

/// State of a running continuous recording (state machine, driven by the 1 s
/// tick; saves complete songs until manually stopped).
pub(crate) struct RecordState {
    pub stream_id: i64,
    /// Absolute byte offset at which the next song to be saved begins.
    pub next_start: u64,
}

/// Formats Unix seconds as "DD.MM.YYYY" (civil date, approximated to UTC).
fn format_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{d:02}.{m:02}.{y}")
}

/// Storage folder for saved recordings: `<Music>/Emilia-Aufnahmen`.
pub(crate) fn recordings_dir() -> std::path::PathBuf {
    let mut dir = dirs::audio_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    dir.push("Emilia-Aufnahmen");
    dir
}

/// Content box for the dialogs (uniform margins).
fn detail_box() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build()
}

/// Activatable action row with an icon prefix (for the detail page).
fn action_row(title: &str, icon: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).activatable(true).build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row
}

/// Embeds the content scrollably in a dialog with a header bar and shows it.
fn present_dialog(dialog: &adw::Dialog, content: &gtk::Box, root: &adw::ApplicationWindow) {
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .vexpand(true)
        .child(content)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    dialog.set_child(Some(&toolbar));
    dialog.set_content_width(600);
    dialog.present(Some(root));
}

/// Subtitle of a station: genre/country, as far as available.
fn stream_subtitle(st: &StreamItem) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = st.tags.as_deref().filter(|s| !s.trim().is_empty()) {
        // Show only the first few tags (comma-separated → "·").
        let tags: Vec<&str> = t.split(',').map(str::trim).filter(|s| !s.is_empty()).take(3).collect();
        if !tags.is_empty() {
            parts.push(tags.join(" · "));
        }
    }
    if let Some(c) = st.country.as_deref().filter(|s| !s.trim().is_empty()) {
        parts.push(c.to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" — "))
    }
}

impl App {
    /// Rebuilds the station list: logo, name, genre/country. **Tapping** plays
    /// the station, **long press** opens the detail page (favorite/remove).
    pub(crate) fn reload_streams(&mut self, sender: &ComponentSender<Self>) {
        self.stream_items = self.library.streams().unwrap_or_default();
        self.stream_play_buttons.borrow_mut().clear();
        while let Some(child) = self.streams_list.first_child() {
            self.streams_list.remove(&child);
        }
        for st in self.stream_items.clone() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&st.name))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            if let Some(sub) = stream_subtitle(&st) {
                row.set_subtitle(&gtk::glib::markup_escape_text(&sub));
            }
            let logo = st
                .favicon
                .as_deref()
                .and_then(crate::core::online::station_image_path);
            row.add_prefix(&crate::ui::app::cover_widget(logo.as_deref(), STREAM_ICON));
            let id = st.id;

            // Play/Pause button (status icon, right-aligned).
            let pp = gtk::Button::builder()
                .icon_name("media-playback-start-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(&gettext("Play/Pause"))
                .build();
            pp.add_css_class("flat");
            {
                let sender = sender.clone();
                pp.connect_clicked(move |_| sender.input(Msg::ToggleStream(id)));
            }
            self.stream_play_buttons.borrow_mut().push((id, pp.clone()));
            row.add_suffix(&pp);

            // Tapping the row = toggle Play/Pause.
            {
                let sender = sender.clone();
                row.connect_activated(move |_| sender.input(Msg::ToggleStream(id)));
            }
            // Long press → detail view (dialog).
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::OpenStream(id));
                });
            }
            row.add_controller(lp);
            self.streams_list.append(&row);
        }
    }

    /// Detail view of a station as a **dialog** (no subpage push): logo +
    /// genre/country as well as actions to play/stop, record, replay,
    /// favorite, and remove. Each action closes the dialog.
    pub(crate) fn open_stream(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        id: i64,
    ) {
        let Some(st) = self.stream_items.iter().find(|s| s.id == id).cloned() else {
            return;
        };
        let dialog = adw::Dialog::builder()
            .title(gtk::glib::markup_escape_text(&st.name))
            .build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let info = adw::PreferencesGroup::new();
        let head = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&st.name))
            .build();
        if let Some(sub) = stream_subtitle(&st) {
            head.set_subtitle(&gtk::glib::markup_escape_text(&sub));
        }
        let logo = st
            .favicon
            .as_deref()
            .and_then(crate::core::online::station_image_path);
        head.add_prefix(&crate::ui::app::cover_widget(logo.as_deref(), STREAM_ICON));
        info.add(&head);
        content.append(&info);

        // Small helper: action row that sends a message and closes.
        let row_action = |title: &str, icon: &str, msg: Msg| {
            let row = action_row(title, icon);
            let (sender, dialog) = (sender.clone(), dialog.clone());
            let msg = std::cell::RefCell::new(Some(msg));
            row.connect_activated(move |_| {
                if let Some(m) = msg.borrow_mut().take() {
                    sender.input(m);
                }
                dialog.close();
            });
            row
        };

        // Playback/recording run via the station list or the player bar;
        // here only replay (buffer) and remove.
        let actions = adw::PreferencesGroup::new();
        if self.recording_buffer_minutes > 5 {
            actions.add(&row_action(
                &gettext("Replay (buffer)"),
                "media-seek-backward-symbolic",
                Msg::OpenStreamReplay(id),
            ));
        }
        let remove = action_row(&gettext("Remove station"), "user-trash-symbolic");
        {
            let sender = sender.clone();
            let (overlay, dialog) = (self.toast_overlay.clone(), dialog.clone());
            remove.connect_activated(move |_| {
                dialog.close();
                crate::ui::app::confirm_destructive(
                    &overlay,
                    &gettext("Remove this station?"),
                    &gettext("Remove"),
                    sender.clone(),
                    Msg::StreamDelete(id),
                );
            });
        }
        actions.add(&remove);
        content.append(&actions);

        present_dialog(&dialog, &content, root);
    }

    /// Refreshes the Play/Pause icons of the station rows to the current
    /// playback state (called from the tick and after state changes). The
    /// record button sits in the player bar and updates itself via `#[watch]`.
    pub(crate) fn refresh_stream_icons(&self) {
        let playing = self.playing;
        let cur = self.playing_stream;
        let mut btns = self.stream_play_buttons.borrow_mut();
        btns.retain(|(_, b)| b.root().is_some());
        for (id, btn) in btns.iter() {
            let active = cur == Some(*id) && playing;
            btn.set_icon_name(if active {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            });
        }
    }

    /// Dialog for adding a station: at the top a **worldwide search**
    /// (Radio-Browser, tappable results), below it a field for a
    /// **stream address** as the manual route.
    pub(crate) fn open_add_stream_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::Dialog::builder().title(&gettext("Add station")).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        // --- Worldwide search (Radio-Browser) ---
        let search_group = adw::PreferencesGroup::builder()
            .title(&gettext("Search"))
            .description(&gettext("Find a station worldwide by name"))
            .build();
        let search_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text(&gettext("Station name …"))
            .hexpand(true)
            .build();
        let search_btn = gtk::Button::builder().label(&gettext("Search")).build();
        search_btn.add_css_class("suggested-action");
        search_row.append(&search_entry);
        search_row.append(&search_btn);
        search_group.add(&search_row);
        content.append(&search_group);

        {
            let (sender, entry) = (sender.clone(), search_entry.clone());
            search_entry.connect_activate(move |_| {
                let term = entry.text().to_string();
                if !term.trim().is_empty() {
                    sender.input(Msg::StreamSearch(term));
                }
            });
        }
        {
            let (sender, entry) = (sender.clone(), search_entry.clone());
            search_btn.connect_clicked(move |_| {
                let term = entry.text().to_string();
                if !term.trim().is_empty() {
                    sender.input(Msg::StreamSearch(term));
                }
            });
        }

        // Results list – initially hidden, filled asynchronously.
        let results = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        results.add_css_class("boxed-list");
        results.set_visible(false);
        content.append(&results);

        // --- Manual: stream address ---
        let url_group = adw::PreferencesGroup::builder()
            .title(&gettext("Or enter a stream address"))
            .build();
        let url_entry = adw::EntryRow::builder()
            .title(&gettext("Stream address (URL)"))
            .show_apply_button(true)
            .build();
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            url_entry.connect_apply(move |e| {
                let url = e.text().to_string();
                if !url.trim().is_empty() {
                    sender.input(Msg::StreamAddUrl(url));
                    dialog.close();
                }
            });
        }
        url_group.add(&url_entry);
        content.append(&url_group);

        *self.stream_search.borrow_mut() = Some((dialog.clone(), results.clone()));
        {
            let slot = self.stream_search.clone();
            dialog.connect_closed(move |_| {
                *slot.borrow_mut() = None;
            });
        }

        present_dialog(&dialog, &content, root);
        search_entry.grab_focus();
    }

    /// Redraws the results list in the open add dialog (from
    /// `self.stream_search_results`). Tapping saves the station and closes the
    /// dialog. Logos come from the local cache (otherwise a placeholder).
    pub(crate) fn rebuild_stream_search_results(&self, sender: &ComponentSender<Self>) {
        let guard = self.stream_search.borrow();
        let Some((dialog, list)) = guard.as_ref() else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        list.set_visible(true);

        if self.stream_search_results.is_empty() {
            let row = adw::ActionRow::builder()
                .title(&gettext("No stations found"))
                .build();
            row.set_sensitive(false);
            list.append(&row);
            dialog.set_content_height(300);
            return;
        }

        let rows = self.stream_search_results.len() as i32;
        dialog.set_content_height((320 + rows * 66).min(760));

        for (i, r) in self.stream_search_results.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&r.name))
                .activatable(true)
                .build();
            let mut sub: Vec<String> = Vec::new();
            if let Some(c) = r.country.as_deref().filter(|s| !s.trim().is_empty()) {
                sub.push(c.to_string());
            }
            if let Some(t) = r.tags.as_deref().filter(|s| !s.trim().is_empty()) {
                let tags: Vec<&str> = t.split(',').map(str::trim).filter(|s| !s.is_empty()).take(2).collect();
                if !tags.is_empty() {
                    sub.push(tags.join(" · "));
                }
            }
            if !sub.is_empty() {
                row.set_subtitle(&gtk::glib::markup_escape_text(&sub.join(" — ")));
            }
            let logo = r
                .favicon
                .as_deref()
                .and_then(crate::core::online::station_image_path);
            row.add_prefix(&crate::ui::app::cover_widget(logo.as_deref(), STREAM_ICON));
            row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
            {
                let (sender, dialog) = (sender.clone(), dialog.clone());
                row.connect_activated(move |_| {
                    sender.input(Msg::StreamAddResult(i));
                    dialog.close();
                });
            }
            list.append(&row);
        }
    }

    /// Starts a saved station (replaces the current playback).
    /// Live stream → no resume, no duration.
    pub(crate) fn play_stream(&mut self, id: i64) {
        let Some(st) = self.stream_items.iter().find(|s| s.id == id).cloned() else {
            return;
        };
        // Save the position/session of a previously playing track.
        self.save_resume();
        self.save_episode_progress();
        self.finalize_play_session(false);
        match self.player.play_uri(&st.url, 0) {
            Ok(()) => {
                self.now_playing = Some(st.name.clone());
                self.playing = true;
                self.playing_path = None;
                self.podcasts.playing_episode_url = None;
                self.playing_stream = Some(id);
                self.playing_remote = false;
                self.stream_title = None;
                self.queue.clear();
                self.queue_pos = 0;
                self.position_ms = 0;
                self.track_duration_ms = 0;
                *self.close_resume.borrow_mut() = None;
                self.mpris.set_metadata(0, &st.name, None, None, None, None);
                self.mpris.set_playing(true);
                self.refresh_queue_icons();
                self.set_chapters(Vec::new());
                // End any previous recording …
                self.record_state = None;
                // … and start the timeshift buffer for this station (provided
                // the buffer is enabled). Dropping the old recorder cleans up.
                self.recorder = if self.recording_buffer_minutes > 0 {
                    Some(crate::core::recorder::Recorder::start(
                        &st.url,
                        self.recording_buffer_minutes,
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
        self.recorder = None;
        self.record_state = None;
    }

    /// Rebuilds the "Recordings" list (saved recordings). Tapping plays the
    /// file, the trash button removes it.
    pub(crate) fn reload_recordings(&mut self, sender: &ComponentSender<Self>) {
        self.recording_items = self.library.recordings().unwrap_or_default();
        while let Some(child) = self.recordings_list.first_child() {
            self.recordings_list.remove(&child);
        }
        for rec in self.recording_items.clone() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&rec.title))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            let mut sub: Vec<String> = Vec::new();
            if let Some(a) = rec.artist.as_deref().filter(|s| !s.trim().is_empty()) {
                sub.push(a.to_string());
            }
            if let Some(s) = rec.station.as_deref().filter(|s| !s.trim().is_empty()) {
                sub.push(s.to_string());
            }
            sub.push(format_date(rec.recorded_at));
            if !sub.is_empty() {
                row.set_subtitle(&gtk::glib::markup_escape_text(&sub.join(" · ")));
            }
            let icon = if rec.incomplete {
                "media-playlist-consecutive-symbolic"
            } else {
                "audio-x-generic-symbolic"
            };
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            if rec.incomplete {
                row.set_tooltip_text(Some(&gettext("Incomplete (beginning was missing)")));
            }
            let del = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(&gettext("Delete"))
                .build();
            del.add_css_class("flat");
            {
                let sender = sender.clone();
                let id = rec.id;
                del.connect_clicked(move |_| sender.input(Msg::RecordingDelete(id)));
            }
            row.add_suffix(&del);
            {
                let sender = sender.clone();
                let path = rec.path.clone();
                row.connect_activated(move |_| sender.input(Msg::PlayRecording(path.clone())));
            }
            self.recordings_list.append(&row);
        }
    }

    /// Adds a search result (index in `stream_search_results`) as a station
    /// and loads its logo in the background.
    pub(crate) fn add_stream_result(&mut self, sender: &ComponentSender<Self>, index: usize) {
        let Some(r) = self.stream_search_results.get(index).cloned() else {
            return;
        };
        match self.library.add_stream(
            &r.name,
            &r.url,
            r.favicon.as_deref(),
            r.tags.as_deref(),
            r.country.as_deref(),
            r.codec.as_deref(),
            r.bitrate,
        ) {
            Ok(_) => {
                self.reload_streams(sender);
                self.toast(&gettext_f("Added: {n}", &[("n", &r.name)]));
                if let Some(fav) = r.favicon.clone() {
                    sender.spawn_command(move |out| {
                        crate::core::online::cache_station_image(&fav);
                        let _ = out.send(crate::ui::app::Cmd::ReloadStreams);
                    });
                }
            }
            Err(_) => self.toast(&gettext("Could not add station")),
        }
    }

    /// Arms the continuous recording: start offset = beginning of the current song.
    pub(crate) fn record_arm(&mut self, id: i64) {
        let Some(rec) = self.recorder.as_ref() else {
            return;
        };
        let snap = rec.snapshot();
        let next_start = snap.current_start.unwrap_or(0);
        self.record_state = Some(RecordState {
            stream_id: id,
            next_start,
        });
        self.toast(&gettext("Recording …"));
    }

    /// Driven by the 1 s tick: saves songs of the running recording that have
    /// finished (at the song boundaries) and advances.
    pub(crate) fn drive_recording(&mut self, sender: &ComponentSender<Self>) {
        let snap = match self.recorder.as_ref() {
            Some(r) => r.snapshot(),
            None => return,
        };
        let (stream_id, mut next_start) = match self.record_state.as_ref() {
            Some(rs) => (rs.stream_id, rs.next_start),
            None => return,
        };
        if snap.ended {
            self.toast(&gettext("Recording stopped (stream ended)"));
            self.record_state = None;
            return;
        }
        let station = self
            .stream_items
            .iter()
            .find(|s| s.id == stream_id)
            .map(|s| s.name.clone());
        let dest = recordings_dir();

        // Collect finished segments (read-only data; no self-mutation).
        let mut segs: Vec<(u64, u64, Option<String>, String, bool)> = Vec::new();
        loop {
            // The song that contains `next_start` …
            let song = match snap
                .songs
                .iter()
                .find(|s| s.start <= next_start && s.end.map_or(true, |e| e > next_start))
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
            let (artist, title) = crate::core::recorder::split_artist_title(&song.title);
            let incomplete = !song.complete || next_start > song.start;
            segs.push((next_start, end, artist, title, incomplete));
            next_start = end;
        }

        let mut saved = 0;
        // Freshly saved files for cover enrichment (background).
        let mut enrich: Vec<(std::path::PathBuf, Option<String>, String)> = Vec::new();
        for (start, end, artist, title, incomplete) in &segs {
            if let Some(rec) = self.recorder.as_ref() {
                match rec.save_song(*start, *end, artist.as_deref(), title, &dest) {
                    Ok(path) => {
                        let _ = self.library.add_recording(
                            &path.to_string_lossy(),
                            artist.as_deref(),
                            title,
                            station.as_deref(),
                            *incomplete,
                        );
                        enrich.push((path, artist.clone(), title.clone()));
                        saved += 1;
                    }
                    Err(e) => tracing::warn!("Could not save recording: {e}"),
                }
            }
        }

        if let Some(rs) = self.record_state.as_mut() {
            rs.next_start = next_start;
        }
        if saved > 0 {
            self.reload_recordings(sender);
        }
        // Look up cover + album online and embed them into the file (best effort).
        for (path, artist, title) in enrich {
            sender.spawn_command(move |_out| {
                let a = artist.as_deref().unwrap_or("");
                if let Some((bytes, album)) = crate::core::online::recording_cover(a, &title) {
                    crate::core::recorder::embed_cover(
                        &path,
                        artist.as_deref(),
                        &title,
                        album.as_deref(),
                        &bytes,
                    );
                }
            });
        }
    }

    /// Replay subpage of a station: the songs detected in the buffer for
    /// previewing or saving after the fact. Reachable from the detail page.
    pub(crate) fn open_stream_replay(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some(st) = self.stream_items.iter().find(|s| s.id == id).cloned() else {
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

        let snap = self.recorder.as_ref().map(|r| r.snapshot());
        // Only finished songs (with a known end), newest first.
        let mut songs: Vec<crate::core::recorder::BufferedSong> = snap
            .map(|s| s.songs)
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.end.is_some())
            .collect();
        songs.reverse();

        let group = adw::PreferencesGroup::builder()
            .title(&gettext("Recently detected"))
            .build();
        if songs.is_empty() {
            group.add(
                &adw::ActionRow::builder()
                    .title(&gettext("Nothing buffered yet"))
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
                .tooltip_text(&gettext("Save"))
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
                .tooltip_text(&gettext("Play"))
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
}
