//! Library search dialog (title-bar search icon).
//! Split out of app_dialogs.rs – pure reordering, no functional change.

use crate::core::db::Library;
use crate::i18n::gettext;
use crate::model::AlbumHit;
use crate::ui::app::{App, Msg};
use crate::ui::app_streaming::StreamMsg;
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
        // Centered modal in the look of the "+" dialogs: heading, search field,
        // results and Cancel set apart below — also on the phone.
        let dialog = adw::AlertDialog::new(Some(&gettext("Search")), None);
        dialog.add_css_class("emilia-modal");
        // Wider on the desktop for the result rows. A libadwaita 1.6 property:
        // set by name so the v1_5 bindings still build, and skipped on older libs.
        if dialog.find_property("prefer-wide-layout").is_some() {
            dialog.set_property("prefer-wide-layout", true);
        }
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.set_close_response("cancel");

        let outer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();

        let entry = gtk::SearchEntry::builder()
            .placeholder_text(gettext("Artist, album, song, station, video, memo …"))
            .hexpand(true)
            .build();
        outer.append(&entry);

        let results = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        results.append(&search_hint());

        // Fixed height: the modal keeps its size while typing, and only the
        // results scroll — the search field stays in view.
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(360)
            .child(&results)
            .build();
        outer.append(&scroller);
        dialog.set_extra_child(Some(&outer));

        // What is running when the dialog opens, so a hit that is the current
        // track shows a pause icon like it does in every other list. A snapshot
        // is enough here: the dialog is short-lived, and pressing the row calls
        // the same toggling playback path, so the icon never lies about what a
        // press does.
        let cur_path = self.transport.playing_path.clone();
        let playing = self.mini.playing;
        let is_running = move |path: &str| {
            crate::ui::app_favorites::entry_is_active(cur_path.as_deref(), None, "track", path)
        };

        // Live search: SQLite is local and the result count is capped, so we can
        // re-query on each (already debounced) change of the search entry.
        let sender = sender.clone();
        let dlg: adw::Dialog = dialog.clone().upcast();
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
                        .css_classes(["compact"])
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

            // Album-like categories — same row layout, one heading each, in the
            // navigation's order. Every hit opens by album name
            // (`SearchOpenAlbum`), which renders the right track list for each
            // kind: singles/compilations open exactly like albums, concerts/
            // audiobooks open their album's tracks. Icons match the nav sections.
            add_album_group(
                &results,
                &gettext("Albums"),
                "media-optical-symbolic",
                &res.albums,
                &sender,
                &dlg,
            );
            add_album_group(
                &results,
                &gettext("Singles"),
                "audio-x-generic-symbolic",
                &res.singles,
                &sender,
                &dlg,
            );
            add_album_group(
                &results,
                &gettext("Compilations"),
                "view-grid-symbolic",
                &res.compilations,
                &sender,
                &dlg,
            );
            add_album_group(
                &results,
                &gettext("Concerts"),
                "ticket-special-symbolic",
                &res.concerts,
                &sender,
                &dlg,
            );
            add_album_group(
                &results,
                &gettext("Audiobooks"),
                "emilia-audiobook-symbolic",
                &res.audiobooks,
                &sender,
                &dlg,
            );

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
                    row.add_suffix(&crate::ui::play_mark::marker(is_running(&s.path), playing));
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
                    row.add_suffix(&crate::ui::play_mark::marker(is_running(&r.path), playing));
                    let sender = sender.clone();
                    let dlg = dlg.clone();
                    let path = r.path.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::Stream(StreamMsg::PlayRecording(path.clone())));
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
                    row.add_suffix(&crate::ui::play_mark::marker(is_running(&m.path), playing));
                    let sender = sender.clone();
                    let dlg = dlg.clone();
                    let path = m.path.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::Stream(StreamMsg::PlayRecording(path.clone())));
                        dlg.close();
                    });
                    group.add(&row);
                }
                results.append(&group);
            }
        });

        crate::ui::widgets::close_on_outside_click(&dialog);
        dialog.present(Some(root));
        entry.grab_focus();
    }
}

/// Appends one album-like result group (Albums / Singles / Compilations /
/// Concerts / Audiobooks) with `label`, `icon` and a row per hit. Nothing is
/// added for an empty group. Activating a row opens the album's track list and
/// closes the dialog. Shared by all album-kind categories — they differ only in
/// heading and icon, never in how a hit is opened.
fn add_album_group(
    results: &gtk::Box,
    label: &str,
    icon: &str,
    hits: &[AlbumHit],
    sender: &ComponentSender<App>,
    dlg: &adw::Dialog,
) {
    if hits.is_empty() {
        return;
    }
    let group = adw::PreferencesGroup::builder()
        .title(format!("{label} ({})", hits.len()))
        .build();
    for a in hits {
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
        row.add_prefix(&gtk::Image::from_icon_name(icon));
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

/// The idle/empty hint shown in the search dialog before anything is typed.
fn search_hint() -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name("system-search-symbolic")
        .title(gettext("Search"))
        .description(gettext(
            "Find artists, albums, songs, stations, recordings, videos and memos.",
        ))
        .css_classes(["compact"])
        .vexpand(true)
        .build()
}
