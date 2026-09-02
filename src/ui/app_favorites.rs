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
use crate::model::Track;
use crate::ui::app::{App, CtxTarget, Msg};
use crate::ui::app_helpers::most_common_artist;
use crate::ui::app_views::most_common_album_base;
use crate::ui::entry_row::EntryRow;
use crate::ui::fs_row::FsEntry;
use crate::ui::play_mark::{Marks, PlaybackSink, PlaybackState};

/// How an entry list keys its play/pause controls in the shared registry:
/// scope and key, so the flip only needs the registry, not the row order.
/// The subpages (artist, album, playlist) key their rows the same way, so their
/// controls can ride along in [`crate::ui::app::LibViewState::page_marks`].
pub(crate) fn mark_key(scope: &str, key: &str) -> String {
    format!("{scope}\u{1}{key}")
}

/// Is this entry the one currently playing? A track by its path, a folder when
/// the running track lies below it, an album by name. Free-standing because
/// both the row building and the [`PlaybackSink`] below ask it — the latter
/// from the shared state, without reaching into the app.
pub(crate) fn entry_is_active(
    path: Option<&Path>,
    album: Option<&str>,
    scope: &str,
    key: &str,
) -> bool {
    let Some(path) = path else {
        return false;
    };
    match scope {
        "track" => path == Path::new(key),
        "folder" => path.starts_with(key),
        "album" => {
            let name = key.split_once('\u{1}').map_or(key, |(_, album)| album);
            album.is_some_and(|a| a.eq_ignore_ascii_case(name))
        }
        _ => false,
    }
}

/// The play controls of one entry list (favorites, concerts, audiobooks), whose
/// keys are `scope\u{1}key` — see [`mark_key`].
pub(crate) struct EntryMarks<'a>(pub(crate) &'a Marks);

impl PlaybackSink for EntryMarks<'_> {
    fn apply_playback(&self, state: &PlaybackState) {
        self.0
            .apply_all(state.playing, |key| match key.split_once('\u{1}') {
                Some((scope, key)) => {
                    entry_is_active(state.path.as_deref(), state.album.as_deref(), scope, key)
                }
                None => false,
            });
    }
}

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
        // The stored order is the user's manual drag arrangement. Only when the
        // user picks an actual sort (Name) do we reorder – and then the reorder
        // handles are hidden (a manual move would be overwritten on reload).
        let crit = self.libview.sort_for("favorites").0;
        let manual = matches!(crit, crate::ui::app::SortCrit::Manual);
        if !manual {
            self.sort_favorites();
        }
        let items = self.favorites.favorite_items.clone();
        // Alphabetical headings (by name) shared by list + gallery; none in the
        // manual order (entry_section_headers returns None for the Manual sort).
        let headers = self.entry_section_headers("favorites", &items);
        *self.libview.favorite_headers.borrow_mut() = headers.clone();
        if self.libview.gallery_on("favorites") {
            let tiles = self.entry_gallery_items(&items);
            self.fill_sectioned_gallery(
                &self.favorites.favorites_gallery_box,
                &self.favorites.favorites_gallery,
                &tiles,
                headers.as_deref(),
                |v0| Msg::Favorite(FavoriteMsg::PlayFavorite(v0)),
                |v0| Msg::Favorite(FavoriteMsg::ShowFavoriteDetail(v0)),
            );
        } else {
            // Drag-to-reorder only in the manual order; a sort would override it.
            let move_msg: Option<fn(usize, usize) -> Msg> =
                manual.then_some(|from, to| Msg::Favorite(FavoriteMsg::MoveFavorite { from, to }));
            self.fill_entry_list(
                &self.favorites.favorites_list,
                &items,
                sender,
                |v0| Msg::Favorite(FavoriteMsg::PlayFavorite(v0)),
                // No trash button - removal via long press ("More info" → star).
                None,
                |v0| Msg::Favorite(FavoriteMsg::ShowFavoriteDetail(v0)),
                move_msg,
                true,
                false,
                false,
                &self.favorites.favorite_marks,
            );
            // Refresh the section headings for the rebuilt rows (or clear them).
            self.favorites.favorites_list.invalidate_headers();
        }
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
        let mut items = self.expand_area_items(crate::core::category::Area::Audiobooks, raw);
        self.sort_entries("audiobooks", &mut items);
        self.favorites.audiobook_items = items.clone();
        // Alphabetical section headings (by name) – shared by the list and the
        // gallery, exactly like the albums overview.
        let headers = self.entry_section_headers("audiobooks", &items);
        *self.libview.audiobook_headers.borrow_mut() = headers.clone();
        if self.libview.gallery_on("audiobooks") {
            let tiles = self.entry_gallery_items(&items);
            self.fill_sectioned_gallery(
                &self.favorites.audiobooks_gallery_box,
                &self.favorites.audiobooks_gallery,
                &tiles,
                headers.as_deref(),
                |v0| Msg::Favorite(FavoriteMsg::OpenAudiobookEntry(v0)),
                |v0| Msg::Favorite(FavoriteMsg::ShowAudiobookDetail(v0)),
            );
        } else {
            self.fill_entry_list(
                &self.favorites.audiobooks_list,
                &items,
                sender,
                |v0| Msg::Favorite(FavoriteMsg::PlayAudiobook(v0)),
                None,
                |v0| Msg::Favorite(FavoriteMsg::ShowAudiobookDetail(v0)),
                None,
                false,
                true,
                true,
                &self.favorites.audiobook_marks,
            );
            // Refresh the section headings for the rebuilt rows (or clear them).
            self.favorites.audiobooks_list.invalidate_headers();
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
        // Audiobook area: say "Audiobook" instead of "Album" for folder/album rows.
        audiobook: bool,
        // Where this list keeps its play/pause controls (one registry per list).
        marks: &Marks,
    ) {
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        marks.clear();
        // Which album is running decides the play/pause icon of every row, so
        // look it up once for the whole list instead of per row.
        let playing_album = self.playing_album();
        for (i, (scope, key, title, is_dir)) in items.iter().enumerate() {
            let subtitle = if track_subtitle && scope == "track" {
                self.track_meta_subtitle(key)
            } else if audiobook && (scope == "folder" || scope == "album") {
                gettext("Audiobook")
            } else if folder_as_album && scope == "folder" {
                gettext("Album")
            } else {
                entry_kind(scope)
            };
            // Cover (album/artist/track) or matching placeholder icon.
            let icon = if folder_as_album && scope == "folder" {
                "media-optical-symbolic"
            } else {
                entry_icon(scope)
            };
            let cover = self.entry_cover(scope, key, *is_dir);
            let mut row = EntryRow::new(title)
                .subtitle(&subtitle)
                .cover(cover.as_deref(), icon)
                // A single track shows its length, an album/concert/audiobook
                // its total runtime.
                .duration(self.entry_duration_ms(scope, key));

            // Reorder handle on the far **left** (favorites only): the prefixes
            // keep their call order, so adding it after the cover would put it
            // to the cover's right — hence the handle goes on first.
            if move_msg.is_some() {
                let handle = gtk::Image::from_icon_name("list-drag-handle-symbolic");
                handle.set_tooltip_text(Some(&gettext("Drag to reorder")));
                handle.add_css_class("dim-label");
                row = row.prefix(&handle);
            }

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
                row = row.suffix(&btn);
            }

            // In concerts/audiobooks an album/folder row *opens* its track list,
            // so playing it needs a button of its own; a single track plays on a
            // tap of the row, where the icon is only a marker.
            let opens_list = folder_as_album && scope != "track";
            let is_active = self.entry_is_active(scope, key, playing_album.as_deref());
            row = if opens_list {
                let sender = sender.clone();
                row.play_button(&gettext("Play"), is_active, self.mini.playing, move || {
                    sender.input(play(i))
                })
            } else {
                row.play_marker(is_active, self.mini.playing)
            };
            row = row.marked_in(marks, mark_key(scope, key));

            row = row.on_activate({
                let sender = sender.clone();
                let (scope, key) = (scope.clone(), key.clone());
                move || {
                    if opens_list {
                        sender.input(Msg::OpenEntryTracks {
                            scope: scope.clone(),
                            key: key.clone(),
                        });
                    } else {
                        sender.input(play(i));
                    }
                }
            });
            let row = row
                .on_detail({
                    let sender = sender.clone();
                    move || sender.input(detail(i))
                })
                .build();

            // Drag & drop reordering sits on the whole row, so it is wired once
            // the row is assembled.
            if let Some(move_msg) = move_msg {
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

    /// Runtime (ms) shown next to the play icon of a list entry: a single
    /// track's length, or the total runtime of an album/folder. 0 = unknown
    /// (no label is rendered).
    fn entry_duration_ms(&self, scope: &str, key: &str) -> i64 {
        match scope {
            "track" => self
                .library
                .track_by_path(key)
                .ok()
                .flatten()
                .and_then(|t| t.duration_ms)
                .filter(|ms| *ms > 0)
                .unwrap_or(0),
            "album" | "folder" => self.entry_total_ms(scope, key),
            _ => 0,
        }
    }

    /// Is this entry the one currently playing? `album` is the running track's
    /// album, passed in so building a whole list costs one lookup.
    pub(crate) fn entry_is_active(&self, scope: &str, key: &str, album: Option<&str>) -> bool {
        entry_is_active(self.transport.playing_path.as_deref(), album, scope, key)
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
            "track" => {
                // YouTube tracks (synthetic `yt:<id>` path) aren't in the `track`
                // table and have no embedded cover; use the enriched cover or the
                // video thumbnail (same lookup as the playlist rows).
                if let Some(vid) = crate::core::youtube::parse_yt_path(key) {
                    return crate::core::online::youtube_cover_path(&vid).or_else(|| {
                        crate::core::online::youtube_thumb_path(
                            &crate::core::youtube::thumbnail_url(&vid),
                        )
                    });
                }
                crate::core::online::local_track_cover(key).or_else(|| {
                    let t = self.library.track_by_path(key).ok().flatten()?;
                    let album = t.album.as_deref().filter(|a| !a.trim().is_empty())?;
                    self.album_cover_for(t.artist.as_deref().unwrap_or(""), album)
                })
            }
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

    /// Cover of a folder: cover of any track within it.
    fn folder_cover(&self, folder: &str) -> Option<String> {
        let tracks: Vec<Track> = self.library.tracks_under_path(folder).ok()?;
        let first = tracks.first()?;
        // A folder recognized as an album: its stored cover (custom upload first),
        // keyed by the SAME resolved (artist, album) the detail/upload use, wins
        // over embedded art — so a just-set cover shows in the list too.
        let refs: Vec<&Track> = tracks.iter().collect();
        if let Some(album) = most_common_album_base(&refs).filter(|a| !a.is_empty()) {
            let artist = most_common_artist(&tracks);
            if let Some(p) = self
                .library
                .get_album_meta(&artist, &album)
                .ok()
                .flatten()
                .and_then(|m| m.cover_path)
                .filter(|p| std::path::Path::new(p).exists())
            {
                return Some(p);
            }
        }
        crate::core::online::local_track_cover(&first.path).or_else(|| {
            let album = first.album.as_deref().filter(|a| !a.trim().is_empty())?;
            self.album_cover_for(first.artist.as_deref().unwrap_or(""), album)
        })
    }

    /// Plays an entry (scope/key).
    pub(crate) fn play_entry(&mut self, scope: &str, key: &str, is_dir: bool) {
        // The row shows a pause icon while its entry is the one running, so the
        // press has to toggle pause/resume instead of restarting it.
        if self.entry_is_active(scope, key, self.playing_album().as_deref()) {
            if self.mini.playing {
                self.save_resume();
            }
            self.flip_playing();
            return;
        }
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

    /// Subtitle of a track entry: the album (or "Track" if unknown). The
    /// duration is shown on the right, next to the play icon, as in the other
    /// track lists.
    fn track_meta_subtitle(&self, path: &str) -> String {
        let album = self
            .library
            .track_by_path(path)
            .ok()
            .flatten()
            .and_then(|t| t.album)
            .unwrap_or_default();
        let album = album.trim();
        if album.is_empty() {
            entry_kind("track")
        } else {
            album.to_string()
        }
    }

    /// Queue = the given files starting at track 1, unless empty.
    pub(crate) fn play_track_set(&mut self, files: Vec<PathBuf>) {
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

/// `Msg` sub-enum of the favorite domain (split out of `App::update`).
#[derive(Debug)]
pub(crate) enum FavoriteMsg {
    // Favorites
    /// Set/remove the current detail target as a favorite.
    ToggleFavorite,
    /// Play a favorite (index in `favorite_items`).
    PlayFavorite(usize),
    /// Open the detail view of a favorite.
    ShowFavoriteDetail(usize),
    /// Reorder favorites (indices in `favorite_items`).
    MoveFavorite { from: usize, to: usize },
    // Audiobooks
    /// Play an audiobook (index in `audiobook_items`).
    PlayAudiobook(usize),
    /// Open gallery audiobook (index): album/folder → track list, track → play.
    OpenAudiobookEntry(usize),
    /// Open the detail view of an audiobook.
    ShowAudiobookDetail(usize),
}

impl App {
    /// Dispatch for [`FavoriteMsg`] (the former `App::update` arms, moved verbatim).
    pub(crate) fn update_favorite(
        &mut self,
        msg: FavoriteMsg,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        match msg {
            FavoriteMsg::ToggleFavorite => self.toggle_favorite(sender),
            FavoriteMsg::PlayFavorite(index) => self.play_favorite(sender, index),
            FavoriteMsg::ShowFavoriteDetail(index) => {
                if let Some((scope, key, _, is_dir)) =
                    self.favorites.favorite_items.get(index).cloned()
                {
                    self.nav.context_target = Some(self.entry_target(&scope, &key, is_dir));
                    self.open_context_menu(root, sender);
                }
            }
            FavoriteMsg::MoveFavorite { from, to } => self.move_favorite(sender, from, to),
            FavoriteMsg::PlayAudiobook(index) => {
                if let Some((scope, key, _, is_dir)) =
                    self.favorites.audiobook_items.get(index).cloned()
                {
                    self.play_entry(&scope, &key, is_dir);
                }
            }
            FavoriteMsg::OpenAudiobookEntry(index) => {
                // Gallery tap: album/folder opens the track list, a single track plays.
                if let Some((scope, key, _, is_dir)) =
                    self.favorites.audiobook_items.get(index).cloned()
                {
                    if scope == "track" {
                        self.play_entry(&scope, &key, is_dir);
                    } else {
                        sender.input(Msg::OpenEntryTracks { scope, key });
                    }
                }
            }
            FavoriteMsg::ShowAudiobookDetail(index) => {
                if let Some((scope, key, _, is_dir)) =
                    self.favorites.audiobook_items.get(index).cloned()
                {
                    self.nav.context_target = Some(self.entry_target(&scope, &key, is_dir));
                    self.open_context_menu(root, sender);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{entry_icon, entry_kind, mark_key};

    #[test]
    fn mark_key_joins_scope_and_key_with_a_control_separator() {
        assert_eq!(mark_key("album", "Abbey Road"), "album\u{1}Abbey Road");
        assert_eq!(mark_key("track", "/m/a.mp3"), "track\u{1}/m/a.mp3");
        assert_ne!(mark_key("album", "X"), mark_key("artist", "X"));
        assert_ne!(mark_key("album", "X"), mark_key("album", "Y"));
    }

    #[test]
    fn entry_icon_per_scope() {
        assert_eq!(entry_icon("album"), "media-optical-symbolic");
        assert_eq!(entry_icon("artist"), "avatar-default-symbolic");
        assert_eq!(entry_icon("folder"), "folder-symbolic");
        assert_eq!(entry_icon("track"), "audio-x-generic-symbolic");
        assert_eq!(entry_icon(""), "audio-x-generic-symbolic");
    }

    #[test]
    fn entry_kind_per_scope() {
        assert_eq!(entry_kind("album"), "Album");
        assert_eq!(entry_kind("artist"), "Artist");
        assert_eq!(entry_kind("folder"), "Folder");
        assert_eq!(entry_kind("track"), "Track");
        assert_eq!(entry_kind("anything"), "Track");
    }
}
