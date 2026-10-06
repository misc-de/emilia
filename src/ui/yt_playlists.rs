//! The playlist half of [`YtPage`]'s inherent impl: the playlist detail
//! dialog, the playlist-songs subpage with its (session + persistent DB)
//! song-list cache, starting a playlist at a song, the detail refresh, and
//! the add-to-library / save-to-Playlists flows with their `on_cmd_*`
//! worker-result handlers. The GTK-free cache/queue/paging rules are free
//! functions at the bottom of this file (unit-tested).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::youtube::{self, YtResult};
use crate::i18n::{gettext, gettext_f};
use crate::ui::app_helpers::{on_long_press, on_secondary_click};
use crate::ui::widgets::{action_row, detail_box, present_detail_refreshable};
use crate::ui::yt_channels::fmt_duration;
use crate::ui::yt_page::{YtCmd, YtInput, YtOutput, YtPage};
use crate::ui::yt_page_lists::count_title;

/// Upper bound of videos indexed when adding a whole playlist to the collection.
const PLAYLIST_INDEX_LIMIT: usize = 200;
/// Song-list length fetched when a playlist detail is refreshed (same cap as
/// opening its songs).
const PLAYLIST_REFRESH_LIMIT: usize = 200;
/// How long a cached browsed-playlist song list is served as-is before a
/// background refresh is kicked off on the next open (6 hours).
const PLAYLIST_CACHE_TTL_SECS: i64 = 6 * 60 * 60;
/// Most threads caching the playlist-songs subpage's thumbnails at once.
const COVER_FETCH_THREADS: usize = 8;

impl YtPage {
    /// Detail refresh of a playlist: re-fetch its songs and the first song's
    /// thumbnail (worker), then reopen the detail.
    pub(super) fn on_refresh_playlist(
        &self,
        sender: &ComponentSender<Self>,
        url: String,
        title: String,
    ) {
        let _ = sender.output(YtOutput::Toast(gettext("Refreshing …")));
        sender.spawn_command(move |out| {
            let result =
                youtube::list_playlist(&url, PLAYLIST_REFRESH_LIMIT).map_err(|e| e.to_string());
            if let Some(first) = result.as_ref().ok().and_then(|v| v.first()) {
                let _ =
                    crate::core::online::recache_youtube_thumb(&youtube::thumbnail_url(&first.id));
            }
            let _ = out.send(YtCmd::PlaylistRefreshed { url, title, result });
        });
    }

    /// A song of the playlist-songs subpage was played: hand the (already
    /// resolved) song list to the transport, starting at `index`.
    pub(super) fn on_play_playlist_at(
        &self,
        sender: &ComponentSender<Self>,
        url: String,
        title: String,
        index: usize,
        close: bool,
    ) {
        if let Some(videos) = self.playlist_songs_cache.get(&url) {
            let _ = sender.output(YtOutput::StartPlaylistAt {
                url,
                title,
                index,
                close,
                videos: playlist_queue(videos),
            });
        }
    }

    /// Detail dialog of a playlist.
    pub(super) fn show_playlist_detail(
        &self,
        sender: &ComponentSender<Self>,
        url: &str,
        title: &str,
    ) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let dialog = adw::Dialog::builder().title(title).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();
        let info = adw::PreferencesGroup::new();
        info.add(
            &adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(title))
                .subtitle(gettext("Playlist"))
                .build(),
        );
        content.append(&info);
        let actions = adw::PreferencesGroup::new();
        let start = action_row(&gettext("Start Playlist"), "media-playback-start-symbolic");
        {
            let (sender, dialog, u, t) = (
                sender.clone(),
                dialog.clone(),
                url.to_string(),
                title.to_string(),
            );
            start.connect_activated(move |_| {
                let _ = sender.output(YtOutput::StartPlaylist {
                    url: u.clone(),
                    title: t.clone(),
                });
                dialog.close();
            });
        }
        actions.add(&start);
        let save = action_row(&gettext("Add to Playlists"), "view-list-symbolic");
        {
            let (sender, dialog, u, t) = (
                sender.clone(),
                dialog.clone(),
                url.to_string(),
                title.to_string(),
            );
            save.connect_activated(move |_| {
                sender.input(YtInput::SavePlaylist {
                    url: u.clone(),
                    title: t.clone(),
                });
                dialog.close();
            });
        }
        actions.add(&save);
        let add = action_row(&gettext("Add to library"), "list-add-symbolic");
        {
            let (sender, dialog, u, t) = (
                sender.clone(),
                dialog.clone(),
                url.to_string(),
                title.to_string(),
            );
            add.connect_activated(move |_| {
                sender.input(YtInput::PlaylistToLibrary {
                    url: u.clone(),
                    title: t.clone(),
                });
                dialog.close();
            });
        }
        actions.add(&add);
        if self.library.is_recent(url).unwrap_or(false) {
            let remove = action_row(&gettext("Remove from recent"), "user-trash-symbolic");
            remove.add_css_class("error");
            let (sender, dialog, u) = (sender.clone(), dialog.clone(), url.to_string());
            remove.connect_activated(move |_| {
                sender.input(YtInput::RemoveRecent(u.clone()));
                dialog.close();
            });
            actions.add(&remove);
        }
        content.append(&actions);
        {
            let (sender, u, t) = (sender.clone(), url.to_string(), title.to_string());
            present_detail_refreshable(&dialog, &content, &root, move || {
                sender.input(YtInput::RefreshPlaylist {
                    url: u.clone(),
                    title: t.clone(),
                });
            });
        }
    }

    /// Loads a (not locally mirrored) playlist's videos, then opens them as a
    /// song-list subpage.
    fn yt_open_playlist_songs(
        &mut self,
        sender: &ComponentSender<Self>,
        url: String,
        title: String,
    ) {
        let _ = sender.output(YtOutput::SetLoading(Some(gettext_f(
            "Loading “{title}” …",
            &[("title", &title)],
        ))));
        sender.spawn_command(move |out| {
            let result =
                youtube::list_playlist(&url, PLAYLIST_INDEX_LIMIT).map_err(|e| e.to_string());
            let _ = out.send(YtCmd::PlaylistSongs { url, title, result });
        });
    }

    /// Subpage listing a YouTube playlist's songs.
    fn show_yt_playlist_songs(
        &mut self,
        sender: &ComponentSender<Self>,
        url: &str,
        title: &str,
        videos: Vec<YtResult>,
    ) {
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        let group = adw::PreferencesGroup::builder()
            .title(count_title(title, videos.len()).as_str())
            .build();
        if videos.is_empty() {
            group.add(
                &adw::ActionRow::builder()
                    .title(gettext("No videos"))
                    .build(),
            );
        }
        let mut pending: Vec<(String, adw::Bin)> = Vec::new();
        for (index, v) in videos.iter().enumerate() {
            let subtitle = v.duration.map(fmt_duration).unwrap_or_default();
            // Not activatable: the video plays from its play button, the detail
            // view opens on long press / right click.
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&v.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .build();
            row.add_css_class("emilia-flush");
            let thumb_url = youtube::thumbnail_url(&v.id);
            let cover = crate::core::online::youtube_cover_path(&v.id)
                .or_else(|| crate::core::online::youtube_thumb_path(&thumb_url));
            let frame = crate::ui::widgets::thumb_frame("audio-x-generic-symbolic", 48);
            match cover.as_deref().and_then(crate::ui::widgets::thumb_cached) {
                Some(tex) => crate::ui::widgets::set_cover_thumb(&frame, &tex),
                None => pending.push((thumb_url, frame.clone())),
            }
            row.add_prefix(&frame);

            let play = gtk::Button::builder()
                .icon_name("media-playback-start-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(gettext("Play"))
                .css_classes(["flat"])
                .build();
            {
                let (sender, u, t) = (sender.clone(), url.to_string(), title.to_string());
                play.connect_clicked(move |_| {
                    sender.input(YtInput::PlayPlaylistAt {
                        url: u.clone(),
                        title: t.clone(),
                        index,
                        close: false,
                    });
                });
            }
            row.add_suffix(&play);
            on_secondary_click(&row, {
                let (sender, vid, t) = (sender.clone(), v.id.clone(), v.title.clone());
                move || {
                    sender.input(YtInput::ShowVideoDetail {
                        video_id: vid.clone(),
                        title: t.clone(),
                    });
                }
            });
            on_long_press(&row, {
                let (sender, vid, t) = (sender.clone(), v.id.clone(), v.title.clone());
                move || {
                    sender.input(YtInput::ShowVideoDetail {
                        video_id: vid.clone(),
                        title: t.clone(),
                    })
                }
            });
            group.add(&row);
        }
        content.append(&group);
        if let Some(first) = videos.first() {
            let _ = self
                .library
                .set_recent_thumb(url, &youtube::thumbnail_url(&first.id));
        }
        self.push_subpage(
            sender,
            gettext_f("Playlist – {title}", &[("title", title)]),
            content,
        );

        self.pl_cover_slots = pending;
        if !self.pl_cover_slots.is_empty() {
            let urls: Vec<String> = self.pl_cover_slots.iter().map(|(u, _)| u.clone()).collect();
            sender.spawn_command(move |out| {
                let chunk = cover_fetch_chunk(urls.len());
                std::thread::scope(|s| {
                    for part in urls.chunks(chunk) {
                        s.spawn(move || {
                            for u in part {
                                let _ = crate::core::online::cache_youtube_thumb(u);
                            }
                        });
                    }
                });
                let _ = out.send(YtCmd::PlaylistCoversReady);
            });
        }
    }

    /// Adds all videos of a playlist to the on-disk music library (background).
    pub(super) fn yt_playlist_to_library(
        &self,
        sender: &ComponentSender<Self>,
        url: String,
        title: String,
    ) {
        let Some(music) = self.library.get_setting("music_dir").ok().flatten() else {
            let _ = sender.output(YtOutput::Toast(gettext(
                "Set a music folder in settings first",
            )));
            return;
        };
        let _ = sender.output(YtOutput::Progress(gettext_f(
            "Adding playlist “{title}” to library …",
            &[("title", &title)],
        )));
        sender.spawn_command(move |out| {
            let r = (|| -> Result<usize, String> {
                let videos = youtube::list_playlist(&url, PLAYLIST_INDEX_LIMIT)
                    .map_err(|e| e.to_string())?;
                let total = videos.len();
                let mut n = 0;
                let _ = out.send(YtCmd::LibraryProgress { done: 0, total });
                for (i, v) in videos.into_iter().enumerate() {
                    let cover = crate::core::online::youtube_cover_path(&v.id);
                    if let Ok(youtube::AddOutcome::Added) = youtube::add_to_library(
                        &v.id,
                        &v.title,
                        None,
                        &music,
                        cover.as_deref(),
                        false,
                    ) {
                        n += 1;
                    }
                    let _ = out.send(YtCmd::LibraryProgress { done: i + 1, total });
                }
                Ok(n)
            })();
            let _ = out.send(YtCmd::LibraryAdded {
                video_id: None,
                result: r,
            });
        });
    }

    /// Saves a found playlist into the Playlists section (background).
    pub(super) fn yt_save_playlist(
        &self,
        sender: &ComponentSender<Self>,
        url: String,
        title: String,
    ) {
        let _ = sender.output(YtOutput::Progress(gettext_f(
            "Saving “{title}” to Playlists …",
            &[("title", &title)],
        )));
        sender.spawn_command(move |out| {
            let r = (|| -> Result<usize, String> {
                let videos = youtube::list_playlist(&url, PLAYLIST_INDEX_LIMIT)
                    .map_err(|e| e.to_string())?;
                let lib = Library::open().map_err(|e| e.to_string())?;
                let mut paths = Vec::with_capacity(videos.len());
                for v in &videos {
                    let _ = lib.set_yt_meta(&v.id, &v.title, v.duration);
                    paths.push(youtube::yt_path(&v.id));
                }
                lib.replace_yt_playlist(&url, &title, &paths)
                    .map_err(|e| e.to_string())?;
                Ok(paths.len())
            })();
            let _ = out.send(YtCmd::PlaylistSaved(r));
        });
    }

    /// Open a recent playlist's song list:
    /// saved DB mirror → session cache → **persistent DB cache** → fetch.
    /// Serving from the DB cache is instant (no YouTube round-trip); if that
    /// cache is stale it is refreshed in the background for the next open.
    pub(super) fn yt_open_recent_playlist(
        &mut self,
        sender: &ComponentSender<Self>,
        url: String,
        title: String,
    ) {
        // A "saved" playlist (Add to Playlists) opens its local mirror directly.
        if let Ok(Some(id)) = self.library.yt_playlist_id(&url) {
            let _ = sender.output(YtOutput::OpenPlaylist { id, name: title });
            return;
        }
        // Already fetched this session → show immediately.
        if let Some(videos) = self.playlist_songs_cache.get(&url).cloned() {
            self.show_yt_playlist_songs(sender, &url, &title, videos);
            return;
        }
        // Persisted from an earlier session → show instantly from the DB cache,
        // and refresh in the background if it has gone stale.
        if let Ok(Some((json, fetched_at))) = self.library.yt_playlist_cache(&url)
            && let Ok(videos) = serde_json::from_str::<Vec<YtResult>>(&json)
        {
            self.playlist_songs_cache
                .insert(url.clone(), videos.clone());
            self.show_yt_playlist_songs(sender, &url, &title, videos);
            if playlist_cache_stale(crate::ui::app_helpers::unix_now(), fetched_at) {
                let (url, title) = (url.clone(), title.clone());
                sender.spawn_command(move |out| {
                    let result = youtube::list_playlist(&url, PLAYLIST_INDEX_LIMIT)
                        .map_err(|e| e.to_string());
                    let _ = out.send(YtCmd::PlaylistCacheRefreshed { url, title, result });
                });
            }
            return;
        }
        // Never seen → fetch (the result is cached on arrival).
        self.yt_open_playlist_songs(sender, url, title);
    }

    /// Serializes a playlist's song list into the persistent DB cache (best
    /// effort: a serialization/DB error just skips the cache, never blocks).
    fn cache_playlist_songs(&self, url: &str, title: &str, videos: &[YtResult]) {
        if let Ok(json) = serde_json::to_string(videos)
            && let Err(e) = self.library.set_yt_playlist_cache(url, title, &json)
        {
            tracing::warn!("caching playlist {url} failed: {e}");
        }
    }

    /// Worker result: a playlist's song list resolved → cache + show subpage.
    pub(super) fn on_cmd_yt_playlist_songs(
        &mut self,
        sender: &ComponentSender<Self>,
        url: String,
        title: String,
        result: Result<Vec<YtResult>, String>,
    ) {
        let _ = sender.output(YtOutput::SetLoading(None));
        match result {
            Ok(videos) => {
                self.cache_playlist_songs(&url, &title, &videos);
                self.playlist_songs_cache
                    .insert(url.clone(), videos.clone());
                self.show_yt_playlist_songs(sender, &url, &title, videos);
            }
            Err(e) => {
                tracing::warn!("yt playlist load failed: {e}");
                let _ = sender.output(YtOutput::Toast(gettext("Could not load playlist")));
            }
        }
    }

    /// Worker result: a stale cached playlist's background refresh finished →
    /// update the persistent + session caches silently (no UI change; the fresh
    /// list shows on the next open).
    pub(super) fn on_cmd_yt_playlist_cache_refreshed(
        &mut self,
        url: String,
        title: String,
        result: Result<Vec<YtResult>, String>,
    ) {
        match result {
            Ok(videos) => {
                self.cache_playlist_songs(&url, &title, &videos);
                self.playlist_songs_cache.insert(url, videos);
            }
            Err(e) => tracing::warn!("yt playlist background refresh failed: {e}"),
        }
    }

    /// Worker result of a playlist detail refresh: cache the fresh song list and
    /// update the "Recently" entry's count/cover (without moving it up).
    pub(super) fn on_cmd_yt_playlist_refreshed(
        &mut self,
        url: &str,
        title: &str,
        result: Result<Vec<YtResult>, String>,
    ) {
        match result {
            Ok(videos) => {
                if self.library.is_recent(url).unwrap_or(false) {
                    let _ = self.library.set_recent_playlist_count(
                        url,
                        videos.len() as i64,
                        playlist_total_duration(&videos),
                    );
                    if let Some(first) = videos.first() {
                        let _ = self
                            .library
                            .set_recent_thumb(url, &youtube::thumbnail_url(&first.id));
                    }
                }
                self.cache_playlist_songs(url, title, &videos);
                self.playlist_songs_cache.insert(url.to_string(), videos);
            }
            Err(e) => tracing::warn!("yt playlist refresh failed: {e}"),
        }
    }

    /// Worker result: pending playlist-songs cover thumbnails finished caching.
    pub(super) fn on_cmd_yt_playlist_covers_ready(&mut self) {
        self.pl_cover_slots.retain(|(thumb_url, frame)| {
            if frame.root().is_none() {
                return false;
            }
            match crate::core::online::youtube_thumb_path(thumb_url)
                .as_deref()
                .and_then(crate::ui::widgets::thumb_cached)
            {
                Some(tex) => {
                    crate::ui::widgets::set_cover_thumb(frame, &tex);
                    false
                }
                None => true,
            }
        });
    }
}

/// The transport's queue entries (video id, title, duration) of a song list.
fn playlist_queue(videos: &[YtResult]) -> Vec<(String, String, Option<i64>)> {
    videos
        .iter()
        .map(|v| (v.id.clone(), v.title.clone(), v.duration))
        .collect()
}

/// Summed runtime of a song list in seconds; `None` when no song has a known
/// (non-zero) length.
fn playlist_total_duration(videos: &[YtResult]) -> Option<i64> {
    let total: i64 = videos.iter().filter_map(|v| v.duration).sum();
    (total > 0).then_some(total)
}

/// Whether a song list persisted at `fetched_at` (unix seconds) is older than
/// [`PLAYLIST_CACHE_TTL_SECS`] at `now` and gets refreshed in the background.
fn playlist_cache_stale(now: i64, fetched_at: i64) -> bool {
    now.saturating_sub(fetched_at) > PLAYLIST_CACHE_TTL_SECS
}

/// Chunk size the thumbnail URLs of the playlist-songs subpage are split into,
/// one thread per chunk (aimed at [`COVER_FETCH_THREADS`] threads).
fn cover_fetch_chunk(urls: usize) -> usize {
    let threads = COVER_FETCH_THREADS.min(urls.max(1));
    (urls / threads).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::youtube::YtKind;

    fn song(id: &str, duration: Option<i64>) -> YtResult {
        YtResult {
            kind: YtKind::Video,
            id: id.to_string(),
            url: format!("https://example.invalid/{id}"),
            title: format!("Song {id}"),
            uploader: None,
            duration,
            thumbnail: None,
        }
    }

    #[test]
    fn queue_keeps_order_ids_titles_and_lengths() {
        let videos = [song("a", Some(60)), song("b", None)];
        assert_eq!(
            playlist_queue(&videos),
            vec![
                ("a".to_string(), "Song a".to_string(), Some(60)),
                ("b".to_string(), "Song b".to_string(), None),
            ]
        );
        assert!(playlist_queue(&[]).is_empty());
    }

    #[test]
    fn total_duration_skips_unknown_lengths() {
        assert_eq!(
            playlist_total_duration(&[song("a", Some(60)), song("b", None), song("c", Some(30))]),
            Some(90)
        );
        assert_eq!(playlist_total_duration(&[song("a", None)]), None);
        assert_eq!(playlist_total_duration(&[song("a", Some(0))]), None);
        assert_eq!(playlist_total_duration(&[]), None);
    }

    #[test]
    fn cache_goes_stale_after_the_ttl() {
        let now = 1_000_000;
        assert!(!playlist_cache_stale(now, now));
        assert!(!playlist_cache_stale(now, now - PLAYLIST_CACHE_TTL_SECS));
        assert!(playlist_cache_stale(now, now - PLAYLIST_CACHE_TTL_SECS - 1));
        // A timestamp from the future (clock skew) never counts as stale.
        assert!(!playlist_cache_stale(now, now + 60));
    }

    #[test]
    fn cover_fetch_chunks() {
        assert_eq!(cover_fetch_chunk(0), 1);
        assert_eq!(cover_fetch_chunk(3), 1);
        assert_eq!(cover_fetch_chunk(16), 2);
        assert_eq!(cover_fetch_chunk(200), 25);
        // Current behaviour (not a hard cap): the integer division rounds the chunk size down, so a
        // list that doesn't divide evenly spawns more than COVER_FETCH_THREADS
        // threads (12 URLs → chunk 1 → 12 threads; 17 → chunk 2 → 9 threads).
        assert_eq!(12_usize.div_ceil(cover_fetch_chunk(12)), 12);
        assert_eq!(17_usize.div_ceil(cover_fetch_chunk(17)), 9);
    }
}
