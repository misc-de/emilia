//! Playlists: overview list, track subpage and the dialogs for creating
//! or adding. Entries are paths (like the queue).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::ui::app::{App, Msg};

impl App {
    /// Rebuilds the playlist list (name, track count, play, delete).
    pub(crate) fn reload_playlists(&mut self, sender: &ComponentSender<Self>) {
        self.playlists.playlist_items = self.library.playlists().unwrap_or_default();

        while let Some(child) = self.playlists.playlists_list.first_child() {
            self.playlists.playlists_list.remove(&child);
        }
        for (id, name, count) in self.playlists.playlist_items.clone() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&name))
                .subtitle(ngettext_n("{n} track", "{n} tracks", count as u32))
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("view-list-symbolic"));

            let play = gtk::Button::builder()
                .icon_name("media-playback-start-symbolic")
                .tooltip_text(&gettext("Play playlist"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let sender = sender.clone();
                play.connect_clicked(move |_| sender.input(Msg::PlayPlaylist(id)));
            }
            row.add_suffix(&play);

            let del = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text(&gettext("Delete playlist"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let sender = sender.clone();
                del.connect_clicked(move |b| {
                    crate::ui::app::confirm_destructive(
                        b,
                        &gettext("Delete this playlist?"),
                        &gettext("Delete"),
                        sender.clone(),
                        Msg::PlaylistDelete(id),
                    );
                });
            }
            row.add_suffix(&del);

            {
                let sender = sender.clone();
                row.connect_activated(move |_| sender.input(Msg::OpenPlaylist(id)));
            }
            // Long press: detail view (cover + actions).
            let long_press = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                long_press.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::ShowPlaylistDetail(id));
                });
            }
            row.add_controller(long_press);
            self.playlists.playlists_list.append(&row);
        }
    }

    /// Short tap on a playlist: a subpage that lists the playlist's
    /// **albums** (2+ tracks of the same album, expandable) and then the
    /// standalone **songs**. Tapping a track plays the playlist from there.
    pub(crate) fn open_playlist(&self, sender: &ComponentSender<Self>, id: i64, name: &str) {
        let paths = self.library.playlist_paths(id).unwrap_or_default();
        let (albums, singles) = self.playlist_sections(&paths);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        if paths.is_empty() {
            content.append(
                &adw::StatusPage::builder()
                    .icon_name("view-list-symbolic")
                    .title(&gettext("No tracks yet"))
                    .description(&gettext("Add tracks via the options of a song, album or artist."))
                    .build(),
            );
        }

        // --- Albums first (like the artist view) ---
        if !albums.is_empty() {
            let group = adw::PreferencesGroup::builder()
                .title(&format!("{} ({})", gettext("Albums"), albums.len()))
                .build();
            for (album, display_artist, tracks) in &albums {
                let album_meta = self.library.get_album_meta(display_artist, album).ok().flatten();
                let year = album_meta.as_ref().and_then(|m| m.year);
                let cover_path = album_meta.as_ref().and_then(|m| m.cover_path.clone());

                let exp = adw::ExpanderRow::builder()
                    .title(gtk::glib::markup_escape_text(album))
                    .subtitle(crate::ui::app::album_subtitle(year, tracks.len()))
                    .build();
                exp.add_prefix(&crate::ui::app::cover_widget(
                    cover_path.as_deref(),
                    "media-optical-symbolic",
                ));
                // Play button: start the playlist at this album's first track.
                if let Some(first) = tracks.first() {
                    let play = gtk::Button::builder()
                        .icon_name("media-playback-start-symbolic")
                        .tooltip_text(&gettext("Play"))
                        .valign(gtk::Align::Center)
                        .css_classes(["flat"])
                        .build();
                    let sender = sender.clone();
                    let path = first.path.clone();
                    play.connect_clicked(move |_| {
                        sender.input(Msg::PlaylistTrack { id, path: path.clone() });
                    });
                    exp.add_suffix(&play);
                }
                for t in tracks {
                    exp.add_row(&self.playlist_track_row(sender, id, &t.path, "audio-x-generic-symbolic"));
                }
                group.add(&exp);
            }
            content.append(&group);
        }

        // --- Standalone songs ---
        if !singles.is_empty() {
            let group = adw::PreferencesGroup::builder()
                .title(&format!("{} ({})", gettext("Songs"), singles.len()))
                .build();
            for t in &singles {
                group.add(&self.playlist_track_row(sender, id, &t.path, "audio-x-generic-symbolic"));
            }
            content.append(&group);
        }

        self.push_subpage(&gettext_f("Playlist – {name}", &[("name", name)]), &content);
    }

    /// Cover for a playlist track row: the YouTube thumbnail/cover for `yt:`
    /// items, otherwise the embedded track cover or its album cover.
    fn playlist_track_cover(&self, path: &str) -> Option<String> {
        if let Some(vid) = crate::core::youtube::parse_yt_path(path) {
            return crate::core::online::youtube_cover_path(&vid).or_else(|| {
                crate::core::online::youtube_thumb_path(&crate::core::youtube::thumbnail_url(&vid))
            });
        }
        if let Some(c) = crate::core::online::local_track_cover(path) {
            return Some(c);
        }
        let t = self.library.track_by_path(path).ok().flatten()?;
        let (artist, album) = (t.artist?, t.album?);
        self.library
            .get_album_meta(&artist, &album)
            .ok()
            .flatten()
            .and_then(|m| m.cover_path)
    }

    /// A single track row inside a playlist subpage: tap plays the playlist
    /// from this track, the trash button removes it (with an undo toast).
    fn playlist_track_row(
        &self,
        sender: &ComponentSender<Self>,
        id: i64,
        path: &str,
        icon: &str,
    ) -> adw::ActionRow {
        let display = self.display_name(std::path::Path::new(path));
        let row = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&display))
            .activatable(true)
            .build();
        let cover = self.playlist_track_cover(path);
        row.add_prefix(&crate::ui::app::cover_widget(cover.as_deref(), icon));

        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text(&gettext("Remove from playlist"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let sender = sender.clone();
            let path = path.to_string();
            remove.connect_clicked(move |b| {
                crate::ui::app::confirm_destructive(
                    b,
                    &gettext("Remove this track from the playlist?"),
                    &gettext("Remove"),
                    sender.clone(),
                    Msg::PlaylistRemoveTrack { id, path: path.clone() },
                );
            });
        }
        row.add_suffix(&remove);
        row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
        {
            let sender = sender.clone();
            let path = path.to_string();
            row.connect_activated(move |_| {
                sender.input(Msg::PlaylistTrack { id, path: path.clone() });
            });
        }
        row
    }

    /// Long press on a playlist: a detail view (cover, name, totals and the
    /// playlist-wide actions). Styled like the artist/album context view –
    /// a bottom sheet on the phone (see [`App::adapt_detail_dialog`]).
    pub(crate) fn open_playlist_detail(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        id: i64,
        name: &str,
    ) {
        let paths = self.library.playlist_paths(id).unwrap_or_default();
        // Total runtime + a representative cover (first track with an album cover).
        let mut total_ms: i64 = 0;
        let mut cover_path: Option<String> = None;
        for p in &paths {
            if let Some(t) = self.library.track_by_path(p).ok().flatten() {
                total_ms += t.duration_ms.unwrap_or(0);
                if cover_path.is_none() {
                    if let (Some(artist), Some(album)) = (t.artist.as_deref(), t.album.as_deref()) {
                        cover_path = self
                            .library
                            .get_album_meta(artist, album)
                            .ok()
                            .flatten()
                            .and_then(|m| m.cover_path);
                    }
                }
            }
        }

        let dialog = adw::Dialog::builder().title(gtk::glib::markup_escape_text(name)).build();
        dialog.set_content_width(360);
        self.adapt_detail_dialog(&dialog);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Cover (or a generic playlist icon).
        let texture = cover_path.as_deref().and_then(crate::ui::widgets::thumb_cached);
        let cover =
            crate::ui::widgets::rounded_image(texture.as_ref(), "view-list-symbolic", 160);
        cover.set_halign(gtk::Align::Center);
        content.append(&cover);

        let title = gtk::Label::builder()
            .label(name)
            .css_classes(["title-2"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        content.append(&title);

        let mut meta = vec![ngettext_n("{n} track", "{n} tracks", paths.len() as u32)];
        if total_ms > 0 {
            meta.push(crate::ui::app::fmt_duration(total_ms));
        }
        content.append(
            &gtk::Label::builder()
                .label(&meta.join(" · "))
                .css_classes(["dim-label"])
                .build(),
        );

        // Actions.
        let group = adw::PreferencesGroup::builder().margin_top(6).build();
        let empty = paths.is_empty();
        let row = |icon: &str, label: &str| -> adw::ActionRow {
            let r = adw::ActionRow::builder().title(label).activatable(true).build();
            r.add_prefix(&gtk::Image::from_icon_name(icon));
            r
        };

        let play = row("media-playback-start-symbolic", &gettext("Play playlist"));
        play.set_sensitive(!empty);
        {
            let sender = sender.clone();
            let dialog = dialog.clone();
            play.connect_activated(move |_| {
                sender.input(Msg::PlayPlaylist(id));
                dialog.close();
            });
        }
        group.add(&play);

        let shuffle = row("media-playlist-shuffle-symbolic", &gettext("Shuffle playlist"));
        shuffle.set_sensitive(!empty);
        {
            let sender = sender.clone();
            let dialog = dialog.clone();
            shuffle.connect_activated(move |_| {
                sender.input(Msg::PlayPlaylistShuffled(id));
                dialog.close();
            });
        }
        group.add(&shuffle);

        let show = row("view-list-symbolic", &gettext("Show songs"));
        {
            let sender = sender.clone();
            let dialog = dialog.clone();
            show.connect_activated(move |_| {
                sender.input(Msg::OpenPlaylist(id));
                dialog.close();
            });
        }
        group.add(&show);

        let rename = row("document-edit-symbolic", &gettext("Rename"));
        {
            let sender = sender.clone();
            let dialog = dialog.clone();
            rename.connect_activated(move |_| {
                sender.input(Msg::PlaylistRenameDialog(id));
                dialog.close();
            });
        }
        group.add(&rename);

        let delete = row("user-trash-symbolic", &gettext("Delete playlist"));
        delete.add_css_class("error");
        {
            let sender = sender.clone();
            let dialog = dialog.clone();
            delete.connect_activated(move |_| {
                sender.input(Msg::PlaylistDelete(id));
                dialog.close();
            });
        }
        group.add(&delete);

        content.append(&group);

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&content)
            .build();
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(root));
    }

    /// Dialog: enter a name and create a new (empty) playlist.
    pub(crate) fn open_new_playlist_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::AlertDialog::new(Some(&gettext("New playlist")), None);
        let entry = gtk::Entry::builder()
            .placeholder_text(&gettext("Playlist name"))
            .activates_default(true)
            .build();
        crate::ui::widgets::no_autofocus(&entry);
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[
            ("cancel", &gettext("Cancel")),
            ("create", &gettext("Create")),
        ]);
        dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("create"));
        {
            let sender = sender.clone();
            dialog.connect_response(None, move |_, resp| {
                if resp == "create" {
                    sender.input(Msg::PlaylistCreate(entry.text().to_string()));
                }
            });
        }
        dialog.present(Some(root));
    }

    /// Dialog: add the current context files to an existing playlist
    /// or create a new one.
    pub(crate) fn open_add_to_playlist_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let playlists = self.library.playlists().unwrap_or_default();

        let dialog = adw::Dialog::builder()
            .title(&gettext("Add to playlist"))
            .build();
        // Roomier window, like the detail view (bottom sheet on the phone).
        dialog.set_content_width(400);
        dialog.set_content_height(560);
        self.adapt_detail_dialog(&dialog);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Create a new playlist (enter a name, Enter confirms).
        let new_group = adw::PreferencesGroup::builder().title(&gettext("New playlist")).build();
        let entry = adw::EntryRow::builder().title(&gettext("Name")).build();
        crate::ui::widgets::no_autofocus(&entry);
        new_group.add(&entry);
        content.append(&new_group);
        {
            let sender = sender.clone();
            let entry2 = entry.clone();
            let dialog2 = dialog.clone();
            entry.connect_entry_activated(move |_| {
                if !entry2.text().trim().is_empty() {
                    sender.input(Msg::PlaylistCreateAddTo(entry2.text().to_string()));
                    dialog2.close();
                }
            });
        }

        // Existing playlists (tap = add).
        if !playlists.is_empty() {
            let group = adw::PreferencesGroup::builder().title(&gettext("Existing")).build();
            for (id, name, count) in playlists {
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(&name))
                    .subtitle(ngettext_n("{n} track", "{n} tracks", count as u32))
                    .activatable(true)
                    .build();
                row.add_prefix(&gtk::Image::from_icon_name("view-list-symbolic"));
                {
                    let sender = sender.clone();
                    let dialog2 = dialog.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::PlaylistAddTo(id));
                        dialog2.close();
                    });
                }
                group.add(&row);
            }
            content.append(&group);
        }

        let scroller = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&content)
            .build();
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(root));
    }

    /// Dialog: rename a playlist (name prefilled).
    pub(crate) fn open_rename_playlist_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        id: i64,
    ) {
        let current = self
            .playlists
            .playlist_items
            .iter()
            .find(|(pid, _, _)| *pid == id)
            .map(|(_, n, _)| n.clone())
            .unwrap_or_default();
        let dialog = adw::AlertDialog::new(Some(&gettext("Rename playlist")), None);
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
                    sender.input(Msg::PlaylistRename {
                        id,
                        name: entry.text().to_string(),
                    });
                }
            });
        }
        dialog.present(Some(root));
    }

    /// Adds the files of the current context target to a playlist.
    pub(crate) fn add_context_to_playlist(&mut self, id: i64, sender: &ComponentSender<Self>) {
        let Some(target) = self.nav.context_target.clone() else {
            return;
        };
        let files: Vec<String> = self
            .ctx_files(&target)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let n = files.len();
        let _ = self.library.add_to_playlist(id, &files);
        self.reload_playlists(sender);
        self.toast(&gettext_f("Added {n} to the playlist", &[("n", &n.to_string())]));
    }
}
