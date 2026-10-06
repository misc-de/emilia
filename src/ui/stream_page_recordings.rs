//! Recordings half of [`StreamPage`]'s inherent impl: the "Recordings" list
//! (with the live "currently recording" entry), the recording detail dialog
//! and its copy into the music library, plus the "Recently heard" list and the
//! detail dialog of a recognized song. The struct, its messages and the
//! `Component` impl stay in [`crate::ui::stream_page`].

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, gettext_f};
use crate::ui::app_helpers::{cover_widget, on_long_press, on_secondary_click};
use crate::ui::entry_row::EntryRow;
use crate::ui::stream_page::{StreamInput, StreamOutput, StreamPage};
use crate::ui::stream_page_logic::{
    first_nonblank, heard_headers, library_dest, live_entry_title, nonblank, recording_headers,
    recording_placeholder, song_subtitle, sort_heard, sort_recordings,
};
use crate::ui::stream_page_stations::confirm_delete;
use crate::ui::widgets::{
    action_row, detail_box, info_expander, info_row, present_detail_refreshable,
};

/// Formats Unix seconds as "DD.MM.YYYY HH:MM" in local time.
fn format_datetime(secs: i64) -> String {
    gtk::glib::DateTime::from_unix_local(secs)
        .and_then(|d| d.format("%d.%m.%Y %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// What a detail refresh found for a song: artist, album and cover from the
/// music database (Deezer), checked against the known artist; for a song
/// without an artist the recording lookup, which also copes with station noise
/// in the title. **Network** — worker threads only.
pub(super) fn lookup_song(
    artist: Option<&str>,
    title: &str,
    station: Option<&str>,
) -> Option<(Option<String>, Option<String>, Vec<u8>)> {
    if let Some(tags) = crate::core::online::track_tags_strict(artist, title) {
        if let Some(cover) = tags.cover {
            return Some((tags.artist, tags.album, cover));
        }
    }
    // The recording lookup also copes with station noise in the title, but it
    // checks no artist — so only when there is none to check against.
    if artist.is_some_and(|a| !a.trim().is_empty()) {
        return None;
    }
    crate::core::online::recording_cover(title, station).map(|(cover, album)| (None, album, cover))
}

impl StreamPage {
    /// Rebuilds the "Recordings" list (live entry + saved recordings).
    pub(super) fn reload_recordings(&mut self, sender: &ComponentSender<Self>) {
        self.recording_items = self.library.recordings().unwrap_or_default();
        for rec in &mut self.recording_items {
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
        let (crit, desc) = self.recordings_sort;
        sort_recordings(&mut self.recording_items, crit, desc);
        // Refresh the title-bar sort control (visibility depends on emptiness).
        self.rebuild_sort(sender);
        // Alphabetical headings (by name) for the saved rows; none while a live
        // entry is prepended (it would offset the labels) or for date/length sorts.
        *self.recording_headers.borrow_mut() = recording_headers(
            &self.recording_items,
            crit,
            self.recordings_no_group,
            self.live_recording.is_some(),
        );
        self.rec_marks.clear();
        while let Some(child) = self.recordings_list.first_child() {
            self.recordings_list.remove(&child);
        }

        // Live entry for the song currently being recorded.
        if let Some((stream_id, current_title)) = self.live_recording.clone() {
            let station = self
                .stream_items
                .iter()
                .find(|s| s.id == stream_id)
                .map(|s| s.name.clone());
            let (artist, title) = live_entry_title(
                current_title.as_deref(),
                station.as_deref(),
                &gettext("Current recording"),
            );
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&title))
                .build();
            row.add_css_class("emilia-flush");
            let sub = song_subtitle(
                artist.as_deref(),
                station.as_deref(),
                &gettext("Recording …"),
            );
            row.set_subtitle(&gtk::glib::markup_escape_text(&sub));
            let cover =
                crate::core::online::recording_cover_path(artist.as_deref().unwrap_or(""), &title);
            row.add_prefix(&cover_widget(cover.as_deref(), "media-record-symbolic"));
            let dot = gtk::Image::from_icon_name("media-record-symbolic");
            dot.set_valign(gtk::Align::Center);
            dot.set_css_classes(&["emilia-record-dot", "emilia-recording"]);
            row.add_suffix(&dot);
            self.recordings_list.append(&row);
        }

        for rec in self.recording_items.clone() {
            let sub = song_subtitle(
                rec.artist.as_deref(),
                rec.station.as_deref(),
                &format_datetime(rec.recorded_at),
            );
            let placeholder = recording_placeholder(rec.incomplete);
            let cover = crate::core::online::recording_cover_path(
                rec.artist.as_deref().unwrap_or(""),
                &rec.title,
            );
            let play = {
                let sender = sender.clone();
                let path = rec.path.clone();
                move || {
                    let _ = sender.output(StreamOutput::PlayRecording(path.clone()));
                }
            };
            let row = EntryRow::new(&rec.title)
                .subtitle(&sub)
                .cover(cover.as_deref(), placeholder)
                .duration(rec.duration_ms)
                .play_button(
                    &gettext("Play"),
                    self.playing_path.as_deref() == Some(rec.path.as_str()),
                    self.playing,
                    play.clone(),
                )
                .marked_in(&self.rec_marks, rec.path.clone())
                .on_activate(play)
                .on_detail({
                    let sender = sender.clone();
                    let id = rec.id;
                    move || sender.input(StreamInput::OpenRecording(id))
                })
                .build();
            if rec.incomplete {
                row.set_tooltip_text(Some(&gettext("Incomplete (beginning was missing)")));
            }
            self.recordings_list.append(&row);
        }
        self.recordings_list.invalidate_headers();
    }

    /// Detail dialog of a saved recording.
    pub(super) fn open_recording(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let Some(rec) = self.recording_items.iter().find(|r| r.id == id).cloned() else {
            return;
        };
        let tag = crate::core::scanner::read_track(std::path::Path::new(&rec.path)).ok();
        let album = tag
            .as_ref()
            .and_then(|t| t.album.clone())
            .filter(|a| !a.trim().is_empty());
        let artist = first_nonblank(
            rec.artist.clone(),
            tag.as_ref().and_then(|t| t.artist.clone()),
        );

        let dialog = adw::Dialog::builder().title(&rec.title).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let cover =
            crate::core::online::recording_cover_path(artist.as_deref().unwrap_or(""), &rec.title);
        content.append(&crate::ui::widgets::detail_cover(
            cover.as_deref(),
            "audio-x-generic-symbolic",
        ));

        // The song's details folded under "Info", as in the music details.
        let (info, details) = info_expander();
        details.add_row(&info_row(&gettext("Title"), &rec.title));
        if let Some(ar) = artist.as_deref() {
            details.add_row(&info_row(&gettext("Artist"), ar));
        }
        if let Some(al) = album.as_deref() {
            details.add_row(&info_row(&gettext("Album"), al));
        }
        if rec.duration_ms > 0 {
            details.add_row(&info_row(
                &gettext("Duration"),
                &crate::ui::app_helpers::fmt_duration(rec.duration_ms),
            ));
        }
        if let Some(st) = nonblank(rec.station.as_deref()) {
            details.add_row(&info_row(&gettext("Station"), st));
        }
        details.add_row(&info_row(
            &gettext("Recorded"),
            &format_datetime(rec.recorded_at),
        ));
        if rec.incomplete {
            details.add_row(&info_row(
                &gettext("Note"),
                &gettext("Incomplete (beginning was missing)"),
            ));
        }
        content.append(&info);

        let actions = adw::PreferencesGroup::new();
        let play = action_row(&gettext("Play"), "media-playback-start-symbolic");
        {
            let (sender, dialog, path) = (sender.clone(), dialog.clone(), rec.path.clone());
            play.connect_activated(move |_| {
                let _ = sender.output(StreamOutput::PlayRecording(path.clone()));
                dialog.close();
            });
        }
        actions.add(&play);
        let add_lib = action_row(&gettext("Add to library"), "list-add-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            add_lib.connect_activated(move |_| {
                sender.input(StreamInput::AddRecordingToLibrary(id));
                dialog.close();
            });
        }
        actions.add(&add_lib);
        let edit = action_row(&gettext("Edit"), "document-edit-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            edit.connect_activated(move |_| {
                let _ = sender.output(StreamOutput::EditRecording(id));
                dialog.close();
            });
        }
        actions.add(&edit);
        let share = action_row(&gettext("Share"), "emilia-share-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            share.connect_activated(move |_| {
                let _ = sender.output(StreamOutput::Share(Box::new(
                    crate::core::sync::share::Selection {
                        recordings: vec![id],
                        ..Default::default()
                    },
                )));
                dialog.close();
            });
        }
        actions.add(&share);
        let remove = action_row(&gettext("Delete recording"), "user-trash-symbolic");
        {
            let (sender, dialog, root) = (sender.clone(), dialog.clone(), root.clone());
            remove.connect_activated(move |_| {
                dialog.close();
                confirm_delete(
                    &root,
                    &sender,
                    &gettext("Delete this recording?"),
                    &gettext("Delete"),
                    StreamInput::RecordingDelete(id),
                );
            });
        }
        actions.add(&remove);
        content.append(&actions);

        {
            let sender = sender.clone();
            present_detail_refreshable(&dialog, &content, &root, move || {
                sender.input(StreamInput::RefreshRecording(id));
            });
        }
    }

    /// Copies a recording into the primary music library, then registers it.
    pub(super) fn add_recording_to_library(&mut self, sender: &ComponentSender<Self>, id: i64) {
        let Some(rec) = self.recording_items.iter().find(|r| r.id == id).cloned() else {
            return;
        };
        let Some(music_dir) = self
            .library
            .get_setting("music_dir")
            .ok()
            .flatten()
            .filter(|s| !s.trim().is_empty())
        else {
            let _ = sender.output(StreamOutput::Toast(gettext("Set a music folder first")));
            return;
        };
        let src = std::path::PathBuf::from(&rec.path);
        if !src.exists() {
            let _ = sender.output(StreamOutput::Toast(gettext("File not found")));
            return;
        }

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
            year: None,
        });
        let artist = first_nonblank(track.artist.clone(), rec.artist.clone());
        let title = if track.title.trim().is_empty() {
            rec.title.clone()
        } else {
            track.title.clone()
        };

        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("mp3");
        let dest = library_dest(
            &music_dir,
            artist.as_deref(),
            track.album.as_deref(),
            &title,
            ext,
        );

        if dest.exists() {
            let _ = sender.output(StreamOutput::Toast(gettext("Already in the library")));
            return;
        }
        if dest
            .parent()
            .is_some_and(|p| std::fs::create_dir_all(p).is_err())
            || std::fs::copy(&src, &dest).is_err()
        {
            let _ = sender.output(StreamOutput::Toast(gettext("Could not add to the library")));
            return;
        }

        let dest_str = dest.to_string_lossy().into_owned();
        if let Some(cover) =
            crate::core::online::recording_cover_path(artist.as_deref().unwrap_or(""), &title)
        {
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
            let _ = sender.output(StreamOutput::LibraryChanged);
            let _ = sender.output(StreamOutput::Toast(gettext("Added to the library")));
        } else {
            let _ = std::fs::remove_file(&dest);
            let _ = sender.output(StreamOutput::Toast(gettext("Could not add to the library")));
        }
    }

    /// Rebuilds the "Recently heard" list (recognized songs). No live entry,
    /// no audio files — each row just opens the detail dialog (tap or long press).
    pub(super) fn reload_heard(&mut self, sender: &ComponentSender<Self>) {
        self.heard_items = self.library.heard_songs().unwrap_or_default();
        let (crit, desc) = self.heard_sort;
        sort_heard(&mut self.heard_items, crit, desc);
        // Refresh the title-bar sort control (visibility depends on emptiness).
        self.rebuild_sort(sender);
        *self.heard_headers.borrow_mut() =
            heard_headers(&self.heard_items, crit, self.heard_no_group);
        while let Some(child) = self.heard_list.first_child() {
            self.heard_list.remove(&child);
        }
        for h in self.heard_items.clone() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&h.title))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            let sub = song_subtitle(
                h.artist.as_deref(),
                h.station.as_deref(),
                &format_datetime(h.heard_at),
            );
            row.set_subtitle(&gtk::glib::markup_escape_text(&sub));
            let cover = crate::core::online::recording_cover_path(
                h.artist.as_deref().unwrap_or(""),
                &h.title,
            );
            row.add_prefix(&cover_widget(cover.as_deref(), "audio-x-generic-symbolic"));
            if h.count > 1 {
                let badge =
                    gtk::Label::new(Some(&gettext_f("{n}×", &[("n", &h.count.to_string())])));
                badge.set_valign(gtk::Align::Center);
                badge.set_css_classes(&["dim-label", "numeric"]);
                row.add_suffix(&badge);
            }
            // Direct play button on the right: plays the recognized song right
            // away — a local copy (timeshift recording or library track) first,
            // and only via YouTube when nothing local matches (see `play_heard`).
            let play_btn = gtk::Button::builder()
                .icon_name("media-playback-start-symbolic")
                .tooltip_text(gettext("Play"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let sender = sender.clone();
                let (artist, title) = (h.artist.clone(), h.title.clone());
                play_btn.connect_clicked(move |_| {
                    let _ = sender.output(StreamOutput::PlayHeard {
                        artist: artist.clone(),
                        title: title.clone(),
                    });
                });
            }
            row.add_suffix(&play_btn);
            let id = h.id;
            {
                let sender = sender.clone();
                row.connect_activated(move |_| sender.input(StreamInput::OpenHeard(id)));
            }
            on_secondary_click(&row, {
                let sender = sender.clone();
                move || sender.input(StreamInput::OpenHeard(id))
            });
            on_long_press(&row, {
                let sender = sender.clone();
                move || sender.input(StreamInput::OpenHeard(id))
            });
            self.heard_list.append(&row);
        }
        self.heard_list.invalidate_headers();
    }

    /// Detail dialog of a recognized song: which station it was heard on, when,
    /// info about the song, and the Play / Download / Remove actions.
    pub(super) fn open_heard(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let Some(h) = self.heard_items.iter().find(|x| x.id == id).cloned() else {
            return;
        };
        let dialog = adw::Dialog::builder().title(&h.title).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let cover =
            crate::core::online::recording_cover_path(h.artist.as_deref().unwrap_or(""), &h.title);
        content.append(&crate::ui::widgets::detail_cover(
            cover.as_deref(),
            "audio-x-generic-symbolic",
        ));

        // The song's details folded under "Info", as in the music details.
        let (info, details) = info_expander();
        details.add_row(&info_row(&gettext("Title"), &h.title));
        if let Some(a) = nonblank(h.artist.as_deref()) {
            details.add_row(&info_row(&gettext("Artist"), a));
        }
        if let Some(s) = nonblank(h.station.as_deref()) {
            details.add_row(&info_row(&gettext("Station"), s));
        }
        details.add_row(&info_row(&gettext("Heard"), &format_datetime(h.heard_at)));
        if h.count > 1 {
            details.add_row(&info_row(&gettext("Times heard"), &h.count.to_string()));
        }
        content.append(&info);

        let actions = adw::PreferencesGroup::new();
        let play = action_row(&gettext("Play"), "media-playback-start-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            let (artist, title) = (h.artist.clone(), h.title.clone());
            play.connect_activated(move |_| {
                let _ = sender.output(StreamOutput::PlayHeard {
                    artist: artist.clone(),
                    title: title.clone(),
                });
                dialog.close();
            });
        }
        actions.add(&play);
        let dl = action_row(&gettext("Download via YouTube"), "folder-download-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            let (artist, title) = (h.artist.clone(), h.title.clone());
            dl.connect_activated(move |_| {
                let _ = sender.output(StreamOutput::DownloadHeard {
                    artist: artist.clone(),
                    title: title.clone(),
                });
                dialog.close();
            });
        }
        actions.add(&dl);
        let remove = action_row(&gettext("Remove from list"), "user-trash-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            remove.connect_activated(move |_| {
                sender.input(StreamInput::HeardDelete(id));
                dialog.close();
            });
        }
        actions.add(&remove);
        content.append(&actions);

        {
            let sender = sender.clone();
            present_detail_refreshable(&dialog, &content, &root, move || {
                sender.input(StreamInput::RefreshHeard(id));
            });
        }
    }
}
