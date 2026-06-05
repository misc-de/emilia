//! Favorites (star in "More info"), audiobooks and concerts share a unified
//! entry model `(scope, key, title, is_dir)`. This module builds the lists
//! (with album/artist cover), toggles the favorite status and resolves
//! playback/detail/cover uniformly.

use std::path::{Path, PathBuf};

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::category::album_key;
use crate::i18n::gettext;
use crate::ui::app::{cover_widget, duration_label, App, CtxTarget, Msg};
use crate::ui::fs_row::FsEntry;

impl App {
    /// Identity (scope, key, display name, is_dir) of a detail target for the
    /// favorites table.
    pub(crate) fn favorite_ref(&self, target: &CtxTarget) -> (&'static str, String, String, bool) {
        match target {
            CtxTarget::Artist(m) => ("artist", m.name.clone(), m.name.clone(), false),
            CtxTarget::Album(m) => (
                "album",
                album_key(&m.artist, &m.album),
                m.album.clone(),
                false,
            ),
            // Remote entries: referenced via their rel path (not present
            // locally). This keeps favorites/markers consistently addressable.
            CtxTarget::Fs(e) if e.is_remote() => {
                let key = e.rel_path().unwrap_or_default().to_string();
                let scope = if e.is_dir() { "folder" } else { "track" };
                (scope, key, e.display_title(), e.is_dir())
            }
            CtxTarget::Fs(e) if e.is_dir() => {
                let p = e.path().map(|p| p.to_path_buf()).unwrap_or_default();
                let path = p.to_string_lossy().into_owned();
                let name = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&path)
                    .to_string();
                ("folder", path, name, true)
            }
            CtxTarget::Fs(e) => {
                let p = e.path().map(|p| p.to_path_buf()).unwrap_or_default();
                let path = p.to_string_lossy().into_owned();
                let title = crate::core::scanner::read_meta(&p)
                    .0
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| {
                        p.file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or(&path)
                            .to_string()
                    });
                ("track", path, title, false)
            }
        }
    }

    /// Whether the current detail target is a favorite.
    pub(crate) fn target_is_favorite(&self, target: &CtxTarget) -> bool {
        let (scope, key, _, _) = self.favorite_ref(target);
        self.library.is_favorite(scope, &key)
    }

    // ---- Build lists ----

    /// Loads the favorites and rebuilds the list (with cover, trash button,
    /// drag handle for reordering).
    pub(crate) fn load_favorites(&mut self, sender: &ComponentSender<Self>) {
        self.favorites.favorite_items = self.library.favorites().unwrap_or_default();
        let items = self.favorites.favorite_items.clone();
        self.fill_entry_list(
            &self.favorites.favorites_list,
            &items,
            sender,
            Msg::PlayFavorite,
            // No trash button - removal via long press ("More info" → star).
            None,
            Msg::ShowFavoriteDetail,
            Some(|from, to| Msg::MoveFavorite { from, to }),
            true,
            false,
        );
    }

    /// Loads the audiobooks (the "Audiobooks" area) - only **albums and single
    /// tracks** are listed. A folder marked as an audiobook is resolved into the
    /// albums and loose tracks it contains (no folder entry).
    pub(crate) fn load_audiobooks(&mut self, sender: &ComponentSender<Self>) {
        // Include folders to resolve them into albums/single tracks; no
        // artists - only albums and single tracks are listed.
        let raw = self
            .library
            .area_entries(crate::core::category::Area::Audiobooks, true, false);
        self.favorites.audiobook_items = self.expand_area_items(raw);
        let items = self.favorites.audiobook_items.clone();
        if self.libview.gallery_view {
            let tiles = self.entry_gallery_items(&items);
            self.fill_gallery(
                &self.favorites.audiobooks_gallery,
                &tiles,
                Msg::OpenAudiobookEntry,
                Msg::ShowAudiobookDetail,
            );
        } else {
            self.fill_entry_list(
                &self.favorites.audiobooks_list,
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
    }

    /// Builds an entry list: cover (album/artist), title, subtitle,
    /// playback (tap), detail (long press), optional trash button and
    /// optional drag handle for reordering.
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
        // For track entries, use the subtitle "<album> / <duration>" instead of "Track".
        track_subtitle: bool,
        // Render folder entries as albums (subtitle "Album", album icon).
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
            // Cover/icon flush to the far left (no prefix inner spacing).
            row.add_css_class("emilia-flush");

            // Cover (album/artist/track) or matching placeholder icon.
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
                    .tooltip_text(gettext("Remove"))
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
            // In concerts/audiobooks an album/folder opens its track list
            // (no direct playback → no play icon, but a chevron instead);
            // only single tracks are played directly and carry the play icon.
            let opens_list = folder_as_album && scope != "track";
            if opens_list {
                // Far right: total duration + play button (plays the whole
                // album/concert). A normal click still opens the list.
                let total_ms = self.entry_total_ms(scope, key);
                if total_ms > 0 {
                    row.add_suffix(&duration_label(total_ms));
                }
                let play_btn = gtk::Button::builder()
                    .icon_name("media-playback-start-symbolic")
                    .tooltip_text(gettext("Play"))
                    .valign(gtk::Align::Center)
                    .css_classes(["flat"])
                    .build();
                {
                    let sender = sender.clone();
                    play_btn.connect_clicked(move |_| sender.input(play(i)));
                }
                row.add_suffix(&play_btn);
            } else {
                // If exactly this track is currently playing, show a pause icon.
                let is_active = scope == "track"
                    && self
                        .transport
                        .playing_path
                        .as_ref()
                        .is_some_and(|p| p.to_string_lossy().as_ref() == key.as_str());
                let play_icon = if is_active && self.mini.playing {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                };
                row.add_suffix(&gtk::Image::from_icon_name(play_icon));
            }

            // Drag handle for reordering (if allowed) - far right. The
            // DragSource sits on the whole row; the handle is just the
            // visible grab zone.
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

    /// Converts entry tuples (favorites/concerts/audiobooks) into gallery tiles
    /// `(cover, placeholder icon, title)` - cover as in the list.
    pub(crate) fn entry_gallery_items(
        &self,
        items: &[(String, String, String, bool)],
    ) -> Vec<(Option<String>, &'static str, String)> {
        items
            .iter()
            .map(|(scope, key, title, is_dir)| {
                let icon = if scope == "folder" {
                    "media-optical-symbolic"
                } else {
                    entry_icon(scope)
                };
                (self.entry_cover(scope, key, *is_dir), icon, title.clone())
            })
            .collect()
    }

    /// Total duration (ms) of an entry shown as an album/folder
    /// (for the duration display in concert/audiobook lists). 0 = unknown.
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

    // ---- Resolution (cover / playback / detail) ----

    /// Cover of an entry: album cover, artist photo or (for tracks) the
    /// embedded cover or the track's album cover.
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

    /// Fallback image for an artist **without a photo**: the cover of one of
    /// their albums (the first one with a cover).
    pub(crate) fn artist_album_cover(&self, name: &str) -> Option<String> {
        // Indexed lookup of the artist's own albums instead of loading and
        // grouping the whole track table (was O(tracks) per photoless artist).
        self.library
            .albums_of_artist(name)
            .unwrap_or_default()
            .into_iter()
            .find_map(|album| self.album_cover_for(name, &album))
    }

    /// Album cover: first an exact match (artist, album), otherwise any of the album.
    pub(crate) fn album_cover_for(&self, artist: &str, album: &str) -> Option<String> {
        self.library
            .get_album_meta(artist, album)
            .ok()
            .flatten()
            .and_then(|m| m.cover_path)
            .or_else(|| self.library.album_cover(album).ok().flatten())
            .or_else(|| {
                self.library
                    .album_track_paths(artist, album)
                    .unwrap_or_default()
                    .into_iter()
                    .find_map(|p| crate::core::online::local_track_cover(&p))
            })
            .or_else(|| {
                self.library
                    .album_track_paths_by_name(album)
                    .unwrap_or_default()
                    .into_iter()
                    .find_map(|p| crate::core::online::local_track_cover(&p))
            })
    }

    /// Local cover of an album from its tracks' embedded/cached images only (no
    /// album_meta lookup). The per-track-scan fallback for the album overview,
    /// used once the batched `album_meta_covers` map has no entry for it.
    pub(crate) fn album_local_cover(&self, artist: &str, album: &str) -> Option<String> {
        self.library
            .album_track_paths(artist, album)
            .unwrap_or_default()
            .into_iter()
            .find_map(|p| crate::core::online::local_track_cover(&p))
            .or_else(|| {
                self.library
                    .album_track_paths_by_name(album)
                    .unwrap_or_default()
                    .into_iter()
                    .find_map(|p| crate::core::online::local_track_cover(&p))
            })
    }

    /// Cover of a folder: cover of any track within it.
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

    /// Plays an entry (scope/key).
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

    /// Detail target (for "More info") of an entry.
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

    /// Subtitle of a track entry: "<album> / <duration>" (the parts present).
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

    /// Queue = the given files starting at track 1, unless empty.
    fn play_track_set(&mut self, files: Vec<PathBuf>) {
        if files.is_empty() {
            return;
        }
        self.transport.queue = files;
        self.transport.queue_pos = 0;
        self.play_current();
        self.refresh_queue_icons();
    }

    /// Toggle the favorite flag on the current context target.
    pub(crate) fn toggle_favorite(&mut self, sender: &ComponentSender<Self>) {
        if let Some(target) = self.nav.context_target.clone() {
            let (scope, key, title, is_dir) = self.favorite_ref(&target);
            let on = !self.library.is_favorite(scope, &key);
            let _ = self.library.set_favorite(scope, &key, &title, is_dir, on);
            self.load_favorites(sender);
            self.toast(&if on {
                gettext("Added to favorites")
            } else {
                gettext("Removed from favorites")
            });
        }
    }

    /// Play (or toggle) the favorite at `index`. A track plays the whole
    /// favorites track list as the queue, starting at that track.
    pub(crate) fn play_favorite(&mut self, sender: &ComponentSender<Self>, index: usize) {
        let Some((scope, key, _, is_dir)) = self.favorites.favorite_items.get(index).cloned()
        else {
            return;
        };
        // If exactly this track is already playing, only toggle play/pause (a
        // click on the shown pause sign pauses), instead of restarting it.
        let is_current = scope == "track"
            && self
                .transport
                .playing_path
                .as_ref()
                .is_some_and(|p| p.to_string_lossy().as_ref() == key.as_str());
        if is_current {
            if self.mini.playing {
                self.save_resume();
                self.player.pause();
                self.mini.playing = false;
            } else {
                self.player.resume();
                self.mini.playing = true;
            }
            self.mpris.set_playing(self.mini.playing);
            self.refresh_queue_icons();
        } else if scope == "track" {
            // Whole favorites track list as the queue (clear the previous one),
            // from the clicked track.
            let tracks: Vec<PathBuf> = self
                .favorites
                .favorite_items
                .iter()
                .filter(|(s, _, _, _)| s == "track")
                .map(|(_, k, _, _)| PathBuf::from(k))
                .collect();
            let pos = tracks
                .iter()
                .position(|p| p.as_path() == Path::new(&key))
                .unwrap_or(0);
            self.transport.shuffle = false;
            self.transport.queue = tracks;
            self.transport.queue_pos = pos;
            self.play_current();
            self.refresh_queue_icons();
        } else {
            self.play_entry(&scope, &key, is_dir);
        }
        // Update the active marking (play/pause icon) in the favorites list.
        self.load_favorites(sender);
    }

    /// Reorder favorites (drag handle): move item `from` → `to` and persist.
    pub(crate) fn move_favorite(&mut self, sender: &ComponentSender<Self>, from: usize, to: usize) {
        if from < self.favorites.favorite_items.len()
            && to < self.favorites.favorite_items.len()
            && from != to
        {
            let item = self.favorites.favorite_items.remove(from);
            self.favorites.favorite_items.insert(to, item);
            let order: Vec<(String, String)> = self
                .favorites
                .favorite_items
                .iter()
                .map(|(s, k, _, _)| (s.clone(), k.clone()))
                .collect();
            let _ = self.library.set_favorite_order(&order);
            self.load_favorites(sender);
        }
    }
}

/// Placeholder icon per level (if no cover is available).
pub(crate) fn entry_icon(scope: &str) -> &'static str {
    match scope {
        "album" => "media-optical-symbolic",
        "artist" => "avatar-default-symbolic",
        "folder" => "folder-symbolic",
        _ => "audio-x-generic-symbolic",
    }
}

/// Subtitle label per level.
fn entry_kind(scope: &str) -> String {
    match scope {
        "album" => gettext("Album"),
        "artist" => gettext("Artist"),
        "folder" => gettext("Folder"),
        _ => gettext("Track"),
    }
}
