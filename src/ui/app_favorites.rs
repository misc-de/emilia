//! Favoriten (Stern in „Mehr Infos"), Hörbücher und Konzerte teilen sich ein
//! einheitliches Eintrags-Modell `(scope, key, Titel, is_dir)`. Dieses Modul
//! baut die Listen (mit Album-/Interpreten-Cover), schaltet den Favoritenstatus
//! um und löst Abspielen/Detail/Cover einheitlich auf.

use std::path::PathBuf;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::category::album_key;
use crate::i18n::gettext;
use crate::ui::app::{cover_widget, duration_label, App, CtxTarget, Msg};
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

    // ---- Listen aufbauen ----

    /// Lädt die Favoriten und baut die Liste neu auf (mit Cover, Mülleimer,
    /// Ziehgriff zum Umsortieren).
    pub(crate) fn load_favorites(&mut self, sender: &ComponentSender<Self>) {
        self.favorite_items = self.library.favorites().unwrap_or_default();
        let items = self.favorite_items.clone();
        self.fill_entry_list(
            &self.favorites_list,
            &items,
            sender,
            Msg::PlayFavorite,
            // Kein Mülleimer – Entfernen über langes Drücken („Mehr Infos" → Stern).
            None,
            Msg::ShowFavoriteDetail,
            Some(|from, to| Msg::MoveFavorite { from, to }),
            true,
            false,
        );
    }

    /// Lädt die Hörbücher (Bereich „Hörbücher") – gelistet werden nur **Alben
    /// und Einzelstücke**. Ein als Hörbuch markierter Ordner wird in die darin
    /// enthaltenen Alben und losen Titel aufgelöst (kein Ordner-Eintrag).
    pub(crate) fn load_audiobooks(&mut self, sender: &ComponentSender<Self>) {
        // Ordner mitnehmen, um sie in Alben/Einzelstücke aufzulösen; keine
        // Interpreten – gelistet werden nur Alben und Einzelstücke.
        let raw = self
            .library
            .area_entries(crate::core::category::Area::Audiobooks, true, false);
        self.audiobook_items = self.expand_area_items(raw);
        let items = self.audiobook_items.clone();
        self.fill_entry_list(
            &self.audiobooks_list,
            &items,
            sender,
            Msg::PlayAudiobook,
            None,
            Msg::ShowAudiobookDetail,
            None,
            false,
            true,
        );
    }

    /// Baut eine Eintragsliste: Cover (Album/Interpret), Titel, Untertitel,
    /// Abspielen (Tippen), Detail (langes Drücken), optional Mülleimer und
    /// optional Ziehgriff zum Umsortieren.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fill_entry_list(
        &self,
        list: &gtk::ListBox,
        items: &[(String, String, String, bool)],
        sender: &ComponentSender<Self>,
        play: fn(usize) -> Msg,
        remove: Option<fn(usize) -> Msg>,
        detail: fn(usize) -> Msg,
        move_msg: Option<fn(usize, usize) -> Msg>,
        // Bei Titel-Einträgen statt „Track" den Untertitel „<Album> / <Dauer>".
        track_subtitle: bool,
        // Ordner-Einträge als Alben darstellen (Untertitel „Album", Album-Icon).
        folder_as_album: bool,
    ) {
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        for (i, (scope, key, title, is_dir)) in items.iter().enumerate() {
            let subtitle = if track_subtitle && scope == "track" {
                self.track_meta_subtitle(key)
            } else if folder_as_album && scope == "folder" {
                gettext("Album")
            } else {
                entry_kind(scope)
            };
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(title))
                .subtitle(&subtitle)
                .activatable(true)
                .build();
            // Cover/Icon bündig nach ganz links (kein Prefix-Innenabstand).
            row.add_css_class("emilia-flush");

            // Cover (Album/Interpret/Titel) oder passendes Platzhalter-Icon.
            let icon = if folder_as_album && scope == "folder" {
                "media-optical-symbolic"
            } else {
                entry_icon(scope)
            };
            let cover = self.entry_cover(scope, key, *is_dir);
            row.add_prefix(&cover_widget(cover.as_deref(), icon));

            if let Some(remove) = remove {
                let btn = gtk::Button::builder()
                    .icon_name("user-trash-symbolic")
                    .tooltip_text(&gettext("Remove"))
                    .valign(gtk::Align::Center)
                    .css_classes(["flat"])
                    .build();
                let sender = sender.clone();
                btn.connect_clicked(move |b| {
                    crate::ui::app::confirm_destructive(
                        b,
                        &gettext("Remove this entry?"),
                        &gettext("Remove"),
                        sender.clone(),
                        remove(i),
                    );
                });
                row.add_suffix(&btn);
            }
            // In Konzerten/Hörbüchern öffnet ein Album/Ordner seine Titelliste
            // (kein direktes Abspielen → kein Play-Icon, sondern ein Chevron);
            // nur Einzelstücke werden direkt abgespielt und tragen das Play-Icon.
            let opens_list = folder_as_album && scope != "track";
            if opens_list {
                // Ganz rechts: Gesamtlaufzeit + Play-Button (spielt das ganze
                // Album/Konzert). Ein normaler Klick öffnet weiterhin die Liste.
                let total_ms = self.entry_total_ms(scope, key);
                if total_ms > 0 {
                    row.add_suffix(&duration_label(total_ms));
                }
                let play_btn = gtk::Button::builder()
                    .icon_name("media-playback-start-symbolic")
                    .tooltip_text(&gettext("Play"))
                    .valign(gtk::Align::Center)
                    .css_classes(["flat"])
                    .build();
                {
                    let sender = sender.clone();
                    play_btn.connect_clicked(move |_| sender.input(play(i)));
                }
                row.add_suffix(&play_btn);
            } else {
                // Läuft genau dieser Titel gerade, ein Pause-Symbol zeigen.
                let is_active = scope == "track"
                    && self
                        .playing_path
                        .as_ref()
                        .is_some_and(|p| p.to_string_lossy().as_ref() == key.as_str());
                let play_icon = if is_active && self.playing {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                };
                row.add_suffix(&gtk::Image::from_icon_name(play_icon));
            }

            // Ziehgriff zum Umsortieren (falls erlaubt) – ganz rechts. Die
            // DragSource sitzt auf der ganzen Zeile; der Griff ist nur die
            // sichtbare Greifzone.
            if let Some(move_msg) = move_msg {
                let handle = gtk::Image::from_icon_name("list-drag-handle-symbolic");
                handle.set_tooltip_text(Some(&gettext("Drag to reorder")));
                row.add_suffix(&handle);

                let drag = gtk::DragSource::new();
                drag.set_actions(gtk::gdk::DragAction::MOVE);
                drag.connect_prepare(move |_, _, _| {
                    Some(gtk::gdk::ContentProvider::for_value(&(i as i32).to_value()))
                });
                row.add_controller(drag);

                let drop = gtk::DropTarget::new(i32::static_type(), gtk::gdk::DragAction::MOVE);
                {
                    let sender = sender.clone();
                    drop.connect_drop(move |_, value, _, _| match value.get::<i32>() {
                        Ok(from) => {
                            sender.input(move_msg(from as usize, i));
                            true
                        }
                        Err(_) => false,
                    });
                }
                row.add_controller(drop);
            }

            {
                let sender = sender.clone();
                if opens_list {
                    let (scope, key) = (scope.clone(), key.clone());
                    row.connect_activated(move |_| {
                        sender.input(Msg::OpenEntryTracks {
                            scope: scope.clone(),
                            key: key.clone(),
                        });
                    });
                } else {
                    row.connect_activated(move |_| sender.input(play(i)));
                }
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

    /// Gesamtlaufzeit (ms) eines als Album/Ordner dargestellten Eintrags
    /// (für die Laufzeitanzeige in Konzert-/Hörbuchlisten). 0 = unbekannt.
    fn entry_total_ms(&self, scope: &str, key: &str) -> i64 {
        let tracks = match scope {
            "album" => {
                let mut parts = key.splitn(2, '\u{1}');
                let artist = parts.next().unwrap_or("");
                let album = parts.next().unwrap_or("");
                self.album_tracks_for_artist(artist, album)
            }
            "folder" => self.folder_tracks_ordered(key),
            _ => Vec::new(),
        };
        tracks.iter().filter_map(|t| t.duration_ms).sum()
    }

    // ---- Auflösung (Cover / Abspielen / Detail) ----

    /// Cover eines Eintrags: Album-Cover, Interpreten-Foto oder (bei Titeln) das
    /// eingebettete bzw. das Album-Cover des Titels.
    pub(crate) fn entry_cover(&self, scope: &str, key: &str, _is_dir: bool) -> Option<String> {
        match scope {
            "album" => {
                let mut parts = key.splitn(2, '\u{1}');
                let artist = parts.next().unwrap_or("");
                let album = parts.next().unwrap_or("");
                self.album_cover_for(artist, album)
            }
            "artist" => self
                .library
                .get_artist_meta(key)
                .ok()
                .flatten()
                .and_then(|m| m.image_path)
                .or_else(|| self.artist_album_cover(key)),
            "track" => crate::core::online::local_track_cover(key).or_else(|| {
                let t = self.library.track_by_path(key).ok().flatten()?;
                let album = t.album.as_deref().filter(|a| !a.trim().is_empty())?;
                self.album_cover_for(t.artist.as_deref().unwrap_or(""), album)
            }),
            "folder" => self.folder_cover(key),
            _ => None,
        }
    }

    /// Fallback-Bild für einen Interpreten **ohne Foto**: das Cover eines seiner
    /// Alben (das erste mit Cover).
    pub(crate) fn artist_album_cover(&self, name: &str) -> Option<String> {
        self.artist_albums(name)
            .into_iter()
            .find_map(|(album, _)| self.album_cover_for(name, &album))
    }

    /// Album-Cover: erst exakt (Interpret, Album), sonst irgendeines des Albums.
    fn album_cover_for(&self, artist: &str, album: &str) -> Option<String> {
        self.library
            .get_album_meta(artist, album)
            .ok()
            .flatten()
            .and_then(|m| m.cover_path)
            .or_else(|| self.library.album_cover(album).ok().flatten())
    }

    /// Cover eines Ordners: Cover eines beliebigen Titels darin.
    fn folder_cover(&self, folder: &str) -> Option<String> {
        let prefix = format!("{}/", folder.trim_end_matches('/'));
        let t = self
            .library
            .all_tracks()
            .ok()?
            .into_iter()
            .find(|t| t.path.starts_with(&prefix))?;
        crate::core::online::local_track_cover(&t.path).or_else(|| {
            let album = t.album.as_deref().filter(|a| !a.trim().is_empty())?;
            self.album_cover_for(t.artist.as_deref().unwrap_or(""), album)
        })
    }

    /// Spielt einen Eintrag (scope/key) ab.
    pub(crate) fn play_entry(&mut self, scope: &str, key: &str, is_dir: bool) {
        match scope {
            "track" => self.play_path(key, false),
            "folder" => self.play_path(key, is_dir),
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
                let files = self.artist_files(key);
                self.play_track_set(files);
            }
            _ => {}
        }
    }

    /// Detailziel (für „Mehr Infos") eines Eintrags.
    pub(crate) fn entry_target(&self, scope: &str, key: &str, is_dir: bool) -> CtxTarget {
        match scope {
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
            "artist" => CtxTarget::Artist(crate::model::ArtistMeta::pending(key.to_string())),
            _ => {
                let path = PathBuf::from(key);
                CtxTarget::Fs(if is_dir {
                    FsEntry::dir(path)
                } else {
                    FsEntry::file(path)
                })
            }
        }
    }

    /// Untertitel eines Titel-Eintrags: „<Album> / <Dauer>" (vorhandene Teile).
    fn track_meta_subtitle(&self, path: &str) -> String {
        let Some(t) = self.library.track_by_path(path).ok().flatten() else {
            return entry_kind("track");
        };
        let album = t.album.unwrap_or_default();
        let album = album.trim();
        let dur = t
            .duration_ms
            .filter(|ms| *ms > 0)
            .map(crate::ui::app::fmt_duration)
            .unwrap_or_default();
        match (album.is_empty(), dur.is_empty()) {
            (false, false) => format!("{album} / {dur}"),
            (false, true) => album.to_string(),
            (true, false) => dur,
            (true, true) => entry_kind("track"),
        }
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

/// Platzhalter-Icon je Ebene (falls kein Cover vorhanden).
fn entry_icon(scope: &str) -> &'static str {
    match scope {
        "album" => "media-optical-symbolic",
        "artist" => "avatar-default-symbolic",
        "folder" => "folder-symbolic",
        _ => "audio-x-generic-symbolic",
    }
}

/// Untertitel-Kennzeichnung je Ebene.
fn entry_kind(scope: &str) -> String {
    match scope {
        "album" => gettext("Album"),
        "artist" => gettext("Artist"),
        "folder" => gettext("Folder"),
        _ => gettext("Track"),
    }
}
