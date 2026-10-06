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
//!
//! This file keeps the struct and the `Component` impl; the rest is split by
//! topic: the messages in [`crate::ui::yt_page_msg`], the free channel/feed
//! helpers in [`crate::ui::yt_channels`], and the inherent impl in
//! [`crate::ui::yt_page_lists`] (sort + lists), [`crate::ui::yt_page_search`]
//! (search dialog), [`crate::ui::yt_page_channels`] (subscriptions),
//! [`crate::ui::yt_playlists`] (playlists), [`crate::ui::yt_live`] (Live tab)
//! and [`crate::ui::yt_page_detail`] (video detail / cards / library add).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::core::db::Library;
use crate::core::youtube::{self, YtKind, YtResult};
use crate::i18n::{gettext, gettext_f};
use crate::ui::app::{SortCrit, YtView};
use crate::ui::app_sort::read_sort;
use crate::ui::yt_channels::{WatchRow, refresh_summary_text};
use crate::ui::yt_page_channels::cache_missing_channel_thumbs;
use crate::ui::yt_page_lists::read_channel_view_prefs;
// Still reached as `crate::ui::yt_page::…` from `app_init.rs` / `app_views_handlers.rs`.
pub(crate) use crate::ui::yt_channels::{
    ensure_channel_image, fmt_duration, refresh_channel_videos,
};
// The messages, reached as `crate::ui::yt_page::…` all over the app.
pub(crate) use crate::ui::yt_page_msg::{SearchKind, YtCmd, YtInput, YtOutput};

/// The YouTube page component.
pub(crate) struct YtPage {
    /// Own DB connection (WAL + per-thread).
    pub(super) library: Library,
    /// Window the dialogs are presented on (set on `SetWindow`).
    pub(super) window: Option<adw::ApplicationWindow>,
    /// Mirror of the transport's `playing_video_id` (for row icons).
    pub(super) playing_video_id: Option<String>,
    /// Mirror of the transport play/pause state.
    pub(super) playing: bool,
    /// Mirror of the global gallery setting.
    pub(super) gallery_view: bool,
    pub(super) gallery_columns: u32,
    /// Narrow (mobile) layout → detail dialogs as bottom sheets.
    mobile: bool,
    /// yt-dlp can no longer parse YouTube → show the warning banner. Mirror of
    /// [`crate::core::youtube::extraction_broken`], refreshed after each command.
    ytdlp_broken: bool,
    /// Which view is visible: newest / recent / channels.
    pub(super) yt_view: YtView,
    /// Sort of the subscriptions (channels) overview (criterion + descending).
    /// Persisted as "sort_channels" / "sort_channels_desc". The date-ordered
    /// Recent/Newest views are not affected.
    pub(super) channels_sort: (SortCrit, bool),
    /// "Without grouping" for the channels list (no alphabetical headings).
    /// Persisted as "nogroup_channels".
    pub(super) channels_no_group: bool,
    /// Sort of the "Recent" (recently played) list (criterion + descending).
    /// Persisted as "sort_yt_recent" / "sort_yt_recent_desc". Default: by date
    /// (most recent first), i.e. the natural `played_at` order from the DB.
    pub(super) recent_sort: (SortCrit, bool),
    /// Per-view gallery override (sort popover); `None` follows the global
    /// `gallery_view`. Persisted as "gallery_channels".
    pub(super) gallery_override: Option<bool>,
    /// "Show description" (sort popover): gallery tiles framed with their title
    /// instead of the bare cover. Persisted as "gallery_desc_channels".
    pub(super) gallery_desc: bool,
    /// Per-row alphabetical headings of the channels list (name sort).
    pub(super) channel_headers: std::rc::Rc<std::cell::RefCell<Option<Vec<String>>>>,
    /// Hand-off for the shared title-bar sort button: [`Self::rebuild_sort`]
    /// writes the popover + direction here (or `None` to hide it) for the active
    /// view, then signals the parent via [`YtOutput::SortChanged`].
    pub(super) sort_slot: crate::ui::app_sort::SortSlot,
    /// (id, title, url, thumbnail, video count) per subscribed channel.
    pub(super) channel_items: Vec<(i64, String, String, Option<String>, i64)>,
    pub(super) channels_list: gtk::ListBox,
    pub(super) channels_gallery: gtk::FlowBox,
    pub(super) newest_items: Vec<crate::model::YtVideoRef>,
    pub(super) newest_list: gtk::Box,
    pub(super) recent_items: Vec<crate::model::YtRecent>,
    pub(super) recent_list: gtk::Box,
    /// Saved live streams (Live tab).
    pub(super) live_items: Vec<crate::model::YtLive>,
    pub(super) live_list: gtk::ListBox,
    pub(super) search_results: Vec<YtResult>,
    /// What the shown search results were searched as (live hits are saved to
    /// the Live tab instead of opening the video dialog).
    pub(super) search_kind: SearchKind,
    pub(super) search_failed: bool,
    /// Monotonic search counter. Every new search bumps it; command results
    /// carrying an older value are ignored. This keeps the "Searching …"
    /// spinner up until the *current* search returns — a still-running worker
    /// from a previous search (e.g. after switching Songs→Playlists→Channels)
    /// can no longer clear the spinner early or flash stale results.
    pub(super) search_seq: u64,
    pub(super) search: Rc<RefCell<Option<(adw::Dialog, gtk::ListBox)>>>,
    /// Play/pause controls of the video rows, keyed by video id.
    pub(super) video_marks: crate::ui::play_mark::Marks,
    /// Watch-progress widgets of the visible rows (long-form items only), so the
    /// per-second transport tick can advance them without rebuilding the list.
    pub(super) watch_progress_rows: Rc<RefCell<Vec<WatchRow>>>,
    pub(super) ctx_video_play: Rc<RefCell<Option<(adw::ActionRow, String)>>>,
    pub(super) ctx_video_download: Rc<RefCell<Option<(adw::ActionRow, gtk::Image, String)>>>,
    pub(super) ctx_video_meta:
        Rc<RefCell<Option<(String, gtk::Box, adw::ActionRow, adw::ActionRow, bool)>>>,
    /// While a video detail dialog is open: (video id, title, the box its
    /// chapters/description are filled into once they are known).
    pub(super) ctx_video_desc: Rc<RefCell<Option<(String, String, gtk::Box)>>>,
    pub(super) downloading_videos: HashSet<String>,
    pub(super) playlist_songs_cache: HashMap<String, Vec<YtResult>>,
    pub(super) pl_cover_slots: Vec<(String, adw::Bin)>,
    /// Hand-off slot for built subpages (the `!Send` widget can't ride a message).
    subpage_slot: Rc<RefCell<Option<(String, gtk::Box)>>>,
    /// Live "adding to library" progress popup (video detail → library add),
    /// kept so async [`YtCmd::AddLibProgress`] commands can drive its bar/label.
    pub(super) progress_popup: Rc<RefCell<Option<ProgressPopup>>>,
}

/// Widgets of the non-blocking library-add progress popup ([`YtPage::show_progress_popup`]).
pub(super) struct ProgressPopup {
    pub(super) dialog: adw::Dialog,
    pub(super) bar: gtk::ProgressBar,
    pub(super) label: gtk::Label,
    /// Which download this popup tracks, so stray progress/finish commands for a
    /// different video don't retarget or close it.
    pub(super) video_id: String,
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
                gtk::ToggleButton {
                    set_label: &gettext("Live"),
                    set_hexpand: true,
                    #[watch]
                    set_active: model.yt_view == YtView::Live,
                    connect_clicked => YtInput::SetView(YtView::Live),
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
                set_icon_name: Some("audio-x-generic-symbolic"),
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
                set_icon_name: Some("audio-x-generic-symbolic"),
                set_title: &gettext("No subscriptions"),
                set_description: Some(&gettext("Search YouTube and subscribe to a channel.")),
                set_vexpand: true,
                #[watch]
                set_visible: model.yt_view == YtView::Channels && model.channel_items.is_empty(),
            },

            // "Live"
            gtk::ScrolledWindow {
                set_vexpand: true,
                #[watch]
                set_visible: model.yt_view == YtView::Live && !model.live_items.is_empty(),
                #[local_ref]
                yt_live_list -> gtk::ListBox {
                    set_valign: gtk::Align::Start,
                    set_margin_top: 10,
                    set_margin_bottom: 12,
                    set_margin_start: 12,
                    set_margin_end: 12,
                    set_selection_mode: gtk::SelectionMode::None,
                    set_css_classes: &["boxed-list"],
                },
            },
            adw::StatusPage {
                set_icon_name: Some("internet-radio-symbolic"),
                set_title: &gettext("No live streams"),
                set_description: Some(&gettext("Search YouTube for live streams, such as lofi radio, and add them here. They are only streamed, never downloaded.")),
                set_vexpand: true,
                #[watch]
                set_visible: model.yt_view == YtView::Live && model.live_items.is_empty(),
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
        let yt_live_list = gtk::ListBox::new();
        // Restore the persisted subscriptions sort (default: by name, ascending) +
        // the grouping/gallery choices.
        let channels_sort = read_sort(&library, "channels", SortCrit::Name, false);
        // Recent list sort (default: by date, most recent first).
        let recent_sort = read_sort(&library, "yt_recent", SortCrit::Release, true);
        let (channels_no_group, gallery_override, gallery_desc) = read_channel_view_prefs(&library);
        let channel_headers = std::rc::Rc::new(std::cell::RefCell::new(None));
        yt_channels_list.set_header_func(crate::ui::app_gallery::list_section_header_func(
            channel_headers.clone(),
        ));
        let mut model = YtPage {
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
            gallery_desc,
            channel_headers,
            sort_slot,
            channel_items: Vec::new(),
            channels_list: yt_channels_list.clone(),
            channels_gallery: yt_channels_gallery.clone(),
            newest_items: Vec::new(),
            newest_list: yt_newest_list.clone(),
            recent_items: Vec::new(),
            recent_list: yt_recent_list.clone(),
            live_items: Vec::new(),
            live_list: yt_live_list.clone(),
            search_results: Vec::new(),
            search_kind: SearchKind::Yt(YtKind::Video),
            search_failed: false,
            search_seq: 0,
            search: Rc::new(RefCell::new(None)),
            video_marks: Default::default(),
            watch_progress_rows: Rc::new(RefCell::new(Vec::new())),
            ctx_video_play: Rc::new(RefCell::new(None)),
            ctx_video_download: Rc::new(RefCell::new(None)),
            ctx_video_meta: Rc::new(RefCell::new(None)),
            ctx_video_desc: Rc::new(RefCell::new(None)),
            downloading_videos: HashSet::new(),
            playlist_songs_cache: HashMap::new(),
            pl_cover_slots: Vec::new(),
            subpage_slot,
            progress_popup: Rc::new(RefCell::new(None)),
        };
        // Fetch the channel thumbnails still missing from the cache in the
        // background; the page is rebuilt only if one came in.
        sender.spawn_oneshot_command(|| YtCmd::CoversCached(cache_missing_channel_thumbs()));
        let widgets = view_output!();
        model.reload_live(&sender);
        // Build the header sort popover for the restored subscriptions sort.
        model.rebuild_sort(&sender);
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: YtInput, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            YtInput::Reload => {
                self.reload_channels(&sender);
                self.reload_live(&sender);
            }
            YtInput::RefreshAll => self.refresh_all_channels(&sender),
            YtInput::ReloadRecent => self.reload_yt_recent(&sender),
            YtInput::PlayVideo { video_id, title } => {
                let _ = sender.output(YtOutput::PlayVideo {
                    video_id,
                    title,
                    keep_recent_order: self.yt_view == YtView::Recent,
                });
            }
            YtInput::PlaybackStateChanged {
                playing_video_id,
                playing,
            } => {
                // Switching videos means the previous one now has a stored
                // position — "Recent" has to be rebuilt to show it (and in the
                // right order).
                let switched =
                    playing_video_id.is_some() && playing_video_id != self.playing_video_id;
                self.playing_video_id = playing_video_id;
                self.playing = playing;
                self.refresh_yt_icons();
                if switched {
                    self.reload_yt_recent(&sender);
                }
            }
            YtInput::VideoProgressTick {
                video_id,
                position_ms,
                duration_ms,
            } => self.apply_watch_progress(&video_id, position_ms, duration_ms, false),
            YtInput::VideoFinished { video_id } => {
                self.apply_watch_progress(&video_id, 0, 0, true);
                self.reload_yt_recent(&sender);
            }
            YtInput::RefreshBroken => self.ytdlp_broken = youtube::extraction_broken(),
            YtInput::SetView(v) => {
                self.yt_view = v;
                // The rows built in `init` were not in the window yet, so their
                // play marks were already discarded (`Marks` drops rootless
                // widgets) — rebuild now that the page is shown.
                if v == YtView::Live {
                    self.reload_live(&sender);
                }
                // Each view has its own sort control (Newest: none).
                self.rebuild_sort(&sender);
            }
            YtInput::SetSort(crit, desc) => self.on_set_channels_sort(&sender, crit, desc),
            YtInput::SetRecentSort(crit, desc) => self.on_set_recent_sort(&sender, crit, desc),
            YtInput::SetNoGroup(off) => self.on_set_no_group(&sender, off),
            YtInput::SetGallery(on) => self.on_set_gallery(&sender, on),
            YtInput::SetGalleryDesc(on) => self.on_set_gallery_desc(&sender, on),
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
            YtInput::Search(term, kind) => self.on_search(&sender, &term, kind),
            YtInput::AddLive(index) => {
                if let Some(hit) = self.search_results.get(index).cloned() {
                    self.add_live(&sender, hit);
                }
            }
            YtInput::ShowLiveDetail(video_id) => self.show_live_detail(&sender, &video_id),
            YtInput::RemoveLive(video_id) => {
                let _ = self.library.delete_live(&video_id);
                self.reload_live(&sender);
                self.rebuild_sort(&sender);
            }
            YtInput::SubscribeChannel(url) => self.on_subscribe_channel(&sender, &url),
            YtInput::OpenChannel(id) => self.on_open_channel(&sender, id),
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
            YtInput::RefreshChannel(id) => self.on_refresh_channel(&sender, id),
            YtInput::RefreshVideo { video_id, title } => {
                self.on_refresh_video(&sender, video_id, title)
            }
            YtInput::RefreshChannelDetail(id) => self.on_refresh_channel_detail(&sender, id),
            YtInput::RefreshPlaylist { url, title } => {
                self.on_refresh_playlist(&sender, url, title)
            }
            YtInput::RefreshLive(video_id) => self.on_refresh_live(&sender, video_id),
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
            } => self.on_play_playlist_at(&sender, url, title, index, close),
            YtInput::AddToLibrary {
                video_id,
                title,
                artist,
            } => self.yt_add_video_to_library(&sender, video_id, title, artist, false),
            YtInput::AddToLibraryConfirmed {
                video_id,
                title,
                artist,
            } => self.yt_add_video_to_library(&sender, video_id, title, artist, true),
            YtInput::PlaylistToLibrary { url, title } => {
                self.yt_playlist_to_library(&sender, url, title)
            }
            YtInput::SavePlaylist { url, title } => self.yt_save_playlist(&sender, url, title),
        }
    }

    fn update_cmd(&mut self, cmd: YtCmd, sender: ComponentSender<Self>, _root: &Self::Root) {
        match cmd {
            // Results of an outdated search (a newer one is already in flight)
            // are dropped — returning before the banner sync below.
            YtCmd::SearchResults(seq, _) | YtCmd::SearchFailed(seq) if seq != self.search_seq => {
                return;
            }
            YtCmd::SearchResults(_, results) => self.on_cmd_search_results(&sender, Some(results)),
            YtCmd::SearchFailed(_) => self.on_cmd_search_results(&sender, None),
            YtCmd::SearchThumbsReady(seq) => {
                if seq == self.search_seq {
                    self.rebuild_youtube_search_results(&sender);
                }
            }
            YtCmd::ChannelFetched(title) => self.on_cmd_channel_fetched(&sender, title),
            YtCmd::ChannelVideosReady => self.reload_channels(&sender),
            YtCmd::RefreshProgress { done, total, title } => {
                let _ = sender.output(YtOutput::RefreshProgress {
                    done,
                    total,
                    label: title,
                });
            }
            YtCmd::ChannelsRefreshed {
                updated,
                failed,
                new_videos,
            } => {
                let _ = sender.output(YtOutput::RefreshFinished);
                let _ = sender.output(YtOutput::RefreshSummary(refresh_summary_text(
                    updated, failed, new_videos,
                )));
                self.ytdlp_broken = youtube::extraction_broken();
                self.reload_channels(&sender);
            }
            YtCmd::VideoRefreshed { video_id, title } => {
                self.on_cmd_video_refreshed(&sender, &video_id, &title)
            }
            YtCmd::ChannelDetailRefreshed(id) => self.on_cmd_channel_detail_refreshed(&sender, id),
            YtCmd::PlaylistRefreshed { url, title, result } => {
                self.on_cmd_yt_playlist_refreshed(&url, &title, result);
                self.reload_yt_recent(&sender);
                self.show_playlist_detail(&sender, &url, &title);
            }
            YtCmd::LiveRefreshed {
                video_id,
                details,
                thumbnail,
            } => self.on_cmd_live_refreshed(&sender, video_id, details, thumbnail),
            YtCmd::RefreshUnavailable => {
                let _ = sender.output(YtOutput::RefreshFinished);
                let _ = sender.output(YtOutput::RefreshSummary(gettext(
                    "yt-dlp is missing — install or update it in the settings",
                )));
            }
            YtCmd::VideoMeta {
                video_id,
                uploader,
                duration,
                cover,
                chapters,
            } => {
                self.apply_video_meta(&video_id, uploader, duration, cover);
                self.fill_video_chapters(&sender, &video_id, &chapters);
            }
            YtCmd::LibraryProgress { done, total } => {
                let _ = sender.output(YtOutput::Progress(gettext_f(
                    "Adding to library … ({done}/{total})",
                    &[("done", &done.to_string()), ("total", &total.to_string())],
                )));
            }
            YtCmd::AddLibProgress { video_id, progress } => {
                self.update_progress_popup(&video_id, progress);
            }
            YtCmd::LibraryAdded { video_id, result } => {
                self.close_progress_popup(video_id.as_deref());
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
                artist,
                dest,
            } => self.on_cmd_yt_library_exists(&sender, video_id, title, artist, dest),
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
            YtCmd::CoversCached(fetched) => {
                if fetched {
                    self.reload_channels(&sender);
                }
            }
        }
        // Keep the broken-banner in sync after any extraction-running command.
        self.ytdlp_broken = youtube::extraction_broken();
    }
}

impl YtPage {
    /// Show detail dialogs as bottom sheets on the phone.
    pub(super) fn adapt_detail_dialog(&self, dialog: &adw::Dialog) {
        crate::ui::widgets::adapt_dialog(dialog, self.mobile);
    }

    /// Park a built subpage in the shared slot and ask the parent to push it.
    pub(super) fn push_subpage(
        &self,
        sender: &ComponentSender<Self>,
        title: String,
        content: gtk::Box,
    ) {
        *self.subpage_slot.borrow_mut() = Some((title, content));
        let _ = sender.output(YtOutput::PushSubpage);
    }
}

/// Same for the video rows: the component owns them, so the shared state is
/// handed over as a message.
impl crate::ui::play_mark::PlaybackSink for relm4::Controller<YtPage> {
    fn apply_playback(&self, state: &crate::ui::play_mark::PlaybackState) {
        use relm4::ComponentController;
        self.emit(YtInput::PlaybackStateChanged {
            playing_video_id: state.video_id.clone(),
            playing: state.playing,
        });
    }
}
