//! YouTube as a standalone relm4 component: channel overview (+ gallery), the
//! "Newest"/"Recent" lists, the search/subscribe dialog, channel/video/playlist
//! detail dialogs, the playlist-songs subpage, and the add-to-library/offline
//! glue. Extracted from the `App` god-object, mirroring [`crate::ui::podcasts_page`].
//!
//! **Boundary:** this component owns the *page*; the transport (playing a
//! video/channel/playlist) and the yt-dlp/settings management stay on `App`
//! (see `app_yt_glue.rs`). Playback is requested through [`YtOutput`]
//! (`PlayVideo`/`PlayChannel`/`StartPlaylist`/`StartPlaylistAt`) and the row
//! play/pause icons are kept in sync via [`YtInput::PlaybackStateChanged`].
//! Toasts, the loading overlay, sub-page navigation, the equalizer dialog, the
//! settings dialog and library/playlist reloads all live on the parent chrome,
//! so they too travel through `YtOutput`.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::core::db::Library;
use crate::core::youtube::{self, YtKind, YtResult};
use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::ui::app::{SortCrit, YtView};
use crate::ui::app_gallery::{gallery_cell, spawn_gallery_decode};
use crate::ui::app_helpers::{cover_widget, on_long_press, on_secondary_click};
use crate::ui::app_sort::{read_sort, sort_popover};
use crate::ui::app_views::natural_key;

/// How many newest videos to cache per channel on subscribe/refresh.
pub(crate) const CHANNEL_VIDEO_LIMIT: usize = 30;
/// Upper bound of videos indexed when adding a whole playlist to the collection.
const PLAYLIST_INDEX_LIMIT: usize = 200;
/// How long a cached browsed-playlist song list is served as-is before a
/// background refresh is kicked off on the next open (6 hours).
const PLAYLIST_CACHE_TTL_SECS: i64 = 6 * 60 * 60;

/// Content box for the detail dialogs (uniform margins).
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
    crate::ui::app_helpers::close_on_click_outside(dialog);
    dialog.present(Some(root));
}

/// Activatable action row with an icon prefix (for the detail dialogs).
fn action_row(title: &str, icon: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row
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

/// Sortable key `YYYYMMDDHHMMSS` from an ISO-8601 publication timestamp.
fn yt_pubdate_key(published: Option<&str>) -> i64 {
    let Some(s) = published.filter(|s| !s.trim().is_empty()) else {
        return 0;
    };
    let Ok(dt) = gtk::glib::DateTime::from_iso8601(s, None) else {
        return 0;
    };
    (((((dt.year() as i64 * 100 + dt.month() as i64) * 100 + dt.day_of_month() as i64) * 100
        + dt.hour() as i64)
        * 100
        + dt.minute() as i64)
        * 100)
        + dt.seconds() as i64
}

/// Formats an ISO-8601 publication timestamp as `DD.MM.YYYY HH:MM`.
fn fmt_published(iso: &str) -> String {
    gtk::glib::DateTime::from_iso8601(iso, None)
        .ok()
        .and_then(|dt| dt.format("%d.%m.%Y %H:%M").ok())
        .map(|g| g.to_string())
        .unwrap_or_else(|| iso.to_string())
}

/// A right-aligned, subtle duration label for a video row.
fn duration_chip(secs: i64) -> gtk::Label {
    let lbl = gtk::Label::new(Some(&fmt_duration(secs)));
    lbl.set_valign(gtk::Align::Center);
    lbl.set_css_classes(&["dim-label", "numeric"]);
    lbl
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

/// Subscribes to a channel and caches its newest videos (worker thread, own DB).
pub(crate) fn fetch_and_store_channel(
    channel_id: &str,
    title: &str,
    url: &str,
    thumbnail: Option<&str>,
) -> Option<String> {
    let lib = Library::open().ok()?;
    let cid = lib
        .subscribe_channel(channel_id, title, url, thumbnail)
        .ok()?;
    let videos = list_channel_videos(url, Some(channel_id));
    let _ = lib.set_channel_videos(cid, &videos);
    if let Some(t) = thumbnail {
        crate::core::online::cache_youtube_thumb(t);
    }
    Some(title.to_string())
}

/// Refreshes a subscribed channel's newest videos (worker thread, own DB).
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

/// Lists a channel's newest videos via yt-dlp and merges in publication dates.
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
    for v in &videos {
        crate::core::online::cache_youtube_thumb(&youtube::thumbnail_url(&v.video_id));
    }
    videos
}

/// The YouTube page component.
pub(crate) struct YtPage {
    /// Own DB connection (WAL + per-thread).
    library: Library,
    /// Window the dialogs are presented on (set on `SetWindow`).
    window: Option<adw::ApplicationWindow>,
    /// Mirror of the transport's `playing_video_id` (for row icons).
    playing_video_id: Option<String>,
    /// Mirror of the transport play/pause state.
    playing: bool,
    /// Mirror of the global gallery setting.
    gallery_view: bool,
    gallery_columns: u32,
    /// Narrow (mobile) layout → detail dialogs as bottom sheets.
    mobile: bool,
    /// yt-dlp can no longer parse YouTube → show the warning banner. Mirror of
    /// [`crate::core::youtube::extraction_broken`], refreshed after each command.
    ytdlp_broken: bool,
    /// Which view is visible: newest / recent / channels.
    yt_view: YtView,
    /// Sort of the subscriptions (channels) overview (criterion + descending).
    /// Persisted as "sort_channels" / "sort_channels_desc". The date-ordered
    /// Recent/Newest views are not affected.
    channels_sort: (SortCrit, bool),
    /// "Without grouping" for the channels list (no alphabetical headings).
    /// Persisted as "nogroup_channels".
    channels_no_group: bool,
    /// Sort of the "Recent" (recently played) list (criterion + descending).
    /// Persisted as "sort_yt_recent" / "sort_yt_recent_desc". Default: by date
    /// (most recent first), i.e. the natural `played_at` order from the DB.
    recent_sort: (SortCrit, bool),
    /// Per-view gallery override (sort popover); `None` follows the global
    /// `gallery_view`. Persisted as "gallery_channels".
    gallery_override: Option<bool>,
    /// Per-row alphabetical headings of the channels list (name sort).
    channel_headers: std::rc::Rc<std::cell::RefCell<Option<Vec<String>>>>,
    /// Hand-off for the shared title-bar sort button: [`Self::rebuild_sort`]
    /// writes the popover + direction here (or `None` to hide it) for the active
    /// view, then signals the parent via [`YtOutput::SortChanged`].
    sort_slot: crate::ui::app_sort::SortSlot,
    /// (id, title, url, thumbnail, video count) per subscribed channel.
    channel_items: Vec<(i64, String, String, Option<String>, i64)>,
    channels_list: gtk::ListBox,
    channels_gallery: gtk::FlowBox,
    newest_items: Vec<crate::model::YtVideoRef>,
    newest_list: gtk::Box,
    recent_items: Vec<crate::model::YtRecent>,
    recent_list: gtk::Box,
    search_results: Vec<YtResult>,
    search_failed: bool,
    /// Monotonic search counter. Every new search bumps it; command results
    /// carrying an older value are ignored. This keeps the "Searching …"
    /// spinner up until the *current* search returns — a still-running worker
    /// from a previous search (e.g. after switching Songs→Playlists→Channels)
    /// can no longer clear the spinner early or flash stale results.
    search_seq: u64,
    search: Rc<RefCell<Option<(adw::Dialog, gtk::ListBox, gtk::Box)>>>,
    video_play_buttons: Rc<RefCell<Vec<(String, gtk::Button)>>>,
    ctx_video_play: Rc<RefCell<Option<(adw::ActionRow, String)>>>,
    ctx_video_download: Rc<RefCell<Option<(adw::ActionRow, String)>>>,
    ctx_video_meta: Rc<RefCell<Option<(String, gtk::Box, adw::ActionRow, adw::ActionRow, bool)>>>,
    downloading_videos: HashSet<String>,
    playlist_songs_cache: HashMap<String, Vec<YtResult>>,
    pl_cover_slots: Vec<(String, adw::Bin)>,
    /// Hand-off slot for built subpages (the `!Send` widget can't ride a message).
    subpage_slot: Rc<RefCell<Option<(String, gtk::Box)>>>,
}

#[derive(Debug)]
pub(crate) enum YtInput {
    // --- driven by the parent ---
    Reload,
    RefreshAll,
    ReloadRecent,
    PlaybackStateChanged {
        playing_video_id: Option<String>,
        playing: bool,
    },
    RefreshBroken,
    SetView(YtView),
    /// Change the subscriptions sort (criterion + descending), from the header.
    SetSort(SortCrit, bool),
    /// Change the "Recent" list sort (criterion + descending), from the header.
    SetRecentSort(SortCrit, bool),
    /// Toggle alphabetical grouping of the channels list (`true` = no grouping).
    SetNoGroup(bool),
    /// Per-view gallery override for the channels (sort popover toggle).
    SetGallery(bool),
    SetGalleryView(bool),
    SetGalleryColumns(u32),
    SetMobile(bool),
    SetWindow(adw::ApplicationWindow),
    // --- view-internal ---
    /// Banner button → ask the parent to open the settings (yt-dlp update).
    OpenSettings,
    Subscribe,
    Search(String, YtKind),
    SubscribeChannel(String),
    OpenChannel(i64),
    OpenChannelAt(usize),
    ShowChannelDetail(i64),
    ShowChannelDetailAt(usize),
    RefreshChannel(i64),
    DeleteChannel(i64),
    DeleteChannelConfirmed(i64),
    AddRecent {
        video_id: String,
        title: String,
    },
    RemoveRecent(String),
    ShowVideoDetail {
        video_id: String,
        title: String,
    },
    ShowNewestDetail(usize),
    ShowPlaylistDetail {
        url: String,
        title: String,
    },
    OpenRecentPlaylist {
        url: String,
        title: String,
    },
    PlayPlaylistAt {
        url: String,
        title: String,
        index: usize,
        close: bool,
    },
    AddToLibrary {
        video_id: String,
        title: String,
    },
    AddToLibraryConfirmed {
        video_id: String,
        title: String,
    },
    PlaylistToLibrary {
        url: String,
        title: String,
    },
    SavePlaylist {
        url: String,
        title: String,
    },
}

#[derive(Debug)]
pub(crate) enum YtOutput {
    /// Transport: play/pause this single video.
    PlayVideo {
        video_id: String,
        title: String,
    },
    /// Transport: play a subscribed channel's videos as the queue.
    PlayChannel(i64),
    /// Transport: resolve a playlist URL and start playing it.
    StartPlaylist {
        url: String,
        title: String,
    },
    /// Transport: play the (already-resolved) playlist videos starting at `index`.
    StartPlaylistAt {
        url: String,
        title: String,
        index: usize,
        close: bool,
        videos: Vec<(String, String, Option<i64>)>,
    },
    /// Open the equalizer dialog for a `yt:<id>` track.
    OpenTrackEq {
        path: String,
        title: String,
    },
    /// Open a mirrored playlist in the Playlists section.
    OpenPlaylist {
        id: i64,
        name: String,
    },
    /// Open the settings dialog (yt-dlp banner button).
    OpenSettings,
    /// Informational toast.
    Toast(String),
    /// Show/update the persistent add-to-library progress toast.
    Progress(String),
    /// Finish the progress toast with a short final message.
    ProgressDone(String),
    /// Set/clear the central loading overlay (`Some(label)` = show, `None` = clear).
    SetLoading(Option<String>),
    /// A track/playlist was added → reload artist/album overviews.
    LibraryChanged,
    /// A playlist was saved → reload the Playlists section.
    PlaylistsChanged,
    /// A built subpage is parked in `subpage_slot` → push it onto the shared nav.
    PushSubpage,
    /// Show the "channel removed" undo toast; deferred deletion comes back as
    /// `DeleteChannelConfirmed`.
    DeleteChannelUndo(i64),
    /// A "refresh all" worker was started / finished → drive the spinner.
    RefreshStarted(bool),
    RefreshFinished,
    /// Share a selection (a YouTube channel or video) over device sync.
    Share(crate::core::sync::share::Selection),
    /// The sort slot was rebuilt → the parent refreshes the shared title-bar
    /// sort button (if the YouTube section is showing).
    SortChanged,
}

#[derive(Debug)]
pub(crate) enum YtCmd {
    SearchResults(u64, Vec<YtResult>),
    SearchFailed(u64),
    SearchThumbsReady(u64),
    ChannelFetched(Option<String>),
    ChannelsRefreshed,
    VideoMeta {
        video_id: String,
        uploader: Option<String>,
        duration: Option<i64>,
        cover: Option<String>,
    },
    LibraryProgress {
        done: usize,
        total: usize,
    },
    LibraryAdded {
        video_id: Option<String>,
        result: Result<usize, String>,
    },
    LibraryExists {
        video_id: String,
        title: String,
        dest: String,
    },
    PlaylistSongs {
        url: String,
        title: String,
        result: Result<Vec<YtResult>, String>,
    },
    /// A stale cached playlist was re-fetched in the background: refresh the DB
    /// cache silently (no UI), so the *next* open shows fresh songs.
    PlaylistCacheRefreshed {
        url: String,
        title: String,
        result: Result<Vec<YtResult>, String>,
    },
    PlaylistCoversReady,
    PlaylistSaved(Result<usize, String>),
    /// Cover for a `yt_add_recent` entry finished caching.
    RecentEnriched {
        video_id: String,
        cover: Option<String>,
    },
    /// Startup channel-thumbnail cache finished → redraw.
    CoversCached,
}

#[relm4::component(pub(crate))]
impl Component for YtPage {
    type Init = (
        Rc<RefCell<Option<(String, gtk::Box)>>>,
        crate::ui::app_sort::SortSlot,
    );
    type Input = YtInput;
    type Output = YtOutput;
    type CommandOutput = YtCmd;

    view! {
        #[root]
        gtk::Box {
            set_orientation: gtk::Orientation::Vertical,

            // Warning when yt-dlp can no longer parse YouTube.
            adw::Banner {
                #[watch]
                set_visible: model.ytdlp_broken,
                #[watch]
                set_revealed: model.ytdlp_broken,
                set_title: &gettext("YouTube isn't working right now – update yt-dlp in the settings, or wait for a newer release."),
                set_button_label: Some(&gettext("Settings")),
                connect_button_clicked => YtInput::OpenSettings,
            },

            // Header: Recent / Newest / Subscriptions switcher + "+" to search.
            gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 6,
                set_margin_top: 2,
                set_margin_bottom: 4,
                set_margin_start: 12,
                set_margin_end: 12,
                add_css_class: "linked",
                add_css_class: "emilia-tabbar",

                gtk::ToggleButton {
                    set_label: &gettext("Recently"),
                    set_hexpand: true,
                    #[watch]
                    set_active: model.yt_view == YtView::Recent,
                    connect_clicked => YtInput::SetView(YtView::Recent),
                },
                gtk::ToggleButton {
                    set_label: &gettext("Newest"),
                    set_hexpand: true,
                    #[watch]
                    set_active: model.yt_view == YtView::Newest,
                    connect_clicked => YtInput::SetView(YtView::Newest),
                },
                gtk::ToggleButton {
                    set_label: &gettext("Subscriptions"),
                    set_hexpand: true,
                    #[watch]
                    set_active: model.yt_view == YtView::Channels,
                    connect_clicked => YtInput::SetView(YtView::Channels),
                },
                gtk::Button {
                    set_icon_name: "list-add-symbolic",
                    set_tooltip_text: Some(&gettext("Search YouTube")),
                    add_css_class: "flat",
                    connect_clicked => YtInput::Subscribe,
                },
            },

            // "Newest"
            gtk::ScrolledWindow {
                set_vexpand: true,
                #[watch]
                set_visible: model.yt_view == YtView::Newest && !model.newest_items.is_empty(),
                #[local_ref]
                yt_newest_list -> gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 6,
                    set_valign: gtk::Align::Start,
                    set_margin_top: 10,
                    set_margin_bottom: 12,
                    set_margin_start: 12,
                    set_margin_end: 12,
                },
            },
            adw::StatusPage {
                set_icon_name: Some("video-x-generic-symbolic"),
                set_title: &gettext("No videos yet"),
                set_description: Some(&gettext("Subscribe to a channel to follow its newest videos.")),
                set_vexpand: true,
                #[watch]
                set_visible: model.yt_view == YtView::Newest && model.newest_items.is_empty(),
            },

            // "Recent"
            gtk::ScrolledWindow {
                set_vexpand: true,
                #[watch]
                set_visible: model.yt_view == YtView::Recent && !model.recent_items.is_empty(),
                #[local_ref]
                yt_recent_list -> gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 6,
                    set_valign: gtk::Align::Start,
                    set_margin_top: 10,
                    set_margin_bottom: 12,
                    set_margin_start: 12,
                    set_margin_end: 12,
                },
            },
            adw::StatusPage {
                set_icon_name: Some("document-open-recent-symbolic"),
                set_title: &gettext("Nothing played yet"),
                set_description: Some(&gettext("Videos you play appear here.")),
                set_vexpand: true,
                #[watch]
                set_visible: model.yt_view == YtView::Recent && model.recent_items.is_empty(),
            },

            // "Channels" (list)
            gtk::ScrolledWindow {
                set_vexpand: true,
                #[watch]
                set_visible: model.yt_view == YtView::Channels && !model.channel_items.is_empty() && !model.gallery_on(),
                #[local_ref]
                yt_channels_list -> gtk::ListBox {
                    set_valign: gtk::Align::Start,
                    set_margin_top: 10,
                    set_margin_bottom: 12,
                    set_margin_start: 12,
                    set_margin_end: 12,
                    set_css_classes: &["boxed-list"],
                },
            },
            // "Channels" (gallery)
            gtk::ScrolledWindow {
                set_vexpand: true,
                #[watch]
                set_visible: model.yt_view == YtView::Channels && !model.channel_items.is_empty() && model.gallery_on(),
                #[local_ref]
                yt_channels_gallery -> gtk::FlowBox {
                    set_valign: gtk::Align::Start,
                    set_margin_top: 10,
                    set_margin_bottom: 12,
                    set_margin_start: 12,
                    set_margin_end: 12,
                },
            },
            adw::StatusPage {
                set_icon_name: Some("video-x-generic-symbolic"),
                set_title: &gettext("No subscriptions"),
                set_description: Some(&gettext("Search YouTube and subscribe to a channel.")),
                set_vexpand: true,
                #[watch]
                set_visible: model.yt_view == YtView::Channels && model.channel_items.is_empty(),
            },
        }
    }

    fn init(
        (subpage_slot, sort_slot): Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let library = Library::open_or_memory();
        let yt_channels_list = gtk::ListBox::new();
        let yt_channels_gallery = gtk::FlowBox::new();
        let yt_newest_list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let yt_recent_list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        // Restore the persisted subscriptions sort (default: by name, ascending) +
        // the grouping/gallery choices.
        let channels_sort = read_sort(&library, "channels", SortCrit::Name, false);
        // Recent list sort (default: by date, most recent first).
        let recent_sort = read_sort(&library, "yt_recent", SortCrit::Release, true);
        let channels_no_group = matches!(
            library
                .get_setting("nogroup_channels")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        let gallery_override = match library
            .get_setting("gallery_channels")
            .ok()
            .flatten()
            .as_deref()
        {
            Some("1") => Some(true),
            Some("0") => Some(false),
            _ => None,
        };
        let channel_headers = std::rc::Rc::new(std::cell::RefCell::new(None));
        yt_channels_list.set_header_func(crate::ui::app_gallery::list_section_header_func(
            channel_headers.clone(),
        ));
        let model = YtPage {
            library,
            window: None,
            playing_video_id: None,
            playing: false,
            gallery_view: false,
            gallery_columns: 4,
            mobile: false,
            ytdlp_broken: false,
            yt_view: YtView::Recent,
            channels_sort,
            channels_no_group,
            recent_sort,
            gallery_override,
            channel_headers,
            sort_slot,
            channel_items: Vec::new(),
            channels_list: yt_channels_list.clone(),
            channels_gallery: yt_channels_gallery.clone(),
            newest_items: Vec::new(),
            newest_list: yt_newest_list.clone(),
            recent_items: Vec::new(),
            recent_list: yt_recent_list.clone(),
            search_results: Vec::new(),
            search_failed: false,
            search_seq: 0,
            search: Rc::new(RefCell::new(None)),
            video_play_buttons: Rc::new(RefCell::new(Vec::new())),
            ctx_video_play: Rc::new(RefCell::new(None)),
            ctx_video_download: Rc::new(RefCell::new(None)),
            ctx_video_meta: Rc::new(RefCell::new(None)),
            downloading_videos: HashSet::new(),
            playlist_songs_cache: HashMap::new(),
            pl_cover_slots: Vec::new(),
            subpage_slot,
        };
        // Cache the channel thumbnails once in the background, then redraw.
        sender.spawn_oneshot_command(|| {
            if let Ok(lib) = Library::open() {
                for (_, _, _, thumb, _) in lib.channels().unwrap_or_default() {
                    if let Some(t) = thumb {
                        crate::core::online::cache_youtube_thumb(&t);
                    }
                }
            }
            YtCmd::CoversCached
        });
        let widgets = view_output!();
        // Build the header sort popover for the restored subscriptions sort.
        model.rebuild_sort(&sender);
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: YtInput, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            YtInput::Reload => self.reload_channels(&sender),
            YtInput::RefreshAll => {
                if self.channel_items.is_empty() {
                    return;
                }
                sender.spawn_oneshot_command(|| {
                    if let Ok(lib) = Library::open() {
                        if youtube::available() {
                            for (id, title, url, thumb, _) in lib.channels().unwrap_or_default() {
                                if let Some(t) = thumb.as_deref() {
                                    crate::core::online::cache_youtube_thumb(t);
                                }
                                let _ = refresh_channel_videos(id, &title, &url);
                            }
                        }
                    }
                    YtCmd::ChannelsRefreshed
                });
                let _ = sender.output(YtOutput::RefreshStarted(true));
            }
            YtInput::ReloadRecent => self.reload_yt_recent(&sender),
            YtInput::PlaybackStateChanged {
                playing_video_id,
                playing,
            } => {
                self.playing_video_id = playing_video_id;
                self.playing = playing;
                self.refresh_yt_icons();
            }
            YtInput::RefreshBroken => self.ytdlp_broken = youtube::extraction_broken(),
            YtInput::SetView(v) => {
                self.yt_view = v;
                // Each view has its own sort control (Newest: none).
                self.rebuild_sort(&sender);
            }
            YtInput::SetSort(crit, desc) => {
                if self.channels_sort != (crit, desc) {
                    self.channels_sort = (crit, desc);
                    let _ = self.library.set_setting("sort_channels", crit.as_key());
                    let _ = self
                        .library
                        .set_setting("sort_channels_desc", if desc { "1" } else { "0" });
                    self.reload_channels(&sender);
                }
            }
            YtInput::SetRecentSort(crit, desc) => {
                if self.recent_sort != (crit, desc) {
                    self.recent_sort = (crit, desc);
                    let _ = self.library.set_setting("sort_yt_recent", crit.as_key());
                    let _ = self
                        .library
                        .set_setting("sort_yt_recent_desc", if desc { "1" } else { "0" });
                    self.reload_yt_recent(&sender);
                }
            }
            YtInput::SetNoGroup(off) => {
                if self.channels_no_group != off {
                    self.channels_no_group = off;
                    let _ = self
                        .library
                        .set_setting("nogroup_channels", if off { "1" } else { "0" });
                    self.reload_channels(&sender);
                }
            }
            YtInput::SetGallery(on) => {
                if self.gallery_override != Some(on) {
                    self.gallery_override = Some(on);
                    let _ = self
                        .library
                        .set_setting("gallery_channels", if on { "1" } else { "0" });
                    self.reload_channels(&sender);
                }
            }
            YtInput::SetGalleryView(on) => {
                self.gallery_view = on;
                self.reload_channels(&sender);
            }
            YtInput::SetGalleryColumns(n) => {
                self.gallery_columns = n.clamp(2, 8);
                if self.gallery_view {
                    self.reload_channels(&sender);
                }
            }
            YtInput::SetMobile(b) => self.mobile = b,
            YtInput::SetWindow(w) => self.window = Some(w),
            YtInput::OpenSettings => {
                let _ = sender.output(YtOutput::OpenSettings);
            }
            YtInput::Subscribe => self.open_youtube_search_dialog(&sender),
            YtInput::Search(term, kind) => {
                let term = term.trim().to_string();
                if !term.is_empty() {
                    self.search_seq = self.search_seq.wrapping_add(1);
                    let seq = self.search_seq;
                    sender.spawn_command(move |out| {
                        let results = match youtube::search(&term, kind, 25) {
                            Ok(r) => r,
                            Err(_) => {
                                let _ = out.send(YtCmd::SearchFailed(seq));
                                return;
                            }
                        };
                        let _ = out.send(YtCmd::SearchResults(seq, results.clone()));
                        for r in &results {
                            if let Some(t) = r.thumbnail.as_deref() {
                                crate::core::online::cache_youtube_thumb(t);
                            }
                        }
                        let _ = out.send(YtCmd::SearchThumbsReady(seq));
                    });
                }
            }
            YtInput::SubscribeChannel(url) => {
                if let Some(r) = self
                    .search_results
                    .iter()
                    .find(|r| r.url == url && r.kind == YtKind::Channel)
                    .cloned()
                {
                    let _ = sender.output(YtOutput::SetLoading(Some(gettext_f(
                        "Subscribing to {t} …",
                        &[("t", &r.title)],
                    ))));
                    sender.spawn_command(move |out| {
                        let t = fetch_and_store_channel(
                            &r.id,
                            &r.title,
                            &r.url,
                            r.thumbnail.as_deref(),
                        );
                        let _ = out.send(YtCmd::ChannelFetched(t));
                    });
                }
            }
            YtInput::OpenChannel(id) => {
                if let Some((_, title, _, _, _)) = self
                    .channel_items
                    .iter()
                    .find(|(cid, _, _, _, _)| *cid == id)
                    .cloned()
                {
                    self.open_channel(&sender, id, &title);
                }
            }
            YtInput::OpenChannelAt(index) => {
                if let Some(id) = self.channel_items.get(index).map(|c| c.0) {
                    sender.input(YtInput::OpenChannel(id));
                }
            }
            YtInput::ShowChannelDetail(id) => self.open_channel_detail(&sender, id),
            YtInput::ShowChannelDetailAt(index) => {
                if let Some(id) = self.channel_items.get(index).map(|c| c.0) {
                    sender.input(YtInput::ShowChannelDetail(id));
                }
            }
            YtInput::RefreshChannel(id) => {
                if let Some((_, title, url, _, _)) = self
                    .channel_items
                    .iter()
                    .find(|(cid, _, _, _, _)| *cid == id)
                    .cloned()
                {
                    let _ = sender.output(YtOutput::Toast(gettext("Refreshing …")));
                    sender.spawn_command(move |out| {
                        let _ = out.send(YtCmd::ChannelFetched(refresh_channel_videos(
                            id, &title, &url,
                        )));
                    });
                }
            }
            YtInput::DeleteChannel(id) => {
                let _ = sender.output(YtOutput::DeleteChannelUndo(id));
            }
            YtInput::DeleteChannelConfirmed(id) => {
                let _ = self.library.delete_channel(id);
                self.reload_channels(&sender);
            }
            YtInput::AddRecent { video_id, title } => self.yt_add_recent(&sender, video_id, title),
            YtInput::RemoveRecent(key) => {
                let _ = self.library.delete_recent(&key);
                self.reload_yt_recent(&sender);
            }
            YtInput::ShowVideoDetail { video_id, title } => {
                self.show_video_detail(&sender, &video_id, &title)
            }
            YtInput::ShowNewestDetail(index) => {
                if let Some(v) = self.newest_items.get(index) {
                    let (vid, title) = (v.video_id.clone(), v.title.clone());
                    self.show_video_detail(&sender, &vid, &title);
                }
            }
            YtInput::ShowPlaylistDetail { url, title } => {
                self.show_playlist_detail(&sender, &url, &title)
            }
            YtInput::OpenRecentPlaylist { url, title } => {
                self.yt_open_recent_playlist(&sender, url, title)
            }
            YtInput::PlayPlaylistAt {
                url,
                title,
                index,
                close,
            } => {
                if let Some(videos) = self.playlist_songs_cache.get(&url) {
                    let videos: Vec<(String, String, Option<i64>)> = videos
                        .iter()
                        .map(|v| (v.id.clone(), v.title.clone(), v.duration))
                        .collect();
                    let _ = sender.output(YtOutput::StartPlaylistAt {
                        url,
                        title,
                        index,
                        close,
                        videos,
                    });
                }
            }
            YtInput::AddToLibrary { video_id, title } => {
                self.yt_add_video_to_library(&sender, video_id, title, false)
            }
            YtInput::AddToLibraryConfirmed { video_id, title } => {
                self.yt_add_video_to_library(&sender, video_id, title, true)
            }
            YtInput::PlaylistToLibrary { url, title } => {
                self.yt_playlist_to_library(&sender, url, title)
            }
            YtInput::SavePlaylist { url, title } => self.yt_save_playlist(&sender, url, title),
        }
    }

    fn update_cmd(&mut self, cmd: YtCmd, sender: ComponentSender<Self>, _root: &Self::Root) {
        match cmd {
            YtCmd::SearchResults(seq, results) => {
                if seq != self.search_seq {
                    return; // a newer search is already in flight
                }
                self.search_failed = false;
                self.search_results = results;
                self.rebuild_youtube_search_results(&sender);
            }
            YtCmd::SearchFailed(seq) => {
                if seq != self.search_seq {
                    return;
                }
                self.search_failed = true;
                self.search_results.clear();
                self.rebuild_youtube_search_results(&sender);
            }
            YtCmd::SearchThumbsReady(seq) => {
                if seq == self.search_seq {
                    self.rebuild_youtube_search_results(&sender);
                }
            }
            YtCmd::ChannelFetched(title) => {
                let _ = sender.output(YtOutput::SetLoading(None));
                self.reload_channels(&sender);
                match title {
                    Some(t) => {
                        self.yt_view = YtView::Channels;
                        let _ = sender
                            .output(YtOutput::Toast(gettext_f("Subscribed: {t}", &[("t", &t)])));
                    }
                    None => {
                        let _ = sender.output(YtOutput::Toast(gettext("Could not load channel")));
                    }
                }
            }
            YtCmd::ChannelsRefreshed => {
                let _ = sender.output(YtOutput::RefreshFinished);
                self.reload_channels(&sender);
            }
            YtCmd::VideoMeta {
                video_id,
                uploader,
                duration,
                cover,
            } => self.apply_video_meta(&video_id, uploader, duration, cover),
            YtCmd::LibraryProgress { done, total } => {
                let _ = sender.output(YtOutput::Progress(gettext_f(
                    "Adding to library … ({done}/{total})",
                    &[("done", &done.to_string()), ("total", &total.to_string())],
                )));
            }
            YtCmd::LibraryAdded { video_id, result } => {
                if let Some(vid) = &video_id {
                    self.downloading_videos.remove(vid);
                    self.refresh_yt_download_row();
                }
                match result {
                    Ok(n) => {
                        let _ = sender.output(YtOutput::LibraryChanged);
                        let _ = sender.output(YtOutput::ProgressDone(gettext_f(
                            "Added {n} track(s) to your library",
                            &[("n", &n.to_string())],
                        )));
                    }
                    Err(e) => {
                        tracing::warn!("yt library add failed: {e}");
                        let _ = sender
                            .output(YtOutput::ProgressDone(gettext("Could not add to library")));
                    }
                }
            }
            YtCmd::LibraryExists {
                video_id,
                title,
                dest,
            } => self.on_cmd_yt_library_exists(&sender, video_id, title, dest),
            YtCmd::PlaylistSongs { url, title, result } => {
                self.on_cmd_yt_playlist_songs(&sender, url, title, result)
            }
            YtCmd::PlaylistCacheRefreshed { url, title, result } => {
                self.on_cmd_yt_playlist_cache_refreshed(url, title, result)
            }
            YtCmd::PlaylistCoversReady => self.on_cmd_yt_playlist_covers_ready(),
            YtCmd::PlaylistSaved(result) => {
                let _ = sender.output(YtOutput::PlaylistsChanged);
                match result {
                    Ok(n) => {
                        let _ = sender.output(YtOutput::ProgressDone(gettext_f(
                            "Saved {n} song(s) to Playlists",
                            &[("n", &n.to_string())],
                        )));
                    }
                    Err(e) => {
                        tracing::warn!("yt playlist save failed: {e}");
                        let _ = sender
                            .output(YtOutput::ProgressDone(gettext("Could not save playlist")));
                    }
                }
            }
            YtCmd::RecentEnriched { video_id, cover } => {
                let _ = self
                    .library
                    .set_recent_meta(&video_id, None, cover.as_deref());
                self.reload_yt_recent(&sender);
            }
            YtCmd::CoversCached => self.reload_channels(&sender),
        }
        // Keep the broken-banner in sync after any extraction-running command.
        self.ytdlp_broken = youtube::extraction_broken();
    }
}

impl YtPage {
    /// Show detail dialogs as bottom sheets on the phone.
    fn adapt_detail_dialog(&self, dialog: &adw::Dialog) {
        if self.mobile {
            dialog.set_presentation_mode(adw::DialogPresentationMode::BottomSheet);
        }
    }

    /// Park a built subpage in the shared slot and ask the parent to push it.
    fn push_subpage(&self, sender: &ComponentSender<Self>, title: String, content: gtk::Box) {
        *self.subpage_slot.borrow_mut() = Some((title, content));
        let _ = sender.output(YtOutput::PushSubpage);
    }

    /// Rebuilds the channel overview (+ "Newest"/"Recent" lists).
    /// Effective gallery mode for the channels overview: the per-view override if
    /// set, else the global `gallery_view`.
    fn gallery_on(&self) -> bool {
        self.gallery_override.unwrap_or(self.gallery_view)
    }

    /// (Re)builds the header sort button: direction icon + criteria popover
    /// (name / video count) plus the grouping + gallery toggles. Called on init
    /// and whenever the sort/grouping/gallery changes.
    fn rebuild_sort(&self, sender: &ComponentSender<Self>) {
        use crate::ui::app_sort::SortToggle;
        let input = sender.input_sender().clone();
        // Subscriptions and Recent both sort; Newest stays date-grouped (no sort).
        let slot = match self.yt_view {
            YtView::Channels => {
                let (crit, desc) = self.channels_sort;
                let crits = [
                    (SortCrit::Name, gettext("Name")),
                    (SortCrit::Songs, gettext("Number of videos")),
                ];
                let group_input = input.clone();
                let gallery_input = input.clone();
                let toggles = vec![
                    SortToggle {
                        label: gettext("Without grouping"),
                        active: self.channels_no_group,
                        on_toggle: Box::new(move |off| {
                            let _ = group_input.send(YtInput::SetNoGroup(off));
                        }),
                    },
                    SortToggle {
                        label: gettext("Gallery view"),
                        active: self.gallery_on(),
                        on_toggle: Box::new(move |on| {
                            let _ = gallery_input.send(YtInput::SetGallery(on));
                        }),
                    },
                ];
                let popover = sort_popover(
                    &crits,
                    crit,
                    desc,
                    move |crit, desc| {
                        let _ = input.send(YtInput::SetSort(crit, desc));
                    },
                    toggles,
                );
                (!self.channel_items.is_empty()).then_some((popover, desc))
            }
            YtView::Recent => {
                let (crit, desc) = self.recent_sort;
                // Recent is a flat list (no grouping / gallery): name, date, length.
                let crits = [
                    (SortCrit::Name, gettext("Name")),
                    (SortCrit::Release, gettext("Date")),
                    (SortCrit::Length, gettext("Length")),
                ];
                let popover = sort_popover(
                    &crits,
                    crit,
                    desc,
                    move |crit, desc| {
                        let _ = input.send(YtInput::SetRecentSort(crit, desc));
                    },
                    vec![],
                );
                (!self.recent_items.is_empty()).then_some((popover, desc))
            }
            YtView::Newest => None,
        };
        *self.sort_slot.borrow_mut() = slot;
        let _ = sender.output(YtOutput::SortChanged);
    }

    /// Orders the "Recent" list by the chosen sort. "Date" keeps the DB order
    /// (recently played first), reversing it for ascending; the others sort by
    /// title or runtime (videos use `duration`, playlists `total_duration`).
    fn sort_recent_items(&mut self) {
        let (crit, desc) = self.recent_sort;
        match crit {
            SortCrit::Name => self
                .recent_items
                .sort_by_cached_key(|r| natural_key(&r.title)),
            SortCrit::Length => self
                .recent_items
                .sort_by_key(|r| r.duration.or(r.total_duration).unwrap_or(0)),
            // Date (Release): the query already returns `played_at` descending.
            _ => {
                if !desc {
                    self.recent_items.reverse();
                }
                return;
            }
        }
        if desc {
            self.recent_items.reverse();
        }
    }

    /// Per-row alphabetical headings (by name) for the channels list; none for the
    /// video-count sort or when grouping is off.
    fn channels_section_headers(&self) -> Option<Vec<String>> {
        if self.channels_no_group {
            return None;
        }
        match self.channels_sort.0 {
            SortCrit::Name => Some(
                self.channel_items
                    .iter()
                    .map(|(_, title, _, _, _)| crate::ui::app_sort::alpha_header(title))
                    .collect(),
            ),
            _ => None,
        }
    }

    /// Orders the subscriptions overview by the chosen sort (shared by list +
    /// gallery, which both read `channel_items`).
    fn sort_channels(&mut self) {
        let (crit, desc) = self.channels_sort;
        match crit {
            SortCrit::Songs => self.channel_items.sort_by_key(|(_, _, _, _, count)| *count),
            // Name is the only other criterion offered for channels.
            _ => self
                .channel_items
                .sort_by_cached_key(|(_, title, _, _, _)| natural_key(title)),
        }
        if desc {
            self.channel_items.reverse();
        }
    }

    fn reload_channels(&mut self, sender: &ComponentSender<Self>) {
        self.channel_items = self.library.channels().unwrap_or_default();
        self.sort_channels();
        // Refresh the title-bar sort control (visibility depends on emptiness).
        self.rebuild_sort(sender);
        *self.channel_headers.borrow_mut() = self.channels_section_headers();
        if self.gallery_on() {
            self.fill_yt_gallery(sender);
        } else {
            while let Some(child) = self.channels_list.first_child() {
                self.channels_list.remove(&child);
            }
            for (id, title, _url, thumb, count) in self.channel_items.clone() {
                let row = adw::ActionRow::builder()
                    .title(format!("{} ({count})", gtk::glib::markup_escape_text(&title)).as_str())
                    .activatable(true)
                    .build();
                row.add_css_class("emilia-flush");
                let cover = thumb
                    .as_deref()
                    .and_then(crate::core::online::youtube_thumb_path);
                row.add_prefix(&cover_widget(cover.as_deref(), "video-x-generic-symbolic"));
                {
                    let sender = sender.clone();
                    row.connect_activated(move |_| sender.input(YtInput::OpenChannel(id)));
                }
                on_secondary_click(&row, {
                    let sender = sender.clone();
                    move || sender.input(YtInput::ShowChannelDetail(id))
                });
                let lp = gtk::GestureLongPress::new();
                {
                    let sender = sender.clone();
                    lp.connect_pressed(move |g, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(YtInput::ShowChannelDetail(id));
                    });
                }
                row.add_controller(lp);
                self.channels_list.append(&row);
            }
            self.channels_list.invalidate_headers();
        }
        self.reload_yt_newest(sender);
        self.reload_yt_recent(sender);
    }

    /// Gallery variant of the channel overview (thumbnail grid).
    fn fill_yt_gallery(&self, sender: &ComponentSender<Self>) {
        let fb = &self.channels_gallery;
        while let Some(c) = fb.first_child() {
            fb.remove(&c);
        }
        fb.set_min_children_per_line(self.gallery_columns);
        fb.set_max_children_per_line(self.gallery_columns);
        fb.set_homogeneous(true);
        fb.set_row_spacing(8);
        fb.set_column_spacing(8);
        fb.set_selection_mode(gtk::SelectionMode::None);
        fb.set_activate_on_single_click(false);
        if !fb.has_css_class("emilia-gallery") {
            fb.add_css_class("emilia-gallery");
        }
        let mut to_decode: Vec<(String, gtk::Picture)> = Vec::new();
        for (i, (_, title, _, thumb, _)) in self.channel_items.iter().enumerate() {
            let cover = thumb
                .as_deref()
                .and_then(crate::core::online::youtube_thumb_path);
            let (cell, pic) = gallery_cell(cover.as_deref(), "video-x-generic-symbolic", title);
            if let (Some(path), Some(pic)) = (cover.as_deref(), pic) {
                if crate::ui::widgets::cached_thumb(path).is_none() {
                    to_decode.push((path.to_string(), pic));
                }
            }
            let click = gtk::GestureClick::new();
            {
                let sender = sender.clone();
                click.connect_released(move |g, n, _, _| {
                    if n == 1 {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(YtInput::OpenChannelAt(i));
                    }
                });
            }
            cell.add_controller(click);
            on_secondary_click(&cell, {
                let sender = sender.clone();
                move || sender.input(YtInput::ShowChannelDetailAt(i))
            });
            let long_press = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                long_press.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(YtInput::ShowChannelDetailAt(i));
                });
            }
            cell.add_controller(long_press);
            fb.append(&cell);
        }
        spawn_gallery_decode(to_decode);
    }

    /// Builds the "Newest videos" list across all subscribed channels.
    fn reload_yt_newest(&mut self, sender: &ComponentSender<Self>) {
        let mut videos = self.library.all_videos().unwrap_or_default();
        videos.sort_by(|a, b| {
            yt_pubdate_key(b.published.as_deref()).cmp(&yt_pubdate_key(a.published.as_deref()))
        });
        videos.truncate(150);
        self.newest_items = videos;
        while let Some(child) = self.newest_list.first_child() {
            self.newest_list.remove(&child);
        }
        if self.newest_items.is_empty() {
            return;
        }
        let (today, yesterday, week_start) = crate::core::podcast::recent_day_buckets();
        let month_start = crate::core::podcast::recent_cutoff_key();
        let bucket_of = |k: i64| -> usize {
            if k >= today {
                0
            } else if k >= yesterday {
                1
            } else if k >= week_start {
                2
            } else if k >= month_start {
                3
            } else {
                4
            }
        };
        let bucket_title = |b: usize| match b {
            0 => gettext("Today"),
            1 => gettext("Yesterday"),
            2 => gettext("This week"),
            3 => gettext("This month"),
            _ => gettext("Older"),
        };
        let mut cur_bucket: Option<usize> = None;
        let mut group: Option<adw::PreferencesGroup> = None;
        for (i, v) in self.newest_items.iter().enumerate() {
            let b = bucket_of(yt_pubdate_key(v.published.as_deref()));
            if cur_bucket != Some(b) {
                cur_bucket = Some(b);
                let g = adw::PreferencesGroup::builder()
                    .title(bucket_title(b))
                    .build();
                self.newest_list.append(&g);
                group = Some(g);
            }
            let mut subtitle = v.channel_title.clone();
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
            let cover = crate::core::online::youtube_cover_path(&v.video_id)
                .or_else(|| {
                    crate::core::online::youtube_thumb_path(&youtube::thumbnail_url(&v.video_id))
                })
                .or_else(|| {
                    v.channel_thumb
                        .as_deref()
                        .and_then(crate::core::online::youtube_thumb_path)
                });
            row.add_prefix(&cover_widget(cover.as_deref(), "video-x-generic-symbolic"));
            if let Some(d) = v.duration.filter(|d| *d > 0) {
                row.add_suffix(&duration_chip(d));
            }
            row.add_suffix(&self.video_play_button(sender, &v.video_id, &v.title));
            {
                let (sender, vid, title) = (sender.clone(), v.video_id.clone(), v.title.clone());
                row.connect_activated(move |_| {
                    let _ = sender.output(YtOutput::PlayVideo {
                        video_id: vid.clone(),
                        title: title.clone(),
                    });
                });
            }
            on_secondary_click(&row, {
                let sender = sender.clone();
                move || sender.input(YtInput::ShowNewestDetail(i))
            });
            on_long_press(&row, {
                let sender = sender.clone();
                move || sender.input(YtInput::ShowNewestDetail(i))
            });
            if let Some(g) = &group {
                g.add(&row);
            }
        }
        self.refresh_yt_icons();
    }

    /// Builds the "Recent" list (recently played videos/playlists, newest first).
    fn reload_yt_recent(&mut self, sender: &ComponentSender<Self>) {
        self.recent_items = self.library.recent_videos(150).unwrap_or_default();
        self.sort_recent_items();
        // Refresh the title-bar sort control (visibility depends on emptiness);
        // before the early-return below so the empty case hides it too.
        self.rebuild_sort(sender);
        while let Some(child) = self.recent_list.first_child() {
            self.recent_list.remove(&child);
        }
        if self.recent_items.is_empty() {
            return;
        }
        let group = adw::PreferencesGroup::new();
        for r in &self.recent_items {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&r.title))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            if r.kind == "playlist" {
                let mut subtitle = gettext_f(
                    "Playlist · {n}",
                    &[("n", &ngettext_n("{n} song", "{n} songs", r.count as u32))],
                );
                if let Some(total) = r.total_duration.filter(|d| *d > 0) {
                    subtitle.push_str(" · ");
                    subtitle.push_str(&fmt_duration(total));
                }
                row.set_subtitle(&subtitle);
                let cover = r.thumbnail.as_deref().and_then(|t| {
                    if std::path::Path::new(t).exists() {
                        Some(t.to_string())
                    } else {
                        crate::core::online::youtube_thumb_path(t)
                    }
                });
                row.add_prefix(&cover_widget(cover.as_deref(), "view-list-symbolic"));
                let btn = gtk::Button::builder()
                    .icon_name("media-playback-start-symbolic")
                    .valign(gtk::Align::Center)
                    .tooltip_text(gettext("Start Playlist"))
                    .build();
                btn.add_css_class("flat");
                {
                    let (sender, url, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                    btn.connect_clicked(move |_| {
                        let _ = sender.output(YtOutput::StartPlaylist {
                            url: url.clone(),
                            title: t.clone(),
                        });
                    });
                }
                row.add_suffix(&btn);
                {
                    let (sender, url, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                    row.connect_activated(move |_| {
                        sender.input(YtInput::OpenRecentPlaylist {
                            url: url.clone(),
                            title: t.clone(),
                        });
                    });
                }
                on_secondary_click(&row, {
                    let (sender, url, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                    move || {
                        sender.input(YtInput::ShowPlaylistDetail {
                            url: url.clone(),
                            title: t.clone(),
                        });
                    }
                });
                on_long_press(&row, {
                    let (sender, url, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                    move || {
                        sender.input(YtInput::ShowPlaylistDetail {
                            url: url.clone(),
                            title: t.clone(),
                        })
                    }
                });
                group.add(&row);
                continue;
            }
            if let Some(a) = r.artist.as_deref().filter(|s| !s.trim().is_empty()) {
                row.set_subtitle(&gtk::glib::markup_escape_text(a));
            }
            let cover = crate::core::online::youtube_cover_path(&r.video_id).or_else(|| {
                crate::core::online::youtube_thumb_path(&youtube::thumbnail_url(&r.video_id))
            });
            row.add_prefix(&cover_widget(cover.as_deref(), "video-x-generic-symbolic"));
            if let Some(d) = r.duration.filter(|d| *d > 0) {
                row.add_suffix(&duration_chip(d));
            }
            row.add_suffix(&self.video_play_button(sender, &r.video_id, &r.title));
            {
                let (sender, vid, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                row.connect_activated(move |_| {
                    let _ = sender.output(YtOutput::PlayVideo {
                        video_id: vid.clone(),
                        title: t.clone(),
                    });
                });
            }
            on_secondary_click(&row, {
                let (sender, vid, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                move || {
                    sender.input(YtInput::ShowVideoDetail {
                        video_id: vid.clone(),
                        title: t.clone(),
                    });
                }
            });
            on_long_press(&row, {
                let (sender, vid, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                move || {
                    sender.input(YtInput::ShowVideoDetail {
                        video_id: vid.clone(),
                        title: t.clone(),
                    })
                }
            });
            group.add(&row);
        }
        self.recent_list.append(&group);
        self.refresh_yt_icons();
    }

    /// Dialog for searching YouTube and subscribing/opening a result.
    fn open_youtube_search_dialog(&self, sender: &ComponentSender<Self>) {
        if !youtube::available() {
            let _ = sender.output(YtOutput::Toast(gettext(
                "Download yt-dlp in the settings first",
            )));
            return;
        }
        let Some(root) = self.window.clone() else {
            return;
        };
        let dialog = adw::Dialog::builder()
            .title(gettext("Search YouTube"))
            .build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let kind = Rc::new(Cell::new(YtKind::Video));
        let kind_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["linked", "emilia-tabbar"])
            .halign(gtk::Align::Center)
            .margin_bottom(6)
            .build();
        let b_video = gtk::ToggleButton::builder()
            // Labelled "Songs" (de "Lieder"): in this music app the video search
            // is used to find songs. It still searches YtKind::Video.
            .label(gettext("Songs"))
            .active(true)
            .build();
        let b_playlist = gtk::ToggleButton::builder()
            .label(gettext("Playlists"))
            .build();
        let b_channel = gtk::ToggleButton::builder()
            .label(gettext("Channels"))
            .build();
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

        let search_group = adw::PreferencesGroup::builder()
            .title(gettext("Search"))
            .build();
        let search_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text(gettext("Search term …"))
            .hexpand(true)
            .build();
        crate::ui::widgets::no_autofocus(&search_entry);
        let search_btn = gtk::Button::builder().label(gettext("Search")).build();
        search_btn.add_css_class("suggested-action");
        search_row.append(&search_entry);
        search_row.append(&search_btn);
        search_group.add(&search_row);
        content.append(&search_group);

        let results = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        results.add_css_class("boxed-list");
        results.set_visible(false);

        let spinner_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .margin_top(24)
            .margin_bottom(24)
            .visible(false)
            .build();
        let spinner = gtk::Spinner::builder().build();
        spinner.set_size_request(36, 36);
        spinner.set_spinning(true);
        spinner_box.append(&spinner);
        spinner_box.append(
            &gtk::Label::builder()
                .label(gettext("Searching …"))
                .css_classes(["dim-label"])
                .build(),
        );

        let trigger = {
            let (sender, entry, kind) = (sender.clone(), search_entry.clone(), kind.clone());
            let (results, spinner_box) = (results.clone(), spinner_box.clone());
            move || {
                let term = entry.text().to_string();
                if !term.trim().is_empty() {
                    results.set_visible(false);
                    spinner_box.set_visible(true);
                    sender.input(YtInput::Search(term, kind.get()));
                }
            }
        };
        {
            let trigger = trigger.clone();
            search_entry.connect_activate(move |_| trigger());
        }
        search_btn.connect_clicked(move |_| trigger());

        content.append(&results);
        content.append(&spinner_box);

        *self.search.borrow_mut() = Some((dialog.clone(), results.clone(), spinner_box.clone()));
        {
            let slot = self.search.clone();
            dialog.connect_closed(move |_| {
                *slot.borrow_mut() = None;
            });
        }
        present_detail(&dialog, &content, &root);
    }

    /// Redraws the results list in the open search dialog.
    fn rebuild_youtube_search_results(&self, sender: &ComponentSender<Self>) {
        let guard = self.search.borrow();
        let Some((dialog, list, spinner_box)) = guard.as_ref() else {
            return;
        };
        spinner_box.set_visible(false);
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        list.set_visible(true);

        if self.search_results.is_empty() {
            let row = if self.search_failed {
                let r = adw::ActionRow::builder()
                    .title(gettext("YouTube unreachable"))
                    .subtitle(gettext(
                        "Check your connection, or update yt-dlp in the settings",
                    ))
                    .build();
                r.set_subtitle_lines(2);
                r
            } else {
                adw::ActionRow::builder()
                    .title(gettext("Nothing found"))
                    .build()
            };
            row.set_sensitive(false);
            list.append(&row);
            dialog.set_content_height(320);
            return;
        }
        let rows = self.search_results.len() as i32;
        dialog.set_content_height((340 + rows * 66).min(760));

        for r in &self.search_results {
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
            let cover = r
                .thumbnail
                .as_deref()
                .and_then(crate::core::online::youtube_thumb_path);
            let icon = match r.kind {
                YtKind::Channel => "avatar-default-symbolic",
                _ => "video-x-generic-symbolic",
            };
            row.add_prefix(&cover_widget(cover.as_deref(), icon));
            match r.kind {
                YtKind::Video => {
                    let btn = gtk::Button::builder()
                        .icon_name("list-add-symbolic")
                        .valign(gtk::Align::Center)
                        .css_classes(["flat"])
                        .tooltip_text(gettext("List as newest"))
                        .build();
                    let (sender, vid, title) = (sender.clone(), r.id.clone(), r.title.clone());
                    btn.connect_clicked(move |b| {
                        sender.input(YtInput::AddRecent {
                            video_id: vid.clone(),
                            title: title.clone(),
                        });
                        b.set_icon_name("object-select-symbolic");
                        b.set_sensitive(false);
                    });
                    row.add_suffix(&btn);
                }
                YtKind::Channel => {
                    row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
                }
                YtKind::Playlist => {
                    row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
                }
            }
            {
                let (sender, dialog) = (sender.clone(), dialog.clone());
                let (kind, url, vid, title) =
                    (r.kind, r.url.clone(), r.id.clone(), r.title.clone());
                row.connect_activated(move |_| {
                    match kind {
                        YtKind::Channel => sender.input(YtInput::SubscribeChannel(url.clone())),
                        YtKind::Playlist => sender.input(YtInput::ShowPlaylistDetail {
                            url: url.clone(),
                            title: title.clone(),
                        }),
                        YtKind::Video => sender.input(YtInput::ShowVideoDetail {
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

    /// Videos subpage of a subscribed channel.
    fn open_channel(&self, sender: &ComponentSender<Self>, id: i64, title: &str) {
        let videos = self.library.channel_videos(id).unwrap_or_default();
        let channel_thumb = self
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
            .title(
                format!(
                    "{} ({})",
                    gtk::glib::markup_escape_text(title),
                    videos.len()
                )
                .as_str(),
            )
            .build();
        if videos.is_empty() {
            group.add(
                &adw::ActionRow::builder()
                    .title(gettext("No videos"))
                    .build(),
            );
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
            row.add_prefix(&cover_widget(cover.as_deref(), "video-x-generic-symbolic"));
            row.add_suffix(&self.video_play_button(sender, &v.video_id, &v.title));
            {
                let (sender, vid, t) = (sender.clone(), v.video_id.clone(), v.title.clone());
                row.connect_activated(move |_| {
                    let _ = sender.output(YtOutput::PlayVideo {
                        video_id: vid.clone(),
                        title: t.clone(),
                    });
                });
            }
            on_secondary_click(&row, {
                let (sender, vid, t) = (sender.clone(), v.video_id.clone(), v.title.clone());
                move || {
                    sender.input(YtInput::ShowVideoDetail {
                        video_id: vid.clone(),
                        title: t.clone(),
                    });
                }
            });
            on_long_press(&row, {
                let (sender, vid, t) = (sender.clone(), v.video_id.clone(), v.title.clone());
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
        self.push_subpage(
            sender,
            gettext_f("Channel – {title}", &[("title", title)]),
            content,
        );
    }

    /// Subscription detail of a channel.
    fn open_channel_detail(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let Some((_, title, url, thumb, count)) = self
            .channel_items
            .iter()
            .find(|(cid, _, _, _, _)| *cid == id)
            .cloned()
        else {
            return;
        };
        let dialog = adw::Dialog::builder().title(&title).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let info = adw::PreferencesGroup::new();
        let head = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&title))
            .subtitle(ngettext_n("{n} video", "{n} videos", count as u32))
            .build();
        let cover = thumb
            .as_deref()
            .and_then(crate::core::online::youtube_thumb_path);
        head.add_prefix(&cover_widget(cover.as_deref(), "video-x-generic-symbolic"));
        info.add(&head);
        content.append(&info);

        let actions = adw::PreferencesGroup::new();
        let play = action_row(&gettext("Play"), "media-playback-start-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            play.connect_activated(move |_| {
                let _ = sender.output(YtOutput::PlayChannel(id));
                dialog.close();
            });
        }
        actions.add(&play);
        let refresh = action_row(&gettext("Refresh"), "view-refresh-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            refresh.connect_activated(move |_| {
                sender.input(YtInput::RefreshChannel(id));
                dialog.close();
            });
        }
        actions.add(&refresh);
        let share = action_row(&gettext("Share"), "emilia-share-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            share.connect_activated(move |_| {
                let _ = sender.output(YtOutput::Share(crate::core::sync::share::Selection {
                    yt_channels: vec![id],
                    ..Default::default()
                }));
                dialog.close();
            });
        }
        actions.add(&share);
        let bell = adw::SwitchRow::builder()
            .title(gettext("Notify of newest publications"))
            .active(true)
            .build();
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            bell.connect_active_notify(move |s| {
                if !s.is_active() {
                    sender.input(YtInput::DeleteChannel(id));
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
                sender.input(YtInput::DeleteChannel(id));
                dialog.close();
            });
        }
        actions.add(&remove);
        content.append(&actions);
        let _ = url;
        present_detail(&dialog, &content, &root);
    }
}

impl YtPage {
    /// Rich detail page of a video (cover, info, play, add-to-library, EQ).
    fn show_video_detail(&self, sender: &ComponentSender<Self>, video_id: &str, title: &str) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let dialog = adw::Dialog::builder().title(title).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let stored = self.library.yt_video_info(video_id).ok().flatten();
        let stored_channel = stored.as_ref().map(|(c, _, _)| c.clone());
        let stored_duration = stored.as_ref().and_then(|(_, d, _)| *d);
        let cover_path = crate::core::online::youtube_cover_path(video_id)
            .or_else(|| crate::core::online::youtube_thumb_path(&youtube::thumbnail_url(video_id)));

        let cover_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .halign(gtk::Align::Center)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let initial = cover_path
            .as_deref()
            .and_then(|p| gtk::gdk::Texture::from_filename(p).ok());
        let cover =
            crate::ui::widgets::rounded_image(initial.as_ref(), "video-x-generic-symbolic", 200);
        // `rounded_image` (via `cover_frame`) defaults to `halign: Start` for
        // list/header use; in this centred detail dialog that left the cover
        // stuck to the left edge with empty space on the right. Override it.
        cover.set_halign(gtk::Align::Center);
        cover_box.append(&cover);
        content.append(&cover_box);

        let (p_artist, p_album, p_title) = youtube::split_title(title, stored_channel.as_deref());
        let artist_from_title = p_artist.is_some();
        let info = adw::PreferencesGroup::new();
        let artist_row = adw::ActionRow::builder()
            .title(gettext("Artist"))
            .subtitle(p_artist.as_deref().unwrap_or("…"))
            .build();
        info.add(&artist_row);
        if let Some(album) = p_album.as_deref() {
            let album_row = adw::ActionRow::builder()
                .title(gettext("Album"))
                .subtitle(gtk::glib::markup_escape_text(album))
                .build();
            info.add(&album_row);
        }
        let title_row = adw::ActionRow::builder()
            .title(gettext("Title"))
            .subtitle(gtk::glib::markup_escape_text(&p_title))
            .build();
        title_row.set_subtitle_lines(3);
        info.add(&title_row);
        let duration_row = adw::ActionRow::builder()
            .title(gettext("Duration"))
            .subtitle(
                stored_duration
                    .map(fmt_duration)
                    .unwrap_or_else(|| "…".into()),
            )
            .build();
        info.add(&duration_row);
        content.append(&info);

        let actions = adw::PreferencesGroup::new();
        let play = action_row(&gettext("Play"), "media-playback-start-symbolic");
        {
            let (sender, dialog, vid, t) = (
                sender.clone(),
                dialog.clone(),
                video_id.to_string(),
                title.to_string(),
            );
            play.connect_activated(move |_| {
                let _ = sender.output(YtOutput::PlayVideo {
                    video_id: vid.clone(),
                    title: t.clone(),
                });
                dialog.close();
            });
        }
        self.ctx_video_play
            .replace(Some((play.clone(), video_id.to_string())));
        actions.add(&play);

        let off = action_row(&gettext("Add to library"), "list-add-symbolic");
        {
            let (sender, vid, t) = (sender.clone(), video_id.to_string(), title.to_string());
            off.connect_activated(move |_| {
                sender.input(YtInput::AddToLibrary {
                    video_id: vid.clone(),
                    title: t.clone(),
                });
            });
        }
        actions.add(&off);
        let eq = action_row(
            &gettext("Equalizer settings"),
            "multimedia-equalizer-symbolic",
        );
        {
            let (sender, dialog, path, t) = (
                sender.clone(),
                dialog.clone(),
                youtube::yt_path(video_id),
                title.to_string(),
            );
            eq.connect_activated(move |_| {
                let _ = sender.output(YtOutput::OpenTrackEq {
                    path: path.clone(),
                    title: t.clone(),
                });
                dialog.close();
            });
        }
        actions.add(&eq);
        let share = action_row(&gettext("Share"), "emilia-share-symbolic");
        {
            let (sender, dialog, vid) = (sender.clone(), dialog.clone(), video_id.to_string());
            share.connect_activated(move |_| {
                let _ = sender.output(YtOutput::Share(crate::core::sync::share::Selection {
                    yt_songs: vec![vid.clone()],
                    ..Default::default()
                }));
                dialog.close();
            });
        }
        actions.add(&share);
        if self.library.is_recent(video_id).unwrap_or(false) {
            let remove = action_row(&gettext("Remove from recent"), "user-trash-symbolic");
            remove.add_css_class("error");
            let (sender, dialog, vid) = (sender.clone(), dialog.clone(), video_id.to_string());
            remove.connect_activated(move |_| {
                sender.input(YtInput::RemoveRecent(vid.clone()));
                dialog.close();
            });
            actions.add(&remove);
        }
        content.append(&actions);

        self.ctx_video_download
            .replace(Some((off.clone(), video_id.to_string())));
        self.ctx_video_meta.replace(Some((
            video_id.to_string(),
            cover_box,
            artist_row,
            duration_row,
            artist_from_title,
        )));
        self.refresh_yt_download_row();
        self.refresh_yt_icons();

        if stored_channel.is_none() || stored_duration.is_none() || initial.is_none() {
            let (sender, vid) = (sender.clone(), video_id.to_string());
            sender.spawn_command(move |out| {
                let meta = youtube::video_meta(&vid).ok();
                let uploader = meta.as_ref().and_then(|m| m.uploader.clone());
                let duration = meta.as_ref().and_then(|m| m.duration);
                let cover = crate::core::online::youtube_cover_path(&vid).or_else(|| {
                    crate::core::online::cache_youtube_thumb(&youtube::thumbnail_url(&vid))
                });
                let _ = out.send(YtCmd::VideoMeta {
                    video_id: vid,
                    uploader,
                    duration,
                    cover,
                });
            });
        }

        {
            let play_slot = self.ctx_video_play.clone();
            let dl_slot = self.ctx_video_download.clone();
            let meta_slot = self.ctx_video_meta.clone();
            dialog.connect_closed(move |_| {
                *play_slot.borrow_mut() = None;
                *dl_slot.borrow_mut() = None;
                *meta_slot.borrow_mut() = None;
            });
        }
        present_detail(&dialog, &content, &root);
    }

    /// Detail dialog of a playlist.
    fn show_playlist_detail(&self, sender: &ComponentSender<Self>, url: &str, title: &str) {
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
        present_detail(&dialog, &content, &root);
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
            .title(
                format!(
                    "{} ({})",
                    gtk::glib::markup_escape_text(title),
                    videos.len()
                )
                .as_str(),
            )
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
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&v.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            let thumb_url = youtube::thumbnail_url(&v.id);
            let cover = crate::core::online::youtube_cover_path(&v.id)
                .or_else(|| crate::core::online::youtube_thumb_path(&thumb_url));
            let frame = crate::ui::widgets::thumb_frame("video-x-generic-symbolic", 48);
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
            {
                let (sender, u, t) = (sender.clone(), url.to_string(), title.to_string());
                row.connect_activated(move |_| {
                    sender.input(YtInput::PlayPlaylistAt {
                        url: u.clone(),
                        title: t.clone(),
                        index,
                        close: true,
                    });
                });
            }
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
                let _ = out.send(YtCmd::PlaylistCoversReady);
            });
        }
    }

    /// Play/Pause button (suffix) for a video row.
    fn video_play_button(
        &self,
        sender: &ComponentSender<Self>,
        video_id: &str,
        title: &str,
    ) -> gtk::Button {
        let btn = gtk::Button::builder()
            .icon_name("media-playback-start-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text(gettext("Play/Pause"))
            .build();
        btn.add_css_class("flat");
        {
            let (sender, vid, t) = (sender.clone(), video_id.to_string(), title.to_string());
            btn.connect_clicked(move |_| {
                let _ = sender.output(YtOutput::PlayVideo {
                    video_id: vid.clone(),
                    title: t.clone(),
                });
            });
        }
        self.video_play_buttons
            .borrow_mut()
            .push((video_id.to_string(), btn.clone()));
        btn
    }

    /// Updates the Play/Pause icons of visible video rows and the detail "Play"
    /// row from the mirrored playback state.
    fn refresh_yt_icons(&self) {
        let active = self.playing_video_id.clone();
        let playing = self.playing;
        let is_active = |vid: &str| playing && active.as_deref() == Some(vid);
        {
            let mut buttons = self.video_play_buttons.borrow_mut();
            buttons.retain(|(_, btn)| btn.root().is_some());
            for (vid, btn) in buttons.iter() {
                btn.set_icon_name(if is_active(vid) {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                });
            }
        }
        if let Some((row, vid)) = self.ctx_video_play.borrow().as_ref() {
            row.set_visible(!is_active(vid));
        }
    }

    /// Updates the "Add to library" row of an open video detail dialog.
    fn refresh_yt_download_row(&self) {
        let guard = self.ctx_video_download.borrow();
        let Some((row, vid)) = guard.as_ref() else {
            return;
        };
        let text = if self.downloading_videos.contains(vid) {
            gettext("Adding to library …")
        } else {
            gettext("Add to library")
        };
        row.set_title(&text);
    }

    /// Fills an open video detail dialog with metadata that arrived async.
    fn apply_video_meta(
        &self,
        video_id: &str,
        uploader: Option<String>,
        duration: Option<i64>,
        cover: Option<String>,
    ) {
        let guard = self.ctx_video_meta.borrow();
        let Some((vid, cover_box, artist_row, duration_row, artist_from_title)) = guard.as_ref()
        else {
            return;
        };
        if vid != video_id {
            return;
        }
        if !*artist_from_title {
            let artist = uploader
                .as_deref()
                .map(youtube::clean_channel_name)
                .filter(|s| !s.trim().is_empty());
            artist_row.set_subtitle(artist.as_deref().unwrap_or("—"));
        }
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

    /// "+" on a search result: list the video in "Recent" (no download/playback).
    fn yt_add_recent(&mut self, sender: &ComponentSender<Self>, video_id: String, title: String) {
        let _ = self.library.add_recent_video(&video_id, &title, None);
        let _ = self.library.set_yt_title(&video_id, &title);
        self.reload_yt_recent(sender);
        self.yt_view = YtView::Recent;
        let vid = video_id;
        sender.spawn_command(move |out| {
            let cover = crate::core::online::cache_youtube_thumb(&youtube::thumbnail_url(&vid));
            let _ = out.send(YtCmd::RecentEnriched {
                video_id: vid,
                cover,
            });
        });
    }

    /// Adds a single video to the on-disk music library (background).
    fn yt_add_video_to_library(
        &mut self,
        sender: &ComponentSender<Self>,
        video_id: String,
        title: String,
        overwrite: bool,
    ) {
        if self.downloading_videos.contains(&video_id) {
            return;
        }
        let Some(music) = self.library.get_setting("music_dir").ok().flatten() else {
            let _ = sender.output(YtOutput::Toast(gettext(
                "Set a music folder in settings first",
            )));
            return;
        };
        self.downloading_videos.insert(video_id.clone());
        self.refresh_yt_download_row();
        let _ = sender.output(YtOutput::Progress(gettext_f(
            "Adding “{title}” to library …",
            &[("title", &title)],
        )));
        let cover = crate::core::online::youtube_cover_path(&video_id);
        let vid = video_id;
        sender.spawn_command(move |out| {
            let cmd =
                match youtube::add_to_library(&vid, &title, &music, cover.as_deref(), overwrite) {
                    Ok(youtube::AddOutcome::Added) => YtCmd::LibraryAdded {
                        video_id: Some(vid),
                        result: Ok(1),
                    },
                    Ok(youtube::AddOutcome::Exists(dest)) => YtCmd::LibraryExists {
                        video_id: vid,
                        title,
                        dest: dest.to_string_lossy().into_owned(),
                    },
                    Err(e) => YtCmd::LibraryAdded {
                        video_id: Some(vid),
                        result: Err(e),
                    },
                };
            let _ = out.send(cmd);
        });
    }

    /// Adds all videos of a playlist to the on-disk music library (background).
    fn yt_playlist_to_library(&self, sender: &ComponentSender<Self>, url: String, title: String) {
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
                    if let Ok(youtube::AddOutcome::Added) =
                        youtube::add_to_library(&v.id, &v.title, &music, cover.as_deref(), false)
                    {
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
    fn yt_save_playlist(&self, sender: &ComponentSender<Self>, url: String, title: String) {
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
    fn yt_open_recent_playlist(
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
        if let Ok(Some((json, fetched_at))) = self.library.yt_playlist_cache(&url) {
            if let Ok(videos) = serde_json::from_str::<Vec<YtResult>>(&json) {
                self.playlist_songs_cache
                    .insert(url.clone(), videos.clone());
                self.show_yt_playlist_songs(sender, &url, &title, videos);
                if crate::ui::app_helpers::unix_now().saturating_sub(fetched_at)
                    > PLAYLIST_CACHE_TTL_SECS
                {
                    let (url, title) = (url.clone(), title.clone());
                    sender.spawn_command(move |out| {
                        let result = youtube::list_playlist(&url, PLAYLIST_INDEX_LIMIT)
                            .map_err(|e| e.to_string());
                        let _ = out.send(YtCmd::PlaylistCacheRefreshed { url, title, result });
                    });
                }
                return;
            }
        }
        // Never seen → fetch (the result is cached on arrival).
        self.yt_open_playlist_songs(sender, url, title);
    }

    /// Serializes a playlist's song list into the persistent DB cache (best
    /// effort: a serialization/DB error just skips the cache, never blocks).
    fn cache_playlist_songs(&self, url: &str, title: &str, videos: &[YtResult]) {
        if let Ok(json) = serde_json::to_string(videos) {
            if let Err(e) = self.library.set_yt_playlist_cache(url, title, &json) {
                tracing::warn!("caching playlist {url} failed: {e}");
            }
        }
    }

    /// Worker result: a library-add hit an existing file → ask before overwriting.
    fn on_cmd_yt_library_exists(
        &mut self,
        sender: &ComponentSender<Self>,
        video_id: String,
        title: String,
        dest: String,
    ) {
        self.downloading_videos.remove(&video_id);
        self.refresh_yt_download_row();
        let _ = sender.output(YtOutput::ProgressDone(gettext("Song already exists")));
        let Some(root) = self.window.clone() else {
            return;
        };
        let confirm = adw::AlertDialog::new(
            Some(&gettext("Overwrite existing song?")),
            Some(&gettext_f(
                "“{title}” is already saved at:\n{dest}",
                &[("title", &title), ("dest", &dest)],
            )),
        );
        confirm.add_response("skip", &gettext("Skip"));
        confirm.add_response("overwrite", &gettext("Overwrite"));
        confirm.set_response_appearance("overwrite", adw::ResponseAppearance::Destructive);
        confirm.set_default_response(Some("skip"));
        confirm.set_close_response("skip");
        {
            let sender = sender.clone();
            confirm.connect_response(None, move |_, resp| {
                if resp == "overwrite" {
                    sender.input(YtInput::AddToLibraryConfirmed {
                        video_id: video_id.clone(),
                        title: title.clone(),
                    });
                }
            });
        }
        confirm.present(Some(&root));
    }

    /// Worker result: a playlist's song list resolved → cache + show subpage.
    fn on_cmd_yt_playlist_songs(
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
    fn on_cmd_yt_playlist_cache_refreshed(
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

    /// Worker result: pending playlist-songs cover thumbnails finished caching.
    fn on_cmd_yt_playlist_covers_ready(&mut self) {
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
