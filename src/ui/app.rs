use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use adw::prelude::*;
use relm4::factory::FactoryVecDeque;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::player::Player;
use crate::i18n::{gettext, gettext_f};
use crate::ui::app_concert::ConcertMsg;
use crate::ui::app_dialogs::CtxMsg;
use crate::ui::app_episode_playback::PodcastMsg;
pub(crate) use crate::ui::app_helpers::{
    album_subtitle, apply_color_scheme, artist_count_subtitle, attach_hscroll_swipe,
    attach_swipe_back, attach_tab_swipe, cover_widget, duration_label, find_scroller, fmt_duration,
    fmt_rate, guarded_resume, initial_gallery_columns, most_common_artist, on_long_press,
    on_secondary_click, online_available, read_entries, save_window_state, unix_now,
};
use crate::ui::app_init::InitState;
use crate::ui::app_lyrics::LyricsMsg;
use crate::ui::app_playback::TransportMsg;
use crate::ui::app_rec_edit::EditMsg;
pub(crate) use crate::ui::app_sections::*;
use crate::ui::app_settings::SettingMsg;
pub(crate) use crate::ui::app_state::*;
use crate::ui::app_streaming::StreamMsg;
use crate::ui::app_yt_glue::YtMsg;
use crate::ui::card_list::CardList;
use crate::ui::fs_row::{FsEntry, FsInput, FsOutput};

pub struct App {
    /// Gates the per-second tick: the timer only delivers `Msg::Transport(TransportMsg::Tick)` while this
    /// is set (playing or recording). When idle it stays unset, so the app does
    /// no per-second work — and triggers no full per-second view re-render.
    pub(crate) tick_active: std::rc::Rc<std::cell::Cell<bool>>,
    /// Last (stream, path, playing) pushed to the StreamPage; lets
    /// `sync_stream_page_icons` skip re-emitting (and re-rendering the page)
    /// when nothing actually changed.
    pub(crate) last_stream_icon_state:
        std::cell::RefCell<Option<(Option<i64>, Option<String>, bool)>>,
    pub(crate) library: Library,
    pub(crate) player: Player,
    /// Lock screen / media keys control (MPRIS, optional).
    pub(crate) mpris: crate::core::mpris::Mpris,
    /// Own input sender to send messages to the component from methods without
    /// a `ComponentSender` (e.g. [`Self::play_current`]).
    pub(crate) input: relm4::Sender<Msg>,
    /// Album/artist overviews + file-list factory + gallery rendering state.
    pub(crate) libview: LibView,
    /// Number of background workers still running for a **manual** refresh
    /// (rescan/cloud/podcasts/YouTube). While > 0 the loading overlay shows a
    /// spinner; each worker's completion decrements it back toward zero.
    pub(crate) refresh_pending: u32,
    /// Progress of a running podcast/YouTube "refresh all": items done, items
    /// total, and the feed/channel currently being fetched. Drives a progress
    /// bar in the loading overlay, so a refresh is no longer a mute spinner.
    pub(crate) refresh_progress: Option<(usize, usize, String)>,
    /// Outcome of the last refresh ("3 channels updated · 2 new videos"), shown
    /// in the overlay for a moment after the run and then cleared. Informational
    /// toasts are disabled app-wide, so this is where a refresh reports back.
    pub(crate) refresh_summary: Option<String>,
    /// A first/initial library scan is running (the music folder is being read
    /// for the very first time, so the views are still empty). Drives the
    /// loading overlay with an explanatory text so the app does not look frozen.
    /// Cleared when the scan reports back (`Cmd::ScanDone`).
    pub(crate) scanning: bool,
    /// Library-scan progress (driven by `Cmd::ScanProgress`): files read / total,
    /// and bytes read / total. Shown as a progress bar + counts under the spinner
    /// while `scanning`. Reset at the start of each scan.
    pub(crate) scan_done: usize,
    pub(crate) scan_total: usize,
    pub(crate) scan_bytes: u64,
    pub(crate) scan_total_bytes: u64,
    /// Cancel flag for the running scan; the "Cancel" button sets it and the
    /// scan worker stops at the next file (shared with the worker thread).
    pub(crate) scan_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Online-enrichment state (covers/artist photos/fingerprint fetching).
    pub(crate) enrich_state: EnrichState,
    /// App-wide preferences (display language, active audio output).
    pub(crate) settings: Settings,
    /// File browser + extra music sources (2nd local folder / Nextcloud) state.
    pub(crate) files: FilesState,
    /// Playback transport: queue, shuffle order, history, resume/stats sessions.
    pub(crate) transport: TransportState,
    /// Mini-player / now-playing strip state.
    pub(crate) mini: MiniState,
    /// Sleep-timer state (header zzz button + countdown / fade-out).
    pub(crate) sleep: SleepState,
    /// Lyrics of the running track + open karaoke view.
    pub(crate) lyrics: LyricsState,
    pub(crate) toast_overlay: adw::ToastOverlay,
    /// Concerts page state (live-recording collection).
    pub(crate) concerts: ConcertsState,
    /// Navigation + layout chrome.
    pub(crate) nav: NavState,
    /// Favorites + audiobooks page state.
    pub(crate) favorites: FavoritesState,
    /// Playlists page state.
    pub(crate) playlists: PlaylistsState,
    /// Podcasts page state.
    pub(crate) podcasts: PodcastsState,
    /// Streaming (internet radio) + timeshift-recording page state.
    pub(crate) streaming: StreamingState,
    /// Voice-memo page state (microphone recordings + categories).
    pub(crate) memo: crate::ui::app_memo::MemoState,
    /// YouTube page state (optional feature, gated behind `youtube_enabled`).
    pub(crate) youtube: YoutubeState,
    /// Source ids that are currently **not reachable** (Nextcloud offline) –
    /// controls the red "Disconnected" hint on their covers/photos/songs.
    pub(crate) offline_sources: std::collections::HashSet<i64>,
    /// Statistics page, extracted into its own relm4 component.
    pub(crate) stats_page: relm4::Controller<crate::ui::stats_page::StatsPage>,
    /// Device sync, extracted into its own relm4 component (dialog + worker).
    pub(crate) sync_page: relm4::Controller<crate::ui::sync_page::SyncPage>,
    /// Whether a device is currently paired – controls the green sync icon at the
    /// top. Kept here (parent chrome); set via the component's `ConnectedChanged`.
    pub(crate) sync_connected: bool,
    /// Nextcloud setup dialog, extracted into its own relm4 component.
    pub(crate) cloud_page: relm4::Controller<crate::ui::cloud_page::CloudPage>,
    /// Podcasts page, extracted into its own relm4 component (list + dialogs +
    /// feed workers). Playback stays in the parent transport; the page reaches it
    /// via `PodcastsOutput` and is told the state back via `PlaybackStateChanged`.
    pub(crate) podcasts_page: relm4::Controller<crate::ui::podcasts_page::PodcastsPage>,
    /// Hand-off slot for episode subpages built by the PodcastsPage component
    /// (read in `Msg::Podcast(PodcastMsg::PushPodcastSubpage)`, then pushed onto the shared nav).
    pub(crate) podcast_subpage: std::rc::Rc<std::cell::RefCell<Option<(String, gtk::Box)>>>,
    /// YouTube page, extracted into its own relm4 component. Transport + yt-dlp
    /// settings stay on `App` (see `app_yt_glue.rs`).
    pub(crate) yt_page: relm4::Controller<crate::ui::yt_page::YtPage>,
    /// Hand-off slot for subpages built by the YtPage component.
    pub(crate) yt_subpage: std::rc::Rc<std::cell::RefCell<Option<(String, gtk::Box)>>>,
    /// Internet-radio page, extracted into its own relm4 component. The timeshift
    /// recorder and playback stay on `App` (see `app_streaming.rs`).
    pub(crate) stream_page: relm4::Controller<crate::ui::stream_page::StreamPage>,
    /// First-run setup assistant, shown once on the very first launch.
    pub(crate) setup_page: relm4::Controller<crate::ui::setup::SetupPage>,
    /// Runtime theming: app scaling + design options (colors, blurred background).
    pub(crate) theme: crate::ui::theme::ThemeState,
    /// Optional desktop tray icon + window behavior (close-to-tray, skip-taskbar).
    pub(crate) tray: TrayState,
    /// MPRIS-style media popup opened by a left click on the tray icon (lazy).
    pub(crate) media_popup: Option<crate::ui::tray_popup::MediaPopup>,
    /// Embedded MCP server state (now-playing snapshot + stop flag).
    pub(crate) mcp: McpState,
}

#[derive(Debug)]
pub enum Msg {
    Activate(usize),
    ToggleQueue(usize),
    ShowContextMenu(usize),
    ShowArtistDetail(usize),
    ShowAlbumDetail(usize),
    /// Open the detail page of an album via (artist, album) (from subpages).
    ShowAlbumDetailFor {
        artist: String,
        album: String,
    },
    /// Open the detail page of a single song via its path.
    ShowTrackDetail(String),
    /// Open the songs subpage of an album from the album overview (short tap).
    ShowAlbumTracks(usize),
    /// Singles / Compilations overviews — same behaviour as the album overview,
    /// indexing into their own factory/overview.
    ShowSingleTracks(usize),
    ShowSingleDetail(usize),
    ShowCompilationTracks(usize),
    ShowCompilationDetail(usize),
    /// Short tap on an artist: list its albums & songs.
    OpenArtistTracks(usize),
    /// Tap on an album in the artist subpage: list its tracks as
    /// a further subpage.
    OpenAlbumTracks {
        artist: String,
        album: String,
    },
    /// Tap on a greyed "missing" track row: confirm searching for it online and
    /// adding it to the album.
    ShowMissingTrack {
        artist: String,
        album: String,
        disc: u32,
        position: u32,
        title: String,
    },
    /// Confirmed: search the missing track online and offer the top hits so the
    /// user picks which version to add (search only — the download happens once
    /// a candidate is chosen, see [`Msg::DownloadMissingTrack`]).
    AddMissingTrack {
        artist: String,
        album: String,
        disc: u32,
        position: u32,
        title: String,
    },
    /// A YouTube candidate was picked from the missing-track chooser: download
    /// that video into the album folder, tag it and index it.
    DownloadMissingTrack {
        artist: String,
        album: String,
        disc: u32,
        position: u32,
        title: String,
        video_id: String,
    },
    /// Play a track from the artist overview (queue = all tracks
    /// of the artist, start at the tapped one). `close` pops the subpage
    /// back to the main view (row tap) vs. keeps it open (play button).
    PlayArtistTrack {
        name: String,
        path: String,
        close: bool,
    },
    /// Play a **single** selected track (from an album or playlist): only this
    /// track is enqueued, not its siblings. `close` pops the subpage back to the
    /// main view (row tap) vs. keeps it open (play button).
    PlayOneTrack {
        path: String,
        close: bool,
    },
    /// Tap on an album/folder entry in concerts/audiobooks: list its
    /// tracks as a subpage (instead of playing directly).
    OpenEntryTracks {
        scope: String,
        key: String,
    },
    /// Play a track of a folder audiobook/concert (queue = folder in
    /// order, start at the tapped one).
    PlayFolderTrack {
        folder: String,
        path: String,
        close: bool,
    },
    /// Play the whole album in track order (play button of the album row).
    PlayAlbum {
        artist: String,
        album: String,
    },
    /// Play the album folder at this file-browser row index (its play button).
    PlayFsAlbum(usize),
    /// Play button of an overview row (albums / singles / compilations): plays
    /// that album, or toggles pause while it is the one already running.
    PlayAlbumAt(usize),
    PlaySingleAt(usize),
    PlayCompilationAt(usize),
    /// Header sync icon → open the pairing / connection-status dialog (no item).
    OpenSync,
    // --- Device synchronization (handled by the SyncPage component) ---
    /// The sync component paired/disconnected → tint the header icon.
    SyncConnected(bool),
    /// The sync component imported metadata → reload the affected views.
    SyncImported,
    /// Command from the lock screen / from media keys (MPRIS).
    Mpris(crate::core::mpris::MprisCommand),
    /// Command from the embedded MCP server (see [`crate::ui::app_mcp`]).
    Mcp(crate::core::mcp::McpCommand),
    /// MCP-server settings (backend mode / LAN exposure / bearer token)
    /// (see [`crate::ui::app_mcp`]).
    McpSetting(crate::ui::app_mcp::McpSettingMsg),
    /// Periodic, quiet background backfill: fetch missing artist photos (first)
    /// and online covers, without the user having to trigger it.
    AutoEnrichTick,
    /// On-demand fingerprint track recognition for the **just started**
    /// track without usable metadata (AcoustID), triggered on play.
    FingerprintCurrent(PathBuf),
    NavUp,
    FilesGoStart,
    Refresh,
    /// Cancel the running library scan (the import progress "Cancel" button).
    ScanCancel,
    OpenSettings,
    /// Set or clear the sleep timer (from the header zzz popover).
    SetSleepTimer(SleepChoice),
    /// Open the library search dialog (title-bar search icon).
    OpenSearch,
    /// A song hit of the search was activated → play it (close the dialog).
    SearchPlayTrack(String),
    /// An album hit of the search was activated → open its track list.
    SearchOpenAlbum(String),
    /// An artist hit of the search was activated → open the artist subpage.
    SearchOpenArtist(String),
    OpenGlobalEq,
    /// Open the equalizer for the currently running track.
    OpenCurrentEq,
    /// Open the track-level equalizer for a specific path (e.g. a YouTube
    /// video from its detail view). `title` is only the header label.
    OpenTrackEq {
        path: String,
        title: String,
    },
    /// Back arrow in the shared header: pop the current subpage.
    NavBack,
    /// Music sources (Files tab bar: extra local folders / Nextcloud)
    /// (see [`crate::ui::app_views_sources`]).
    Source(crate::ui::app_views_sources::SourceMsg),
    /// Appearance / design: scaling, colours, background (see [`crate::ui::theme`]).
    Design(crate::ui::theme::DesignMsg),
    /// Desktop tray icon: settings + click actions (see [`crate::ui::app_tray`]).
    Tray(crate::ui::app_tray::TrayMsg),
    /// Sort + gallery: the title-bar sort popover, the global gallery view, and
    /// the page `*Changed` mirrors (see [`crate::ui::app_sort`]).
    Sort(crate::ui::app_sort::SortMsg),
    /// Equalizer: set / enable / clear bands per output × level
    /// (see [`crate::ui::app_eq`]).
    Eq(crate::ui::app_eq::EqMsg),
    // Playlists
    /// Playlists section: create / open / play / rename / delete + cover
    /// (see [`crate::ui::app_playlist`]).
    Playlist(crate::ui::app_playlist::PlaylistMsg),
    /// A podcast/YouTube "refresh all" advanced by one item → overlay bar.
    RefreshProgress {
        done: usize,
        total: usize,
        label: String,
    },
    /// A refresh reported its outcome → show it in the overlay for a moment.
    RefreshSummary(String),
    /// The summary's display time elapsed → clear the overlay.
    ClearRefreshSummary,

    // ---- Voice memos ----
    /// Voice memos + categories (see [`crate::ui::app_memo`]).
    Memo(crate::ui::app_memo::MemoMsg),
    /// StreamMsg — see `crate::ui::app_streaming`.
    Stream(crate::ui::app_streaming::StreamMsg),
    /// EditMsg — see `crate::ui::app_rec_edit`.
    Edit(crate::ui::app_rec_edit::EditMsg),
    /// YtMsg — see `crate::ui::app_yt_glue`.
    Yt(crate::ui::app_yt_glue::YtMsg),
    /// PodcastMsg — see `crate::ui::app_episode_playback`.
    Podcast(crate::ui::app_episode_playback::PodcastMsg),
    /// LyricsMsg — see `crate::ui::app_lyrics`.
    Lyrics(crate::ui::app_lyrics::LyricsMsg),
    /// ConcertMsg — see `crate::ui::app_concert`.
    Concert(crate::ui::app_concert::ConcertMsg),
    /// FavoriteMsg — see `crate::ui::app_favorites`.
    Favorite(crate::ui::app_favorites::FavoriteMsg),
    /// CoverMsg — see `crate::ui::app_covers`.
    Cover(crate::ui::app_covers::CoverMsg),
    /// SettingMsg — see `crate::ui::app_settings`.
    Setting(crate::ui::app_settings::SettingMsg),
    /// CtxMsg — see `crate::ui::app_dialogs`.
    Ctx(crate::ui::app_dialogs::CtxMsg),
    /// TransportMsg — see `crate::ui::app_playback`.
    Transport(crate::ui::app_playback::TransportMsg),
}

/// Results of the background workers (read folder or online enrichment).
#[derive(Debug)]
pub enum Cmd {
    Entries(Vec<FsEntry>),
    /// Result of a WebDAV directory listing (background PROPFIND). Carries the
    /// source and the rel path along, so that an intervening source/folder
    /// switch can discard the stale result.
    RemoteEntries(
        Result<Vec<crate::core::webdav::DavEntry>, String>,
        ActiveSource,
        String,
    ),
    /// Backfilled tags of remote files: (rel path, title, artist, duration).
    RemoteTags(Vec<(String, Option<String>, Option<String>, Option<i64>)>),
    /// A remote file was downloaded: (rel path, local copy) or error.
    RemoteDownloaded(Result<(String, PathBuf), String>),
    /// Online enrichment finished; `changed` = something new was added
    /// (controls during the quiet backfill whether the views are reloaded).
    EnrichDone {
        changed: bool,
    },
    /// Intermediate state: reload albums/artists view (e.g. after a phase).
    ReloadViews,
    /// Local library scan finished; `then_enrich` = possibly fetch online
    /// afterwards. `manual` = part of a user-triggered refresh (clears one slot
    /// of the refresh spinner on completion).
    ScanDone {
        then_enrich: bool,
        manual: bool,
    },
    /// Library-scan progress tick (throttled): files read / total + bytes.
    ScanProgress {
        done: usize,
        total: usize,
        bytes: u64,
        total_bytes: u64,
    },
    /// Found concert candidates (for the import dialog).
    Candidates(Vec<crate::core::concert::Candidate>),
    /// yt-dlp install/update/startup-check finished: the version on success,
    /// or an error message. Drives the settings status and `youtube.ytdlp_version`.
    YtDlpReady(Result<String, String>),
    /// Silent background yt-dlp auto-update finished (the version on success, or
    /// an error message). Unlike [`Cmd::YtDlpReady`] it never toasts: a routine
    /// refresh — or a failure while offline — must not nag the user.
    YtDlpAutoUpdated(Result<String, String>),
    /// Background yt-dlp version probe (opened settings) finished: `Some(v)` if a
    /// usable yt-dlp is present, `None` otherwise. Caches the result and refreshes
    /// the settings row without ever blocking the UI thread on the subprocess.
    YtDlpChecked(Option<String>),
    /// A playlist's videos were listed → start playing them, log the playlist to
    /// "Recent", and mirror it into the Playlists section. (Transport; the page
    /// requests it via `YtOutput::StartPlaylist`.)
    YtPlaylistStart {
        url: String,
        title: String,
        items: Vec<(String, String)>,
        /// Summed runtime (seconds) of the playlist, for the Recent row. `None`
        /// when no durations were available.
        total_duration: Option<i64>,
    },
    /// Startup background refresh finished → tell the YtPage component to reload.
    YtReload,
    /// A recognized song ("Recently heard") was resolved to a YouTube video:
    /// `video_id` is `None` when nothing matched. `download` distinguishes the
    /// two actions — play the stream, or import it into the library.
    HeardResolved {
        video_id: Option<String>,
        title: String,
        /// Artist as the song recognition reported it — a better metadata hint
        /// for the import than anything YouTube says.
        artist: Option<String>,
        download: bool,
    },
    /// The canonical (MusicBrainz) tracklist of an album finished fetching and
    /// was cached → refill the album page so missing tracks show up.
    AlbumTracklistFetched {
        artist: String,
        album: String,
    },
    /// Top YouTube hits for a missing track → present a chooser so the user
    /// picks which version to download.
    MissingTrackCandidates {
        artist: String,
        album: String,
        disc: u32,
        position: u32,
        title: String,
        results: Vec<crate::core::youtube::YtResult>,
    },
    /// A missing track finished downloading (or failed) → close the spinner,
    /// refill the album page, toast the outcome.
    MissingTrackDone {
        artist: String,
        album: String,
        ok: bool,
        message: String,
    },
    /// Reachability of the sources (source id → reachable?).
    SourceStatus(Vec<(i64, bool)>),
    /// Cloud sources were re-indexed → rebuild views + covers. `manual` = the
    /// user pressed refresh (force online enrichment regardless of the passive
    /// auto-enrich setting); `false` = silent background top-up at startup.
    CloudReindexed {
        manual: bool,
    },
    /// Background LRCLIB lookup for the running track finished. Carries the path
    /// it was started for (to ignore stale results) and the lyrics if found.
    LyricsLoaded {
        path: String,
        lyrics: Option<crate::core::lyrics::Lyrics>,
    },
}

#[relm4::component(pub)]
impl Component for App {
    type Init = ();
    type Input = Msg;
    type Output = ();
    /// Result of the background workers (read folder / online enrichment).
    type CommandOutput = Cmd;

    view! {
        adw::ApplicationWindow {
            set_title: Some("Emilia"),
            set_default_width: 800,
            set_default_height: 600,

            #[local_ref]
            toast_overlay -> adw::ToastOverlay {
                // Blurred-background layer: the cover/custom image fills the
                // window behind everything (bottom child), a scrim tints it for
                // readability, and the real UI rides on top as a measured
                // overlay (so the window still sizes to the content, not the
                // tiny background texture — set in `finish_init`).
                #[wrap(Some)]
                #[name = "bg_overlay"]
                set_child = &gtk::Overlay {
                    #[wrap(Some)]
                    #[name = "bg_picture"]
                    set_child = &gtk::Picture {
                        set_content_fit: gtk::ContentFit::Cover,
                    },
                    #[name = "bg_scrim"]
                    add_overlay = &gtk::Box {
                        add_css_class: "emilia-bg-scrim",
                    },
                    #[name = "split"]
                    add_overlay = &adw::OverlaySplitView {
                set_collapsed: false,
                set_enable_show_gesture: false,
                set_enable_hide_gesture: false,
                set_min_sidebar_width: 180.0,
                set_max_sidebar_width: 240.0,

                // Sidebar (desktop): icon-only navigation on the left
                #[wrap(Some)]
                set_sidebar = &adw::ToolbarView {
                    add_top_bar = &adw::HeaderBar {
                        #[wrap(Some)]
                        set_title_widget = &adw::WindowTitle::new("", ""),
                    },
                    #[wrap(Some)]
                    #[name = "sidebar_nav"]
                    set_content = &gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 4,
                        set_margin_top: 8,
                        set_margin_bottom: 8,
                        set_margin_start: 6,
                        set_margin_end: 6,
                        set_halign: gtk::Align::Fill,
                        // Full height, so that "Settings" sits at the very bottom.
                        set_valign: gtk::Align::Fill,
                        set_vexpand: true,
                    },
                },

                // The content side hosts its own NavigationView, so artist/album
                // subpages are pushed only here (in the content area). In desktop
                // mode the sidebar stays visible; when narrow the split is
                // collapsed and the content fills the window as before.
                // The persistent chrome (header, top nav, player) wraps the
                // NavigationView, so pushed subpages (album/track lists) appear in
                // the body **without** hiding the top/bottom navigation.
                #[wrap(Some)]
                #[name = "content_view"]
                set_content = &adw::ToolbarView {
                    add_top_bar = &adw::HeaderBar {
                        // Back arrow on a pushed subpage (the only header now).
                        #[name = "nav_back_btn"]
                        pack_start = &gtk::Button {
                            set_icon_name: "go-previous-symbolic",
                            set_tooltip_text: Some(&gettext("Back")),
                            set_visible: false,
                            connect_clicked => Msg::NavBack,
                        },
                        #[wrap(Some)]
                        #[name = "win_title"]
                        set_title_widget = &adw::WindowTitle::new("Emilia", ""),
                        // Library search: opens a dialog that searches artists,
                        // albums, songs and the file date and lists the hits. Kept
                        // as the leftmost item of the title bar.
                        pack_start = &gtk::Button {
                            set_icon_name: "system-search-symbolic",
                            set_tooltip_text: Some(&gettext("Search")),
                            connect_clicked => Msg::OpenSearch,
                        },
                        // Settings at the top only in narrow (mobile) mode – in
                        // desktop mode the item sits at the bottom of the sidebar.
                        // On mobile it sits on the right of the title bar.
                        #[name = "settings_top_btn"]
                        pack_end = &gtk::Button {
                            set_icon_name: "xsi-view-more-symbolic",
                            set_tooltip_text: Some(&gettext("Settings")),
                            set_visible: false,
                            connect_clicked => Msg::OpenSettings,
                        },
                        // Per-category sorting. The popover (criteria + direction)
                        // is built per section in `rebuild_sort_menu`; the button
                        // is hidden on sections without a sort control.
                        #[name = "sort_btn"]
                        pack_end = &gtk::MenuButton {
                            set_icon_name: "view-sort-descending-symbolic",
                            set_tooltip_text: Some(&gettext("Sort")),
                            set_visible: false,
                        },
                        // Sleep timer ("zzz"): a popover with presets (15/30/45/60
                        // min, end of track, off). The popover content + handlers
                        // are built in `setup_sleep_button`; the icon gets the
                        // "sleep-armed" CSS class while a timer is running.
                        #[name = "sleep_btn"]
                        pack_end = &gtk::MenuButton {
                            set_icon_name: "emilia-sleep-symbolic",
                            set_tooltip_text: Some(&gettext("Sleep timer")),
                        },
                        pack_start = &gtk::Button {
                            set_icon_name: "view-refresh-symbolic",
                            set_tooltip_text: Some(&gettext("Refresh")),
                            connect_clicked => Msg::Refresh,
                            // Disabled while a manual refresh is still running, so
                            // a second click can't reset the spinner counter.
                            #[watch]
                            set_sensitive: model.refresh_pending == 0,
                        },
                        // Device sync: opens the pairing / connection-status dialog
                        // (QR offer / scan, or "Connected with X"). Sharing itself
                        // is always started per item from a detail view, not here.
                        // With an existing pairing the icon is rendered green
                        // (CSS class, see below).
                        #[name = "sync_btn"]
                        pack_start = &gtk::Button {
                            set_icon_name: "emilia-share-symbolic",
                            set_tooltip_text: Some(&gettext("Connect to share")),
                            connect_clicked => Msg::OpenSync,
                            // Keep `flat` in both states — set_css_classes replaces
                            // the whole list, so dropping it would re-add the button
                            // background that header buttons are flattened out of.
                            #[watch]
                            set_css_classes: if model.sync_connected {
                                &["flat", "sync-connected"]
                            } else {
                                &["flat"]
                            },
                        },
                    },

                    // Top navigation (icon-only) – only in narrow (mobile) mode.
                    // Wrapped in a horizontal ScrolledWindow so the icon strip can
                    // scroll / swipe sideways when many sections are enabled and
                    // would otherwise overflow the narrow width.
                    #[name = "top_nav_scroller"]
                    add_top_bar = &gtk::ScrolledWindow {
                        // Standard kinetic-scrolling path for a smooth swipe; the
                        // scrollbar itself is hidden via CSS (`emilia-nav-scroller`)
                        // so the icon strip stays clean but swipes properly.
                        set_hscrollbar_policy: gtk::PolicyType::Automatic,
                        set_vscrollbar_policy: gtk::PolicyType::Never,
                        set_kinetic_scrolling: true,
                        set_propagate_natural_height: true,
                        set_css_classes: &["emilia-nav-scroller"],
                        set_visible: false,
                        #[wrap(Some)]
                        #[name = "top_nav"]
                        set_child = &gtk::Box {
                            set_spacing: 3,
                            // Mobile menu strip: 5px higher than before (top 12 → 7)
                            // with 5px more breathing room below (bottom 2 → 7).
                            set_margin_top: 7,
                            set_margin_bottom: 7,
                            // Center the icon strip when it fits; it still scrolls
                            // (left-aligned) once the icons overflow the width.
                            set_halign: gtk::Align::Center,
                        },
                    },

                    // Content with loading overlay. Desktop: a bit of space **between
                    // the title bar and the content** (top); in narrow (mobile) mode
                    // back to 0 via breakpoint (see `init`).
                    // The NavigationView lives in the body; the chrome around it
                    // stays put. Subpages are pushed onto it (header-less; the
                    // shared header above provides the back arrow + title).
                    #[wrap(Some)]
                    #[name = "nav_view"]
                    set_content = &adw::NavigationView {
                        adw::NavigationPage {
                            set_title: "Emilia",
                            set_tag: Some("main"),
                    #[wrap(Some)]
                    #[name = "content_overlay"]
                    set_child = &gtk::Overlay {
                        set_margin_top: 10,
                        #[wrap(Some)]
                        #[name = "view_stack"]
                        set_child = &adw::ViewStack {
                            add_titled_with_icon[Some("files"), &gettext("Files"), "folder-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    #[name = "files_page"]
                                    gtk::Box {
                                        set_orientation: gtk::Orientation::Vertical,
                                        set_vexpand: true,

                                        // Source tab bar: holds the linked source toggles (only
                                        // built when there is more than one folder) plus a trailing
                                        // "+" to add a folder/Nextcloud. Always shown on the Files
                                        // page so the "+" stays reachable. Filled in
                                        // `rebuild_source_tabs`.
                                        #[name = "source_tabs"]
                                        gtk::Box {
                                            set_orientation: gtk::Orientation::Horizontal,
                                            set_spacing: 6,
                                            // Same top gap as the Podcasts/Streaming/YouTube tab bars.
                                            set_margin_top: 2,
                                            // A small gap below the source tab menu.
                                            set_margin_bottom: 4,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                        },

                                        // Path/back bar – only in subfolders
                                        gtk::Box {
                                            set_spacing: 6,
                                            set_margin_all: 6,
                                            #[watch]
                                            set_visible: model.can_go_up(),
                                            gtk::Button {
                                                set_icon_name: "go-previous-symbolic",
                                                set_tooltip_text: Some(&gettext("Back")),
                                                add_css_class: "flat",
                                                #[watch]
                                                set_sensitive: model.can_go_up(),
                                                connect_clicked => Msg::NavUp,
                                            },
                                            gtk::Label {
                                                set_hexpand: true,
                                                set_xalign: 0.0,
                                                set_ellipsize: gtk::pango::EllipsizeMode::Start,
                                                add_css_class: "heading",
                                                #[watch]
                                                set_label: &model.folder_label(),
                                            },
                                        },

                                        gtk::ScrolledWindow {
                                            set_vexpand: true,
                                            // Hidden while a remote source failed to
                                            // load — the error status below shows why.
                                            #[watch]
                                            set_visible: model.files.remote_error.is_none(),
                                            #[local_ref]
                                            entries_box -> gtk::ListBox {
                                                set_valign: gtk::Align::Start,
                                                // When the source tab menu is shown, leave the same
                                                // gap below it as the YouTube/Channels lists; flush
                                                // to the top otherwise (like Artists/Albums).
                                                #[watch]
                                                set_margin_top: if model.source_tabs_visible() { 10 } else { 0 },
                                                set_margin_bottom: 0,
                                                set_margin_start: 12,
                                                set_margin_end: 12,
                                                set_css_classes: &["boxed-list"],
                                            },
                                        },

                                        // Remote (Nextcloud/WebDAV) load failure: show
                                        // the actual reason + a Retry button, instead of
                                        // a silently blank list.
                                        adw::StatusPage {
                                            set_icon_name: Some("network-error-symbolic"),
                                            set_title: &gettext("Could not load the folder"),
                                            set_vexpand: true,
                                            #[watch]
                                            set_visible: model.files.remote_error.is_some(),
                                            #[watch]
                                            set_description: model.files.remote_error.as_deref(),
                                            #[wrap(Some)]
                                            set_child = &gtk::Button {
                                                set_label: &gettext("Retry"),
                                                set_halign: gtk::Align::Center,
                                                add_css_class: "pill",
                                                add_css_class: "suggested-action",
                                                connect_clicked => Msg::Refresh,
                                            },
                                        },
                                    },
                                },
                            add_titled_with_icon[Some("artists"), &gettext("Artists"), "avatar-default-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    adw::StatusPage {
                                        set_icon_name: Some("avatar-default-symbolic"),
                                        set_title: &gettext("No artists"),
                                        set_description: Some(
                                            &gettext("Scan a music folder and fetch online metadata"),
                                        ),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.artist_count == 0,
                                    },

                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.artist_count > 0 && !model.libview.gallery_on("artists"),
                                        #[local_ref]
                                        artists_box -> gtk::ListView {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            // `boxed-list` is a GtkListBox style;
                                            // the equivalent for the virtualised
                                            // list lives in `emilia-card-list`.
                                            set_css_classes: &["emilia-card-list"],
                                        },
                                    },
                                    // Gallery variant (photo grid). The box holds either
                                    // a single grid or alphabetically grouped sections.
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.artist_count > 0 && model.libview.gallery_on("artists"),
                                        #[local_ref]
                                        artists_gallery_box -> gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 6,
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                        },
                                    },
                                },
                            add_titled_with_icon[Some("albums"), &gettext("Albums"), "media-optical-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    // Empty state while no albums are known
                                    adw::StatusPage {
                                        set_icon_name: Some("media-optical-symbolic"),
                                        set_title: &gettext("No albums"),
                                        set_description: Some(
                                            &gettext("Scan a music folder and fetch online metadata"),
                                        ),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.album_count == 0,
                                    },

                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.album_count > 0 && !model.libview.gallery_on("albums"),
                                        #[local_ref]
                                        albums_box -> gtk::ListView {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            // `boxed-list` is a GtkListBox style;
                                            // the equivalent for the virtualised
                                            // list lives in `emilia-card-list`.
                                            set_css_classes: &["emilia-card-list"],
                                        },
                                    },
                                    // Gallery variant (cover grid). The box holds either
                                    // a single grid or year-grouped sections (date sort).
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.album_count > 0 && model.libview.gallery_on("albums"),
                                        #[local_ref]
                                        albums_gallery_box -> gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 6,
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                        },
                                    },
                                },
                            add_titled_with_icon[Some("singles"), &gettext("Singles"), "audio-x-generic-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    adw::StatusPage {
                                        set_icon_name: Some("audio-x-generic-symbolic"),
                                        set_title: &gettext("No singles"),
                                        set_description: Some(
                                            &gettext("Singles are one-artist releases with just a few tracks"),
                                        ),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.single_count == 0,
                                    },

                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.single_count > 0 && !model.libview.gallery_on("singles"),
                                        #[local_ref]
                                        singles_box -> gtk::ListView {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            // `boxed-list` is a GtkListBox style;
                                            // the equivalent for the virtualised
                                            // list lives in `emilia-card-list`.
                                            set_css_classes: &["emilia-card-list"],
                                        },
                                    },
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.single_count > 0 && model.libview.gallery_on("singles"),
                                        #[local_ref]
                                        singles_gallery_box -> gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 6,
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                        },
                                    },
                                },
                            add_titled_with_icon[Some("compilations"), &gettext("Compilations"), "view-grid-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    adw::StatusPage {
                                        set_icon_name: Some("view-grid-symbolic"),
                                        set_title: &gettext("No compilations"),
                                        set_description: Some(
                                            &gettext("Compilations are albums with tracks by several artists"),
                                        ),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.compilation_count == 0,
                                    },

                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.compilation_count > 0 && !model.libview.gallery_on("compilations"),
                                        #[local_ref]
                                        compilations_box -> gtk::ListView {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            // `boxed-list` is a GtkListBox style;
                                            // the equivalent for the virtualised
                                            // list lives in `emilia-card-list`.
                                            set_css_classes: &["emilia-card-list"],
                                        },
                                    },
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.compilation_count > 0 && model.libview.gallery_on("compilations"),
                                        #[local_ref]
                                        compilations_gallery_box -> gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 6,
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                        },
                                    },
                                },
                            add_titled_with_icon[Some("concerts"), &gettext("Concerts"), "ticket-special-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    // List of the marked concerts
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.concerts.concert_items.is_empty() && !model.libview.gallery_on("concerts"),
                                        #[local_ref]
                                        concerts_list -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },
                                    // Gallery variant of the concerts. The box holds
                                    // either a single grid or alphabetically grouped
                                    // sections (name sort).
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.concerts.concert_items.is_empty() && model.libview.gallery_on("concerts"),
                                        #[local_ref]
                                        concerts_gallery_box -> gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 6,
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                        },
                                    },

                                    // Hint + actions (empty & hint active)
                                    adw::StatusPage {
                                        set_icon_name: Some("ticket-special-symbolic"),
                                        set_title: &gettext("Concerts"),
                                        set_description: Some(&gettext("Here you can list your collected concerts. Via Import concerts you get an overview of likely concerts: albums with live, unplugged or concert in the name, plus single files of 30 minutes or more. Mark them as a concert and they'll appear here. You can also add concerts later at any time via the options.")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.concerts.concert_items.is_empty() && !model.concerts.concert_hint_dismissed,
                                        #[wrap(Some)]
                                        set_child = &gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 8,
                                            set_halign: gtk::Align::Center,
                                            gtk::Button {
                                                set_label: &gettext("Import concerts"),
                                                set_css_classes: &["suggested-action", "pill"],
                                                connect_clicked => Msg::Concert(ConcertMsg::ConcertImport),
                                            },
                                            gtk::Button {
                                                set_label: &gettext("I'll do it myself"),
                                                add_css_class: "pill",
                                                connect_clicked => Msg::Concert(ConcertMsg::ConcertDismissHint),
                                            },
                                            gtk::Button {
                                                set_label: &gettext("Hide menu item"),
                                                add_css_class: "pill",
                                                connect_clicked => Msg::Concert(ConcertMsg::ConcertHideSection),
                                            },
                                        },
                                    },

                                    // Empty state (empty & hint hidden):
                                    // user chose "I'll do it myself" – therefore
                                    // deliberately NO import button anymore.
                                    adw::StatusPage {
                                        set_icon_name: Some("ticket-special-symbolic"),
                                        set_title: &gettext("No concerts"),
                                        set_description: Some(&gettext("Mark an album or a track as a concert via the options.")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.concerts.concert_items.is_empty() && model.concerts.concert_hint_dismissed,
                                    },
                                },
                            add_titled_with_icon[Some("playlists"), &gettext("Playlists"), "view-list-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.playlists.playlist_items.is_empty() && !model.libview.gallery_on("playlists"),
                                        #[local_ref]
                                        playlists_list -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },
                                    // Gallery variant of the playlists (cover grid).
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.playlists.playlist_items.is_empty() && model.libview.gallery_on("playlists"),
                                        #[local_ref]
                                        playlists_gallery_box -> gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 6,
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                        },
                                    },

                                    adw::StatusPage {
                                        set_icon_name: Some("view-list-symbolic"),
                                        set_title: &gettext("No playlists"),
                                        set_description: Some(&gettext("Create a playlist or add tracks via the options.")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.playlists.playlist_items.is_empty(),
                                    },
                                    // The explicit "New playlist" button was removed –
                                    // playlists are created from a track's "Add to
                                    // playlist" options (which can create one inline).
                                },
                            // Podcasts live in their own relm4 component.
                            add_titled_with_icon[Some("podcasts"), &gettext("Podcasts"), "podcast-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    append: model.podcasts_page.widget(),
                                },
                            // Internet radio lives in its own relm4 component.
                            add_titled_with_icon[Some("streaming"), &gettext("Streaming"), "internet-radio-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    append: model.stream_page.widget(),
                                },
                            add_titled_with_icon[Some("favorites"), &gettext("Favorites"), "emilia-favorite-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.favorites.favorite_items.is_empty() && !model.libview.gallery_on("favorites"),
                                        #[local_ref]
                                        favorites_list -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },
                                    // Gallery variant of the favorites (cover grid).
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.favorites.favorite_items.is_empty() && model.libview.gallery_on("favorites"),
                                        #[local_ref]
                                        favorites_gallery_box -> gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 6,
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                        },
                                    },

                                    adw::StatusPage {
                                        set_icon_name: Some("emilia-favorite-symbolic"),
                                        set_title: &gettext("No favorites"),
                                        set_description: Some(&gettext("Mark tracks, albums or artists with the star under \"More info\".")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.favorites.favorite_items.is_empty(),
                                    },
                                },
                            // YouTube lives in its own relm4 component.
                            add_titled_with_icon[Some("youtube"), &gettext("YouTube"), "im-youtube-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    append: model.yt_page.widget(),
                                },
                            add_titled_with_icon[Some("audiobooks"), &gettext("Audiobooks"), "emilia-audiobook-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.favorites.audiobook_items.is_empty() && !model.libview.gallery_on("audiobooks"),
                                        #[local_ref]
                                        audiobooks_list -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },
                                    // Gallery variant of the audiobooks. The box holds
                                    // either a single grid or alphabetically grouped
                                    // sections (name sort).
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.favorites.audiobook_items.is_empty() && model.libview.gallery_on("audiobooks"),
                                        #[local_ref]
                                        audiobooks_gallery_box -> gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 6,
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                        },
                                    },

                                    adw::StatusPage {
                                        set_icon_name: Some("emilia-audiobook-symbolic"),
                                        set_title: &gettext("No audiobooks"),
                                        set_description: Some(&gettext("Mark albums, folders or tracks as audiobooks via the properties.")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.favorites.audiobook_items.is_empty(),
                                    },
                                },
                            add_titled_with_icon[Some("memo"), &gettext("Memo"), "audio-input-microphone-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    // Header: Recent / Category switcher + "+" (same layout and
                                    // top height as the YouTube header).
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
                                            set_active: model.memo.view == MemoView::Recent,
                                            connect_clicked => Msg::Memo(crate::ui::app_memo::MemoMsg::SetView(MemoView::Recent)),
                                        },
                                        gtk::ToggleButton {
                                            set_label: &gettext("Category"),
                                            set_hexpand: true,
                                            #[watch]
                                            set_active: model.memo.view == MemoView::Category,
                                            connect_clicked => Msg::Memo(crate::ui::app_memo::MemoMsg::SetView(MemoView::Category)),
                                        },
                                        gtk::Button {
                                            set_icon_name: "list-add-symbolic",
                                            set_tooltip_text: Some(&gettext("Add category")),
                                            add_css_class: "flat",
                                            connect_clicked => Msg::Memo(crate::ui::app_memo::MemoMsg::CategoryAddPrompt),
                                        },
                                    },
                                    // Memo list + empty state.
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[wrap(Some)]
                                        set_child = &gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            #[local_ref]
                                            memos_list -> gtk::ListBox {
                                                set_valign: gtk::Align::Start,
                                                set_margin_top: 10,
                                                set_margin_start: 12,
                                                set_margin_end: 12,
                                                set_margin_bottom: 12,
                                                add_css_class: "boxed-list",
                                            },
                                            adw::StatusPage {
                                                set_icon_name: Some("audio-input-microphone-symbolic"),
                                                set_title: &gettext("No memos yet"),
                                                set_description: Some(&gettext("Use the microphone button in the player bar to record a voice memo.")),
                                                set_vexpand: true,
                                                // Only on the Recent tab; the Category tree shows nothing when empty.
                                                #[watch]
                                                set_visible: model.memo.view == MemoView::Recent
                                                    && model.memo.memo_items.is_empty()
                                                    && !model.memo.recording,
                                            },
                                        },
                                    },
                                },
                            add_titled_with_icon[Some("stats"), &gettext("Statistics"), "emilia-stats-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    // Statistics live in their own relm4 component.
                                    append: model.stats_page.widget(),
                                },
                        },

                        // Centered spinner while reading – on a
                        // semi-transparent surface, so that the text over the
                        // content stays readable (CSS class, see `init`).
                        add_overlay = &gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_halign: gtk::Align::Center,
                            set_valign: gtk::Align::Center,
                            set_spacing: 12,
                            // Click-through normally; targetable during a scan so
                            // the "Cancel" button below can be clicked.
                            #[watch]
                            set_can_target: model.scanning,
                            add_css_class: "emilia-loading",
                            #[watch]
                            set_visible: model.overlay_visible(),

                            gtk::Spinner {
                                set_spinning: true,
                                set_size_request: (48, 48),
                                // While the outcome of a finished refresh is on
                                // screen there is nothing left to wait for.
                                #[watch]
                                set_visible: model.refresh_summary.is_none(),
                            },
                            gtk::Label {
                                #[watch]
                                set_label: &model.overlay_text(),
                                add_css_class: "dim-label",
                                // Long status lines (e.g. the first-scan hint) must
                                // wrap, not push the overlay wider than a narrow
                                // phone screen.
                                set_wrap: true,
                                set_justify: gtk::Justification::Center,
                                set_max_width_chars: 28,
                            },

                            // Refresh progress (podcast feeds / YouTube channels):
                            // a bar with "2 of 7" over the name currently being
                            // fetched, so a long refresh isn't a mute spinner.
                            gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_halign: gtk::Align::Center,
                                set_spacing: 4,
                                #[watch]
                                set_visible: model.refresh_progress.is_some(),

                                gtk::ProgressBar {
                                    set_width_request: 260,
                                    set_show_text: true,
                                    #[watch]
                                    set_fraction: match &model.refresh_progress {
                                        Some((done, total, _)) if *total > 0 => {
                                            *done as f64 / *total as f64
                                        }
                                        _ => 0.0,
                                    },
                                    #[watch]
                                    set_text: model.refresh_progress.as_ref().map(|(done, total, _)| {
                                        gettext_f(
                                            "{done} of {total}",
                                            &[
                                                ("done", &done.to_string()),
                                                ("total", &total.to_string()),
                                            ],
                                        )
                                    }).as_deref(),
                                },
                                gtk::Label {
                                    add_css_class: "caption",
                                    add_css_class: "dim-label",
                                    set_ellipsize: gtk::pango::EllipsizeMode::End,
                                    set_max_width_chars: 28,
                                    #[watch]
                                    set_label: model
                                        .refresh_progress
                                        .as_ref()
                                        .map(|(_, _, label)| label.as_str())
                                        .unwrap_or_default(),
                                },
                            },

                            // Import progress: a bar with "X of Y songs" and a
                            // subtle "X MB of Y MB" line, plus a Cancel button.
                            // Only while a library scan is actually running.
                            gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_halign: gtk::Align::Center,
                                set_spacing: 4,
                                #[watch]
                                set_visible: model.scanning && model.scan_total > 0,

                                gtk::ProgressBar {
                                    set_width_request: 260,
                                    set_show_text: true,
                                    #[watch]
                                    set_fraction: if model.scan_total > 0 {
                                        model.scan_done as f64 / model.scan_total as f64
                                    } else {
                                        0.0
                                    },
                                    // Only format while actually scanning; the
                                    // watch runs every view pass (incl. each tick
                                    // while playing), but the block is hidden then.
                                    #[watch]
                                    set_text: model.scanning.then(|| gettext_f(
                                        "{done} of {total} songs",
                                        &[
                                            ("done", &model.scan_done.to_string()),
                                            ("total", &model.scan_total.to_string()),
                                        ],
                                    )).as_deref(),
                                },
                                gtk::Label {
                                    add_css_class: "caption",
                                    add_css_class: "dim-label",
                                    #[watch]
                                    set_label: &if model.scanning {
                                        gettext_f(
                                            "{done} MB of {total} MB",
                                            &[
                                                ("done", &(model.scan_bytes / 1_048_576).to_string()),
                                                ("total", &(model.scan_total_bytes / 1_048_576).to_string()),
                                            ],
                                        )
                                    } else {
                                        String::new()
                                    },
                                },
                                gtk::Button {
                                    set_label: &gettext("Cancel"),
                                    set_halign: gtk::Align::Center,
                                    set_margin_top: 4,
                                    add_css_class: "pill",
                                    connect_clicked => Msg::ScanCancel,
                                },
                            },
                        },
                    },
                        }, // close the main NavigationPage
                    }, // close the NavigationView (nav_view)

                    // Mini player at the bottom with transport controls. The bar stays
                    // always visible; without a selected track only the
                    // song row (title + seek bar) is hidden and the
                    // transport buttons are grayed out.
                    add_bottom_bar = &gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        // Tighter bar: the vertical space above/below the song line
                        // is halved (spacing 2→1, top 4→2, bottom 6→3, song top 5→2).
                        set_spacing: 1,
                        set_margin_top: 2,
                        // 5px more room below the transport row (the big play
                        // button) to the bottom edge.
                        set_margin_bottom: 8,
                        set_margin_start: 10,
                        set_margin_end: 10,

                        gtk::Button {
                            add_css_class: "flat",
                            // Vertical breathing room around the title comes from the
                            // button's own padding (CSS `emilia-songline`), so the
                            // hover/press area includes it; only a small top margin here.
                            add_css_class: "emilia-songline",
                            set_tooltip_text: Some(&gettext("Show details of the current track")),
                            // 5px more breathing room above the song name (2 → 7).
                            set_margin_top: 7,
                            // Without a selected track, hide entirely (frees up space).
                            // Also hidden while recording a memo, so the level meter
                            // below can take the title's place.
                            #[watch]
                            set_visible: model.mini.now_playing.is_some() && !model.memo.recording,
                            // A plain tap on the song display opens the track detail view.
                            connect_clicked[sender] => move |_| {
                                sender.input(Msg::Transport(TransportMsg::OpenNowPlaying));
                            },
                            // Long press (touch) keeps working too; it claims the sequence
                            // so the button's own click won't also fire.
                            add_controller = gtk::GestureLongPress {
                                connect_pressed[sender] => move |gesture, _, _| {
                                    gesture.set_state(gtk::EventSequenceState::Claimed);
                                    sender.input(Msg::Transport(TransportMsg::OpenNowPlaying));
                                },
                            },
                            // Right click (classic mouse): same detail view.
                            add_controller = gtk::GestureClick {
                                set_button: gtk::gdk::BUTTON_SECONDARY,
                                connect_pressed[sender] => move |gesture, _, _, _| {
                                    gesture.set_state(gtk::EventSequenceState::Claimed);
                                    sender.input(Msg::Transport(TransportMsg::OpenNowPlaying));
                                },
                            },
                            #[wrap(Some)]
                            set_child = &gtk::Label {
                                set_xalign: 0.5,
                                set_justify: gtk::Justification::Center,
                                // Wrap long titles onto up to 2 lines instead of
                                // breaking the bar; then truncate with …. The
                                // width limit prevents a long title from
                                // inflating the minimum width of the window.
                                set_wrap: true,
                                set_wrap_mode: gtk::pango::WrapMode::WordChar,
                                set_lines: 2,
                                set_ellipsize: gtk::pango::EllipsizeMode::End,
                                set_max_width_chars: 28,
                                add_css_class: "caption",
                                // Nothing selected → no text (bar appears inactive).
                                #[watch]
                                set_label: model.mini.now_playing.as_deref().unwrap_or(""),
                            },
                        },

                        // While recording a voice memo, the live input level takes
                        // the place of the (hidden) track title. Driven by the mic
                        // `level` element via a poll timeout.
                        #[local_ref]
                        rec_meter -> gtk::DrawingArea {
                            set_content_width: 220,
                            set_content_height: 22,
                            set_halign: gtk::Align::Center,
                            set_margin_top: 7,
                            set_tooltip_text: Some(&gettext("Recording level")),
                            #[watch]
                            set_visible: model.memo.recording,
                        },

                        // Chapter name when hovering over the seek bar
                        // (controlled imperatively via the hover controller).
                        #[name = "chapter_label"]
                        gtk::Label {
                            set_xalign: 0.5,
                            set_ellipsize: gtk::pango::EllipsizeMode::End,
                            set_max_width_chars: 36,
                            set_visible: false,
                            add_css_class: "caption",
                            add_css_class: "dim-label",
                        },

                        // Seek bar: position / slider / total duration.
                        gtk::Box {
                            set_spacing: 6,
                            set_margin_start: 4,
                            set_margin_end: 4,
                            #[watch]
                            set_visible: model.mini.now_playing.is_some(),

                            gtk::Label {
                                add_css_class: "caption",
                                add_css_class: "numeric",
                                #[watch]
                                set_label: &fmt_duration(model.mini.position_ms),
                            },
                            #[name = "seek_scale"]
                            gtk::Scale {
                                set_orientation: gtk::Orientation::Horizontal,
                                set_hexpand: true,
                                set_draw_value: false,
                                set_valign: gtk::Align::Center,
                                #[watch]
                                set_range: (0.0, model.mini.track_duration_ms.max(1000) as f64),
                                #[watch]
                                set_value: model.mini.position_ms as f64,
                            },
                            gtk::Label {
                                add_css_class: "caption",
                                add_css_class: "numeric",
                                #[watch]
                                set_label: &fmt_duration(model.mini.track_duration_ms),
                            },
                        },

                        gtk::CenterBox {
                            // On the left EQ + shuffle, in the center the transport buttons. The
                            // centered group is symmetric (back | play | next),
                            // so that play/back/next stay in the **absolute center**
                            // independently of EQ/shuffle/queue.
                            #[wrap(Some)]
                            set_start_widget = &gtk::Box {
                                set_spacing: 6,
                                set_valign: gtk::Align::Center,
                                #[name = "eq_btn"]
                                gtk::Button {
                                    set_icon_name: "multimedia-equalizer-symbolic",
                                    set_tooltip_text: Some(&gettext("Equalizer for this track")),
                                    set_valign: gtk::Align::Center,
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.mini.now_playing.is_some(),
                                    connect_clicked => Msg::OpenCurrentEq,
                                },
                                // Playback speed (0.25–2.0). Label shows the current
                                // rate; the popover holds the step slider. Hidden for
                                // live streams (not seekable).
                                gtk::MenuButton {
                                    set_valign: gtk::Align::Center,
                                    add_css_class: "flat",
                                    set_tooltip_text: Some(&gettext("Playback speed")),
                                    #[watch]
                                    set_label: &fmt_rate(model.mini.playback_rate),
                                    #[watch]
                                    set_visible: model.streaming.playing_stream.is_none(),
                                    #[watch]
                                    set_sensitive: model.mini.now_playing.is_some(),
                                    #[wrap(Some)]
                                    set_popover = &gtk::Popover {
                                        gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 8,
                                            set_margin_top: 10,
                                            set_margin_bottom: 10,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            gtk::Label {
                                                set_label: &gettext("Playback speed"),
                                                add_css_class: "dim-label",
                                                set_xalign: 0.0,
                                            },
                                            gtk::Scale {
                                                set_orientation: gtk::Orientation::Horizontal,
                                                set_width_request: 220,
                                                set_draw_value: true,
                                                set_value_pos: gtk::PositionType::Right,
                                                set_digits: 2,
                                                set_round_digits: 2,
                                                set_range: (0.25, 2.0),
                                                set_increments: (0.25, 0.25),
                                                // #[watch] snaps the thumb to the
                                                // rounded (0.25-step) model value.
                                                #[watch]
                                                set_value: model.mini.playback_rate,
                                                connect_value_changed[sender] => move |s| {
                                                    sender.input(Msg::Transport(TransportMsg::SetPlaybackRate(s.value())));
                                                },
                                            },
                                        }
                                    },
                                },
                                // Shuffle (only from 2 tracks); on the left near EQ, so that
                                // the transport center is not shifted.
                                gtk::ToggleButton {
                                    set_icon_name: "media-playlist-shuffle-symbolic",
                                    set_tooltip_text: Some(&gettext("Shuffle")),
                                    set_valign: gtk::Align::Center,
                                    add_css_class: "flat",
                                    #[watch]
                                    set_visible: model.transport.queue.len() >= 2,
                                    #[watch]
                                    set_sensitive: model.mini.now_playing.is_some(),
                                    #[watch]
                                    set_active: model.transport.shuffle,
                                    #[watch]
                                    set_opacity: if model.transport.shuffle { 1.0 } else { 0.4 },
                                    connect_clicked => Msg::Transport(TransportMsg::ToggleShuffle),
                                },
                                // Repeat (loop): at the end of the queue or of the
                                // single track, start over. Active = white, off = gray.
                                // Sits on the left next to shuffle.
                                gtk::ToggleButton {
                                    set_icon_name: "media-playlist-repeat-symbolic",
                                    set_tooltip_text: Some(&gettext("Repeat")),
                                    set_valign: gtk::Align::Center,
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.mini.now_playing.is_some(),
                                    #[watch]
                                    set_active: model.transport.repeat,
                                    #[watch]
                                    set_opacity: if model.transport.repeat { 1.0 } else { 0.4 },
                                    connect_clicked => Msg::Transport(TransportMsg::ToggleRepeat),
                                },
                            },
                            #[wrap(Some)]
                            set_center_widget = &gtk::Box {
                                set_spacing: 6,
                                gtk::Button {
                                    set_icon_name: "media-skip-backward-symbolic",
                                    set_tooltip_text: Some(&gettext("Back")),
                                    add_css_class: "flat",
                                    // Nothing selected → grayed out.
                                    #[watch]
                                    set_sensitive: model.mini.now_playing.is_some(),
                                    connect_clicked => Msg::Transport(TransportMsg::Prev),
                                },
                                gtk::Button {
                                    // Play/pause icon, or a spinner while a slow
                                    // source (Nextcloud/YouTube) resolves/buffers.
                                    #[wrap(Some)]
                                    set_child = &gtk::Stack {
                                        #[watch]
                                        set_visible_child_name: if model.mini.loading { "spinner" } else { "icon" },
                                        add_named[Some("icon")] = &gtk::Image {
                                            #[watch]
                                            set_icon_name: Some(if model.mini.playing {
                                                "media-playback-pause-symbolic"
                                            } else {
                                                "media-playback-start-symbolic"
                                            }),
                                        },
                                        add_named[Some("spinner")] = &gtk::Spinner {
                                            #[watch]
                                            set_spinning: model.mini.loading,
                                        },
                                    },
                                    set_tooltip_text: Some(&gettext("Play/Pause")),
                                    add_css_class: "circular",
                                    // Larger than the other transport buttons
                                    // (size via CSS class, see `init`).
                                    add_css_class: "emilia-bigplay",
                                    set_valign: gtk::Align::Center,
                                    // Enabled while something is loaded OR a queue
                                    // exists (so a freshly enqueued track can be
                                    // started without auto-playing on add).
                                    #[watch]
                                    set_sensitive: model.mini.now_playing.is_some()
                                        || !model.transport.queue.is_empty()
                                        || !model.transport.user_queue.is_empty(),
                                    connect_clicked => Msg::Transport(TransportMsg::TogglePlay),
                                },
                                // Shared record button, same size as play/pause
                                // (emilia-bigplay); blinks red while recording. On the
                                // Memo section it records a voice memo; in Streaming it
                                // toggles the timeshift recording of the running
                                // station. Shown only in those contexts.
                                gtk::Button {
                                    set_valign: gtk::Align::Center,
                                    #[watch]
                                    set_visible: model.record_btn_visible(),
                                    #[watch]
                                    set_icon_name: model.record_btn_icon(),
                                    #[watch]
                                    set_tooltip_text: Some(&model.record_btn_tooltip()),
                                    #[watch]
                                    set_css_classes: if model.record_btn_recording() {
                                        &["circular", "emilia-bigplay", "emilia-record-dot", "emilia-recording"]
                                    } else {
                                        // Red even when idle; only pulses while recording.
                                        &["circular", "emilia-bigplay", "emilia-record-dot"]
                                    },
                                    connect_clicked => Msg::Stream(StreamMsg::RecordToggle),
                                },
                                gtk::Button {
                                    set_icon_name: "media-skip-forward-symbolic",
                                    set_tooltip_text: Some(&gettext("Forward")),
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.mini.now_playing.is_some(),
                                    connect_clicked => Msg::Transport(TransportMsg::Next),
                                },
                            },
                            // Bottom right: lyrics, the album shortcut and the queue.
                            // (Repeat moved to the left, next to shuffle.)
                            #[wrap(Some)]
                            set_end_widget = &gtk::Box {
                                set_spacing: 6,
                                set_valign: gtk::Align::Center,
                                // Lyrics: shown whenever the running track has any
                                // lyrics (embedded/plain or online). Opens the view;
                                // synchronized (.lrc) lyrics additionally highlight
                                // and auto-scroll the current line.
                                gtk::Button {
                                    set_icon_name: "media-view-subtitles-symbolic",
                                    set_tooltip_text: Some(&gettext("Lyrics")),
                                    set_valign: gtk::Align::Center,
                                    add_css_class: "flat",
                                    #[watch]
                                    set_visible: model.lyrics.current.as_ref()
                                        .is_some_and(|l| l.has_any()),
                                    connect_clicked => Msg::Lyrics(LyricsMsg::ShowLyrics),
                                },
                                // Album shortcut: only while a local album track
                                // plays. Opens the album's song page (back returns).
                                gtk::Button {
                                    set_icon_name: "media-optical-symbolic",
                                    set_tooltip_text: Some(&gettext("Show album")),
                                    set_valign: gtk::Align::Center,
                                    add_css_class: "flat",
                                    #[watch]
                                    set_visible: model.mini.current_album.is_some(),
                                    connect_clicked => Msg::Transport(TransportMsg::ShowCurrentAlbum),
                                },
                                gtk::Button {
                                    set_icon_name: "list-high-priority-symbolic",
                                    set_tooltip_text: Some(&gettext("Queue")),
                                    set_valign: gtk::Align::Center,
                                    add_css_class: "flat",
                                    // Greyed out while the queue is empty (the
                                    // queue view shows the user queue).
                                    #[watch]
                                    set_sensitive: !model.transport.user_queue.is_empty(),
                                    connect_clicked => Msg::Transport(TransportMsg::ShowQueue),
                                },
                            },
                        },
                    },
                },
                }
                }
            }
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Register the bundled app icons and the application-wide CSS.
        Self::install_styles();

        // The main app cannot run without its on-disk library (an in-memory
        // fallback would silently hide the user's real data). On failure, log a
        // diagnostic with the path and exit cleanly instead of panicking.
        let library = Library::open().unwrap_or_else(|e| {
            let path = crate::core::db::db_path();
            tracing::error!(
                "could not open the library database at {}: {e}",
                path.display()
            );
            eprintln!(
                "Emilia: could not open the library database at {}: {e}",
                path.display()
            );
            std::process::exit(1);
        });
        // Move any existing plaintext secrets (API keys, Nextcloud credentials)
        // into the Secret Service once, before they are read below.
        library.migrate_secrets();
        let player = Player::new().expect("Failed to initialize GStreamer");
        // Apply the color scheme (fresh-install default: dark) immediately.
        apply_color_scheme(
            library
                .get_setting("color_scheme")
                .ok()
                .flatten()
                .as_deref()
                .unwrap_or("dark"),
        );
        // All persisted startup settings, read in one place (see
        // `App::read_init_state`) and destructured back into locals so the model
        // literal below stays unchanged.
        let InitState {
            music_dir,
            root_dir,
            browse_dir,
            sources,
            first_run,
            saved_w,
            saved_h,
            saved_max,
            concert_hint_dismissed,
            hidden_sections,
            youtube_enabled,
            section_order,
            auto_enrich,
            repeat_on,
            ui_language,
            sort,
            no_group,
            gallery_view,
            section_gallery,
            gallery_columns,
            recording_buffer_minutes,
            saved_section,
        } = Self::read_init_state(&library);

        // Runtime theming (scaling + design) and tray prefs. Plain DB reads, so
        // they sit here next to the model build rather than in `InitState`.
        // Takes `lib` as a param so it captures nothing — `library` is moved into
        // the model literal further down.
        let setting_on = |lib: &Library, key: &str| {
            matches!(lib.get_setting(key).ok().flatten().as_deref(), Some("1"))
        };
        let ui_scale = library
            .get_setting("ui_scale")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(1.0)
            .clamp(0.5, 1.5);
        // Appearance (background + colours) is stored per theme; this reads the
        // current theme's values (see `read_design_settings`).
        let design = read_design_settings(&library);
        let theme = crate::ui::theme::ThemeState::new(ui_scale, design);
        let tray = TrayState {
            enabled: setting_on(&library, "tray_enabled"),
            close_hides: setting_on(&library, "tray_close_hides"),
            start_hidden: setting_on(&library, "tray_start_hidden"),
            skip_taskbar: setting_on(&library, "tray_skip_taskbar"),
            icon_gray: setting_on(&library, "tray_icon_gray"),
            handle: None,
            hold: None,
        };

        let entries = FactoryVecDeque::builder()
            .launch(gtk::ListBox::default())
            .forward(sender.input_sender(), |out| match out {
                FsOutput::Activated(index) => Msg::Activate(index.current_index()),
                FsOutput::LongPress(index) => Msg::ShowContextMenu(index.current_index()),
                FsOutput::DoubleClick(index) => Msg::ToggleQueue(index.current_index()),
                FsOutput::PlayDir(index) => Msg::PlayFsAlbum(index.current_index()),
            });

        // The library overviews are virtualised (see `card_list`): they grow with
        // the whole library, so a widget per entry would cost seconds on the
        // phone. Their rows report a *position* rather than a factory index.
        // `play` is the row's play button; `None` for the lists whose rows are
        // only opened (artists), whose rows then carry no play button at all.
        let card_list = |icon: &str,
                         activate: fn(usize) -> Msg,
                         context: fn(usize) -> Msg,
                         play: Option<fn(usize) -> Msg>|
         -> CardList {
            let (a, c, p) = (
                sender.input_sender().clone(),
                sender.input_sender().clone(),
                sender.input_sender().clone(),
            );
            CardList::new(
                icon,
                move |i| a.emit(activate(i)),
                move |i| c.emit(context(i)),
                move |i| {
                    if let Some(play) = play {
                        p.emit(play(i));
                    }
                },
            )
        };

        let albums = card_list(
            "media-optical-symbolic",
            Msg::ShowAlbumTracks,
            Msg::ShowAlbumDetail,
            Some(Msg::PlayAlbumAt),
        );
        let singles = card_list(
            "media-optical-symbolic",
            Msg::ShowSingleTracks,
            Msg::ShowSingleDetail,
            Some(Msg::PlaySingleAt),
        );
        let compilations = card_list(
            "media-optical-symbolic",
            Msg::ShowCompilationTracks,
            Msg::ShowCompilationDetail,
            Some(Msg::PlayCompilationAt),
        );
        let artists = card_list(
            "avatar-default-symbolic",
            Msg::OpenArtistTracks,
            Msg::ShowArtistDetail,
            None,
        );

        let acoustid_key = library.get_secret_setting("acoustid_key").ok().flatten();
        let fanart_key = library.get_secret_setting("fanart_key").ok().flatten();
        let active_output = crate::core::output::default_output().unwrap_or_default();

        // At the end of a track, automatically play the next entry of the queue;
        // report title tags (for stations: the running ICY title) as `StreamTitle`.
        {
            let sender = sender.clone();
            player.connect_bus_events(
                {
                    let sender = sender.clone();
                    move || sender.input(Msg::Transport(TransportMsg::TrackFinished))
                },
                {
                    let sender = sender.clone();
                    move |title| sender.input(Msg::Stream(StreamMsg::StreamTitle(title)))
                },
                {
                    let sender = sender.clone();
                    move || sender.input(Msg::Transport(TransportMsg::PlaybackError))
                },
                {
                    let sender = sender.clone();
                    move || sender.input(Msg::Transport(TransportMsg::PlaybackReady))
                },
                move || sender.input(Msg::Transport(TransportMsg::GaplessAdvanced)),
            );
        }

        // Gates the per-second tick AND the resume-persist timer: both only need
        // to run while playing/recording. While idle they still fire but deliver
        // nothing, so there is no periodic view re-render / overlay re-measure.
        // `sync_tick_active` keeps the flag current after every message.
        let tick_active = std::rc::Rc::new(std::cell::Cell::new(false));

        // During playback, regularly save the resume position, so that
        // an audio drama also resumes there after a crash/close.
        {
            let sender = sender.clone();
            let tick_active = tick_active.clone();
            gtk::glib::timeout_add_seconds_local(5, move || {
                if tick_active.get() {
                    sender.input(Msg::Transport(TransportMsg::PersistResume));
                }
                gtk::glib::ControlFlow::Continue
            });
        }

        // Per-second tick for the seek bar (position/duration), gated by
        // `tick_active` like the resume-persist timer above.
        {
            let sender = sender.clone();
            let tick_active = tick_active.clone();
            gtk::glib::timeout_add_seconds_local(1, move || {
                if tick_active.get() {
                    sender.input(Msg::Transport(TransportMsg::Tick));
                }
                gtk::glib::ControlFlow::Continue
            });
        }

        // Quiet background backfill: gradually fills in missing artist photos
        // (first) and online covers, without user action – so that even without a new
        // scan (returning users, no signal on the first run, failed
        // individual fetches) the overview gets enriched. The worker is rate-limited
        // and skips already loaded/permanently unsuccessful items; if nothing is pending,
        // the tick fizzles out almost for free (no network, no UI update).
        {
            let sender = sender.clone();
            gtk::glib::timeout_add_seconds_local(AUTO_ENRICH_INTERVAL_SECS, move || {
                sender.input(Msg::AutoEnrichTick);
                gtk::glib::ControlFlow::Continue
            });
        }

        // Check reachability of the Nextcloud sources once at startup and then
        // regularly (controls the red "Disconnected" hint).
        {
            let sender = sender.clone();
            sender.input(Msg::Source(crate::ui::app_views_sources::SourceMsg::Check));
            gtk::glib::timeout_add_seconds_local(45, move || {
                sender.input(Msg::Source(crate::ui::app_views_sources::SourceMsg::Check));
                gtk::glib::ControlFlow::Continue
            });
        }

        // Keep the managed yt-dlp fresh hands-off: check once at startup and then
        // every 12 h. The handler is a no-op unless YouTube is on and the copy is
        // actually stale (so it costs nothing on most ticks).
        {
            let sender = sender.clone();
            sender.input(Msg::Yt(YtMsg::YtDlpAutoUpdate));
            gtk::glib::timeout_add_seconds_local(12 * 60 * 60, move || {
                sender.input(Msg::Yt(YtMsg::YtDlpAutoUpdate));
                gtk::glib::ControlFlow::Continue
            });
        }

        // Start the MPRIS service: commands from the lock screen/from media keys
        // are fed into the component as Msg::Mpris.
        let mpris = crate::core::mpris::Mpris::start({
            let sender = sender.clone();
            move |cmd| sender.input(Msg::Mpris(cmd))
        });

        let toast_overlay = adw::ToastOverlay::new();
        let concerts_list = gtk::ListBox::new();
        let playlists_list = gtk::ListBox::new();
        let memos_list = gtk::ListBox::new();
        // Recording level meter (player bar); its draw func is wired in MemoState.
        let rec_meter = gtk::DrawingArea::new();
        let favorites_list = gtk::ListBox::new();
        let audiobooks_list = gtk::ListBox::new();
        let queue_list = gtk::ListBox::new();
        let stats_page = crate::ui::stats_page::StatsPage::builder()
            .launch(())
            .detach();
        let sync_page = crate::ui::sync_page::SyncPage::builder()
            .launch(())
            .forward(sender.input_sender(), |out| match out {
                crate::ui::sync_page::SyncOutput::ConnectedChanged(b) => Msg::SyncConnected(b),
                crate::ui::sync_page::SyncOutput::Imported => Msg::SyncImported,
            });
        let cloud_page = crate::ui::cloud_page::CloudPage::builder()
            .launch(())
            .forward(sender.input_sender(), |out| match out {
                crate::ui::cloud_page::CloudOutput::SourcesChanged(id) => {
                    Msg::Source(crate::ui::app_views_sources::SourceMsg::Added(id))
                }
                crate::ui::cloud_page::CloudOutput::Indexed => {
                    Msg::Source(crate::ui::app_views_sources::SourceMsg::CloudIndexed)
                }
            });
        // Shared hand-off slots for the title-bar sort control of each component
        // page (filled by the component, read by `apply_component_sort`).
        let podcast_sort: crate::ui::app_sort::SortSlot = Default::default();
        let stream_sort: crate::ui::app_sort::SortSlot = Default::default();
        let yt_sort: crate::ui::app_sort::SortSlot = Default::default();
        // Shared hand-off slot for episode subpages built by the component (its
        // `!Send` widget can't ride on the parent's `Send` `Msg`).
        let podcast_subpage: std::rc::Rc<std::cell::RefCell<Option<(String, gtk::Box)>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let podcasts_page = crate::ui::podcasts_page::PodcastsPage::builder()
            .launch((podcast_subpage.clone(), podcast_sort.clone()))
            .forward(sender.input_sender(), |out| {
                use crate::ui::podcasts_page::PodcastsOutput as O;
                match out {
                    O::ToggleEpisode { url, title } => {
                        Msg::Podcast(PodcastMsg::ToggleEpisode { url, title })
                    }
                    O::EpisodeSeekTo { url, title, ms } => {
                        Msg::Podcast(PodcastMsg::EpisodeSeekTo { url, title, ms })
                    }
                    O::OpenPodcastEqualizer(id) => Msg::Podcast(PodcastMsg::OpenPodcastEq(id)),
                    O::OpenEpisodeEqualizer { url, title } => {
                        Msg::Podcast(PodcastMsg::OpenEpisodeEq { url, title })
                    }
                    O::PushSubpage => Msg::Podcast(PodcastMsg::PushPodcastSubpage),
                    O::Share(sel) => Msg::Ctx(CtxMsg::ShareItems(sel)),
                    O::Toast(s) => Msg::Podcast(PodcastMsg::PodcastToast(s)),
                    O::DeletedUndoToast(id) => Msg::Podcast(PodcastMsg::PodcastUndoToast(id)),
                    O::RefreshStarted(b) => Msg::Podcast(PodcastMsg::PodcastRefreshStarted(b)),
                    O::RefreshFinished => Msg::Podcast(PodcastMsg::PodcastRefreshFinished),
                    O::RefreshProgress { done, total, label } => {
                        Msg::RefreshProgress { done, total, label }
                    }
                    O::RefreshSummary(s) => Msg::RefreshSummary(s),
                    O::SortChanged => Msg::Sort(crate::ui::app_sort::SortMsg::PodcastChanged),
                }
            });
        let yt_subpage: std::rc::Rc<std::cell::RefCell<Option<(String, gtk::Box)>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let yt_page = crate::ui::yt_page::YtPage::builder()
            .launch((yt_subpage.clone(), yt_sort.clone()))
            .forward(sender.input_sender(), |out| {
                use crate::ui::yt_page::YtOutput as O;
                match out {
                    O::PlayVideo { video_id, title } => {
                        Msg::Yt(YtMsg::YtPlayVideo { video_id, title })
                    }
                    O::PlayVideoAt {
                        video_id,
                        title,
                        ms,
                    } => Msg::Yt(YtMsg::YtPlayVideoAt {
                        video_id,
                        title,
                        ms,
                    }),
                    O::PlayChannel(id) => Msg::Yt(YtMsg::YtPlayChannel(id)),
                    O::StartPlaylist { url, title } => {
                        Msg::Yt(YtMsg::YtStartPlaylist { url, title })
                    }
                    O::StartPlaylistAt {
                        url,
                        title,
                        index,
                        close,
                        videos,
                    } => Msg::Yt(YtMsg::YtStartPlaylistAt {
                        url,
                        title,
                        index,
                        close,
                        videos,
                    }),
                    O::OpenTrackEq { path, title } => Msg::OpenTrackEq { path, title },
                    O::OpenPlaylist { id, name } => Msg::Yt(YtMsg::YtOpenPlaylist { id, name }),
                    O::OpenSettings => Msg::OpenSettings,
                    O::Toast(s) => Msg::Yt(YtMsg::YtToast(s)),
                    O::Progress(s) => Msg::Yt(YtMsg::YtProgress(s)),
                    O::ProgressDone(s) => Msg::Yt(YtMsg::YtProgressDone(s)),
                    O::SetLoading(o) => Msg::Yt(YtMsg::YtSetLoading(o)),
                    O::LibraryChanged => Msg::Yt(YtMsg::YtLibraryChanged),
                    O::PlaylistsChanged => Msg::Yt(YtMsg::YtPlaylistsChanged),
                    O::PushSubpage => Msg::Yt(YtMsg::PushYtSubpage),
                    O::DeleteChannelUndo(id) => Msg::Yt(YtMsg::YtChannelUndo(id)),
                    O::RefreshStarted(b) => Msg::Yt(YtMsg::YtRefreshStarted(b)),
                    O::RefreshFinished => Msg::Yt(YtMsg::YtRefreshFinished),
                    O::RefreshProgress { done, total, label } => {
                        Msg::RefreshProgress { done, total, label }
                    }
                    O::RefreshSummary(s) => Msg::RefreshSummary(s),
                    O::Share(sel) => Msg::Ctx(CtxMsg::ShareItems(Box::new(sel))),
                    O::SortChanged => Msg::Sort(crate::ui::app_sort::SortMsg::YtChanged),
                }
            });
        let stream_page = crate::ui::stream_page::StreamPage::builder()
            .launch(stream_sort.clone())
            .forward(sender.input_sender(), |out| {
                use crate::ui::stream_page::StreamOutput as O;
                match out {
                    O::ToggleStream(id) => Msg::Stream(StreamMsg::ToggleStream(id)),
                    O::PlayRecording(path) => Msg::Stream(StreamMsg::PlayRecording(path)),
                    O::OpenReplay(id) => Msg::Stream(StreamMsg::OpenStreamReplay(id)),
                    O::OpenEqualizer(id) => Msg::Stream(StreamMsg::OpenStreamEq(id)),
                    O::EditRecording(id) => Msg::Edit(EditMsg::EditRecording(id)),
                    O::StreamDeleteUndo(id) => Msg::Stream(StreamMsg::StreamDeleteUndo(id)),
                    O::RecordingDeleteUndo(id) => Msg::Stream(StreamMsg::RecordingDeleteUndo(id)),
                    O::LibraryChanged => Msg::Stream(StreamMsg::StreamLibraryChanged),
                    O::PlayHeard { artist, title } => {
                        Msg::Stream(StreamMsg::PlayHeard { artist, title })
                    }
                    O::DownloadHeard { artist, title } => {
                        Msg::Stream(StreamMsg::DownloadHeard { artist, title })
                    }
                    O::Share(sel) => Msg::Ctx(CtxMsg::ShareItems(sel)),
                    O::Toast(s) => Msg::Stream(StreamMsg::StreamToast(s)),
                    O::SortChanged => Msg::Sort(crate::ui::app_sort::SortMsg::StreamChanged),
                }
            });
        let setup_page = crate::ui::setup::SetupPage::builder().launch(()).forward(
            sender.input_sender(),
            |out| match out {
                crate::ui::setup::SetupOutput::Finished {
                    lang_code,
                    music_dir,
                    enabled_sections,
                } => Msg::Setting(SettingMsg::SetupFinished {
                    lang_code,
                    music_dir,
                    enabled_sections,
                }),
            },
        );

        // Gapless / crossfade preferences (read before `library` is moved into
        // the model). Gapless defaults on; crossfade defaults off (0 s).
        let gapless = !matches!(
            library.get_setting("gapless").ok().flatten().as_deref(),
            Some("0")
        );
        let crossfade_secs = library
            .get_setting("crossfade_secs")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0)
            .clamp(0.0, 12.0);
        // Apply to the player up front so the very first track honors them.
        player.set_gapless(gapless);
        player.set_crossfade_secs(crossfade_secs);

        let mut model = App {
            tick_active,
            last_stream_icon_state: std::cell::RefCell::new(None),
            library,
            player,
            mpris,
            input: sender.input_sender().clone(),
            mcp: McpState::new(),
            libview: LibView {
                entries,
                albums,
                albums_gallery: gtk::FlowBox::new(),
                albums_gallery_box: gtk::Box::new(gtk::Orientation::Vertical, 6),
                album_headers: std::rc::Rc::new(std::cell::RefCell::new(None)),
                albums_overview: Vec::new(),
                album_count: 0,
                singles,
                singles_gallery: gtk::FlowBox::new(),
                singles_gallery_box: gtk::Box::new(gtk::Orientation::Vertical, 6),
                single_headers: std::rc::Rc::new(std::cell::RefCell::new(None)),
                singles_overview: Vec::new(),
                single_count: 0,
                compilations,
                compilations_gallery: gtk::FlowBox::new(),
                compilations_gallery_box: gtk::Box::new(gtk::Orientation::Vertical, 6),
                compilation_headers: std::rc::Rc::new(std::cell::RefCell::new(None)),
                compilations_overview: Vec::new(),
                compilation_count: 0,
                artists,
                artists_gallery: gtk::FlowBox::new(),
                artists_gallery_box: gtk::Box::new(gtk::Orientation::Vertical, 6),
                artist_headers: std::rc::Rc::new(std::cell::RefCell::new(None)),
                artists_overview: Vec::new(),
                artist_count: 0,
                concert_headers: std::rc::Rc::new(std::cell::RefCell::new(None)),
                audiobook_headers: std::rc::Rc::new(std::cell::RefCell::new(None)),
                favorite_headers: std::rc::Rc::new(std::cell::RefCell::new(None)),
                playlist_headers: std::rc::Rc::new(std::cell::RefCell::new(None)),
                memo_headers: std::rc::Rc::new(std::cell::RefCell::new(None)),
                files_headers: std::rc::Rc::new(std::cell::RefCell::new(None)),
                sort,
                no_group,
                gallery_view,
                section_gallery,
                gallery_columns,
                loading: false,
                loading_label: None,
                gallery_tried: std::cell::RefCell::new(std::collections::HashSet::new()),
                album_page: std::rc::Rc::new(std::cell::RefCell::new(None)),
                missing_busy: None,
                page_marks: Default::default(),
            },
            refresh_pending: 0,
            refresh_progress: None,
            refresh_summary: None,
            scanning: false,
            scan_done: 0,
            scan_total: 0,
            scan_bytes: 0,
            scan_total_bytes: 0,
            scan_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            enrich_state: EnrichState {
                enriching: false,
                auto_enrich,
                enrich_cancel: Arc::new(AtomicBool::new(false)),
                acoustid_key,
                fanart_key,
            },
            settings: Settings {
                ui_language,
                active_output,
                gapless,
                crossfade_secs,
            },
            files: FilesState {
                music_dir,
                root_dir,
                browse_dir,
                shown_dir: None,
                fs_scroll: std::rc::Rc::new(std::cell::RefCell::new(
                    std::collections::HashMap::new(),
                )),
                sources,
                active_source: ActiveSource::Primary,
                source_tabs: gtk::Box::new(gtk::Orientation::Horizontal, 0),
                source_tab_buttons: Vec::new(),
                remote_browse: None,
                remote_error: None,
                remote_queue: Vec::new(),
                remote_pos: 0,
                playing_remote: false,
            },
            transport: TransportState {
                queue: Vec::new(),
                queue_pos: 0,
                user_queue: Vec::new(),
                shuffle: false,
                shuffle_order: Vec::new(),
                shuffle_idx: 0,
                repeat: repeat_on,
                play_history: Vec::new(),
                skip_history_push: false,
                interrupted_queue: None,
                nav_stack: Vec::new(),
                prev_ctx: None,
                playing_path: None,
                close_resume: std::rc::Rc::new(std::cell::RefCell::new(None)),
                next_source: None,
                play_session: None,
                close_session: std::rc::Rc::new(std::cell::RefCell::new(None)),
                queue_list: queue_list.clone(),
                queue_marks: Default::default(),
                skip_count: 0,
                forced_start_ms: None,
            },
            mini: MiniState {
                now_playing: None,
                current_album: None,
                playing: false,
                loading: false,
                position_ms: 0,
                track_duration_ms: 0,
                playback_rate: 1.0,
                seek_scale: gtk::Scale::default(),
                chapter_label: gtk::Label::default(),
                chapters: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
                hovering_seek: std::rc::Rc::new(std::cell::Cell::new(false)),
            },
            sleep: SleepState::default(),
            lyrics: LyricsState {
                current: None,
                for_path: None,
                view: None,
                file_pending: std::rc::Rc::new(std::cell::RefCell::new(None)),
            },
            toast_overlay: toast_overlay.clone(),
            concerts: ConcertsState {
                concert_items: Vec::new(),
                concert_marks: Default::default(),
                concerts_list: concerts_list.clone(),
                concerts_gallery: gtk::FlowBox::new(),
                concerts_gallery_box: gtk::Box::new(gtk::Orientation::Vertical, 6),
                concert_hint_dismissed,
            },
            favorites: FavoritesState {
                favorite_items: Vec::new(),
                favorite_marks: Default::default(),
                favorites_list: favorites_list.clone(),
                favorites_gallery: gtk::FlowBox::new(),
                favorites_gallery_box: gtk::Box::new(gtk::Orientation::Vertical, 6),
                audiobook_items: Vec::new(),
                audiobook_marks: Default::default(),
                audiobooks_list: audiobooks_list.clone(),
                audiobooks_gallery: gtk::FlowBox::new(),
                audiobooks_gallery_box: gtk::Box::new(gtk::Orientation::Vertical, 6),
            },
            playlists: PlaylistsState {
                playlist_items: Vec::new(),
                playlists_list: playlists_list.clone(),
                playlists_gallery: gtk::FlowBox::new(),
                playlists_gallery_box: gtk::Box::new(gtk::Orientation::Vertical, 6),
            },
            podcasts: PodcastsState {
                playing_episode_url: None,
            },
            streaming: StreamingState {
                playing_stream: None,
                stream_title: None,
                recorder: None,
                record_state: None,
                recording_buffer_minutes,
                resolve_busy: None,
            },
            memo: crate::ui::app_memo::MemoState::new(memos_list.clone(), rec_meter.clone()),
            youtube: YoutubeState {
                enabled: youtube_enabled,
                ytdlp_version: None,
                settings_status: std::rc::Rc::new(std::cell::RefCell::new(None)),
                settings_dl_btn: std::rc::Rc::new(std::cell::RefCell::new(None)),
                ytdlp_busy: false,
                playing_video_id: None,
                video_titles: std::collections::HashMap::new(),
                playing_playlist: false,
                pending_seek: None,
                progress_toast: std::rc::Rc::new(std::cell::RefCell::new(None)),
            },
            offline_sources: std::collections::HashSet::new(),
            stats_page,
            nav: NavState {
                split: adw::OverlaySplitView::new(),
                view_stack: adw::ViewStack::new(),
                sort_btn: gtk::MenuButton::new(),
                podcast_sort,
                stream_sort,
                yt_sort,
                nav_view: adw::NavigationView::new(),
                sidebar_nav: gtk::Box::new(gtk::Orientation::Vertical, 0),
                top_nav: gtk::Box::new(gtk::Orientation::Horizontal, 0),
                nav_buttons: Vec::new(),
                section_order,
                hidden_sections,
                context_target: None,
                ctx_play: std::rc::Rc::new(std::cell::RefCell::new(None)),
                ctx_dialog: std::rc::Rc::new(std::cell::RefCell::new(None)),
                overview_scroll: std::rc::Rc::new(std::cell::RefCell::new(None)),
                narrow: std::rc::Rc::new(std::cell::Cell::new(false)),
                nav_hidden: std::rc::Rc::new(std::cell::Cell::new(false)),
                apply_chrome: std::rc::Rc::new(|| {}),
            },
            sync_page,
            sync_connected: false,
            cloud_page,
            podcasts_page,
            podcast_subpage,
            yt_page,
            yt_subpage,
            stream_page,
            setup_page,
            theme,
            tray,
            media_popup: None,
        };

        // Restore the queue from last time (only still existing
        // files). It is **not** played automatically – the track sits
        // ready in the mini player and starts when "Play" is pressed.
        let saved_pos: usize = model
            .library
            .get_setting("queue_pos")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let raw_queue: Vec<PathBuf> = model
            .library
            .get_setting("queue_paths")
            .ok()
            .flatten()
            .map(|s| {
                s.split('\n')
                    .filter(|l| !l.is_empty())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        let mut q = Vec::new();
        let mut q_pos = 0usize;
        for (i, p) in raw_queue.iter().enumerate() {
            if p.exists() {
                if i <= saved_pos {
                    q_pos = q.len();
                }
                q.push(p.clone());
            }
        }
        if !q.is_empty() {
            q_pos = q_pos.min(q.len() - 1);
            model.mini.now_playing = Some(model.display_name(&q[q_pos]));
            model.transport.queue = q;
            model.transport.queue_pos = q_pos;
        }

        // Restore the explicit user queue ("Add to queue"). Streamable remote
        // entries (YouTube `yt:` / Nextcloud `nc:`) have no local file but are
        // still playable, so they are kept alongside existing local files.
        model.transport.user_queue = model
            .library
            .get_setting("user_queue_paths")
            .ok()
            .flatten()
            .map(|s| {
                s.split('\n')
                    .filter(|l| !l.is_empty())
                    .map(PathBuf::from)
                    .filter(|p| {
                        let s = p.to_string_lossy();
                        p.exists()
                            || crate::core::youtube::parse_yt_path(&s).is_some()
                            || crate::core::webdav::parse_nc_path(&s).is_some()
                    })
                    .collect()
            })
            .unwrap_or_default();

        // With no primary music folder configured the "Music" tab is dropped, so
        // a stale Primary selection is moved to the first real source (which then
        // becomes the lone, tab-less folder). `apply_source` re-roots and loads.
        match model.active_source_fallback() {
            Some(s) => model.apply_source(s, &sender),
            None => model.load_dir(&sender),
        }
        model.reload_library_overviews();
        model.load_concerts(&sender);
        model.load_favorites(&sender);
        model.load_audiobooks(&sender);
        model.reload_playlists(&sender);
        model.startup_configure(&root, &sender);

        let entries_box = model.libview.entries.widget();
        let albums_box = model.libview.albums.widget();
        let singles_box = model.libview.singles.widget();
        let compilations_box = model.libview.compilations.widget();
        let artists_box = model.libview.artists.widget();
        let albums_gallery_box = model.libview.albums_gallery_box.clone();
        let singles_gallery_box = model.libview.singles_gallery_box.clone();
        let compilations_gallery_box = model.libview.compilations_gallery_box.clone();
        let artists_gallery_box = model.libview.artists_gallery_box.clone();
        let concerts_gallery_box = model.concerts.concerts_gallery_box.clone();
        let audiobooks_gallery_box = model.favorites.audiobooks_gallery_box.clone();
        let favorites_gallery_box = model.favorites.favorites_gallery_box.clone();
        let playlists_gallery_box = model.playlists.playlists_gallery_box.clone();
        let widgets = view_output!();
        model.finish_init(
            &widgets,
            &root,
            &sender,
            saved_w,
            saved_h,
            saved_max,
            saved_section,
        );
        // On the very first launch, present the setup assistant once the main
        // window is shown (relm4 maps it only after `init` returns, so defer the
        // dialog to the next main-loop iteration).
        if first_run {
            let setup_sender = model.setup_page.sender().clone();
            let win = root.clone();
            gtk::glib::idle_add_local_once(move || {
                setup_sender.emit(crate::ui::setup::SetupInput::Open(win));
            });
        }
        // Start the embedded MCP server if enabled (reads the persisted mode).
        model.start_mcp_if_enabled();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, root: &Self::Root) {
        match msg {
            Msg::Activate(index) => self.on_activate(index, &sender),
            Msg::ToggleQueue(index) => self.on_toggle_queue(index),
            Msg::ShowContextMenu(index) => self.on_show_context_menu(index, root, &sender),
            Msg::ShowArtistDetail(index) => self.on_show_artist_detail(index, root, &sender),
            Msg::ShowAlbumDetail(index) => self.on_show_album_detail(index, root, &sender),
            Msg::ShowAlbumDetailFor { artist, album } => {
                self.on_show_album_detail_for(artist, album, root, &sender)
            }
            Msg::ShowTrackDetail(path) => {
                self.nav.context_target = Some(CtxTarget::Fs(FsEntry::file(PathBuf::from(path))));
                self.open_context_menu(root, &sender);
            }
            Msg::ShowAlbumTracks(index) => self.on_show_album_tracks(index, &sender),
            Msg::PlayAlbumAt(index) => self.on_play_album_at("albums", index),
            Msg::PlaySingleAt(index) => self.on_play_album_at("singles", index),
            Msg::PlayCompilationAt(index) => self.on_play_album_at("compilations", index),
            Msg::ShowSingleTracks(index) => self.on_show_single_tracks(index, &sender),
            Msg::ShowSingleDetail(index) => self.on_show_single_detail(index, root, &sender),
            Msg::ShowCompilationTracks(index) => self.on_show_compilation_tracks(index, &sender),
            Msg::ShowCompilationDetail(index) => {
                self.on_show_compilation_detail(index, root, &sender)
            }
            Msg::OpenArtistTracks(index) => self.on_open_artist_tracks(index, &sender),
            Msg::OpenAlbumTracks { artist, album } => {
                self.fetch_focus_album(&sender, &artist, &album);
                self.open_album_tracks(&sender, &artist, &album);
            }
            Msg::ShowMissingTrack {
                artist,
                album,
                disc,
                position,
                title,
            } => self.show_missing_track(root, &sender, artist, album, disc, position, title),
            Msg::AddMissingTrack {
                artist,
                album,
                disc,
                position,
                title,
            } => self.add_missing_track(root, &sender, artist, album, disc, position, title),
            Msg::DownloadMissingTrack {
                artist,
                album,
                disc,
                position,
                title,
                video_id,
            } => self.download_missing_track(
                root, &sender, artist, album, disc, position, title, video_id,
            ),
            Msg::OpenEntryTracks { scope, key } => match scope.as_str() {
                "album" => {
                    // key = "Artist\u{1}Album"
                    let mut parts = key.splitn(2, '\u{1}');
                    let artist = parts.next().unwrap_or("").to_string();
                    let album = parts.next().unwrap_or("").to_string();
                    self.open_album_tracks(&sender, &artist, &album);
                }
                "folder" => self.open_folder_tracks(&sender, &key),
                _ => {}
            },
            Msg::PlayFolderTrack {
                folder,
                path,
                close,
            } => self.on_play_folder_track(folder, path, close),
            Msg::PlayArtistTrack { name, path, close } => {
                self.on_play_artist_track(name, path, close)
            }
            Msg::PlayOneTrack { path, close } => self.on_play_one_track(path, close),
            Msg::PlayAlbum { artist, album } => self.on_play_album(artist, album),
            Msg::PlayFsAlbum(idx) => {
                // The play button on an album folder in the file browser.
                let info = self
                    .libview
                    .entries
                    .guard()
                    .get(idx)
                    .and_then(|r| r.entry.album().cloned());
                if let Some(a) = info {
                    sender.input(Msg::PlayAlbum {
                        artist: a.artist,
                        album: a.album,
                    });
                }
            }
            Msg::Playlist(m) => self.update_playlist(m, root, &sender),
            // --- Voice memos ---
            Msg::Memo(m) => self.update_memo(m, root, &sender),
            Msg::RefreshProgress { done, total, label } => {
                self.refresh_summary = None;
                self.refresh_progress = Some((done, total, label));
            }
            Msg::RefreshSummary(text) => {
                self.refresh_progress = None;
                self.refresh_summary = Some(text);
                // Hold the outcome in the overlay just long enough to read it,
                // then let the view go back to the content.
                let input = sender.input_sender().clone();
                gtk::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(2600),
                    move || {
                        let _ = input.send(Msg::ClearRefreshSummary);
                    },
                );
            }
            Msg::ClearRefreshSummary => self.refresh_summary = None,
            Msg::OpenSync => {
                use crate::ui::sync_page::SyncInput;
                self.sync_page.emit(SyncInput::Open(root.clone()));
            }
            Msg::SyncConnected(connected) => self.sync_connected = connected,
            Msg::SyncImported => {
                self.load_favorites(&sender);
                self.reload_playlists(&sender);
                self.podcasts_page
                    .emit(crate::ui::podcasts_page::PodcastsInput::Reload);
                // Received audio files were indexed into the `track` table as they
                // arrived → rebuild the artist/album overviews so they show up.
                self.reload_library_overviews();
            }
            Msg::AutoEnrichTick => self.on_auto_enrich_tick(&sender),
            Msg::FingerprintCurrent(path) => self.fetch_focus_track(&sender, &path),
            Msg::Mpris(cmd) => self.handle_mpris(root, cmd),
            Msg::Mcp(cmd) => self.handle_mcp(cmd),
            Msg::McpSetting(m) => self.update_mcp_setting(m),
            Msg::NavUp => self.on_nav_up(&sender),
            Msg::FilesGoStart => self.on_files_go_start(&sender),
            Msg::Refresh => self.on_refresh(&sender),
            Msg::ScanCancel => {
                self.scan_cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Msg::OpenSettings => self.open_settings(root, &sender),
            Msg::SetSleepTimer(choice) => self.on_set_sleep_timer(choice),
            Msg::OpenSearch => self.open_search_dialog(root, &sender),
            Msg::SearchPlayTrack(path) => self.on_search_play_track(path, &sender),
            Msg::SearchOpenAlbum(album) => self.open_album_by_name(&sender, &album),
            Msg::SearchOpenArtist(name) => self.on_search_open_artist(name, &sender),
            Msg::OpenGlobalEq => self.open_global_eq(root, &sender),
            Msg::OpenCurrentEq => self.on_open_current_eq(root, &sender),
            Msg::OpenTrackEq { path, title } => {
                self.open_eq_editor(root, &sender, "the track", &title, None, "track", path);
            }
            Msg::NavBack => {
                self.nav.nav_view.pop();
            }
            Msg::Source(m) => self.update_source(m, root, &sender),
            Msg::Design(m) => self.update_design(m, root, &sender),
            Msg::Tray(m) => self.update_tray(m, root, &sender),
            Msg::Sort(m) => self.update_sort(m, &sender),
            Msg::Eq(m) => self.update_eq(m),
            Msg::Stream(m) => self.update_stream(m, root, &sender),
            Msg::Edit(m) => self.update_edit(m, root, &sender),
            Msg::Yt(m) => self.update_yt(m, root, &sender),
            Msg::Podcast(m) => self.update_podcast(m, root, &sender),
            Msg::Lyrics(m) => self.update_lyrics(m, root, &sender),
            Msg::Concert(m) => self.update_concert(m, root, &sender),
            Msg::Favorite(m) => self.update_favorite(m, root, &sender),
            Msg::Cover(m) => self.update_cover(m, root, &sender),
            Msg::Setting(m) => self.update_setting(m, root, &sender),
            Msg::Ctx(m) => self.update_ctx(m, root, &sender),
            Msg::Transport(m) => self.update_transport(m, root, &sender),
        }
        // Suppress the per-second tick the moment the app goes idle (and resume
        // it when playback/recording starts) — see `tick_active`.
        self.sync_tick_active();
    }

    /// Process the results of the background workers.
    fn update_cmd(
        &mut self,
        msg: Self::CommandOutput,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match msg {
            Cmd::Entries(entries) => self.on_cmd_entries(entries),
            Cmd::RemoteEntries(result, source, rel) => {
                self.on_cmd_remote_entries(result, source, rel, &sender)
            }
            Cmd::RemoteTags(tags) => self.on_cmd_remote_tags(tags),
            Cmd::RemoteDownloaded(result) => match result {
                Ok((rel, path)) => {
                    let idx = {
                        let guard = self.libview.entries.guard();
                        (0..guard.len()).find(|&i| {
                            guard.get(i).is_some_and(|r| {
                                matches!(&r.entry, FsEntry::RemoteFile { rel_path, .. } if *rel_path == rel)
                            })
                        })
                    };
                    if let Some(i) = idx {
                        self.libview.entries.send(i, FsInput::SetDownloaded(path));
                    }
                    self.toast(&gettext("Download complete"));
                }
                Err(e) => {
                    tracing::warn!("Download failed: {e}");
                    self.toast(&gettext("Download failed"));
                }
            },
            Cmd::EnrichDone { changed } => {
                self.enrich_state.enriching = false;
                // Only rebuild if the run changed something – the quiet
                // per-minute backfill otherwise runs empty and would re-render the
                // lists for no reason.
                if changed {
                    self.reload_library_overviews();
                }
            }
            Cmd::ReloadViews => {
                self.reload_library_overviews();
            }
            Cmd::ScanDone {
                then_enrich,
                manual,
            } => self.on_cmd_scan_done(then_enrich, manual, &sender),
            Cmd::ScanProgress {
                done,
                total,
                bytes,
                total_bytes,
            } => {
                self.scan_done = done;
                self.scan_total = total;
                self.scan_bytes = bytes;
                self.scan_total_bytes = total_bytes;
            }
            Cmd::CloudReindexed { manual } => self.on_cmd_cloud_reindexed(manual, &sender),
            Cmd::Candidates(candidates) => {
                if candidates.is_empty() {
                    self.toast(&gettext("No new concert candidates found"));
                } else {
                    self.open_concert_import_dialog(root, &sender, candidates);
                }
            }
            Cmd::YtDlpReady(result) => {
                self.youtube.ytdlp_busy = false;
                match result {
                    Ok(v) => {
                        self.youtube.ytdlp_version = Some(v.clone());
                        self.toast(&gettext_f("yt-dlp ready (version {v})", &[("v", &v)]));
                    }
                    Err(e) => {
                        tracing::warn!("yt-dlp setup failed: {e}");
                        self.toast(&gettext("yt-dlp download failed"));
                    }
                }
                self.refresh_ytdlp_status_label();
            }
            Cmd::YtDlpAutoUpdated(result) => {
                self.youtube.ytdlp_busy = false;
                match result {
                    // Silent on success (version label only) and on failure (just
                    // log) — an auto-update must not interrupt with toasts.
                    Ok(v) => self.youtube.ytdlp_version = Some(v),
                    Err(e) => tracing::debug!("yt-dlp auto-update skipped: {e}"),
                }
                self.refresh_ytdlp_status_label();
            }
            Cmd::YtDlpChecked(version) => {
                self.youtube.ytdlp_version = version;
                self.refresh_ytdlp_status_label();
            }
            Cmd::YtReload => self.yt_page.emit(crate::ui::yt_page::YtInput::Reload),
            Cmd::LyricsLoaded { path, lyrics } => self.on_lyrics_loaded(path, lyrics),
            Cmd::YtPlaylistStart {
                url,
                title,
                items,
                total_duration,
            } => self.on_cmd_yt_playlist_start(url, title, items, total_duration, &sender),
            Cmd::HeardResolved {
                video_id,
                title,
                artist,
                download,
            } => self.on_heard_resolved(video_id, title, artist, download),
            Cmd::AlbumTracklistFetched { artist, album } => {
                self.refill_album_page(&sender, &artist, &album);
            }
            Cmd::MissingTrackCandidates {
                artist,
                album,
                disc,
                position,
                title,
                results,
            } => self.show_missing_candidates(
                root, &sender, artist, album, disc, position, title, results,
            ),
            Cmd::MissingTrackDone {
                artist,
                album,
                ok,
                message,
            } => self.on_missing_track_done(&sender, artist, album, ok, message),
            Cmd::SourceStatus(status) => {
                let mut changed = false;
                for (id, ok) in status {
                    if ok {
                        changed |= self.offline_sources.remove(&id);
                    } else {
                        changed |= self.offline_sources.insert(id);
                    }
                }
                // Changed connection state → rebuild the views, so that the
                // red "Disconnected" hint appears/disappears.
                if changed {
                    self.reload_library_overviews();
                }
            }
        }
        self.sync_tick_active();
    }
}

impl App {
    /// Keep `tick_active` current: the per-second tick is only needed while
    /// playing or while a timeshift recording runs. Called after every message
    /// so the timer stops delivering ticks the moment the app goes idle.
    pub(crate) fn sync_tick_active(&self) {
        self.tick_active
            .set(self.mini.playing || self.streaming.record_state.is_some());
    }

    /// One background worker of a manual refresh reported back → decrement the
    /// pending counter (saturating, so a stray completion can never wrap it).
    /// When it hits zero the loading overlay hides itself again (see the view).
    pub(crate) fn refresh_done(&mut self) {
        self.refresh_pending = self.refresh_pending.saturating_sub(1);
        if self.refresh_pending == 0 {
            self.refresh_progress = None;
        }
    }

    /// Whether the loading overlay should be shown: either a folder/list load is
    /// in progress or a manual refresh still has background workers running.
    pub(crate) fn overlay_visible(&self) -> bool {
        self.libview.loading
            || self.refresh_pending > 0
            || self.scanning
            || self.refresh_summary.is_some()
    }

    /// Text beneath the overlay spinner. A specific load label (e.g. a YouTube
    /// playlist) wins; otherwise a manual refresh shows "Updating …", and
    /// finally the default "reading data" of a plain folder/list load.
    pub(crate) fn overlay_text(&self) -> String {
        if let Some(summary) = &self.refresh_summary {
            summary.clone()
        } else if let Some(label) = &self.libview.loading_label {
            label.clone()
        } else if self.scanning {
            gettext("Reading in your music collection — this may take a moment the first time")
        } else if self.refresh_pending > 0 {
            gettext("Updating …")
        } else {
            self.libview.loading_text()
        }
    }

    /// Rebuilds **all** lists (after switching gallery/list or the
    /// column count). Each reload function fills – depending on `gallery_view` – the
    /// list or the gallery variant.
    pub(crate) fn rebuild_all_lists(&mut self, sender: &ComponentSender<Self>) {
        self.reload_library_overviews();
        self.load_dir(sender);
        self.load_favorites(sender);
        self.load_audiobooks(sender);
        self.load_concerts(sender);
        // Podcasts rebuild themselves in their component (told via
        // `PodcastsInput::SetGalleryView` from the gallery toggle).
    }

    /// Narrow (mobile) mode? Driven purely by the width breakpoint – not by the
    /// split's `collapsed`, which is also forced when the navigation is hidden
    /// (single visible menu item) and would otherwise misreport desktop as
    /// mobile.
    pub(crate) fn is_mobile(&self) -> bool {
        self.nav.narrow.get()
    }

    /// Show detail dialogs on the phone over the **full width**
    /// (bottom sheet); on the desktop floating as before (auto).
    pub(crate) fn adapt_detail_dialog(&self, dialog: &adw::Dialog) {
        crate::ui::widgets::adapt_dialog(dialog, self.is_mobile());
    }

    /// Only upwards, as long as we stay within the start folder.
    pub(crate) fn can_go_up(&self) -> bool {
        // Remote source: going back possible as long as not at the music root.
        if let Some(rel) = &self.files.remote_browse {
            return !rel.is_empty();
        }
        match (&self.files.browse_dir, &self.files.root_dir) {
            (Some(cur), Some(root)) => cur != root && cur.starts_with(root),
            _ => false,
        }
    }

    /// Display name of the active source (for the path bar at the root).
    pub(crate) fn active_source_name(&self) -> String {
        match &self.files.active_source {
            ActiveSource::Primary => gettext("Music"),
            ActiveSource::Source(id) => self
                .files
                .sources
                .iter()
                .find(|s| s.id == *id)
                .map(|s| s.name.clone())
                .unwrap_or_default(),
        }
    }

    /// Label of the path bar (current folder name or hint).
    pub(crate) fn folder_label(&self) -> String {
        // Remote source: last path segment or source name at the root.
        if let Some(rel) = &self.files.remote_browse {
            if rel.is_empty() {
                return self.active_source_name();
            }
            return rel.rsplit('/').next().unwrap_or(rel).to_string();
        }
        match &self.files.browse_dir {
            Some(dir) => dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("/")
                .to_string(),
            None => gettext("No music folder – please set one in settings"),
        }
    }
}
