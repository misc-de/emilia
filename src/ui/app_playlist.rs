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
            // Long press: rename.
            let long_press = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                long_press.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::PlaylistRenameDialog(id));
                });
            }
            row.add_controller(long_press);
            self.playlists.playlists_list.append(&row);
        }
    }

    /// Opens the track subpage of a playlist (tap = play from here).
    pub(crate) fn open_playlist(&self, sender: &ComponentSender<Self>, id: i64, name: &str) {
        let paths = self.library.playlist_paths(id).unwrap_or_default();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        let group = adw::PreferencesGroup::builder()
            .title(gtk::glib::markup_escape_text(name))
            .description(ngettext_n("{n} track", "{n} tracks", paths.len() as u32))
            .build();

        if paths.is_empty() {
            group.add(&adw::ActionRow::builder().title(&gettext("No tracks yet")).build());
        }
        for path in &paths {
            let display = self.display_name(std::path::Path::new(path));
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&display))
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("audio-x-generic-symbolic"));
            let remove = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text(&gettext("Remove from playlist"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let sender = sender.clone();
                let path = path.clone();
                remove.connect_clicked(move |b| {
                    crate::ui::app::confirm_destructive(
                        b,
                        &gettext("Remove this track from the playlist?"),
                        &gettext("Remove"),
                        sender.clone(),
                        Msg::PlaylistRemoveTrack {
                            id,
                            path: path.clone(),
                        },
                    );
                });
            }
            row.add_suffix(&remove);
            row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
            {
                let sender = sender.clone();
                let path = path.clone();
                row.connect_activated(move |_| {
                    sender.input(Msg::PlaylistTrack {
                        id,
                        path: path.clone(),
                    });
                });
            }
            group.add(&row);
        }
        content.append(&group);
        self.push_subpage(&gettext_f("Playlist – {name}", &[("name", name)]), &content);
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
