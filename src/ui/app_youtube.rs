//! YouTube: channel overview, newest-videos list, search/subscribe dialog,
//! channel/video/playlist detail pages, and the playback/offline/collection
//! glue. The extractor (`yt-dlp`) is downloaded at runtime, never bundled, and
//! the whole section is gated behind the `youtube_enabled` setting.
//!
//! Mirrors the podcast feature (`app_podcast.rs`): channels ≙ subscriptions,
//! videos ≙ episodes. "Add to my music" reuses the synthetic-path scheme
//! (`yt:<video_id>`) the way Nextcloud uses `nc:<id>:<rel>`; "available offline"
//! mirrors the episode download.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use std::cell::Cell;
use std::rc::Rc;

use crate::core::db::Library;
use crate::core::youtube::{self, YtKind, YtResult};
use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::ui::app::{App, Msg};

/// How many newest videos to cache per channel on subscribe/refresh.
pub(crate) const CHANNEL_VIDEO_LIMIT: usize = 30;
/// Upper bound of videos indexed when adding a whole playlist to the collection.
const PLAYLIST_INDEX_LIMIT: usize = 200;

/// Outcome of a library-add attempt: the file was written, or the destination
/// already holds a (different) file and the user must decide whether to
/// overwrite it.
enum AddOutcome {
    Added,
    Exists(std::path::PathBuf),
}

/// Downloads a video (if no local copy yet), transcodes it into the on-disk
/// music library under `<music>/YouTube/<Artist>/<Album>/<Title>.mp3` (the album
/// folder is dropped when none is known), tags it, gives it the enriched cover,
/// indexes it, and removes the temporary download. With `overwrite == false` a
/// pre-existing destination is reported back (so the caller can ask the user)
/// instead of being clobbered. Worker only.
fn library_add_one(
    video_id: &str,
    title_hint: &str,
    music_dir: &str,
    cover: Option<&str>,
    overwrite: bool,
) -> Result<AddOutcome, String> {
    // Prefer yt-dlp's metadata (uploader = artist, clean title); fall back to
    // the title we already have so this works offline too. Fetched first (cheap)
    // so the destination — and thus the existence check — needs no full download.
    let meta = youtube::video_meta(video_id).ok();
    // The channel (uploader) is the artist – normalised ("… - Topic"/"…VEVO").
    let artist = meta
        .as_ref()
        .and_then(|m| m.uploader.clone())
        .map(|c| youtube::clean_channel_name(&c))
        .filter(|s| !s.trim().is_empty());
    let title = meta
        .as_ref()
        .map(|m| m.title.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| title_hint.to_string());

    // YouTube itself carries no album. Look it up in the external DB (Deezer) so
    // the track can be filed under <Artist>/<Album>/; this also yields a cover we
    // can fall back to. Best effort – no match → no album folder.
    let (album, dz_cover) = match artist.as_deref() {
        Some(a) => match crate::core::online::track_cover(a, &title) {
            Some((bytes, alb)) => (alb.filter(|s| !s.trim().is_empty()), Some(bytes)),
            None => (None, None),
        },
        None => (None, None),
    };

    // <music>/YouTube/<Artist>/[<Album>/]<Title>.mp3
    let mut dest = std::path::PathBuf::from(music_dir);
    dest.push("YouTube");
    if let Some(a) = artist.as_deref().filter(|s| !s.trim().is_empty()) {
        dest.push(youtube::sanitize_filename(a));
    }
    if let Some(al) = album.as_deref() {
        dest.push(youtube::sanitize_filename(al));
    }
    dest.push(format!("{}.mp3", youtube::sanitize_filename(&title)));

    // Never silently overwrite a different song – hand the decision back up
    // (before downloading, so a skip wastes nothing).
    if dest.exists() && !overwrite {
        return Ok(AddOutcome::Exists(dest));
    }

    // Ensure a source audio file: reuse a previous download or fetch it now.
    let source = match youtube::find_download(video_id) {
        Some(p) => p,
        None => youtube::download_audio(video_id).map_err(|e| e.to_string())?,
    };
    youtube::transcode_to_mp3(&source, &dest, &title, artist.as_deref(), album.as_deref())
        .map_err(|e| e.to_string())?;
    let dest_str = dest.to_string_lossy().into_owned();
    // In-app cover: the enrichment's cover if present, else Deezer's.
    if let Some(bytes) = cover.and_then(|c| std::fs::read(c).ok()) {
        crate::core::online::store_track_cover_bytes(&dest_str, &bytes);
    } else if let Some(bytes) = dz_cover {
        crate::core::online::store_track_cover_bytes(&dest_str, &bytes);
    }
    if let Ok(lib) = Library::open() {
        let track = crate::model::Track {
            id: 0,
            path: dest_str,
            title,
            artist,
            album,
            genre: None,
            track_no: None,
            disc_no: None,
            duration_ms: meta.and_then(|m| m.duration).map(|s| s.saturating_mul(1000)),
            resume_ms: 0,
        };
        let _ = lib.upsert_track(&track);
        let _ = lib.delete_yt_download(video_id);
    }
    // The downloaded file was only the transcode source – drop it now.
    let _ = std::fs::remove_file(&source);
    Ok(AddOutcome::Added)
}

/// Content box for the detail dialogs (uniform margins; local copy of the
/// podcast module's private helper).
fn detail_box() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build()
}

/// Embeds the content scrollably in a dialog with a header bar and shows it.
fn present_detail(dialog: &adw::Dialog, content: &gtk::Box, root: &adw::ApplicationWindow) {
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .vexpand(true)
        .child(content)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    dialog.set_child(Some(&toolbar));
    dialog.set_content_width(600);
    dialog.present(Some(root));
}

/// Converts a search/listing hit into a storable video row.
fn to_model_video(r: YtResult) -> crate::model::YtVideo {
    crate::model::YtVideo {
        video_id: r.id,
        title: r.title,
        url: r.url,
        duration: r.duration,
        published: None,
        thumbnail: r.thumbnail,
    }
}

/// Formats an ISO-8601 publication timestamp as `DD.MM.YYYY HH:MM` (local
/// formatting via glib); falls back to the raw string.
fn fmt_published(iso: &str) -> String {
    gtk::glib::DateTime::from_iso8601(iso, None)
        .ok()
        .and_then(|dt| dt.format("%d.%m.%Y %H:%M").ok())
        .map(|g| g.to_string())
        .unwrap_or_else(|| iso.to_string())
}

/// Formats a duration in seconds as `M:SS` or `H:MM:SS` (display only).
pub(crate) fn fmt_duration(secs: i64) -> String {
    let s = secs.max(0);
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// Subscribes to a channel and caches its newest videos (worker thread, own DB
/// connection). Returns the channel title on success. Mirrors
/// [`crate::ui::app_podcast::fetch_and_store_podcast`].
pub(crate) fn fetch_and_store_channel(
    channel_id: &str,
    title: &str,
    url: &str,
    thumbnail: Option<&str>,
) -> Option<String> {
    let lib = Library::open().ok()?;
    let cid = lib.subscribe_channel(channel_id, title, url, thumbnail).ok()?;
    let videos = list_channel_videos(url, Some(channel_id));
    let _ = lib.set_channel_videos(cid, &videos);
    if let Some(t) = thumbnail {
        crate::core::online::cache_youtube_thumb(t);
    }
    Some(title.to_string())
}

/// Refreshes a subscribed channel's newest videos (worker thread, own DB).
/// Returns the channel title on success.
pub(crate) fn refresh_channel_videos(channel_db_id: i64, title: &str, url: &str) -> Option<String> {
    let lib = Library::open().ok()?;
    let cid = youtube::channel_id_from_url(url);
    let videos = list_channel_videos(url, cid.as_deref());
    if videos.is_empty() {
        return None;
    }
    lib.set_channel_videos(channel_db_id, &videos).ok()?;
    Some(title.to_string())
}

/// Lists a channel's newest videos via yt-dlp and merges in publication dates
/// from the channel's Atom feed (which `--flat-playlist` omits).
fn list_channel_videos(url: &str, channel_id: Option<&str>) -> Vec<crate::model::YtVideo> {
    let mut videos: Vec<crate::model::YtVideo> = youtube::list_entries(url, CHANNEL_VIDEO_LIMIT)
        .unwrap_or_default()
        .into_iter()
        .map(to_model_video)
        .collect();
    if let Some(cid) = channel_id {
        let dates = youtube::channel_rss_published(cid);
        if !dates.is_empty() {
            for v in videos.iter_mut() {
                if let Some(p) = dates.get(&v.video_id) {
                    v.published = Some(p.clone());
                }
            }
        }
    }
    // Pre-cache the per-video thumbnails (reliable hqdefault) so the lists and
    // the detail show covers immediately. Deduped: cached ones are skipped.
    for v in &videos {
        crate::core::online::cache_youtube_thumb(&youtube::thumbnail_url(&v.video_id));
    }
    videos
}

impl App {
    // ---- overview + newest list ------------------------------------------

    /// Rebuilds the channel overview (thumbnail, title, video count) and the
    /// "Newest videos" list. Tapping a channel opens its videos; long press
    /// opens the subscription detail.
    pub(crate) fn reload_channels(&mut self, sender: &ComponentSender<Self>) {
        self.youtube.channel_items = self.library.channels().unwrap_or_default();
        if self.libview.gallery_view {
            let tiles: Vec<(Option<String>, &'static str, String)> = self
                .youtube
                .channel_items
                .iter()
                .map(|(_, title, _, thumb, _)| {
                    let cover = thumb.as_deref().and_then(crate::core::online::youtube_thumb_path);
                    (cover, "video-x-generic-symbolic", title.clone())
                })
                .collect();
            self.fill_gallery(
                &self.youtube.channels_gallery,
                &tiles,
                Msg::YtOpenChannelAt,
                Msg::YtShowChannelDetailAt,
            );
        } else {
            while let Some(child) = self.youtube.channels_list.first_child() {
                self.youtube.channels_list.remove(&child);
            }
            for (id, title, _url, thumb, count) in self.youtube.channel_items.clone() {
                let row = adw::ActionRow::builder()
                    .title(format!("{} ({count})", gtk::glib::markup_escape_text(&title)).as_str())
                    .activatable(true)
                    .build();
                row.add_css_class("emilia-flush");
                let cover = thumb.as_deref().and_then(crate::core::online::youtube_thumb_path);
                row.add_prefix(&crate::ui::app::cover_widget(cover.as_deref(), "video-x-generic-symbolic"));
                {
                    let sender = sender.clone();
                    row.connect_activated(move |_| sender.input(Msg::YtOpenChannel(id)));
                }
                let lp = gtk::GestureLongPress::new();
                {
                    let sender = sender.clone();
                    lp.connect_pressed(move |g, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(Msg::YtShowChannelDetail(id));
                    });
                }
                row.add_controller(lp);
                self.youtube.channels_list.append(&row);
            }
        }
        self.reload_yt_newest(sender);
        self.reload_yt_recent(sender);
    }

    /// Builds the "Newest videos" list across all subscribed channels.
    pub(crate) fn reload_yt_newest(&mut self, sender: &ComponentSender<Self>) {
        let mut videos = self.library.all_videos().unwrap_or_default();
        videos.truncate(150);
        self.youtube.newest_items = videos;
        while let Some(child) = self.youtube.newest_list.first_child() {
            self.youtube.newest_list.remove(&child);
        }
        if self.youtube.newest_items.is_empty() {
            return;
        }
        let group = adw::PreferencesGroup::new();
        for (i, v) in self.youtube.newest_items.iter().enumerate() {
            let mut subtitle = v.channel_title.clone();
            if let Some(d) = v.duration {
                subtitle.push_str(" · ");
                subtitle.push_str(&fmt_duration(d));
            }
            if let Some(p) = v.published.as_deref().filter(|s| !s.trim().is_empty()) {
                subtitle.push_str(" · ");
                subtitle.push_str(&fmt_published(p));
            }
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&v.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            // Prefer an enriched cover, else the video's own thumbnail (cached on
            // refresh), else the channel avatar.
            let cover = crate::core::online::youtube_cover_path(&v.video_id)
                .or_else(|| {
                    crate::core::online::youtube_thumb_path(&youtube::thumbnail_url(&v.video_id))
                })
                .or_else(|| {
                    v.channel_thumb.as_deref().and_then(crate::core::online::youtube_thumb_path)
                });
            row.add_prefix(&crate::ui::app::cover_widget(cover.as_deref(), "video-x-generic-symbolic"));
            row.add_suffix(&self.video_play_button(sender, &v.video_id, &v.title));
            {
                let (sender, vid, title) = (sender.clone(), v.video_id.clone(), v.title.clone());
                row.connect_activated(move |_| {
                    sender.input(Msg::YtPlayVideo { video_id: vid.clone(), title: title.clone() });
                });
            }
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::YtShowNewestDetail(i));
                });
            }
            row.add_controller(lp);
            group.add(&row);
        }
        self.youtube.newest_list.append(&group);
        self.refresh_yt_icons();
    }

    /// Builds the "Recent" list (recently played videos, newest first). Tap =
    /// play, long press = detail.
    pub(crate) fn reload_yt_recent(&mut self, sender: &ComponentSender<Self>) {
        self.youtube.recent_items = self.library.recent_videos(150).unwrap_or_default();
        while let Some(child) = self.youtube.recent_list.first_child() {
            self.youtube.recent_list.remove(&child);
        }
        if self.youtube.recent_items.is_empty() {
            return;
        }
        let group = adw::PreferencesGroup::new();
        for r in &self.youtube.recent_items {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&r.title))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            if r.kind == "playlist" {
                // A played playlist: "Playlist · N songs"; tap/▶ replays it,
                // long press opens its detail.
                row.set_subtitle(
                    &gettext_f(
                        "Playlist · {n}",
                        &[("n", &ngettext_n("{n} song", "{n} songs", r.count as u32))],
                    ),
                );
                row.add_prefix(&crate::ui::app::cover_widget(None, "view-list-symbolic"));
                let btn = gtk::Button::builder()
                    .icon_name("media-playback-start-symbolic")
                    .valign(gtk::Align::Center)
                    .tooltip_text(&gettext("Start Playlist"))
                    .build();
                btn.add_css_class("flat");
                {
                    let (sender, url, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                    btn.connect_clicked(move |_| {
                        sender.input(Msg::YtStartPlaylist { url: url.clone(), title: t.clone() });
                    });
                }
                row.add_suffix(&btn);
                {
                    // Simple click: show the playlist's song list (▶ button plays it).
                    let (sender, url, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                    row.connect_activated(move |_| {
                        sender.input(Msg::YtOpenRecentPlaylist { url: url.clone(), title: t.clone() });
                    });
                }
                let lp = gtk::GestureLongPress::new();
                {
                    let (sender, url, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                    lp.connect_pressed(move |g, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(Msg::YtShowPlaylistDetail { url: url.clone(), title: t.clone() });
                    });
                }
                row.add_controller(lp);
                group.add(&row);
                continue;
            }
            if let Some(a) = r.artist.as_deref().filter(|s| !s.trim().is_empty()) {
                row.set_subtitle(&gtk::glib::markup_escape_text(a));
            }
            // Resolve the cover the same way as the Newest list (by video id):
            // an enriched cover, else the cached hqdefault thumbnail.
            let cover = crate::core::online::youtube_cover_path(&r.video_id)
                .or_else(|| {
                    crate::core::online::youtube_thumb_path(&youtube::thumbnail_url(&r.video_id))
                });
            row.add_prefix(&crate::ui::app::cover_widget(cover.as_deref(), "video-x-generic-symbolic"));
            row.add_suffix(&self.video_play_button(sender, &r.video_id, &r.title));
            {
                let (sender, vid, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                row.connect_activated(move |_| {
                    sender.input(Msg::YtPlayVideo { video_id: vid.clone(), title: t.clone() });
                });
            }
            let lp = gtk::GestureLongPress::new();
            {
                let (sender, vid, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::YtShowVideoDetail { video_id: vid.clone(), title: t.clone() });
                });
            }
            row.add_controller(lp);
            group.add(&row);
        }
        self.youtube.recent_list.append(&group);
        self.refresh_yt_icons();
    }

    // ---- search + subscribe dialog ---------------------------------------

    /// Dialog for searching YouTube (channels / playlists / videos) and
    /// subscribing/opening a result. Mirrors the podcast subscribe dialog.
    pub(crate) fn open_youtube_search_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        if !youtube::available() {
            self.toast(&gettext("Download yt-dlp in the settings first"));
            return;
        }
        let dialog = adw::Dialog::builder().title(&gettext("Search YouTube")).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        // Kind selector (Videos / Playlists / Channels) – shared via an Rc<Cell>.
        let kind = Rc::new(Cell::new(YtKind::Video));
        let kind_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["linked"])
            .halign(gtk::Align::Center)
            .margin_bottom(6)
            .build();
        let b_video = gtk::ToggleButton::builder().label(&gettext("Videos")).active(true).build();
        let b_playlist = gtk::ToggleButton::builder().label(&gettext("Playlists")).build();
        let b_channel = gtk::ToggleButton::builder().label(&gettext("Channels")).build();
        b_playlist.set_group(Some(&b_video));
        b_channel.set_group(Some(&b_video));
        for (btn, k) in [
            (&b_video, YtKind::Video),
            (&b_playlist, YtKind::Playlist),
            (&b_channel, YtKind::Channel),
        ] {
            let kind = kind.clone();
            btn.connect_toggled(move |b| {
                if b.is_active() {
                    kind.set(k);
                }
            });
            kind_box.append(btn);
        }
        content.append(&kind_box);

        // Search field + button.
        let search_group = adw::PreferencesGroup::builder().title(&gettext("Search")).build();
        let search_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text(&gettext("Search term …"))
            .hexpand(true)
            .build();
        crate::ui::widgets::no_autofocus(&search_entry);
        let search_btn = gtk::Button::builder().label(&gettext("Search")).build();
        search_btn.add_css_class("suggested-action");
        search_row.append(&search_entry);
        search_row.append(&search_btn);
        search_group.add(&search_row);
        content.append(&search_group);

        let trigger = {
            let (sender, entry, kind) = (sender.clone(), search_entry.clone(), kind.clone());
            move || {
                let term = entry.text().to_string();
                if !term.trim().is_empty() {
                    sender.input(Msg::YtSearch(term, kind.get()));
                }
            }
        };
        {
            let trigger = trigger.clone();
            search_entry.connect_activate(move |_| trigger());
        }
        search_btn.connect_clicked(move |_| trigger());

        let results = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).build();
        results.add_css_class("boxed-list");
        results.set_visible(false);
        content.append(&results);

        *self.youtube.search.borrow_mut() = Some((dialog.clone(), results.clone()));
        {
            let slot = self.youtube.search.clone();
            dialog.connect_closed(move |_| {
                *slot.borrow_mut() = None;
            });
        }
        present_detail(&dialog, &content, root);
    }

    /// Redraws the results list in the open search dialog from
    /// `self.youtube.search_results`. Each result is tappable.
    pub(crate) fn rebuild_youtube_search_results(&self, sender: &ComponentSender<Self>) {
        let guard = self.youtube.search.borrow();
        let Some((dialog, list)) = guard.as_ref() else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        list.set_visible(true);

        if self.youtube.search_results.is_empty() {
            let row = adw::ActionRow::builder().title(&gettext("Nothing found")).build();
            row.set_sensitive(false);
            list.append(&row);
            dialog.set_content_height(320);
            return;
        }
        let rows = self.youtube.search_results.len() as i32;
        dialog.set_content_height((340 + rows * 66).min(760));

        for r in &self.youtube.search_results {
            let mut subtitle = match r.kind {
                YtKind::Video => gettext("Video"),
                YtKind::Playlist => gettext("Playlist"),
                YtKind::Channel => gettext("Channel"),
            };
            if let Some(u) = r.uploader.as_deref().filter(|s| !s.trim().is_empty()) {
                subtitle.push_str(" · ");
                subtitle.push_str(u);
            }
            if let Some(d) = r.duration {
                subtitle.push_str(" · ");
                subtitle.push_str(&fmt_duration(d));
            }
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&r.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .activatable(true)
                .build();
            let cover = r.thumbnail.as_deref().and_then(crate::core::online::youtube_thumb_path);
            let icon = match r.kind {
                YtKind::Channel => "avatar-default-symbolic",
                _ => "video-x-generic-symbolic",
            };
            row.add_prefix(&crate::ui::app::cover_widget(cover.as_deref(), icon));
            let suffix = match r.kind {
                YtKind::Channel => "list-add-symbolic",
                _ => "go-next-symbolic",
            };
            row.add_suffix(&gtk::Image::from_icon_name(suffix));
            {
                let (sender, dialog) = (sender.clone(), dialog.clone());
                let (kind, url, vid, title) =
                    (r.kind, r.url.clone(), r.id.clone(), r.title.clone());
                row.connect_activated(move |_| {
                    match kind {
                        YtKind::Channel => sender.input(Msg::YtSubscribeChannel(url.clone())),
                        YtKind::Playlist => sender.input(Msg::YtShowPlaylistDetail {
                            url: url.clone(),
                            title: title.clone(),
                        }),
                        YtKind::Video => sender.input(Msg::YtShowVideoDetail {
                            video_id: vid.clone(),
                            title: title.clone(),
                        }),
                    }
                    dialog.close();
                });
            }
            list.append(&row);
        }
    }

    // ---- channel videos subpage + detail ---------------------------------

    /// Videos subpage of a subscribed channel (tap = play, long press = detail).
    pub(crate) fn open_channel(&self, sender: &ComponentSender<Self>, id: i64, title: &str) {
        let videos = self.library.channel_videos(id).unwrap_or_default();
        // Fallback cover (channel avatar) when a video has no cached thumbnail.
        let channel_thumb = self
            .youtube
            .channel_items
            .iter()
            .find(|(cid, _, _, _, _)| *cid == id)
            .and_then(|(_, _, _, t, _)| t.as_deref())
            .and_then(crate::core::online::youtube_thumb_path);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        let group = adw::PreferencesGroup::builder()
            .title(format!("{} ({})", gtk::glib::markup_escape_text(title), videos.len()).as_str())
            .build();
        if videos.is_empty() {
            group.add(&adw::ActionRow::builder().title(&gettext("No videos")).build());
        }
        for v in &videos {
            let subtitle = v.duration.map(fmt_duration).unwrap_or_default();
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&v.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            let cover = crate::core::online::youtube_cover_path(&v.video_id)
                .or_else(|| {
                    crate::core::online::youtube_thumb_path(&youtube::thumbnail_url(&v.video_id))
                })
                .or_else(|| channel_thumb.clone());
            row.add_prefix(&crate::ui::app::cover_widget(cover.as_deref(), "video-x-generic-symbolic"));
            row.add_suffix(&self.video_play_button(sender, &v.video_id, &v.title));
            {
                let (sender, vid, t) = (sender.clone(), v.video_id.clone(), v.title.clone());
                row.connect_activated(move |_| {
                    sender.input(Msg::YtPlayVideo { video_id: vid.clone(), title: t.clone() });
                });
            }
            let lp = gtk::GestureLongPress::new();
            {
                let (sender, vid, t) = (sender.clone(), v.video_id.clone(), v.title.clone());
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::YtShowVideoDetail {
                        video_id: vid.clone(),
                        title: t.clone(),
                    });
                });
            }
            row.add_controller(lp);
            group.add(&row);
        }
        content.append(&group);
        self.push_subpage(&gettext_f("Channel – {title}", &[("title", title)]), &content);
        self.refresh_yt_icons();
    }

    /// Subscription detail of a channel: open videos, refresh, the "bell"
    /// (subscribe/unsubscribe), and remove.
    pub(crate) fn open_channel_detail(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        id: i64,
    ) {
        let Some((_, title, url, thumb, count)) = self
            .youtube
            .channel_items
            .iter()
            .find(|(cid, _, _, _, _)| *cid == id)
            .cloned()
        else {
            return;
        };
        // Plain-text title bar – pass it raw (not markup-escaped).
        let dialog = adw::Dialog::builder().title(&title).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let info = adw::PreferencesGroup::new();
        let head = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&title))
            .subtitle(ngettext_n("{n} video", "{n} videos", count as u32))
            .build();
        let cover = thumb.as_deref().and_then(crate::core::online::youtube_thumb_path);
        head.add_prefix(&crate::ui::app::cover_widget(cover.as_deref(), "video-x-generic-symbolic"));
        info.add(&head);
        content.append(&info);

        let actions = adw::PreferencesGroup::new();
        let play = action_row(&gettext("Play"), "media-playback-start-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            play.connect_activated(move |_| {
                sender.input(Msg::YtPlayChannel(id));
                dialog.close();
            });
        }
        actions.add(&play);
        let refresh = action_row(&gettext("Refresh"), "view-refresh-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            refresh.connect_activated(move |_| {
                sender.input(Msg::YtRefreshChannel(id));
                dialog.close();
            });
        }
        actions.add(&refresh);
        // The bell: on → subscribed; turning it off unsubscribes. (Reached from
        // the subscription list, so it is always on here.)
        let bell = adw::SwitchRow::builder()
            .title(&gettext("Notify of newest publications"))
            .active(true)
            .build();
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            bell.connect_active_notify(move |s| {
                if !s.is_active() {
                    sender.input(Msg::YtDeleteChannel(id));
                    dialog.close();
                }
            });
        }
        actions.add(&bell);
        let remove = action_row(&gettext("Remove"), "user-trash-symbolic");
        remove.add_css_class("error");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            remove.connect_activated(move |_| {
                sender.input(Msg::YtDeleteChannel(id));
                dialog.close();
            });
        }
        actions.add(&remove);
        content.append(&actions);
        let _ = url;
        present_detail(&dialog, &content, root);
    }

    // ---- video + playlist detail -----------------------------------------

    /// Rich detail page of a video – styled like the album/song detail: a large
    /// cover, an info group (title / channel / duration), and the actions (play,
    /// offline → add to library). Channel/duration/cover are filled in
    /// asynchronously via `Cmd::YtVideoMeta`, so this works the same from the
    /// Recent, Newest, channel and search lists.
    pub(crate) fn show_video_detail(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        video_id: &str,
        title: &str,
    ) {
        // The dialog title bar is plain text (not Pango markup) – pass it raw,
        // otherwise `&`, `<`, `>` would show as `&amp;` etc.
        let dialog = adw::Dialog::builder().title(title).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        // Persisted info for a subscribed channel's video: (channel, duration,
        // thumbnail). Shown synchronously – the network fetch below only runs to
        // fill in anything missing (e.g. for non-subscribed search results).
        let stored = self.library.yt_video_info(video_id).ok().flatten();
        let stored_channel = stored.as_ref().map(|(c, _, _)| c.clone());
        let stored_duration = stored.as_ref().and_then(|(_, d, _)| *d);
        // Cover: an already-enriched cover, else the (pre-cached) thumbnail.
        let cover_path = crate::core::online::youtube_cover_path(video_id).or_else(|| {
            crate::core::online::youtube_thumb_path(&youtube::thumbnail_url(video_id))
        });

        // Large cover header (centered); updated by the async fetch if needed.
        let cover_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .halign(gtk::Align::Center)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let initial = cover_path
            .as_deref()
            .and_then(|p| gtk::gdk::Texture::from_filename(p).ok());
        cover_box.append(&crate::ui::widgets::rounded_image(
            initial.as_ref(),
            "video-x-generic-symbolic",
            200,
        ));
        content.append(&cover_box);

        // Info: title / channel / duration (from storage; gaps filled async).
        let info = adw::PreferencesGroup::new();
        let title_row = adw::ActionRow::builder()
            .title(&gettext("Title"))
            .subtitle(gtk::glib::markup_escape_text(title))
            .build();
        title_row.set_subtitle_lines(3);
        info.add(&title_row);
        let channel_row = adw::ActionRow::builder()
            .title(&gettext("Channel"))
            .subtitle(stored_channel.as_deref().unwrap_or("…"))
            .build();
        info.add(&channel_row);
        let duration_row = adw::ActionRow::builder()
            .title(&gettext("Duration"))
            .subtitle(&stored_duration.map(fmt_duration).unwrap_or_else(|| "…".into()))
            .build();
        info.add(&duration_row);
        content.append(&info);

        // Actions: Play + progressive offline/library row.
        let actions = adw::PreferencesGroup::new();
        let play = action_row(&gettext("Play"), "media-playback-start-symbolic");
        {
            let (sender, dialog, vid, t) =
                (sender.clone(), dialog.clone(), video_id.to_string(), title.to_string());
            play.connect_activated(move |_| {
                sender.input(Msg::YtPlayVideo { video_id: vid.clone(), title: t.clone() });
                dialog.close();
            });
        }
        self.youtube
            .ctx_video_play
            .replace(Some((play.clone(), video_id.to_string())));
        actions.add(&play);

        let off = action_row(&gettext("Add to library"), "list-add-symbolic");
        {
            let (sender, vid, t) = (sender.clone(), video_id.to_string(), title.to_string());
            off.connect_activated(move |_| {
                sender.input(Msg::YtAddToLibrary { video_id: vid.clone(), title: t.clone() });
            });
        }
        actions.add(&off);
        // For a video in the "Recent" history: offer removing it from there.
        if self.library.is_recent(video_id).unwrap_or(false) {
            let remove = action_row(&gettext("Remove from recent"), "user-trash-symbolic");
            remove.add_css_class("error");
            let (sender, dialog, vid) = (sender.clone(), dialog.clone(), video_id.to_string());
            remove.connect_activated(move |_| {
                sender.input(Msg::YtRemoveRecent(vid.clone()));
                dialog.close();
            });
            actions.add(&remove);
        }
        content.append(&actions);

        self.youtube
            .ctx_video_download
            .replace(Some((off.clone(), video_id.to_string())));
        self.youtube.ctx_video_meta.replace(Some((
            video_id.to_string(),
            cover_box,
            channel_row,
            duration_row,
        )));
        self.refresh_yt_download_row();
        self.refresh_yt_icons();

        // Only hit the network when persisted data left a gap (channel, duration
        // or cover missing) – e.g. for non-subscribed search results.
        if stored_channel.is_none() || stored_duration.is_none() || initial.is_none() {
            let (sender, vid) = (sender.clone(), video_id.to_string());
            sender.spawn_command(move |out| {
                let meta = youtube::video_meta(&vid).ok();
                let uploader = meta.as_ref().and_then(|m| m.uploader.clone());
                let duration = meta.as_ref().and_then(|m| m.duration);
                let cover = crate::core::online::youtube_cover_path(&vid).or_else(|| {
                    crate::core::online::cache_youtube_thumb(&youtube::thumbnail_url(&vid))
                });
                let _ = out.send(crate::ui::app::Cmd::YtVideoMeta {
                    video_id: vid,
                    uploader,
                    duration,
                    cover,
                });
            });
        }

        {
            let play_slot = self.youtube.ctx_video_play.clone();
            let dl_slot = self.youtube.ctx_video_download.clone();
            let meta_slot = self.youtube.ctx_video_meta.clone();
            dialog.connect_closed(move |_| {
                *play_slot.borrow_mut() = None;
                *dl_slot.borrow_mut() = None;
                *meta_slot.borrow_mut() = None;
            });
        }
        present_detail(&dialog, &content, root);
    }

    /// Detail dialog of a playlist: add the whole playlist to the collection.
    pub(crate) fn show_playlist_detail(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        url: &str,
        title: &str,
    ) {
        // The dialog title bar is plain text (not Pango markup) – pass it raw,
        // otherwise `&`, `<`, `>` would show as `&amp;` etc.
        let dialog = adw::Dialog::builder().title(title).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();
        let info = adw::PreferencesGroup::new();
        info.add(
            &adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(title))
                .subtitle(&gettext("Playlist"))
                .build(),
        );
        content.append(&info);
        let actions = adw::PreferencesGroup::new();
        let start = action_row(&gettext("Start Playlist"), "media-playback-start-symbolic");
        {
            let (sender, dialog, u, t) =
                (sender.clone(), dialog.clone(), url.to_string(), title.to_string());
            start.connect_activated(move |_| {
                sender.input(Msg::YtStartPlaylist { url: u.clone(), title: t.clone() });
                dialog.close();
            });
        }
        actions.add(&start);
        let save = action_row(&gettext("Add to Playlists"), "view-list-symbolic");
        {
            let (sender, dialog, u, t) =
                (sender.clone(), dialog.clone(), url.to_string(), title.to_string());
            save.connect_activated(move |_| {
                sender.input(Msg::YtSavePlaylist { url: u.clone(), title: t.clone() });
                dialog.close();
            });
        }
        actions.add(&save);
        let add = action_row(&gettext("Add to library"), "list-add-symbolic");
        {
            let (sender, dialog, u, t) =
                (sender.clone(), dialog.clone(), url.to_string(), title.to_string());
            add.connect_activated(move |_| {
                sender.input(Msg::YtPlaylistToLibrary { url: u.clone(), title: t.clone() });
                dialog.close();
            });
        }
        actions.add(&add);
        // For a playlist in the "Recent" history: offer removing it from there.
        if self.library.is_recent(url).unwrap_or(false) {
            let remove = action_row(&gettext("Remove from recent"), "user-trash-symbolic");
            remove.add_css_class("error");
            let (sender, dialog, u) = (sender.clone(), dialog.clone(), url.to_string());
            remove.connect_activated(move |_| {
                sender.input(Msg::YtRemoveRecent(u.clone()));
                dialog.close();
            });
            actions.add(&remove);
        }
        content.append(&actions);
        present_detail(&dialog, &content, root);
    }

    /// Loads the videos of a (not locally mirrored) YouTube playlist in the
    /// background, then opens them as a song-list subpage. Used when tapping a
    /// recent playlist that was played but never saved to the Playlists section.
    pub(crate) fn yt_open_playlist_songs(
        &self,
        url: String,
        title: String,
        sender: &ComponentSender<Self>,
    ) {
        self.yt_progress(&gettext_f("Loading “{title}” …", &[("title", &title)]));
        sender.spawn_command(move |out| {
            let result =
                youtube::list_playlist(&url, PLAYLIST_INDEX_LIMIT).map_err(|e| e.to_string());
            let _ = out.send(crate::ui::app::Cmd::YtPlaylistSongs { url, title, result });
        });
    }

    /// Subpage listing a YouTube playlist's songs. Tapping a row plays the
    /// playlist from there **and closes** the subpage; the ▶ button plays from
    /// there but **keeps it open**; long press opens the video's detail. Covers
    /// that aren't cached yet are loaded in the background and filled in.
    pub(crate) fn show_yt_playlist_songs(
        &mut self,
        sender: &ComponentSender<Self>,
        url: &str,
        title: &str,
        videos: Vec<youtube::YtResult>,
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
            .title(format!("{} ({})", gtk::glib::markup_escape_text(title), videos.len()).as_str())
            .build();
        if videos.is_empty() {
            group.add(&adw::ActionRow::builder().title(&gettext("No videos")).build());
        }
        // Cover frames whose thumbnail isn't cached yet (filled in the background).
        let mut pending: Vec<(String, adw::Bin)> = Vec::new();
        for (index, v) in videos.iter().enumerate() {
            let subtitle = v.duration.map(fmt_duration).unwrap_or_default();
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&v.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            // An enriched cover or an already-cached thumbnail shows at once; an
            // uncached thumbnail is queued for background loading.
            let thumb_url = youtube::thumbnail_url(&v.id);
            let cover = crate::core::online::youtube_cover_path(&v.id)
                .or_else(|| crate::core::online::youtube_thumb_path(&thumb_url));
            let frame = crate::ui::widgets::thumb_frame("video-x-generic-symbolic", 48);
            match cover.as_deref().and_then(crate::ui::widgets::thumb_cached) {
                Some(tex) => crate::ui::widgets::set_cover_thumb(&frame, &tex),
                None => pending.push((thumb_url, frame.clone())),
            }
            row.add_prefix(&frame);

            // ▶ button: play the playlist from here, keep the list open.
            let play = gtk::Button::builder()
                .icon_name("media-playback-start-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(&gettext("Play"))
                .css_classes(["flat"])
                .build();
            {
                let (sender, u, t) = (sender.clone(), url.to_string(), title.to_string());
                play.connect_clicked(move |_| {
                    sender.input(Msg::YtPlayPlaylistAt {
                        url: u.clone(),
                        title: t.clone(),
                        index,
                        close: false,
                    });
                });
            }
            row.add_suffix(&play);

            // Row tap: play the playlist from here and close the subpage.
            {
                let (sender, u, t) = (sender.clone(), url.to_string(), title.to_string());
                row.connect_activated(move |_| {
                    sender.input(Msg::YtPlayPlaylistAt {
                        url: u.clone(),
                        title: t.clone(),
                        index,
                        close: true,
                    });
                });
            }
            // Long press: the video's own detail.
            let lp = gtk::GestureLongPress::new();
            {
                let (sender, vid, t) = (sender.clone(), v.id.clone(), v.title.clone());
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::YtShowVideoDetail { video_id: vid.clone(), title: t.clone() });
                });
            }
            row.add_controller(lp);
            group.add(&row);
        }
        content.append(&group);
        self.push_subpage(&gettext_f("Playlist – {title}", &[("title", title)]), &content);

        // Load the missing covers in the background (a few in parallel), then fill
        // them in place via `Cmd::YtPlaylistCoversReady`.
        self.youtube.pl_cover_slots = pending;
        if !self.youtube.pl_cover_slots.is_empty() {
            let urls: Vec<String> =
                self.youtube.pl_cover_slots.iter().map(|(u, _)| u.clone()).collect();
            sender.spawn_command(move |out| {
                let threads = 8.min(urls.len().max(1));
                let chunk = (urls.len() / threads).max(1);
                std::thread::scope(|s| {
                    for part in urls.chunks(chunk) {
                        s.spawn(move || {
                            for u in part {
                                let _ = crate::core::online::cache_youtube_thumb(u);
                            }
                        });
                    }
                });
                let _ = out.send(crate::ui::app::Cmd::YtPlaylistCoversReady);
            });
        }
    }

    // ---- play/pause buttons + icon refresh -------------------------------

    /// Play/Pause button (suffix) for a video row. Registered in
    /// `video_play_buttons` so its icon tracks the playback state.
    fn video_play_button(
        &self,
        sender: &ComponentSender<Self>,
        video_id: &str,
        title: &str,
    ) -> gtk::Button {
        let btn = gtk::Button::builder()
            .icon_name("media-playback-start-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text(&gettext("Play/Pause"))
            .build();
        btn.add_css_class("flat");
        {
            let (sender, vid, t) = (sender.clone(), video_id.to_string(), title.to_string());
            btn.connect_clicked(move |_| {
                sender.input(Msg::YtPlayVideo { video_id: vid.clone(), title: t.clone() });
            });
        }
        self.youtube
            .video_play_buttons
            .borrow_mut()
            .push((video_id.to_string(), btn.clone()));
        btn
    }

    /// Updates the Play/Pause icons of visible video rows and the detail "Play"
    /// row. Detached rows are discarded.
    pub(crate) fn refresh_yt_icons(&self) {
        let active = self.youtube.playing_video_id.clone();
        let playing = self.mini.playing;
        let is_active = |vid: &str| playing && active.as_deref() == Some(vid);
        {
            let mut buttons = self.youtube.video_play_buttons.borrow_mut();
            buttons.retain(|(_, btn)| btn.root().is_some());
            for (vid, btn) in buttons.iter() {
                btn.set_icon_name(if is_active(vid) {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                });
            }
        }
        if let Some((row, vid)) = self.youtube.ctx_video_play.borrow().as_ref() {
            row.set_visible(!is_active(vid));
        }
    }

    /// Updates the "Add to library" action row of an open video detail dialog to
    /// reflect whether the add (download + transcode) is currently running.
    pub(crate) fn refresh_yt_download_row(&self) {
        let guard = self.youtube.ctx_video_download.borrow();
        let Some((row, vid)) = guard.as_ref() else {
            return;
        };
        let text = if self.youtube.downloading_videos.contains(vid) {
            gettext("Adding to library …")
        } else {
            gettext("Add to library")
        };
        row.set_title(&text);
    }

    /// Fills an open video detail dialog with metadata that arrived in the
    /// background (channel, duration, cover). No-op if the dialog closed or
    /// shows a different video.
    pub(crate) fn apply_video_meta(
        &self,
        video_id: &str,
        uploader: Option<String>,
        duration: Option<i64>,
        cover: Option<String>,
    ) {
        let guard = self.youtube.ctx_video_meta.borrow();
        let Some((vid, cover_box, channel_row, duration_row)) = guard.as_ref() else {
            return;
        };
        if vid != video_id {
            return;
        }
        channel_row.set_subtitle(
            uploader.as_deref().filter(|s| !s.trim().is_empty()).unwrap_or("—"),
        );
        duration_row.set_subtitle(&duration.map(fmt_duration).unwrap_or_else(|| "—".into()));
        if let Some(tex) = cover
            .as_deref()
            .and_then(|p| gtk::gdk::Texture::from_filename(p).ok())
        {
            while let Some(ch) = cover_box.first_child() {
                cover_box.remove(&ch);
            }
            cover_box.append(&crate::ui::widgets::rounded_image(
                Some(&tex),
                "video-x-generic-symbolic",
                200,
            ));
        }
    }

    /// Logs a played video to the "Recent" history and enriches it (artist +
    /// cover) from the online DB in the background. The enriched data arrives via
    /// [`Msg::YtEnriched`]. Called from `play_current` for every `yt:` track.
    pub(crate) fn note_youtube_play(&self, video_id: &str, title: &str) {
        // Persist the title so playlist/queue rows show a name, not the id.
        let _ = self.library.set_yt_title(video_id, title);
        // Log to Recent – but when a whole playlist is playing, the playlist is
        // logged as one entry instead of each video.
        if !self.youtube.playing_playlist {
            let _ = self.library.add_recent_video(video_id, title, None);
        }
        let input = self.input.clone();
        let (vid, t) = (video_id.to_string(), title.to_string());
        std::thread::spawn(move || {
            // The channel is the artist: take it from storage, else a metadata
            // fetch, and normalise ("… - Topic"/"…VEVO" → artist).
            let artist = Library::open()
                .ok()
                .and_then(|l| l.yt_video_info(&vid).ok().flatten())
                .map(|(c, _, _)| c)
                .or_else(|| youtube::video_meta(&vid).ok().and_then(|m| m.uploader))
                .map(|c| youtube::clean_channel_name(&c))
                .filter(|s| !s.trim().is_empty());
            // Cover from the external DB using the channel as the artist; fall
            // back to the video's own (reliable hqdefault) thumbnail.
            let cover = artist
                .as_deref()
                .and_then(|a| crate::core::online::track_cover(a, &t))
                .and_then(|(bytes, _album)| crate::core::online::store_youtube_cover(&vid, &bytes))
                .or_else(|| {
                    crate::core::online::cache_youtube_thumb(&youtube::thumbnail_url(&vid))
                });
            let _ = input.send(Msg::YtEnriched { video_id: vid, artist, cover });
        });
    }

    /// Shows or updates the persistent "adding to library" progress toast.
    /// (Bypasses the disabled informational `toast()` on purpose – progress is
    /// requested feedback.)
    pub(crate) fn yt_progress(&self, msg: &str) {
        let mut slot = self.youtube.progress_toast.borrow_mut();
        match slot.as_ref() {
            Some(t) => t.set_title(msg),
            None => {
                let t = adw::Toast::new(msg);
                t.set_timeout(0); // stays until finished
                self.toast_overlay.add_toast(t.clone());
                *slot = Some(t);
            }
        }
    }

    /// Finishes the progress toast with a short final message.
    pub(crate) fn yt_progress_done(&self, msg: &str) {
        if let Some(t) = self.youtube.progress_toast.borrow_mut().take() {
            t.dismiss();
        }
        let t = adw::Toast::new(msg);
        t.set_timeout(3);
        self.toast_overlay.add_toast(t);
    }

    /// Adds a single video to the on-disk music library: download + transcode +
    /// index in one step (background). Needs a music folder set.
    pub(crate) fn yt_add_video_to_library(
        &mut self,
        video_id: String,
        title: String,
        sender: &ComponentSender<Self>,
        overwrite: bool,
    ) {
        if self.youtube.downloading_videos.contains(&video_id) {
            return;
        }
        let Some(music) = self.files.music_dir.clone() else {
            self.toast(&gettext("Set a music folder in settings first"));
            return;
        };
        self.youtube.downloading_videos.insert(video_id.clone());
        self.refresh_yt_download_row();
        self.yt_progress(&gettext_f("Adding “{title}” to library …", &[("title", &title)]));
        let cover = crate::core::online::youtube_cover_path(&video_id);
        let vid = video_id;
        sender.spawn_command(move |out| {
            let cmd = match library_add_one(&vid, &title, &music, cover.as_deref(), overwrite) {
                Ok(AddOutcome::Added) => {
                    crate::ui::app::Cmd::YtLibraryAdded { video_id: Some(vid), result: Ok(1) }
                }
                // Destination taken by a different song → ask the user.
                Ok(AddOutcome::Exists(dest)) => crate::ui::app::Cmd::YtLibraryExists {
                    video_id: vid,
                    title,
                    dest: dest.to_string_lossy().into_owned(),
                },
                Err(e) => {
                    crate::ui::app::Cmd::YtLibraryAdded { video_id: Some(vid), result: Err(e) }
                }
            };
            let _ = out.send(cmd);
        });
    }

    /// Adds all videos of a playlist to the on-disk music library (download +
    /// transcode + index each). Background.
    pub(crate) fn yt_playlist_to_library(
        &self,
        url: String,
        title: String,
        sender: &ComponentSender<Self>,
    ) {
        let Some(music) = self.files.music_dir.clone() else {
            self.toast(&gettext("Set a music folder in settings first"));
            return;
        };
        self.yt_progress(&gettext_f("Adding playlist “{title}” to library …", &[("title", &title)]));
        sender.spawn_command(move |out| {
            let r = (|| -> Result<usize, String> {
                let videos = youtube::list_playlist(&url, PLAYLIST_INDEX_LIMIT)
                    .map_err(|e| e.to_string())?;
                let total = videos.len();
                let mut n = 0;
                let _ = out.send(crate::ui::app::Cmd::YtLibraryProgress { done: 0, total });
                for (i, v) in videos.into_iter().enumerate() {
                    let cover = crate::core::online::youtube_cover_path(&v.id);
                    // overwrite = false → tracks already on disk are skipped, never
                    // clobbered (no per-track prompt for a whole playlist).
                    if let Ok(AddOutcome::Added) =
                        library_add_one(&v.id, &v.title, &music, cover.as_deref(), false)
                    {
                        n += 1;
                    }
                    let _ = out.send(crate::ui::app::Cmd::YtLibraryProgress { done: i + 1, total });
                }
                Ok(n)
            })();
            let _ = out.send(crate::ui::app::Cmd::YtLibraryAdded { video_id: None, result: r });
        });
    }

    /// Saves a found playlist into the Playlists section (mirrors its videos as
    /// `yt:` items) without playing it. Background.
    pub(crate) fn yt_save_playlist(
        &self,
        url: String,
        title: String,
        sender: &ComponentSender<Self>,
    ) {
        self.yt_progress(&gettext_f("Saving “{title}” to Playlists …", &[("title", &title)]));
        sender.spawn_command(move |out| {
            let r = (|| -> Result<usize, String> {
                let videos = youtube::list_playlist(&url, PLAYLIST_INDEX_LIMIT)
                    .map_err(|e| e.to_string())?;
                let lib = Library::open().map_err(|e| e.to_string())?;
                let mut paths = Vec::with_capacity(videos.len());
                for v in &videos {
                    let _ = lib.set_yt_title(&v.id, &v.title);
                    paths.push(crate::core::youtube::yt_path(&v.id));
                }
                lib.replace_yt_playlist(&url, &title, &paths).map_err(|e| e.to_string())?;
                Ok(paths.len())
            })();
            let _ = out.send(crate::ui::app::Cmd::YtPlaylistSaved(r));
        });
    }

    /// Plays a subscribed channel's cached videos as the queue.
    pub(crate) fn yt_play_channel(&mut self, id: i64) {
        let videos = self.library.channel_videos(id).unwrap_or_default();
        if videos.is_empty() {
            self.toast(&gettext("No videos"));
            return;
        }
        // A channel is not a playlist – its videos log to Recent individually.
        self.youtube.playing_playlist = false;
        self.youtube.video_titles.clear();
        let mut queue = Vec::with_capacity(videos.len());
        for v in videos {
            let _ = self.library.set_yt_title(&v.video_id, &v.title);
            self.youtube.video_titles.insert(v.video_id.clone(), v.title);
            queue.push(std::path::PathBuf::from(crate::core::youtube::yt_path(&v.video_id)));
        }
        self.transport.queue = queue;
        self.transport.queue_pos = 0;
        self.play_current();
    }

    /// Resets the optimistic now-playing state after a failed resolve/stream.
    pub(crate) fn youtube_playback_failed(&mut self, _sender: &ComponentSender<Self>) {
        self.mini.playing = false;
        self.youtube.playing_video_id = None;
        self.mpris.set_playing(false);
        self.refresh_yt_icons();
        self.refresh_queue_icons();
        self.toast(&gettext("Could not play video"));
    }

    // ---- yt-dlp install / status -----------------------------------------

    /// Starts a yt-dlp download (or update) in the background; the result lands
    /// in `Cmd::YtDlpReady`. Ignores repeat taps while one is running.
    pub(crate) fn start_ytdlp_fetch(&mut self, update: bool, sender: &ComponentSender<Self>) {
        if self.youtube.ytdlp_busy {
            return;
        }
        self.youtube.ytdlp_busy = true;
        let msg = if update {
            gettext("Updating yt-dlp …")
        } else {
            gettext("Downloading yt-dlp …")
        };
        self.toast(&msg);
        self.refresh_ytdlp_status_label();
        sender.spawn_command(move |out| {
            let result = if update {
                youtube::update_ytdlp()
            } else {
                youtube::download_ytdlp()
            }
            .map_err(|e| e.to_string());
            let _ = out.send(crate::ui::app::Cmd::YtDlpReady(result));
        });
    }

    /// Refreshes the yt-dlp status label in the open settings dialog.
    pub(crate) fn refresh_ytdlp_status_label(&self) {
        let guard = self.youtube.settings_status.borrow();
        let Some(label) = guard.as_ref() else {
            return;
        };
        let text = if self.youtube.ytdlp_busy {
            gettext("Working …")
        } else {
            match self.youtube.ytdlp_version.clone().or_else(youtube::version) {
                Some(v) => gettext_f("Installed (version {v})", &[("v", &v)]),
                None => gettext("Not installed"),
            }
        };
        label.set_text(&text);
    }

    // ---- enable/disable the feature --------------------------------------

    /// Toggles the YouTube feature: persists the setting, shows/hides the
    /// section (reusing [`Self::set_section_visible`]), and (when enabling)
    /// checks/refreshes in the background.
    pub(crate) fn set_youtube_enabled(&mut self, on: bool, sender: &ComponentSender<Self>) {
        self.youtube.enabled = on;
        let _ = self
            .library
            .set_setting("youtube_enabled", if on { "1" } else { "0" });
        self.set_section_visible("youtube", on);
        if on {
            if youtube::available() {
                self.reload_channels(sender);
            } else {
                self.toast(&gettext("Download yt-dlp in the settings to use YouTube"));
            }
        }
    }
}

/// Activatable action row with an icon prefix (local copy of the podcast
/// module's private helper).
fn action_row(title: &str, icon: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).activatable(true).build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row
}
