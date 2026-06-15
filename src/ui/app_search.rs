//! Library search dialog (title-bar search icon).
//! Split out of app_dialogs.rs – pure reordering, no functional change.

use crate::core::db::Library;
use crate::i18n::gettext;
use crate::ui::app::{App, Msg};
use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

impl App {
    /// Library search (title-bar search icon): a search field that, as you type,
    /// lists matching artists, albums and songs (incl. file-date matches).
    /// Activating a hit plays the song / opens the album / opens the artist.
    pub(crate) fn open_search_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::Dialog::builder().title(gettext("Search")).build();
        // Same fixed width as the other detail dialogs; full-width bottom sheet
        // on the phone.
        dialog.set_content_width(600);
        dialog.set_content_height(560);
        self.adapt_detail_dialog(&dialog);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());

        let outer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        let entry = gtk::SearchEntry::builder()
            .placeholder_text(gettext("Artist, album, song, station, video, memo …"))
            .hexpand(true)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        outer.append(&entry);

        let results = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        results.append(&search_hint());

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&results)
            .build();
        outer.append(&scroller);
        toolbar.set_content(Some(&outer));
        dialog.set_child(Some(&toolbar));

        // Live search: SQLite is local and the result count is capped, so we can
        // re-query on each (already debounced) change of the search entry.
        let sender = sender.clone();
        let dlg = dialog.clone();
        entry.connect_search_changed(move |e| {
            while let Some(c) = results.first_child() {
                results.remove(&c);
            }
            let term = e.text().to_string();
            let q = term.trim();
            if q.is_empty() {
                results.append(&search_hint());
                return;
            }
            let Ok(lib) = Library::open() else { return };
            let res = lib.search_library(q, 30).unwrap_or_default();
            if res.is_empty() {
                results.append(
                    &adw::StatusPage::builder()
                        .icon_name("system-search-symbolic")
                        .title(gettext("No results"))
                        .vexpand(true)
                        .build(),
                );
                return;
            }

            // --- Artists ---
            if !res.artists.is_empty() {
                let group = adw::PreferencesGroup::builder()
                    .title(format!("{} ({})", gettext("Artists"), res.artists.len()))
                    .build();
                for name in &res.artists {
                    let row = adw::ActionRow::builder()
                        .title(gtk::glib::markup_escape_text(name))
                        .activatable(true)
                        .build();
                    row.add_prefix(&gtk::Image::from_icon_name("avatar-default-symbolic"));
                    let sender = sender.clone();
                    let dlg = dlg.clone();
                    let name = name.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::SearchOpenArtist(name.clone()));
                        dlg.close();
                    });
                    group.add(&row);
                }
                results.append(&group);
            }

            // --- Albums ---
            if !res.albums.is_empty() {
                let group = adw::PreferencesGroup::builder()
                    .title(format!("{} ({})", gettext("Albums"), res.albums.len()))
                    .build();
                for a in &res.albums {
                    let mut sub = a.artist.clone();
                    if let Some(y) = a.year {
                        sub = if sub.trim().is_empty() {
                            y.to_string()
                        } else {
                            format!("{sub} · {y}")
                        };
                    }
                    let row = adw::ActionRow::builder()
                        .title(gtk::glib::markup_escape_text(&a.album))
                        .subtitle(gtk::glib::markup_escape_text(&sub))
                        .activatable(true)
                        .build();
                    row.add_prefix(&gtk::Image::from_icon_name("media-optical-symbolic"));
                    let sender = sender.clone();
                    let dlg = dlg.clone();
                    let album = a.album.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::SearchOpenAlbum(album.clone()));
                        dlg.close();
                    });
                    group.add(&row);
                }
                results.append(&group);
            }

            // --- Songs ---
            if !res.songs.is_empty() {
                let group = adw::PreferencesGroup::builder()
                    .title(format!("{} ({})", gettext("Songs"), res.songs.len()))
                    .build();
                for s in &res.songs {
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(a) = s.artist.as_ref().filter(|a| !a.trim().is_empty()) {
                        parts.push(a.clone());
                    }
                    if let Some(al) = s.album.as_ref().filter(|a| !a.trim().is_empty()) {
                        parts.push(al.clone());
                    }
                    let row = adw::ActionRow::builder()
                        .title(gtk::glib::markup_escape_text(&s.title))
                        .subtitle(gtk::glib::markup_escape_text(&parts.join(" · ")))
                        .activatable(true)
                        .build();
                    row.add_prefix(&gtk::Image::from_icon_name("audio-x-generic-symbolic"));
                    row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
                    let sender = sender.clone();
                    let dlg = dlg.clone();
                    let path = s.path.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::SearchPlayTrack(path.clone()));
                        dlg.close();
                    });
                    group.add(&row);
                }
                results.append(&group);
            }

            // Streaming stations and YouTube channels/videos are intentionally
            // *not* listed here – the global library search covers the local
            // collection (artists, albums, songs, recordings, memos). YouTube has
            // its own dedicated search (which also accepts a pasted link).

            // --- Recordings (timeshift; tap = play) ---
            if !res.recordings.is_empty() {
                let group = adw::PreferencesGroup::builder()
                    .title(format!(
                        "{} ({})",
                        gettext("Recordings"),
                        res.recordings.len()
                    ))
                    .build();
                for r in &res.recordings {
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(a) = r.artist.as_ref().filter(|a| !a.trim().is_empty()) {
                        parts.push(a.clone());
                    }
                    if let Some(st) = r.station.as_ref().filter(|s| !s.trim().is_empty()) {
                        parts.push(st.clone());
                    }
                    let row = adw::ActionRow::builder()
                        .title(gtk::glib::markup_escape_text(&r.title))
                        .subtitle(gtk::glib::markup_escape_text(&parts.join(" · ")))
                        .activatable(true)
                        .build();
                    row.add_prefix(&gtk::Image::from_icon_name("media-record-symbolic"));
                    row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
                    let sender = sender.clone();
                    let dlg = dlg.clone();
                    let path = r.path.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::PlayRecording(path.clone()));
                        dlg.close();
                    });
                    group.add(&row);
                }
                results.append(&group);
            }

            // --- Voice memos (tap = play) ---
            if !res.memos.is_empty() {
                let group = adw::PreferencesGroup::builder()
                    .title(format!("{} ({})", gettext("Memos"), res.memos.len()))
                    .build();
                for m in &res.memos {
                    let row = adw::ActionRow::builder()
                        .title(gtk::glib::markup_escape_text(&m.title))
                        .activatable(true)
                        .build();
                    row.add_prefix(&gtk::Image::from_icon_name(
                        "audio-input-microphone-symbolic",
                    ));
                    row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
                    let sender = sender.clone();
                    let dlg = dlg.clone();
                    let path = m.path.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::PlayRecording(path.clone()));
                        dlg.close();
                    });
                    group.add(&row);
                }
                results.append(&group);
            }
        });

        crate::ui::app_helpers::close_on_click_outside(&dialog);
        dialog.present(Some(root));
        entry.grab_focus();
    }
}

/// The idle/empty hint shown in the search dialog before anything is typed.
fn search_hint() -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name("system-search-symbolic")
        .title(gettext("Search"))
        .description(gettext(
            "Find artists, albums, songs, stations, recordings, videos and memos.",
        ))
        .vexpand(true)
        .build()
}
