//! Streaming (internet radio): station list, station detail page, and the
//! add dialog (manual stream URL **and** worldwide search via the
//! Radio-Browser API). Stations are streamed directly – nothing is downloaded.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, gettext_f};
use crate::model::StreamItem;
use crate::ui::app::{App, Cmd, Msg};

/// Placeholder icon when a station has no logo.
const STREAM_ICON: &str = "audio-x-generic-symbolic";

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

/// Formats Unix seconds as "DD.MM.YYYY HH:MM" in **local** time, so a recording
/// shows at what time the song played, not just the date. Falls back to the
/// date-only formatter if the conversion fails.
fn format_datetime(secs: i64) -> String {
    gtk::glib::DateTime::from_unix_local(secs)
        .and_then(|d| d.format("%d.%m.%Y %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_else(|_| format_date(secs))
}

/// Storage folder for saved recordings: `<Music>/Streaming`.
pub(crate) fn recordings_dir() -> std::path::PathBuf {
    let mut dir = dirs::audio_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    dir.push("Streaming");
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
    let row = adw::ActionRow::builder()
        .title(title)
        .activatable(true)
        .build();
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
        let tags: Vec<&str> = t
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .take(3)
            .collect();
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
        self.streaming.stream_items = self.library.streams().unwrap_or_default();
        self.streaming.stream_play_buttons.borrow_mut().clear();
        while let Some(child) = self.streaming.streams_list.first_child() {
            self.streaming.streams_list.remove(&child);
        }
        for st in self.streaming.stream_items.clone() {
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
                .tooltip_text(gettext("Play/Pause"))
                .build();
            pp.add_css_class("flat");
            {
                let sender = sender.clone();
                pp.connect_clicked(move |_| sender.input(Msg::ToggleStream(id)));
            }
            self.streaming
                .stream_play_buttons
                .borrow_mut()
                .push((id, pp.clone()));
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
            self.streaming.streams_list.append(&row);
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
        let Some(st) = self
            .streaming
            .stream_items
            .iter()
            .find(|s| s.id == id)
            .cloned()
        else {
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
        if self.streaming.recording_buffer_minutes > 5 {
            actions.add(&row_action(
                &gettext("Replay (buffer)"),
                "media-seek-backward-symbolic",
                Msg::OpenStreamReplay(id),
            ));
        }
        actions.add(&row_action(
            &gettext("Rename station"),
            "document-edit-symbolic",
            Msg::StreamRenameDialog(id),
        ));
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

    /// Dialog: rename a station (name prefilled). Reached from the detail view.
    pub(crate) fn open_rename_stream_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        id: i64,
    ) {
        let current = self
            .streaming
            .stream_items
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        let dialog = adw::AlertDialog::new(Some(&gettext("Rename station")), None);
        let entry = gtk::Entry::builder()
            .text(&current)
            .activates_default(true)
            .build();
        crate::ui::widgets::no_autofocus(&entry);
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[
            ("cancel", &gettext("Cancel")),
            ("rename", &gettext("Rename")),
        ]);
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        {
            let sender = sender.clone();
            dialog.connect_response(None, move |_, resp| {
                if resp == "rename" {
                    sender.input(Msg::StreamRename {
                        id,
                        name: entry.text().to_string(),
                    });
                }
            });
        }
        dialog.present(Some(root));
    }

    /// Refreshes the Play/Pause icons of the station rows to the current
    /// playback state (called from the tick and after state changes). The
    /// record button sits in the player bar and updates itself via `#[watch]`.
    pub(crate) fn refresh_stream_icons(&self) {
        let playing = self.mini.playing;
        let cur = self.streaming.playing_stream;
        let mut btns = self.streaming.stream_play_buttons.borrow_mut();
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

    /// Keeps the play/pause icon of each recording row in sync with the
    /// playback state (the active recording shows a pause icon while running).
    pub(crate) fn refresh_recording_icons(&self) {
        let playing = self.mini.playing;
        let cur = self
            .transport
            .playing_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let mut btns = self.streaming.rec_play_buttons.borrow_mut();
        btns.retain(|(_, b)| b.root().is_some());
        for (path, btn) in btns.iter() {
            let active = cur.as_deref() == Some(path.as_str()) && playing;
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
        let dialog = adw::Dialog::builder().title(gettext("Add station")).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        // --- Worldwide search (Radio-Browser) ---
        let search_group = adw::PreferencesGroup::builder()
            .title(gettext("Search"))
            .description(gettext("Find a station worldwide by name"))
            .build();
        let search_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text(gettext("Station name …"))
            .hexpand(true)
            .build();
        crate::ui::widgets::no_autofocus(&search_entry);
        let search_btn = gtk::Button::builder().label(gettext("Search")).build();
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
            .title(gettext("Or enter a stream address"))
            .build();
        let url_entry = adw::EntryRow::builder()
            .title(gettext("Stream address (URL)"))
            .show_apply_button(true)
            .build();
        crate::ui::widgets::no_autofocus(&url_entry);
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

        *self.streaming.stream_search.borrow_mut() = Some((dialog.clone(), results.clone()));
        {
            let slot = self.streaming.stream_search.clone();
            dialog.connect_closed(move |_| {
                *slot.borrow_mut() = None;
            });
        }

        present_dialog(&dialog, &content, root);
    }

    /// Redraws the results list in the open add dialog (from
    /// `self.streaming.stream_search_results`). Tapping saves the station and closes the
    /// dialog. Logos come from the local cache (otherwise a placeholder).
    pub(crate) fn rebuild_stream_search_results(&self, sender: &ComponentSender<Self>) {
        let guard = self.streaming.stream_search.borrow();
        let Some((dialog, list)) = guard.as_ref() else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        list.set_visible(true);

        if self.streaming.stream_search_results.is_empty() {
            let row = adw::ActionRow::builder()
                .title(gettext("No stations found"))
                .build();
            row.set_sensitive(false);
            list.append(&row);
            dialog.set_content_height(300);
            return;
        }

        let rows = self.streaming.stream_search_results.len() as i32;
        dialog.set_content_height((320 + rows * 66).min(760));

        for (i, r) in self.streaming.stream_search_results.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&r.name))
                .activatable(true)
                .build();
            let mut sub: Vec<String> = Vec::new();
            if let Some(c) = r.country.as_deref().filter(|s| !s.trim().is_empty()) {
                sub.push(c.to_string());
            }
            if let Some(t) = r.tags.as_deref().filter(|s| !s.trim().is_empty()) {
                let tags: Vec<&str> = t
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .take(2)
                    .collect();
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
        let Some(st) = self
            .streaming
            .stream_items
            .iter()
            .find(|s| s.id == id)
            .cloned()
        else {
            return;
        };
        // Save the position/session of a previously playing track.
        self.save_resume();
        self.save_episode_progress();
        self.finalize_play_session(false);
        match self.player.play_uri(&st.url, 0) {
            Ok(()) => {
                self.mini.now_playing = Some(st.name.clone());
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

    /// Rebuilds the "Recordings" list (saved recordings). Tapping plays the
    /// file, the trash button removes it.
    pub(crate) fn reload_recordings(&mut self, sender: &ComponentSender<Self>) {
        self.streaming.recording_items = self.library.recordings().unwrap_or_default();
        // Backfill the playback length for rows stored before durations were
        // tracked (probe the file header once, then cache it in the DB).
        for rec in &mut self.streaming.recording_items {
            if rec.duration_ms <= 0 {
                let ms = crate::core::scanner::duration_secs(std::path::Path::new(&rec.path))
                    as i64
                    * 1000;
                if ms > 0 {
                    let _ = self.library.set_recording_duration(rec.id, ms);
                    rec.duration_ms = ms;
                }
            }
        }
        self.streaming.rec_play_buttons.borrow_mut().clear();
        while let Some(child) = self.streaming.recordings_list.first_child() {
            self.streaming.recordings_list.remove(&child);
        }

        // Live entry for the song currently being recorded (not yet a saved
        // file): newest on top. Shows the firmly-assigned song/artist, otherwise
        // a "Current recording" placeholder; its cover arrives via the online
        // lookup ([`maybe_fetch_live_cover`]).
        if let Some(rs) = self.streaming.record_state.as_ref() {
            let station = self
                .streaming
                .stream_items
                .iter()
                .find(|s| s.id == rs.stream_id)
                .map(|s| s.name.clone());
            let (artist, title) = match rs.current_title.as_deref() {
                Some(t) => crate::core::online::recording_query_candidates(t, station.as_deref())
                    .into_iter()
                    .next()
                    .unwrap_or((None, t.trim().to_string())),
                None => (None, gettext("Current recording")),
            };
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&title))
                .build();
            row.add_css_class("emilia-flush");
            let mut sub: Vec<String> = Vec::new();
            if let Some(a) = artist.as_deref().filter(|s| !s.trim().is_empty()) {
                sub.push(a.to_string());
            }
            if let Some(s) = station.as_deref().filter(|s| !s.trim().is_empty()) {
                sub.push(s.to_string());
            }
            sub.push(gettext("Recording …"));
            row.set_subtitle(&gtk::glib::markup_escape_text(&sub.join(" · ")));
            let cover =
                crate::core::online::recording_cover_path(artist.as_deref().unwrap_or(""), &title);
            row.add_prefix(&crate::ui::app::cover_widget(
                cover.as_deref(),
                "media-record-symbolic",
            ));
            // Blinking red record dot marks this as the live entry.
            let dot = gtk::Image::from_icon_name("media-record-symbolic");
            dot.set_valign(gtk::Align::Center);
            dot.set_css_classes(&["emilia-record-dot", "emilia-recording"]);
            row.add_suffix(&dot);
            self.streaming.recordings_list.append(&row);
        }

        for rec in self.streaming.recording_items.clone() {
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
            sub.push(format_datetime(rec.recorded_at));
            if !sub.is_empty() {
                row.set_subtitle(&gtk::glib::markup_escape_text(&sub.join(" · ")));
            }
            let placeholder = if rec.incomplete {
                "media-playlist-consecutive-symbolic"
            } else {
                "audio-x-generic-symbolic"
            };
            // Cover from the online lookup (cached); placeholder icon otherwise.
            let cover = crate::core::online::recording_cover_path(
                rec.artist.as_deref().unwrap_or(""),
                &rec.title,
            );
            row.add_prefix(&crate::ui::app::cover_widget(cover.as_deref(), placeholder));
            if rec.incomplete {
                row.set_tooltip_text(Some(&gettext("Incomplete (beginning was missing)")));
            }
            // Total length of the song (right-aligned, subtle) – like the
            // library track rows.
            if rec.duration_ms > 0 {
                let dur = gtk::Label::new(Some(&crate::ui::app::fmt_duration(rec.duration_ms)));
                dur.set_valign(gtk::Align::Center);
                dur.set_css_classes(&["dim-label", "numeric"]);
                row.add_suffix(&dur);
            }
            // Play/pause button on the far right – starts the recording or
            // toggles pause/resume when it is already the active one (the icon
            // tracks the state via `refresh_recording_icons`).
            let is_active = self
                .transport
                .playing_path
                .as_ref()
                .is_some_and(|p| p.to_string_lossy() == rec.path);
            let play_btn = gtk::Button::from_icon_name(if is_active && self.mini.playing {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            });
            play_btn.set_valign(gtk::Align::Center);
            play_btn.set_tooltip_text(Some(&gettext("Play")));
            play_btn.add_css_class("flat");
            {
                let sender = sender.clone();
                let path = rec.path.clone();
                play_btn.connect_clicked(move |_| sender.input(Msg::PlayRecording(path.clone())));
            }
            row.add_suffix(&play_btn);
            self.streaming
                .rec_play_buttons
                .borrow_mut()
                .push((rec.path.clone(), play_btn));
            // No delete button in the list: a recording is removed only from its
            // detail page (long press → "Delete recording").
            {
                let sender = sender.clone();
                let path = rec.path.clone();
                row.connect_activated(move |_| sender.input(Msg::PlayRecording(path.clone())));
            }
            // Long press → detail page (metadata + cover).
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                let id = rec.id;
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::OpenRecording(id));
                });
            }
            row.add_controller(lp);
            self.streaming.recordings_list.append(&row);
        }
    }

    /// Detail page of a saved recording as a **dialog**: cover + metadata read
    /// from the file tags (artist/title/album), the station and the recording
    /// date, plus play/delete actions. Reachable via long press in the list.
    pub(crate) fn open_recording(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        id: i64,
    ) {
        let Some(rec) = self
            .streaming
            .recording_items
            .iter()
            .find(|r| r.id == id)
            .cloned()
        else {
            return;
        };
        // Album/artist come from the embedded tag (written during enrichment);
        // best effort. The artist falls back to the tag when the DB column is
        // empty, so it is shown reliably in the detail view.
        let tag = crate::core::scanner::read_track(std::path::Path::new(&rec.path)).ok();
        let album = tag
            .as_ref()
            .and_then(|t| t.album.clone())
            .filter(|a| !a.trim().is_empty());
        let artist = rec
            .artist
            .clone()
            .filter(|a| !a.trim().is_empty())
            .or_else(|| tag.as_ref().and_then(|t| t.artist.clone()))
            .filter(|a| !a.trim().is_empty());

        let dialog = adw::Dialog::builder()
            .title(gtk::glib::markup_escape_text(&rec.title))
            .build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        // Header: cover + title/artist.
        let info = adw::PreferencesGroup::new();
        let head = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&rec.title))
            .build();
        if let Some(a) = artist.as_deref() {
            head.set_subtitle(&gtk::glib::markup_escape_text(a));
        }
        let cover =
            crate::core::online::recording_cover_path(artist.as_deref().unwrap_or(""), &rec.title);
        head.add_prefix(&crate::ui::app::cover_widget(
            cover.as_deref(),
            "audio-x-generic-symbolic",
        ));
        info.add(&head);
        content.append(&info);

        // Metadata (album / station / date), each as label → value.
        let details = adw::PreferencesGroup::new();
        let info_row = |label: &str, value: &str| {
            let r = adw::ActionRow::builder().title(label).build();
            r.set_subtitle(&gtk::glib::markup_escape_text(value));
            r.add_css_class("property");
            r
        };
        if let Some(ar) = artist.as_deref() {
            details.add(&info_row(&gettext("Artist"), ar));
        }
        if let Some(al) = album.as_deref() {
            details.add(&info_row(&gettext("Album"), al));
        }
        if let Some(st) = rec.station.as_deref().filter(|s| !s.trim().is_empty()) {
            details.add(&info_row(&gettext("Station"), st));
        }
        details.add(&info_row(
            &gettext("Recorded"),
            &format_datetime(rec.recorded_at),
        ));
        if rec.incomplete {
            details.add(&info_row(
                &gettext("Note"),
                &gettext("Incomplete (beginning was missing)"),
            ));
        }
        content.append(&details);

        // Actions: play / delete.
        let actions = adw::PreferencesGroup::new();
        let play = action_row(&gettext("Play"), "media-playback-start-symbolic");
        {
            let (sender, dialog, path) = (sender.clone(), dialog.clone(), rec.path.clone());
            play.connect_activated(move |_| {
                sender.input(Msg::PlayRecording(path.clone()));
                dialog.close();
            });
        }
        actions.add(&play);
        let add_lib = action_row(&gettext("Add to library"), "list-add-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            add_lib.connect_activated(move |_| {
                sender.input(Msg::AddRecordingToLibrary(id));
                dialog.close();
            });
        }
        actions.add(&add_lib);
        let edit = action_row(&gettext("Edit"), "document-edit-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            edit.connect_activated(move |_| {
                sender.input(Msg::EditRecording(id));
                dialog.close();
            });
        }
        actions.add(&edit);
        let remove = action_row(&gettext("Delete recording"), "user-trash-symbolic");
        {
            let sender = sender.clone();
            let (overlay, dialog) = (self.toast_overlay.clone(), dialog.clone());
            remove.connect_activated(move |_| {
                dialog.close();
                crate::ui::app::confirm_destructive(
                    &overlay,
                    &gettext("Delete this recording?"),
                    &gettext("Delete"),
                    sender.clone(),
                    Msg::RecordingDelete(id),
                );
            });
        }
        actions.add(&remove);
        content.append(&actions);

        present_dialog(&dialog, &content, root);
    }

    /// Adds a search result (index in `stream_search_results`) as a station
    /// and loads its logo in the background.
    pub(crate) fn add_stream_result(&mut self, sender: &ComponentSender<Self>, index: usize) {
        let Some(r) = self.streaming.stream_search_results.get(index).cloned() else {
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
        self.reload_recordings(sender);
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
        let station = self
            .streaming
            .stream_items
            .iter()
            .find(|s| s.id == stream_id)
            .map(|s| s.name.clone());
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
            self.reload_recordings(sender);
            return;
        }
        let station = self
            .streaming
            .stream_items
            .iter()
            .find(|s| s.id == stream_id)
            .map(|s| s.name.clone());

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
            if self.store_segment(
                sender,
                *start,
                *end,
                raw_title,
                station.as_deref(),
                *incomplete,
            ) {
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
            self.reload_recordings(sender);
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
        let Some(raw_title) = live_title.or_else(|| song.map(|s| s.title.clone())) else {
            return; // nothing identified yet → don't save an untitled blob
        };
        let incomplete = song.is_none_or(|s| !s.complete || next_start > s.start);
        let station = self
            .streaming
            .stream_items
            .iter()
            .find(|s| s.id == stream_id)
            .map(|s| s.name.clone());
        self.store_segment(
            sender,
            next_start,
            end,
            &raw_title,
            station.as_deref(),
            incomplete,
        );
    }

    /// Replay subpage of a station: the songs detected in the buffer for
    /// previewing or saving after the fact. Reachable from the detail page.
    pub(crate) fn open_stream_replay(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some(st) = self
            .streaming
            .stream_items
            .iter()
            .find(|s| s.id == id)
            .cloned()
        else {
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

    /// Search internet radio stations (shows hits, then fetches logos).
    pub(crate) fn stream_search(&mut self, sender: &ComponentSender<Self>, term: String) {
        let term = term.trim().to_string();
        if !term.is_empty() {
            self.toast(&gettext("Searching …"));
            sender.spawn_command(move |out| {
                let results = crate::core::streaming::search_stations(&term).unwrap_or_default();
                // Show hits immediately (still without logos) …
                let _ = out.send(Cmd::StreamSearchResults(results.clone()));
                // … and fetch the logos afterwards in the background.
                for r in &results {
                    if let Some(img) = r.favicon.as_deref() {
                        crate::core::online::cache_station_image(img);
                    }
                }
                let _ = out.send(Cmd::StreamSearchCoversReady);
            });
        }
    }

    /// Add a station directly from a URL.
    pub(crate) fn stream_add_url(&mut self, sender: &ComponentSender<Self>, url: String) {
        let url = url.trim().to_string();
        if !url.is_empty() {
            let name = crate::core::streaming::name_from_url(&url);
            match self
                .library
                .add_stream(&name, &url, None, None, None, None, None)
            {
                Ok(_) => {
                    self.reload_streams(sender);
                    self.toast(&gettext("Station added"));
                }
                Err(_) => self.toast(&gettext("Could not add station")),
            }
        }
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
        self.refresh_stream_icons();
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
            self.refresh_stream_icons();
        }
    }

    /// ICY `StreamTitle` update while a station is running → mini player + MPRIS.
    pub(crate) fn stream_title(&mut self, title: String) {
        let title = title.trim().to_string();
        if let Some(id) = self.streaming.playing_stream {
            if !title.is_empty() && self.streaming.stream_title.as_deref() != Some(title.as_str()) {
                self.streaming.stream_title = Some(title.clone());
                let station = self
                    .streaming
                    .stream_items
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| s.name.clone());
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
    pub(crate) fn stream_delete_confirmed(&mut self, sender: &ComponentSender<Self>, id: i64) {
        if self.streaming.playing_stream == Some(id) {
            self.player.stop();
            self.mini.playing = false;
            self.streaming.playing_stream = None;
            self.mini.now_playing = None;
            self.mpris.set_playing(false);
            self.stop_recorder();
        }
        let _ = self.library.delete_stream(id);
        self.reload_streams(sender);
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
            .and_then(|id| self.streaming.stream_items.iter().find(|s| s.id == id))
            .map(|s| s.name.clone());
        if self.store_segment(sender, start, end, &title, station.as_deref(), false) {
            self.reload_recordings(sender);
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

    /// Copies a recording into the primary music library (`<music>/<Artist>/…`),
    /// keeping its original format, then registers it as a regular track so it
    /// shows up in the library views. Best effort with a toast on each outcome.
    pub(crate) fn add_recording_to_library(&mut self, id: i64) {
        let Some(rec) = self
            .streaming
            .recording_items
            .iter()
            .find(|r| r.id == id)
            .cloned()
        else {
            return;
        };
        let Some(music_dir) = self.files.music_dir.clone().filter(|s| !s.trim().is_empty()) else {
            self.toast(&gettext("Set a music folder first"));
            return;
        };
        let src = std::path::PathBuf::from(&rec.path);
        if !src.exists() {
            self.toast(&gettext("File not found"));
            return;
        }

        // Metadata from the embedded tag, with the DB row as the fallback.
        let mut track = crate::core::scanner::read_track(&src).unwrap_or(crate::model::Track {
            id: 0,
            path: rec.path.clone(),
            title: rec.title.clone(),
            artist: rec.artist.clone(),
            album: None,
            genre: None,
            track_no: None,
            disc_no: None,
            duration_ms: None,
            resume_ms: 0,
        });
        let artist = track
            .artist
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| rec.artist.clone())
            .filter(|s| !s.trim().is_empty());
        let title = if track.title.trim().is_empty() {
            rec.title.clone()
        } else {
            track.title.clone()
        };

        // <music>/<Artist|"Recordings">/[<Album>/]<Title>.<ext> – keep the format.
        use crate::core::youtube::sanitize_filename;
        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("mp3");
        let mut dest = std::path::PathBuf::from(&music_dir);
        match artist.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(a) => dest.push(sanitize_filename(a)),
            None => dest.push("Recordings"),
        }
        if let Some(al) = track.album.as_deref().filter(|s| !s.trim().is_empty()) {
            dest.push(sanitize_filename(al));
        }
        dest.push(format!("{}.{ext}", sanitize_filename(&title)));

        if dest.exists() {
            self.toast(&gettext("Already in the library"));
            return;
        }
        if dest
            .parent()
            .is_some_and(|p| std::fs::create_dir_all(p).is_err())
            || std::fs::copy(&src, &dest).is_err()
        {
            self.toast(&gettext("Could not add to the library"));
            return;
        }

        let dest_str = dest.to_string_lossy().into_owned();
        // Carry over the recording's cached cover, if one was fetched.
        if let Some(cover) = crate::core::online::recording_cover_path(
            artist.as_deref().unwrap_or(""),
            &title,
        ) {
            if let Ok(bytes) = std::fs::read(&cover) {
                crate::core::online::store_track_cover_bytes(&dest_str, &bytes);
            }
        }

        track.id = 0;
        track.path = dest_str;
        track.title = title;
        track.artist = artist;
        track.resume_ms = 0;
        if self.library.upsert_track(&track).is_ok() {
            self.reload_library_overviews();
            self.toast(&gettext("Added to the library"));
        } else {
            let _ = std::fs::remove_file(&dest);
            self.toast(&gettext("Could not add to the library"));
        }
    }
}
