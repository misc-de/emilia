//! Playlisten: Übersichtsliste, Titel-Unterseite und die Dialoge zum Anlegen
//! bzw. Hinzufügen. Einträge sind Pfade (wie die Warteschlange).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::ui::app::{App, Msg};

impl App {
    /// Baut die Playlisten-Liste neu auf (Name, Titelzahl, Abspielen, Löschen).
    pub(crate) fn reload_playlists(&mut self, sender: &ComponentSender<Self>) {
        self.playlist_items = self.library.playlists().unwrap_or_default();

        while let Some(child) = self.playlists_list.first_child() {
            self.playlists_list.remove(&child);
        }
        for (id, name, count) in self.playlist_items.clone() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&name))
                .subtitle(format!("{count} Titel"))
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("view-list-symbolic"));

            let play = gtk::Button::builder()
                .icon_name("media-playback-start-symbolic")
                .tooltip_text("Playlist abspielen")
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
                .tooltip_text("Playlist löschen")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let sender = sender.clone();
                del.connect_clicked(move |_| sender.input(Msg::PlaylistDelete(id)));
            }
            row.add_suffix(&del);

            {
                let sender = sender.clone();
                row.connect_activated(move |_| sender.input(Msg::OpenPlaylist(id)));
            }
            // Langes Drücken: umbenennen.
            let long_press = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                long_press.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::PlaylistRenameDialog(id));
                });
            }
            row.add_controller(long_press);
            self.playlists_list.append(&row);
        }
    }

    /// Öffnet die Titel-Unterseite einer Playlist (Tippen = ab hier abspielen).
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
            .description(format!("{} Titel", paths.len()))
            .build();

        if paths.is_empty() {
            group.add(&adw::ActionRow::builder().title("Noch keine Titel").build());
        }
        for path in &paths {
            let display = Self::track_display_name(std::path::Path::new(path));
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&display))
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("audio-x-generic-symbolic"));
            let remove = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Aus Playlist entfernen")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let sender = sender.clone();
                let path = path.clone();
                remove.connect_clicked(move |_| {
                    sender.input(Msg::PlaylistRemoveTrack {
                        id,
                        path: path.clone(),
                    });
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
        self.push_subpage(&format!("Playlist – {name}"), &content);
    }

    /// Dialog: Name eingeben und eine neue (leere) Playlist anlegen.
    pub(crate) fn open_new_playlist_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::AlertDialog::new(Some("Neue Playlist"), None);
        let entry = gtk::Entry::builder()
            .placeholder_text("Name der Playlist")
            .activates_default(true)
            .build();
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", "Abbrechen"), ("create", "Erstellen")]);
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

    /// Dialog: aktuelle Kontext-Dateien zu einer bestehenden Playlist hinzufügen
    /// oder eine neue anlegen.
    pub(crate) fn open_add_to_playlist_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let playlists = self.library.playlists().unwrap_or_default();

        let dialog = adw::Dialog::builder()
            .title("Zur Playlist hinzufügen")
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

        // Neue Playlist anlegen (Name eingeben, Enter bestätigt).
        let new_group = adw::PreferencesGroup::builder().title("Neue Playlist").build();
        let entry = adw::EntryRow::builder().title("Name").build();
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

        // Bestehende Playlisten (Tippen = hinzufügen).
        if !playlists.is_empty() {
            let group = adw::PreferencesGroup::builder().title("Bestehende").build();
            for (id, name, count) in playlists {
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(&name))
                    .subtitle(format!("{count} Titel"))
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

    /// Dialog: Playlist umbenennen (Name vorbelegt).
    pub(crate) fn open_rename_playlist_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        id: i64,
    ) {
        let current = self
            .playlist_items
            .iter()
            .find(|(pid, _, _)| *pid == id)
            .map(|(_, n, _)| n.clone())
            .unwrap_or_default();
        let dialog = adw::AlertDialog::new(Some("Playlist umbenennen"), None);
        let entry = gtk::Entry::builder()
            .text(&current)
            .activates_default(true)
            .build();
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", "Abbrechen"), ("rename", "Umbenennen")]);
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

    /// Fügt die Dateien des aktuellen Kontextziels einer Playlist hinzu.
    pub(crate) fn add_context_to_playlist(&mut self, id: i64, sender: &ComponentSender<Self>) {
        let Some(target) = self.context_target.clone() else {
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
        self.toast(&format!("{n} zur Playlist hinzugefügt"));
    }
}
