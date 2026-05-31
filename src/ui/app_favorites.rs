//! Favoriten (Stern in „Mehr Infos") und Hörbücher (Bereich „Hörbücher"):
//! Listen aufbauen, Favoritenstatus umschalten und Einträge abspielen.

use std::path::PathBuf;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::category::album_key;
use crate::ui::app::{App, CtxTarget, Msg};
use crate::ui::fs_row::FsEntry;

impl App {
    /// Identität (scope, key, Anzeigename, is_dir) eines Detailziels für die
    /// Favoriten-Tabelle.
    pub(crate) fn favorite_ref(&self, target: &CtxTarget) -> (&'static str, String, String, bool) {
        match target {
            CtxTarget::Artist(m) => ("artist", m.name.clone(), m.name.clone(), false),
            CtxTarget::Album(m) => (
                "album",
                album_key(&m.artist, &m.album),
                m.album.clone(),
                false,
            ),
            CtxTarget::Fs(e) if e.is_dir() => {
                let path = e.path().to_string_lossy().into_owned();
                let name = e
                    .path()
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&path)
                    .to_string();
                ("folder", path, name, true)
            }
            CtxTarget::Fs(e) => {
                let path = e.path().to_string_lossy().into_owned();
                let title = crate::core::scanner::read_meta(e.path())
                    .0
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| {
                        e.path()
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or(&path)
                            .to_string()
                    });
                ("track", path, title, false)
            }
        }
    }

    /// Ob das aktuelle Detailziel ein Favorit ist.
    pub(crate) fn target_is_favorite(&self, target: &CtxTarget) -> bool {
        let (scope, key, _, _) = self.favorite_ref(target);
        self.library.is_favorite(scope, &key)
    }

    /// Lädt die Favoriten aus der DB und baut die Liste neu auf.
    pub(crate) fn load_favorites(&mut self, sender: &ComponentSender<Self>) {
        self.favorite_items = self.library.favorites().unwrap_or_default();
        Self::fill_entry_list(
            &self.favorites_list,
            self.favorite_items
                .iter()
                .map(|(scope, _key, title, is_dir)| (fav_icon(scope), title.clone(), fav_kind(scope), *is_dir)),
            sender,
            |i| Msg::PlayFavorite(i),
            Some(|i| Msg::FavoriteRemove(i)),
            |i| Msg::ShowFavoriteDetail(i),
        );
    }

    /// Lädt die Hörbücher (aus dem Bereich „Hörbücher") und baut die Liste auf.
    pub(crate) fn load_audiobooks(&mut self, sender: &ComponentSender<Self>) {
        self.audiobook_items = self.library.audiobook_entries().unwrap_or_default();
        Self::fill_entry_list(
            &self.audiobooks_list,
            self.audiobook_items.iter().map(|(_p, title, is_dir)| {
                let icon = if *is_dir {
                    "folder-symbolic"
                } else {
                    "audio-x-generic-symbolic"
                };
                let kind = if *is_dir { "Album/Ordner" } else { "Datei" };
                (icon, title.clone(), kind, *is_dir)
            }),
            sender,
            |i| Msg::PlayAudiobook(i),
            None,
            |i| Msg::ShowAudiobookDetail(i),
        );
    }

    /// Baut eine einfache Eintragsliste (Icon, Titel, Untertitel) mit Abspielen
    /// (Tippen), optionalem Mülleimer und Detailansicht (langes Drücken).
    fn fill_entry_list(
        list: &gtk::ListBox,
        rows: impl Iterator<Item = (&'static str, String, &'static str, bool)>,
        sender: &ComponentSender<Self>,
        play: fn(usize) -> Msg,
        remove: Option<fn(usize) -> Msg>,
        detail: fn(usize) -> Msg,
    ) {
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        for (i, (icon, title, kind, _is_dir)) in rows.enumerate() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&title))
                .subtitle(kind)
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(icon));

            if let Some(remove) = remove {
                let btn = gtk::Button::builder()
                    .icon_name("user-trash-symbolic")
                    .tooltip_text("Entfernen")
                    .valign(gtk::Align::Center)
                    .css_classes(["flat"])
                    .build();
                let sender = sender.clone();
                btn.connect_clicked(move |_| sender.input(remove(i)));
                row.add_suffix(&btn);
            }
            row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));

            {
                let sender = sender.clone();
                row.connect_activated(move |_| sender.input(play(i)));
            }
            let long_press = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                long_press.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(detail(i));
                });
            }
            row.add_controller(long_press);

            list.append(&row);
        }
    }

    /// Spielt einen Favoriten anhand seiner gespeicherten Kennung ab.
    pub(crate) fn play_favorite(&mut self, index: usize) {
        let Some((scope, key, _title, is_dir)) = self.favorite_items.get(index).cloned() else {
            return;
        };
        match scope.as_str() {
            "track" => self.play_path(&key, false),
            "folder" => self.play_path(&key, is_dir),
            "album" => {
                let mut parts = key.splitn(2, '\u{1}');
                let artist = parts.next().unwrap_or("").to_string();
                let album = parts.next().unwrap_or("").to_string();
                let files: Vec<PathBuf> = self
                    .album_tracks_for_artist(&artist, &album)
                    .into_iter()
                    .map(|t| PathBuf::from(t.path))
                    .collect();
                self.play_track_set(files);
            }
            "artist" => {
                let files = self.artist_files(&key);
                self.play_track_set(files);
            }
            _ => {}
        }
    }

    /// Detailziel eines Favoriten (für die Detailansicht/„Mehr Infos").
    pub(crate) fn favorite_target(&self, index: usize) -> Option<CtxTarget> {
        let (scope, key, title, is_dir) = self.favorite_items.get(index).cloned()?;
        Some(match scope.as_str() {
            "album" => {
                let mut parts = key.splitn(2, '\u{1}');
                let artist = parts.next().unwrap_or("").to_string();
                let album = parts.next().unwrap_or("").to_string();
                let meta = self
                    .library
                    .get_album_meta(&artist, &album)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| crate::model::AlbumMeta::pending(artist, album));
                CtxTarget::Album(meta)
            }
            "artist" => CtxTarget::Artist(crate::model::ArtistMeta::pending(title)),
            _ => {
                let path = PathBuf::from(&key);
                CtxTarget::Fs(if is_dir {
                    FsEntry::dir(path)
                } else {
                    FsEntry::file(path)
                })
            }
        })
    }

    /// Queue = übergebene Dateien ab Titel 1, sofern nicht leer.
    fn play_track_set(&mut self, files: Vec<PathBuf>) {
        if files.is_empty() {
            return;
        }
        self.queue = files;
        self.queue_pos = 0;
        self.play_current();
        self.refresh_queue_icons();
    }
}

fn fav_icon(scope: &str) -> &'static str {
    match scope {
        "album" => "media-optical-symbolic",
        "artist" => "avatar-default-symbolic",
        "folder" => "folder-symbolic",
        _ => "audio-x-generic-symbolic",
    }
}

fn fav_kind(scope: &str) -> &'static str {
    match scope {
        "album" => "Album",
        "artist" => "Interpret",
        "folder" => "Ordner",
        _ => "Titel",
    }
}
