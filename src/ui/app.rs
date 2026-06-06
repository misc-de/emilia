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
use crate::model::{AlbumMeta, ArtistMeta, Source};
use crate::ui::album_row::{AlbumCard, AlbumOutput};
pub(crate) use crate::ui::app_helpers::{
    album_subtitle, apply_color_scheme, cover_widget, duration_label, find_scroller, fmt_duration,
    fmt_rate, guarded_resume, initial_gallery_columns, most_common_artist, online_available,
    read_entries, save_window_state, unix_now,
};
use crate::ui::app_podcast::fetch_and_store_podcast;
use crate::ui::artist_row::{ArtistCard, ArtistOutput};
use crate::ui::fs_row::{FsEntry, FsInput, FsOutput, FsRow, RowOpts};

/// Target of the detail view (long press): a file/folder in the
/// file browser, an artist, an album or a concert (= path → `Fs`).
#[derive(Clone)]
pub(crate) enum CtxTarget {
    Fs(FsEntry),
    Artist(ArtistMeta),
    Album(AlbumMeta),
}

impl CtxTarget {
    /// Heading of the detail dialog.
    pub(crate) fn heading(&self) -> String {
        match self {
            CtxTarget::Fs(e) => e.heading(),
            CtxTarget::Artist(m) => m.name.clone(),
            CtxTarget::Album(m) => {
                if m.artist.is_empty() {
                    m.album.clone()
                } else {
                    format!("{} - {}", m.artist, m.album)
                }
            }
        }
    }
}

/// Source currently selected in the file view: the primary `music_dir`
/// (implicit first tab "Music") or an additional source by ID.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ActiveSource {
    /// The primary music directory (`music_dir`).
    Primary,
    /// An additional source (local secondary folder or WebDAV) by `source.id`.
    Source(i64),
}

/// A track of the remote (cloud) playback queue. Kept self-contained,
/// separate from the local `PathBuf` queue.
#[derive(Debug, Clone)]
pub(crate) struct RemoteTrack {
    /// Path relative to the source's music root (leading slash).
    pub(crate) rel_path: String,
    /// Display name (for "Now Playing").
    pub(crate) title: String,
}

/// Musical meaning of a file system folder (for playback & EQ).
pub(crate) enum FsKind {
    /// Folder of an artist (name = known artist).
    Artist(String),
    /// Folder of exactly one album.
    Album { artist: String, album: String },
}

/// Navigation sections: (stack name, tooltip, icon). The **default** order;
/// the actual display/menu order is stored in `section_order`
/// and can be reordered by the user.
// The labels are English gettext `msgid`s; translate them at the display site
// with `gettext()` (see usage in `build_nav` / `win_title`).
pub(crate) const SECTIONS: [(&str, &str, &str); 11] = [
    ("favorites", "Favorites", "emilia-favorite-symbolic"),
    ("files", "Files", "folder-symbolic"),
    ("artists", "Artists", "avatar-default-symbolic"),
    ("albums", "Albums", "media-optical-symbolic"),
    ("concerts", "Concerts", "ticket-special-symbolic"),
    ("podcasts", "Podcasts", "microphone-symbolic"),
    ("streaming", "Streaming", "audio-x-generic-symbolic"),
    ("youtube", "YouTube", "im-youtube-symbolic"),
    ("audiobooks", "Audiobooks", "emilia-audiobook-symbolic"),
    ("playlists", "Playlists", "view-list-symbolic"),
    ("stats", "Statistics", "emilia-stats-symbolic"),
];

/// Returns (tooltip/label as msgid, icon) of a section by its
/// stack name. Translate the label at the display site with `gettext()`.
pub(crate) fn section_meta(name: &str) -> Option<(&'static str, &'static str)> {
    SECTIONS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, label, icon)| (*label, *icon))
}

/// Safety prompt before destructive actions (delete/remove). Shows a
/// confirmation dialog relative to `parent` (any widget in the window) and
/// sends `msg` only after confirmation. `confirm_label` labels the
/// (destructive) confirm button, e.g. `gettext("Delete")` / `gettext("Remove")`.
pub(crate) fn confirm_destructive(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    confirm_label: &str,
    sender: ComponentSender<App>,
    msg: Msg,
) {
    let confirm = adw::AlertDialog::new(Some(heading), None);
    confirm.add_response("cancel", &gettext("Cancel"));
    confirm.add_response("ok", confirm_label);
    confirm.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
    confirm.set_default_response(Some("cancel"));
    confirm.set_close_response("cancel");
    // `connect_response` is `Fn`; so take the message only once.
    let msg = std::cell::RefCell::new(Some(msg));
    confirm.connect_response(None, move |_, resp| {
        if resp == "ok" {
            if let Some(m) = msg.borrow_mut().take() {
                sender.input(m);
            }
        }
    });
    confirm.present(Some(parent));
}

/// Cadence of the quiet background backfill of missing artist photos & covers.
/// Deliberately low (~1 min) so new users quickly get an enriched overview;
/// the worker throttles the actual network requests itself.
const AUTO_ENRICH_INTERVAL_SECS: u32 = 60;

/// Which view the podcast page shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PodcastView {
    /// Newest episodes (entries) across all subscriptions.
    Newest,
    /// Overview of the subscribed podcasts.
    Overview,
}

/// Which view the streaming page shows (tab switcher).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamView {
    /// Saved stations/channels.
    Channels,
    /// Timeshift recordings.
    Recordings,
}

/// Which view the YouTube page shows (tab switcher).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum YtView {
    /// Newest videos across all subscribed channels.
    Newest,
    /// Recently played videos (history).
    Recent,
    /// Overview of the subscribed channels.
    Channels,
}

/// Time period of the listening statistics. Deliberately sliding windows
/// (instead of a calendar year) – calendar-free and without an extra date dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatsPeriod {
    /// Last 4 weeks.
    Weeks4,
    /// Last 12 months.
    Year,
    /// Since the beginning.
    All,
}

/// Ongoing listening session of a track. On switch/end it is written as **one**
/// `play_event` into the statistics (see `finalize_play_session`).
/// Purely local – never leaves the device.
pub(crate) struct PlaySession {
    pub(crate) path: PathBuf,
    /// Start time (Unix seconds).
    pub(crate) started_at: i64,
    /// Actually listened time (from the 1-s tick, counted only during "Playing").
    pub(crate) played_ms: i64,
    /// Snapshot of the track length (0 = still unknown → backfilled on tick).
    pub(crate) duration_ms: i64,
}

/// Album/artist overviews + file-list factory + gallery rendering state.
pub(crate) struct LibView {
    pub(crate) entries: FactoryVecDeque<FsRow>,
    pub(crate) albums: FactoryVecDeque<AlbumCard>,
    /// Gallery variant of the albums (cover grid), parallel to the list factory.
    pub(crate) albums_gallery: gtk::FlowBox,
    /// Album overview (same order as factory/gallery); index resolution for the gallery.
    pub(crate) albums_overview: Vec<crate::model::AlbumMeta>,
    pub(crate) album_count: usize,
    pub(crate) artists: FactoryVecDeque<ArtistCard>,
    /// Gallery variant of the artists (photo grid).
    pub(crate) artists_gallery: gtk::FlowBox,
    /// Artist overview (same order); index resolution for the gallery.
    pub(crate) artists_overview: Vec<crate::model::ArtistMeta>,
    pub(crate) artist_count: usize,
    /// Show lists as a gallery (cover grid) instead of a list.
    pub(crate) gallery_view: bool,
    /// Number of tiles per row in the gallery view (2–8).
    pub(crate) gallery_columns: u32,
    pub(crate) loading: bool,
    /// Custom text for the loading overlay (e.g. while a YouTube playlist loads);
    /// `None` falls back to the default "Reading music data".
    pub(crate) loading_label: Option<String>,
    /// Galleries (artist/album) for which an on-demand fetch already ran this session.
    pub(crate) gallery_tried: std::cell::RefCell<std::collections::HashSet<String>>,
    /// Gallery FlowBoxes whose resize hook has already been connected once.
    pub(crate) gallery_hooked: std::cell::RefCell<std::collections::HashSet<usize>>,
}

impl LibView {
    /// Text shown beneath the loading spinner: the custom label if set, else the
    /// default. Used by the overlay both for the local library and remote loads.
    pub(crate) fn loading_text(&self) -> String {
        self.loading_label
            .clone()
            .unwrap_or_else(|| gettext("Reading music data"))
    }
}

/// Playback transport: queue, shuffle order, history, resume/stats sessions.
pub(crate) struct TransportState {
    /// Active playback context: the album/artist/folder/track currently being
    /// played through. Replaced freely whenever the user starts something new.
    pub(crate) queue: Vec<PathBuf>,
    pub(crate) queue_pos: usize,
    /// Explicitly enqueued tracks ("Add to queue"). This is the user-curated
    /// queue shown in the queue dialog – it is **never** overwritten by simply
    /// playing an album/song. Its entries jump ahead of the rest of the context
    /// (spliced in by `play_next`) and are consumed as they play.
    pub(crate) user_queue: Vec<PathBuf>,
    pub(crate) shuffle: bool,
    /// Random order of the queue indices (Fisher-Yates) for shuffle.
    pub(crate) shuffle_order: Vec<usize>,
    /// Position within `shuffle_order`.
    pub(crate) shuffle_idx: usize,
    /// Repeat: at the end of the queue / single track, start over.
    pub(crate) repeat: bool,
    /// Recently played tracks (for "previous song" via double-click on back).
    pub(crate) play_history: Vec<PathBuf>,
    /// When jumping back out of history, do not write to the history again.
    pub(crate) skip_history_push: bool,
    /// Time of the last back click (double-click detection, < 1 s).
    pub(crate) last_prev: Option<std::time::Instant>,
    /// Queue paused while a single song is played in between (list + position).
    pub(crate) interrupted_queue: Option<(Vec<PathBuf>, usize)>,
    /// Back stack of displaced playback contexts (queue + position).
    pub(crate) nav_stack: Vec<(Vec<PathBuf>, usize)>,
    /// Context last played by `play_current` (to detect queue replacement).
    pub(crate) prev_ctx: Option<(Vec<PathBuf>, usize)>,
    /// Path of the track currently loaded into the player.
    pub(crate) playing_path: Option<PathBuf>,
    /// Snapshot (path, position, duration) of the running resume track.
    pub(crate) close_resume: std::rc::Rc<std::cell::RefCell<Option<(String, i64, i64)>>>,
    /// Ongoing listening session for the statistics (see [`PlaySession`]).
    pub(crate) play_session: Option<PlaySession>,
    /// Snapshot of the session for close (path, start, listened, duration).
    pub(crate) close_session: std::rc::Rc<std::cell::RefCell<Option<(String, i64, i64, i64)>>>,
    /// List in the queue dialog (rebuilt on changes).
    pub(crate) queue_list: gtk::ListBox,
    /// Consecutive unplayable tracks skipped since the last successful start.
    /// Bounds auto-skipping so an entirely unplayable queue stops instead of
    /// looping (see [`App::skip_current_track`]).
    pub(crate) skip_count: u32,
    /// One-shot start position (ms) for the next `play_current`, overriding the
    /// saved resume position. Used by the recording editor's "play from the
    /// playhead" preview. Consumed (reset to `None`) on use.
    pub(crate) forced_start_ms: Option<i64>,
}

/// Mini-player / now-playing strip state, grouped off the `App` god-object.
pub(crate) struct MiniState {
    /// Title shown in the player bar; `None` when nothing is loaded.
    pub(crate) now_playing: Option<String>,
    pub(crate) playing: bool,
    /// A slow source (Nextcloud/YouTube) is resolving/buffering: show a spinner
    /// in the play button until the pipeline is ready. Local files start fast
    /// enough that a spinner would only flicker, so it stays off for them.
    pub(crate) loading: bool,
    /// Current position and total duration of the running track (ms).
    pub(crate) position_ms: i64,
    pub(crate) track_duration_ms: i64,
    /// Playback speed (0.25–2.0, pitch-preserving). Not used for live streams.
    pub(crate) playback_rate: f64,
    /// Seek bar of the mini player (for chapter marks via `add_mark`).
    pub(crate) seek_scale: gtk::Scale,
    /// Label that, on hover over the seek bar, shows the chapter at the cursor.
    pub(crate) chapter_label: gtk::Label,
    /// Chapters (time + name) of the running episode.
    pub(crate) chapters: std::rc::Rc<std::cell::RefCell<Vec<(i64, String)>>>,
    /// Is the seek bar currently being hovered?
    pub(crate) hovering_seek: std::rc::Rc<std::cell::Cell<bool>>,
}

/// Navigation + layout chrome, grouped off the `App` god-object.
pub(crate) struct NavState {
    /// Main split view – collapsed (`is_collapsed`) means narrow/mobile mode.
    pub(crate) split: adw::OverlaySplitView,
    pub(crate) view_stack: adw::ViewStack,
    /// Navigation container for the subpages (artist → albums → album).
    pub(crate) nav_view: adw::NavigationView,
    /// Navigation containers (sidebar, top bar) for reordering.
    pub(crate) sidebar_nav: gtk::Box,
    pub(crate) top_nav: gtk::Box,
    /// All navigation buttons per menu item with container marker
    /// (`true` = sidebar, `false` = top bar), for showing/hiding and reordering.
    pub(crate) nav_buttons: Vec<(&'static str, bool, gtk::ToggleButton)>,
    /// Display order of the menu items (stack names). Reorderable by the user.
    pub(crate) section_order: Vec<&'static str>,
    /// Hidden navigation menu items (stack names).
    pub(crate) hidden_sections: std::collections::HashSet<String>,
    /// Target of the open context/detail dialog.
    pub(crate) context_target: Option<CtxTarget>,
    /// Play row of the open detail dialog + its track path (hidden while playing).
    pub(crate) ctx_play: std::rc::Rc<std::cell::RefCell<Option<(adw::ActionRow, PathBuf)>>>,
    /// Remembered scroll position of the most recently left overview page.
    pub(crate) overview_scroll: std::rc::Rc<std::cell::RefCell<Option<(gtk::ScrolledWindow, f64)>>>,
    /// Narrow/mobile layout active (driven by the width breakpoint). The source
    /// of truth for [`App::is_narrow`]; the split's `collapsed` is derived from
    /// this **and** `nav_hidden`, so it can't be used to detect narrowness.
    pub(crate) narrow: std::rc::Rc<std::cell::Cell<bool>>,
    /// Only one menu item is visible → the whole navigation is suppressed
    /// (sidebar collapsed, top bar hidden, Settings moved to the title bar).
    pub(crate) nav_hidden: std::rc::Rc<std::cell::Cell<bool>>,
    /// Reconciles the layout chrome (sidebar/top-nav/Settings visibility) with
    /// the current `narrow` + `nav_hidden` state. Set up in `init`.
    pub(crate) apply_chrome: std::rc::Rc<dyn Fn()>,
}

/// File browser + extra music sources (2nd local folder / Nextcloud) state.
pub(crate) struct FilesState {
    pub(crate) music_dir: Option<String>,
    pub(crate) root_dir: Option<PathBuf>,
    pub(crate) browse_dir: Option<PathBuf>,
    /// Folder currently shown in the file browser (remembers scroll position).
    pub(crate) shown_dir: Option<PathBuf>,
    /// Remembered scroll positions per folder in the file browser.
    pub(crate) fs_scroll: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<PathBuf, f64>>>,
    /// Extra music sources (2nd local folder / Nextcloud), from the `source` table.
    pub(crate) sources: Vec<Source>,
    /// Source active in the file view (primary = `music_dir`).
    pub(crate) active_source: ActiveSource,
    /// Tab bar above the file list (linked ToggleButtons).
    pub(crate) source_tabs: gtk::Box,
    /// Tab buttons per source (incl. primary), for mirroring the active state.
    pub(crate) source_tab_buttons: Vec<(ActiveSource, gtk::ToggleButton)>,
    /// Current subpath in the remote source (leading slash; `""` = root).
    pub(crate) remote_browse: Option<String>,
    /// Remote (cloud) playback queue of the most recently opened folder.
    pub(crate) remote_queue: Vec<RemoteTrack>,
    pub(crate) remote_pos: usize,
    /// Is a remote file currently playing (instead of local queue/episode/station)?
    pub(crate) playing_remote: bool,
}

/// Streaming (internet radio) + timeshift-recording page state.
pub(crate) struct StreamingState {
    /// Which streaming view is visible: channels or recordings.
    pub(crate) stream_view: StreamView,
    /// Saved stations.
    pub(crate) stream_items: Vec<crate::model::StreamItem>,
    pub(crate) streams_list: gtk::ListBox,
    /// Hits of the last station search (Radio Browser), for the add dialog.
    pub(crate) stream_search_results: Vec<crate::core::streaming::StationResult>,
    /// While the add dialog is open: (dialog, hit list), so that asynchronously
    /// arriving hits fit into the already shown list.
    pub(crate) stream_search: std::rc::Rc<std::cell::RefCell<Option<(adw::Dialog, gtk::ListBox)>>>,
    /// ID of the currently running station; `None` when nothing/other is running.
    pub(crate) playing_stream: Option<i64>,
    /// Currently running track of the station (ICY metadata) for "Now Playing".
    pub(crate) stream_title: Option<String>,
    /// Timeshift recording of the running station (ring buffer); `None` if no
    /// station is running or the buffer is set to 0 minutes.
    pub(crate) recorder: Option<crate::core::recorder::Recorder>,
    /// Active recording (state machine that saves at the song boundaries).
    pub(crate) record_state: Option<crate::ui::app_streaming::RecordState>,
    /// Size of the timeshift buffer in minutes (0 = off, max. 60).
    pub(crate) recording_buffer_minutes: u32,
    /// Saved timeshift recordings.
    pub(crate) recording_items: Vec<crate::model::RecordingItem>,
    pub(crate) recordings_list: gtk::ListBox,
    /// Play/pause buttons of the station rows (station id → button), for
    /// refreshing the icon when the playback state changes.
    pub(crate) stream_play_buttons: std::rc::Rc<std::cell::RefCell<Vec<(i64, gtk::Button)>>>,
    /// Play/pause buttons of the recording rows (file path → button), same
    /// purpose as [`Self::stream_play_buttons`].
    pub(crate) rec_play_buttons: std::rc::Rc<std::cell::RefCell<Vec<(String, gtk::Button)>>>,
}

/// Podcasts page state, grouped off the `App` god-object.
pub(crate) struct PodcastsState {
    /// (id, title, image URL, episode count) per podcast.
    pub(crate) podcast_items: Vec<(i64, String, Option<String>, i64)>,
    pub(crate) podcasts_list: gtk::ListBox,
    /// Gallery variant of the podcast overview (cover grid).
    pub(crate) podcasts_gallery: gtk::FlowBox,
    /// Which podcast view is visible: newest episodes or subscription overview.
    pub(crate) podcast_view: PodcastView,
    /// Newest episodes across all subscriptions (for the "Newest" view).
    pub(crate) newest_items: Vec<crate::model::EpisodeRef>,
    /// Container of the "Newest" list (filled imperatively in `reload_newest`).
    pub(crate) newest_list: gtk::Box,
    /// Hits of the last podcast search (iTunes), for the subscribe dialog.
    pub(crate) podcast_search_results: Vec<crate::core::podcast::PodcastSearchResult>,
    /// While the subscribe search dialog is open: (dialog, hit list), so that
    /// asynchronously arriving hits can be inserted into the shown list.
    pub(crate) podcast_search: std::rc::Rc<std::cell::RefCell<Option<(adw::Dialog, gtk::ListBox)>>>,
    /// URL of the currently loaded podcast episode (play/pause row marker);
    /// `None` when music is playing or no episode is running.
    pub(crate) playing_episode_url: Option<String>,
    /// Play/pause buttons of the visible episode rows (audio URL → button), to
    /// refresh their icon on playback-state changes.
    pub(crate) episode_play_buttons: std::rc::Rc<std::cell::RefCell<Vec<(String, gtk::Button)>>>,
    /// "Play" row of an open episode detail dialog (row, audio URL) – hidden
    /// while exactly this episode is playing.
    pub(crate) ctx_episode_play: std::rc::Rc<std::cell::RefCell<Option<(adw::ActionRow, String)>>>,
    /// "Download" column of an open episode detail dialog: (value label, audio
    /// URL). The label text reflects the offline state and is refreshed when a
    /// background download starts or finishes.
    pub(crate) ctx_episode_download: std::rc::Rc<std::cell::RefCell<Option<(gtk::Label, String)>>>,
    /// Audio URLs of episodes whose download is currently running (to show a
    /// spinner and ignore repeated taps).
    pub(crate) downloading_episodes: std::collections::HashSet<String>,
}

/// YouTube page state, grouped off the `App` god-object. The whole section is
/// gated behind the `youtube_enabled` setting; the extractor (`yt-dlp`) is
/// downloaded at runtime, never bundled (see [`crate::core::youtube`]).
pub(crate) struct YoutubeState {
    /// Whether the user enabled the YouTube feature (off by default).
    pub(crate) enabled: bool,
    /// Installed `yt-dlp` version (cached for the settings status; `None` if not
    /// installed/runnable).
    pub(crate) ytdlp_version: Option<String>,
    /// Status label of the yt-dlp row in the open settings dialog (download /
    /// update progress + version), refreshed via `Cmd::YtDlpReady`.
    pub(crate) settings_status: std::rc::Rc<std::cell::RefCell<Option<gtk::Label>>>,
    /// Download/update button of the yt-dlp row in the open settings dialog. Its
    /// label flips between "Download" and "Update" once the background version
    /// probe resolves (`Cmd::YtDlpChecked`).
    pub(crate) settings_dl_btn: std::rc::Rc<std::cell::RefCell<Option<gtk::Button>>>,
    /// Whether a yt-dlp download/update is currently running (ignore repeat taps).
    pub(crate) ytdlp_busy: bool,
    /// Whether the last YouTube extraction looked broken (yt-dlp can't parse
    /// YouTube anymore). Mirrors [`crate::core::youtube::extraction_broken`] into
    /// the model so a warning banner can `#[watch]` it. Refreshed when entering
    /// the YouTube view and after each extraction result.
    pub(crate) ytdlp_broken: bool,
    /// Which YouTube view is visible: newest videos or channel overview.
    pub(crate) yt_view: YtView,
    /// (id, title, url, thumbnail, video count) per subscribed channel.
    pub(crate) channel_items: Vec<(i64, String, String, Option<String>, i64)>,
    pub(crate) channels_list: gtk::ListBox,
    /// Gallery variant of the channel overview (thumbnail grid).
    pub(crate) channels_gallery: gtk::FlowBox,
    /// Newest videos across all subscriptions (for the "Newest" view).
    pub(crate) newest_items: Vec<crate::model::YtVideoRef>,
    /// Container of the "Newest videos" list (filled imperatively).
    pub(crate) newest_list: gtk::Box,
    /// Recently played videos (history) and its list container.
    pub(crate) recent_items: Vec<crate::model::YtRecent>,
    pub(crate) recent_list: gtk::Box,
    /// Hits of the last search, for the subscribe/search dialog.
    pub(crate) search_results: Vec<crate::core::youtube::YtResult>,
    /// While the search dialog is open: (dialog, hit list), so asynchronously
    /// arriving hits can be inserted into the shown list.
    pub(crate) search: std::rc::Rc<std::cell::RefCell<Option<(adw::Dialog, gtk::ListBox)>>>,
    /// Video id currently loaded/playing (play/pause row marker); `None` when
    /// music/other is playing or nothing is running.
    pub(crate) playing_video_id: Option<String>,
    /// Play/pause buttons of visible video rows (video id → button), to refresh
    /// their icon on playback-state changes.
    pub(crate) video_play_buttons: std::rc::Rc<std::cell::RefCell<Vec<(String, gtk::Button)>>>,
    /// "Play" row of an open video detail dialog (row, video id) – hidden while
    /// exactly this video is playing.
    pub(crate) ctx_video_play: std::rc::Rc<std::cell::RefCell<Option<(adw::ActionRow, String)>>>,
    /// Offline/library action row of an open video detail dialog + its video id;
    /// its title reflects the state and is refreshed on download start/finish.
    pub(crate) ctx_video_download:
        std::rc::Rc<std::cell::RefCell<Option<(adw::ActionRow, String)>>>,
    /// Open video detail dialog awaiting async metadata: (video id, cover box,
    /// channel row, duration row), filled in by `Cmd::YtVideoMeta`.
    pub(crate) ctx_video_meta:
        std::rc::Rc<std::cell::RefCell<Option<(String, gtk::Box, adw::ActionRow, adw::ActionRow)>>>,
    /// Video ids whose download is currently running (spinner + dedupe taps).
    pub(crate) downloading_videos: std::collections::HashSet<String>,
    /// Titles for the videos in the current play context (video id → title), so
    /// `yt:` tracks not in the library show a name instead of their id. Cleared
    /// and repopulated when a video or playlist is started.
    pub(crate) video_titles: std::collections::HashMap<String, String>,
    /// Whether the current play context is a YouTube playlist – then individual
    /// videos are not logged to "Recent" (the playlist is logged as one entry).
    pub(crate) playing_playlist: bool,
    /// Live progress toast shown while adding video(s) to the on-disk library.
    pub(crate) progress_toast: std::rc::Rc<std::cell::RefCell<Option<adw::Toast>>>,
    /// Session cache of fetched playlist song lists (playlist URL → its videos),
    /// so reopening a recent playlist is instant instead of re-running yt-dlp.
    pub(crate) playlist_songs_cache:
        std::collections::HashMap<String, Vec<crate::core::youtube::YtResult>>,
    /// Cover frames of the currently shown playlist-songs subpage that still need
    /// their thumbnail (thumbnail URL → frame), filled in once pre-cached in the
    /// background so the list shows immediately and covers fill in afterwards.
    pub(crate) pl_cover_slots: Vec<(String, adw::Bin)>,
}

/// Favorites + audiobooks page state, grouped off the `App` god-object.
pub(crate) struct FavoritesState {
    /// Favorites: (scope, key, title, is_dir).
    pub(crate) favorite_items: Vec<(String, String, String, bool)>,
    pub(crate) favorites_list: gtk::ListBox,
    /// Audiobooks: (scope, key, title, is_dir).
    pub(crate) audiobook_items: Vec<(String, String, String, bool)>,
    pub(crate) audiobooks_list: gtk::ListBox,
    /// Gallery variant of the audiobooks (cover grid).
    pub(crate) audiobooks_gallery: gtk::FlowBox,
}

/// Playlists page state, grouped off the `App` god-object.
pub(crate) struct PlaylistsState {
    /// (id, name, track count) per playlist.
    pub(crate) playlist_items: Vec<(i64, String, i64)>,
    pub(crate) playlists_list: gtk::ListBox,
}

/// Concerts page state, grouped off the `App` god-object.
pub(crate) struct ConcertsState {
    /// Concerts/audiobooks entries: (scope, key, title, is_dir) – like favorites.
    pub(crate) concert_items: Vec<(String, String, String, bool)>,
    pub(crate) concerts_list: gtk::ListBox,
    /// Gallery variant of the concerts (cover grid).
    pub(crate) concerts_gallery: gtk::FlowBox,
    pub(crate) concert_hint_dismissed: bool,
}

/// Online-enrichment state, grouped off the `App` god-object.
pub(crate) struct EnrichState {
    /// Is an enrichment run currently in progress? (prevents parallel runs; without
    /// a visible progress indicator – the fetch runs silently in the background).
    pub(crate) enriching: bool,
    /// Automatically fetch covers & metadata online at startup (only on a
    /// non-metered connection; can be disabled in the settings).
    pub(crate) auto_enrich: bool,
    /// Cancel flag for the enrichment worker.
    pub(crate) enrich_cancel: Arc<AtomicBool>,
    pub(crate) acoustid_key: Option<String>,
    pub(crate) fanart_key: Option<String>,
}

/// App-wide preferences, grouped off the `App` god-object.
pub(crate) struct Settings {
    /// Display language: "system" (system locale), "de" or "en". Can be
    /// switched in the settings; takes effect after restarting the app.
    pub(crate) ui_language: String,
    /// Currently active audio output (PipeWire sink), for the EQ resolution.
    pub(crate) active_output: String,
}

pub struct App {
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
    /// YouTube page state (optional feature, gated behind `youtube_enabled`).
    pub(crate) youtube: YoutubeState,
    /// "Other sources" list in the open settings dialog, so that it can be
    /// refreshed immediately after add/remove or a successful Nextcloud connect.
    pub(crate) settings_src_list: std::rc::Rc<std::cell::RefCell<Option<gtk::ListBox>>>,
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
    /// First-run setup assistant, shown once on the very first launch.
    pub(crate) setup_page: relm4::Controller<crate::ui::setup::SetupPage>,
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
    ShowConcertDetail(usize),
    /// Short tap on an artist: list its albums & songs.
    OpenArtistTracks(usize),
    /// Tap on an album in the artist subpage: list its tracks as
    /// a further subpage.
    OpenAlbumTracks {
        artist: String,
        album: String,
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
    CtxPlay,
    /// Play the album in track order (shuffle off, stop at the end).
    CtxPlayAlbum,
    /// Play all tracks of the artist: albums by year (newest or
    /// oldest first), each album from track 1 top-down (shuffle off).
    CtxPlayArtist {
        newest_first: bool,
    },
    CtxAddQueue,
    CtxAddPlaylist,
    CtxEqualizer,
    CtxShare,
    /// Header sync icon → open the pairing / connection-status dialog (no item).
    OpenSync,
    // --- Device synchronization (handled by the SyncPage component) ---
    /// The sync component paired/disconnected → tint the header icon.
    SyncConnected(bool),
    /// The sync component imported metadata → reload the affected views.
    SyncImported,
    TrackFinished,
    /// Periodic tick: save the resume position of the running track.
    PersistResume,
    /// Command from the lock screen / from media keys (MPRIS).
    Mpris(crate::core::mpris::MprisCommand),
    /// 1-s tick: update position/duration of the seek bar.
    Tick,
    /// Periodic, quiet background backfill: fetch missing artist photos (first)
    /// and online covers, without the user having to trigger it.
    AutoEnrichTick,
    /// On-demand fingerprint track recognition for the **just started**
    /// track without usable metadata (AcoustID), triggered on play.
    FingerprintCurrent(PathBuf),
    /// Jump to a position (ms) by dragging/clicking the seek bar.
    Seek(i64),
    Next,
    Prev,
    ToggleShuffle,
    ToggleRepeat,
    NavUp,
    FilesGoStart,
    Refresh,
    TogglePlay,
    /// Open the detail view of the currently running track (click on the bar).
    OpenNowPlaying,
    OpenSettings,
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
    /// Open the queue dialog.
    ShowQueue,
    /// Play a user-queue entry now (its index + length; album rows span `len`
    /// tracks). The entry jumps ahead of the rest of the queue.
    PlayQueueAt {
        start: usize,
        len: usize,
    },
    /// Set the playback speed (0.25–2.0, in 0.25 steps).
    SetPlaybackRate(f64),
    /// The current track failed to play (missing file/mount, unreachable
    /// Nextcloud, …) → skip to the next entry.
    PlaybackError,
    /// The freshly loaded pipeline prerolled (buffered enough to play) → clear
    /// the loading spinner of a slow source (Nextcloud/YouTube).
    PlaybackReady,
    /// Clear the user queue (after confirmation). Playback keeps running.
    QueueClear,
    /// Reorder the user queue: move the `len`-track block starting at `from` so
    /// it lands at index `to` (album rows move as one block).
    QueueMoveRange {
        from: usize,
        len: usize,
        to: usize,
    },
    SetMusicDir(PathBuf),
    /// The first-run setup assistant completed: persist the chosen language,
    /// music folder and enabled menu items, then scan (or restart for a language
    /// change).
    SetupFinished {
        lang_code: String,
        music_dir: PathBuf,
        enabled_sections: Vec<String>,
    },
    /// Switch to another source (tab) in the file view.
    SelectSource(ActiveSource),
    /// The source list has changed (added/removed in the settings dialog)
    /// – reload sources and update the tab bar.
    SourcesChanged,
    /// Check reachability of the Nextcloud sources (periodically + at startup).
    CheckSources,
    /// Open the Nextcloud setup dialog (QR scan or manual).
    AddCloudSource,
    /// The CloudPage component finished indexing a newly added source.
    CloudIndexed,
    /// Download a remote file offline (rel path in the active source).
    CtxDownloadRemote(String),
    SetAcoustidKey(String),
    /// Set the primary cover of an album (last shown in the gallery carousel).
    SetAlbumCover {
        artist: String,
        album: String,
        path: String,
    },
    /// Set the primary photo of an artist (last shown in the gallery carousel).
    SetArtistImage {
        name: String,
        path: String,
    },
    /// Upload a custom cover/photo for the current detail target (file dialog).
    UploadCover,
    SetFanartKey(String),
    /// Turn the automatic online fetch on/off.
    SetAutoEnrich(bool),
    /// Change the display language ("system"/"de"/"en"); restarts the app.
    SetLanguage(String),
    /// Change the color scheme ("system"/"dark"/"light"); takes effect immediately.
    SetColorScheme(String),
    /// Gallery view (cover grid) on/off; rebuilds the lists.
    SetGalleryView(bool),
    /// Tiles per row in the gallery view (2–8); rebuilds the lists.
    SetGalleryColumns(u32),
    /// Set a property of a level (or with `None` reset to "inherit").
    /// Set the areas (properties) of a level; empty value = hidden.
    SetAreas {
        scope: &'static str,
        key: String,
        value: String,
    },
    /// Save and apply the equalizer bands of an output + a level.
    SetEq {
        output: String,
        scope: &'static str,
        key: String,
        bands: [f64; 10],
    },
    /// Enable/disable one equalizer setting without changing its saved bands.
    SetEqEnabled {
        output: String,
        scope: &'static str,
        key: String,
        enabled: bool,
    },
    /// Reset the equalizer of an output + a level (inherits again).
    ClearEq {
        output: String,
        scope: &'static str,
        key: String,
    },
    // Concerts
    ConcertImport,
    ConcertDismissHint,
    ConcertHideSection,
    ConcertAdd(Vec<(String, String, bool)>),
    PlayConcert(usize),
    /// Open gallery concert (index): album/folder → track list, track → play.
    OpenConcertEntry(usize),
    /// Show/hide a navigation menu item (stack name).
    SetSectionVisible {
        section: &'static str,
        visible: bool,
    },
    /// Move a menu item in the order (indices in `section_order`).
    MoveSection {
        from: usize,
        to: usize,
    },
    /// Show a hidden content again (reset the override).
    UnhideEntry {
        scope: String,
        key: String,
    },
    // Favorites
    /// Set/remove the current detail target as a favorite.
    ToggleFavorite,
    /// Play a favorite (index in `favorite_items`).
    PlayFavorite(usize),
    /// Open the detail view of a favorite.
    ShowFavoriteDetail(usize),
    /// Reorder favorites (indices in `favorite_items`).
    MoveFavorite {
        from: usize,
        to: usize,
    },
    // Audiobooks
    /// Play an audiobook (index in `audiobook_items`).
    PlayAudiobook(usize),
    /// Open gallery audiobook (index): album/folder → track list, track → play.
    OpenAudiobookEntry(usize),
    /// Open the detail view of an audiobook.
    ShowAudiobookDetail(usize),
    // Playlists
    /// Create a playlist and add the current context files.
    PlaylistCreateAddTo(String),
    /// Open the tracks subpage of a playlist (short tap: albums + songs).
    OpenPlaylist(i64),
    /// Open the detail view of a playlist (long press: cover + actions).
    ShowPlaylistDetail(i64),
    /// Play the whole playlist.
    PlayPlaylist(i64),
    /// Play the whole playlist shuffled (random order, random start).
    PlayPlaylistShuffled(i64),
    /// Delete a playlist (shows an undo toast; the real delete is deferred to
    /// `PlaylistDeleteConfirmed`).
    PlaylistDelete(i64),
    /// Actually delete a playlist (fired when the undo toast expires).
    PlaylistDeleteConfirmed(i64),
    /// Add the current context files to this playlist.
    PlaylistAddTo(i64),
    /// Set the chosen cover of a playlist (last shown in the detail carousel).
    SetPlaylistCover {
        id: i64,
        path: String,
    },
    /// Open the rename dialog of a playlist.
    PlaylistRenameDialog(i64),
    /// Rename a playlist.
    PlaylistRename {
        id: i64,
        name: String,
    },
    // Podcasts
    /// Open the subscribe dialog (search + feed address).
    PodcastSubscribe,
    /// Search for podcasts matching this search term (iTunes directory, in the background).
    PodcastSearch(String),
    /// Subscribe to the feed at this address (fetch in the background).
    PodcastSubscribeUrl(String),
    /// Open the episodes subpage of a podcast.
    OpenPodcast(i64),
    /// Open gallery podcast (index in `podcast_items`) → `OpenPodcast`.
    OpenPodcastAt(usize),
    /// Subscription detail of a gallery podcast (index in `podcast_items`) → `ShowPodcastDetail`.
    ShowPodcastDetailAt(usize),
    /// Remove a podcast (undo toast; deferred to `PodcastDeleteConfirmed`).
    PodcastDelete(i64),
    /// Actually remove a podcast (after the undo toast expires).
    PodcastDeleteConfirmed(i64),
    /// Reload the feed of a podcast.
    PodcastRefresh(i64),
    /// Toggle an entry (episode): start or – if already the running one –
    /// pause/resume. From tapping the row and from the play/pause button.
    ToggleEpisode {
        url: String,
        title: String,
    },
    /// Switch the podcast view (Newest / Overview).
    SetPodcastView(PodcastView),
    /// Switch the streaming view (channels/recordings).
    SetStreamView(StreamView),
    /// Detail view of an entry (episode) from the "Newest" list (index).
    ShowEpisodeDetail(usize),
    /// Detail view of an episode from the episode list of a podcast.
    ShowPodcastEpisodeDetail {
        podcast_id: i64,
        index: usize,
    },
    /// Click on a time-jump mark in the show notes: jump to the spot
    /// (start the episode there if needed).
    EpisodeSeekTo {
        url: String,
        title: String,
        ms: i64,
    },
    /// Download row in the episode detail: if not downloaded, fetch the audio
    /// for offline playback (background); if already downloaded, delete the
    /// local copy. `title` is only used for the toast.
    ToggleEpisodeDownload {
        url: String,
        title: String,
    },
    /// Detail view/management of a subscription (podcast id) – refresh/remove.
    ShowPodcastDetail(i64),
    // YouTube (optional feature)
    /// Toggle the YouTube feature on/off (settings switch). Shows/hides the
    /// section and persists the setting.
    SetYoutubeEnabled(bool),
    /// Fetch yt-dlp (settings button): installs it, or re-downloads the latest
    /// when one is already present. The download/update choice is decided from the
    /// cached version at handling time, so the button works even before the
    /// background version probe has resolved.
    FetchYtDlp,
    /// Open the YouTube search/subscribe dialog.
    YtSubscribe,
    /// Search YouTube for this term + kind filter (background).
    YtSearch(String, crate::core::youtube::YtKind),
    /// Subscribe to the channel at this URL (the "bell"; fetch newest in background).
    YtSubscribeChannel(String),
    /// Open the videos subpage of a subscribed channel (DB id).
    YtOpenChannel(i64),
    /// Open gallery channel (index in `channel_items`) → `YtOpenChannel`.
    YtOpenChannelAt(usize),
    /// Subscription detail of a channel (DB id) – refresh/remove.
    YtShowChannelDetail(i64),
    /// Subscription detail of a gallery channel (index) → `YtShowChannelDetail`.
    YtShowChannelDetailAt(usize),
    /// Refresh a channel's newest videos (DB id).
    YtRefreshChannel(i64),
    /// Remove a channel subscription (undo toast; deferred to confirmed).
    YtDeleteChannel(i64),
    /// Actually remove a channel subscription (after the undo toast expires).
    YtDeleteChannelConfirmed(i64),
    /// Play a subscribed channel's cached videos as the queue.
    YtPlayChannel(i64),
    /// Remove an item (video id or playlist URL) from the "Recent" history.
    YtRemoveRecent(String),
    /// Detail view of a video (play / add to collection / offline).
    YtShowVideoDetail {
        video_id: String,
        title: String,
    },
    /// Detail view of a video from the "Newest" list (index in `newest_items`).
    YtShowNewestDetail(usize),
    /// Detail/contents of a playlist (start / offline / add to library).
    YtShowPlaylistDetail {
        url: String,
        title: String,
    },
    /// Start playing a whole playlist (loads its videos as the queue).
    YtStartPlaylist {
        url: String,
        title: String,
    },
    /// Play a cached playlist (by URL) starting at the given song index. Plays the
    /// whole playlist as the queue (so the songs are not logged to "Recent"
    /// individually). `close` pops the song-list subpage afterwards (row tap), a
    /// play-button click keeps it open.
    YtPlayPlaylistAt {
        url: String,
        title: String,
        index: usize,
        close: bool,
    },
    /// Save a found playlist into the Playlists section (without playing it).
    YtSavePlaylist {
        url: String,
        title: String,
    },
    /// Open a recent playlist's song list (the mirrored local playlist).
    YtOpenRecentPlaylist {
        url: String,
        title: String,
    },
    /// Play a video: resolves the stream URL asynchronously (or plays the
    /// offline copy), then starts playback.
    YtPlayVideo {
        video_id: String,
        title: String,
    },
    /// Internal: a video's stream URL was resolved (or failed) in a worker →
    /// start streaming. Dispatched from `play_current` for `yt:` tracks.
    YtStreamResolved {
        video_id: String,
        resume: i64,
        result: Result<String, String>,
    },
    /// Internal: online enrichment (artist + cover) for a played video finished.
    YtEnriched {
        video_id: String,
        artist: Option<String>,
        cover: Option<String>,
    },
    /// Switch the YouTube view (Newest / Channels).
    SetYtView(YtView),
    /// Add a video to the on-disk music library: download + transcode + index
    /// in one step (background). Skips (and asks) if the target already exists.
    YtAddToLibrary {
        video_id: String,
        title: String,
    },
    /// Like [`Msg::YtAddToLibrary`] but after the user confirmed overwriting an
    /// existing file (from the collision dialog).
    YtAddToLibraryConfirmed {
        video_id: String,
        title: String,
    },
    /// Add a whole playlist to the on-disk music library (download + transcode).
    YtPlaylistToLibrary {
        url: String,
        title: String,
    },
    // Streaming (internet radio)
    /// Open the add dialog (search + stream address).
    StreamAdd,
    /// Search for stations matching this search term (Radio Browser, in the background).
    StreamSearch(String),
    /// Save a search hit (index in `stream_search_results`) as a station.
    StreamAddResult(usize),
    /// Save a stream address manually as a station.
    StreamAddUrl(String),
    /// Tap a station: starts it, toggle pause/resume on a running station.
    ToggleStream(i64),
    /// Record button of a station row: starts/stops the continuous recording.
    StreamRecordToggle(i64),
    /// Record button in the player bar: records/stops the running station.
    TransportRecordToggle,
    /// Title tag from the playback (for stations: the running ICY title).
    StreamTitle(String),
    /// Open the detail page of a station.
    OpenStream(i64),
    /// Open the rename dialog of a station.
    StreamRenameDialog(i64),
    /// Rename a station.
    StreamRename {
        id: i64,
        name: String,
    },
    /// Remove a station (undo toast; deferred to `StreamDeleteConfirmed`).
    StreamDelete(i64),
    /// Actually remove a station (after the undo toast expires).
    StreamDeleteConfirmed(i64),
    // Recording (timeshift)
    /// Stop the running recording.
    RecordStop,
    /// Open the replay subpage of a station.
    OpenStreamReplay(i64),
    /// Preview a buffered song (absolute byte range).
    ReplayPlay {
        start: u64,
        end: u64,
    },
    /// Save a buffered song after the fact.
    ReplaySave {
        start: u64,
        end: u64,
        title: String,
    },
    /// Play a saved recording (path).
    PlayRecording(String),
    /// Open the detail page of a recording (id) – via long press.
    OpenRecording(i64),
    /// Delete a recording (id) – undo toast; deferred to `RecordingDeleteConfirmed`.
    RecordingDelete(i64),
    /// Actually delete a recording (after the undo toast expires).
    RecordingDeleteConfirmed(i64),
    /// Copy a recording (id) into the music library so it appears as a track.
    AddRecordingToLibrary(i64),
    /// Open the waveform editor subpage for a recording (id).
    EditRecording(i64),
    /// Preview a recording file from a chosen position (ms) – editor playhead.
    RecordingPlayFrom { path: String, ms: i64 },
    /// Pause the editor preview (pauses the main player it plays through).
    RecordingPreviewPause,
    /// Apply the editor's cut ranges (seconds) to a recording and overwrite it.
    RecordingApplyCut { id: i64, cuts: Vec<(f64, f64)> },
    /// Result of the background cut: new path (`None` = failed) + new duration.
    RecordingCutDone {
        id: i64,
        path: Option<String>,
        duration_ms: i64,
    },
    /// Set the size of the timeshift buffer in minutes (0–60).
    SetRecordingBufferMinutes(u32),
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
    /// Found concert candidates (for the import dialog).
    Candidates(Vec<crate::core::concert::Candidate>),
    /// Podcast feed fetched: `Some(title)` on success, otherwise `None`.
    PodcastFetched(Option<String>),
    /// An episode download finished: the audio URL and the local path on
    /// success, or an error message.
    EpisodeDownloaded {
        url: String,
        result: Result<String, String>,
    },
    /// Hits of the podcast search (for the open subscribe dialog).
    PodcastSearchResults(Vec<crate::core::podcast::PodcastSearchResult>),
    /// Cover thumbnails of the search hits are cached → redraw the hit list.
    PodcastSearchCoversReady,
    /// Rebuild the podcast list (e.g. after feed images were cached).
    ReloadPodcasts,
    /// yt-dlp install/update/startup-check finished: the version on success,
    /// or an error message. Drives the settings status and `youtube.ytdlp_version`.
    YtDlpReady(Result<String, String>),
    /// Background yt-dlp version probe (opened settings) finished: `Some(v)` if a
    /// usable yt-dlp is present, `None` otherwise. Caches the result and refreshes
    /// the settings row without ever blocking the UI thread on the subprocess.
    YtDlpChecked(Option<String>),
    /// Hits of the YouTube search (for the open search dialog).
    YtSearchResults(Vec<crate::core::youtube::YtResult>),
    /// Thumbnails of the search hits cached → redraw the hit list.
    YtSearchThumbsReady,
    /// Channel subscribed/refreshed: `Some(title)` on success, otherwise `None`.
    YtChannelFetched(Option<String>),
    /// Rebuild the channel list / newest-videos list.
    ReloadChannels,
    /// A found playlist was saved into the Playlists section (count) or error.
    YtPlaylistSaved(Result<usize, String>),
    /// Progress while adding videos to the library: `done` of `total` finished.
    YtLibraryProgress {
        done: usize,
        total: usize,
    },
    /// A playlist's videos were listed → start playing them, log the playlist to
    /// "Recent", and mirror it into the Playlists section.
    YtPlaylistStart {
        url: String,
        title: String,
        items: Vec<(String, String)>,
    },
    /// Async metadata (channel/duration/cover) for an open video detail dialog.
    YtVideoMeta {
        video_id: String,
        uploader: Option<String>,
        duration: Option<i64>,
        cover: Option<String>,
    },
    /// Video(s) transcoded into the on-disk library (count) or an error → rebuild
    /// views. `video_id` is set for a single video (to clear its busy marker),
    /// `None` for a whole playlist.
    YtLibraryAdded {
        video_id: Option<String>,
        result: Result<usize, String>,
    },
    /// A single library-add hit an existing file at `dest` → ask the user whether
    /// to overwrite it (the add was not performed).
    YtLibraryExists {
        video_id: String,
        title: String,
        dest: String,
    },
    /// The videos of a (not locally mirrored) YouTube playlist were fetched → cache
    /// and open them as a song-list subpage.
    YtPlaylistSongs {
        url: String,
        title: String,
        result: Result<Vec<crate::core::youtube::YtResult>, String>,
    },
    /// The thumbnails of the open playlist-songs subpage finished pre-caching in
    /// the background → fill the pending cover frames.
    YtPlaylistCoversReady,
    /// Hits of the station search (for the open add dialog).
    StreamSearchResults(Vec<crate::core::streaming::StationResult>),
    /// Logos of the search hits are cached → redraw the hit list.
    StreamSearchCoversReady,
    /// Rebuild the station list (e.g. after logos were cached).
    ReloadStreams,
    /// Rebuild the recordings list (e.g. after a recording cover was cached).
    ReloadRecordings,
    /// Reachability of the sources (source id → reachable?).
    SourceStatus(Vec<(i64, bool)>),
    /// Cloud sources were re-indexed → rebuild views + covers. `manual` = the
    /// user pressed refresh (force online enrichment regardless of the passive
    /// auto-enrich setting); `false` = silent background top-up at startup.
    CloudReindexed {
        manual: bool,
    },
    /// All podcast feeds finished refreshing (manual refresh) → rebuild the
    /// overview and clear one slot of the refresh spinner.
    PodcastsRefreshed,
    /// All YouTube subscriptions finished refreshing (manual refresh) → rebuild
    /// the overview and clear one slot of the refresh spinner.
    ChannelsRefreshed,
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
                #[wrap(Some)]
                #[name = "split"]
                set_child = &adw::OverlaySplitView {
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
                #[wrap(Some)]
                #[name = "nav_view"]
                set_content = &adw::NavigationView {
                    // Root page: the actual content (header, tabs, mini player).
                    adw::NavigationPage {
                        set_title: "Emilia",
                        set_tag: Some("main"),
                        #[wrap(Some)]
                        #[name = "content_view"]
                        set_child = &adw::ToolbarView {
                    add_top_bar = &adw::HeaderBar {
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
                        pack_start = &gtk::Button {
                            set_icon_name: "view-refresh-symbolic",
                            set_tooltip_text: Some(&gettext("Rescan folder")),
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
                            set_tooltip_text: Some(&gettext("Device sync")),
                            connect_clicked => Msg::OpenSync,
                            #[watch]
                            set_css_classes: if model.sync_connected {
                                &["sync-connected"]
                            } else {
                                &[]
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
                            set_margin_top: 2,
                            set_margin_bottom: 2,
                        },
                    },

                    // Content with loading overlay. Desktop: a bit of space **between
                    // the title bar and the content** (top); in narrow (mobile) mode
                    // back to 0 via breakpoint (see `init`).
                    #[wrap(Some)]
                    #[name = "content_overlay"]
                    set_content = &gtk::Overlay {
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

                                        // Source tabs (linked) – only visible if, besides the
                                        // primary music folder, at least one additional source
                                        // (SD card/Nextcloud) is set up. Filled in
                                        // `rebuild_source_tabs`.
                                        #[name = "source_tabs"]
                                        gtk::Box {
                                            set_orientation: gtk::Orientation::Horizontal,
                                            set_spacing: 6,
                                            add_css_class: "linked",
                                            // Flush to the top like the Artists/Albums lists.
                                            set_margin_top: 0,
                                            // A small gap below the source tab menu.
                                            set_margin_bottom: 4,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            #[watch]
                                            set_visible: model.source_tabs_visible(),
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
                                        set_visible: model.libview.artist_count > 0 && !model.libview.gallery_view,
                                        #[local_ref]
                                        artists_box -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.artist_count > 0 && model.libview.gallery_view,
                                        #[local_ref]
                                        artists_gallery -> gtk::FlowBox {
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
                                        set_visible: model.libview.album_count > 0 && !model.libview.gallery_view,
                                        #[local_ref]
                                        albums_box -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },
                                    // Gallery variant (cover grid)
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.album_count > 0 && model.libview.gallery_view,
                                        #[local_ref]
                                        albums_gallery -> gtk::FlowBox {
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
                                        set_visible: !model.concerts.concert_items.is_empty() && !model.libview.gallery_view,
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
                                    // Gallery variant of the concerts
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.concerts.concert_items.is_empty() && model.libview.gallery_view,
                                        #[local_ref]
                                        concerts_gallery -> gtk::FlowBox {
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
                                                connect_clicked => Msg::ConcertImport,
                                            },
                                            gtk::Button {
                                                set_label: &gettext("I'll do it myself"),
                                                add_css_class: "pill",
                                                connect_clicked => Msg::ConcertDismissHint,
                                            },
                                            gtk::Button {
                                                set_label: &gettext("Hide menu item"),
                                                add_css_class: "pill",
                                                connect_clicked => Msg::ConcertHideSection,
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
                                        set_visible: !model.playlists.playlist_items.is_empty(),
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
                            add_titled_with_icon[Some("podcasts"), &gettext("Podcasts"), "microphone-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    // Header: linked tab switcher "Newest" / "Overview"
                                    // (same style as the Streaming tab) and "+" to subscribe.
                                    gtk::Box {
                                        set_orientation: gtk::Orientation::Horizontal,
                                        set_spacing: 6,
                                        set_margin_top: 2,
                                        // A bit of (sparse) space below the switches; the first
                                        // section heading thus sits ~10px higher.
                                        set_margin_bottom: 4,
                                        set_margin_start: 12,
                                        set_margin_end: 12,
                                        add_css_class: "linked",

                                        gtk::ToggleButton {
                                            set_label: &gettext("Newest"),
                                            set_hexpand: true,
                                            #[watch]
                                            set_active: model.podcasts.podcast_view == PodcastView::Newest,
                                            connect_clicked => Msg::SetPodcastView(PodcastView::Newest),
                                        },
                                        gtk::ToggleButton {
                                            set_label: &gettext("Overview"),
                                            set_hexpand: true,
                                            #[watch]
                                            set_active: model.podcasts.podcast_view == PodcastView::Overview,
                                            connect_clicked => Msg::SetPodcastView(PodcastView::Overview),
                                        },
                                        gtk::Button {
                                            set_icon_name: "list-add-symbolic",
                                            set_tooltip_text: Some(&gettext("Subscribe to podcast")),
                                            add_css_class: "flat",
                                            connect_clicked => Msg::PodcastSubscribe,
                                        },
                                    },

                                    // "Newest": newest episodes across all subscriptions.
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.podcasts.podcast_view == PodcastView::Newest && !model.podcasts.newest_items.is_empty(),
                                        #[local_ref]
                                        newest_list -> gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 6,
                                            set_valign: gtk::Align::Start,
                                            // First heading closer to the switchers (≈10px higher).
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                        },
                                    },
                                    adw::StatusPage {
                                        set_icon_name: Some("microphone-symbolic"),
                                        set_title: &gettext("No episodes"),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.podcasts.podcast_view == PodcastView::Newest && model.podcasts.newest_items.is_empty(),
                                    },

                                    // "Overview": subscribed podcasts.
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.podcasts.podcast_view == PodcastView::Overview && !model.podcasts.podcast_items.is_empty() && !model.libview.gallery_view,
                                        #[local_ref]
                                        podcasts_list -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            // 10px space down to the content (not stuck to the switcher).
                                            set_margin_top: 10,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },
                                    // Gallery variant of the subscription overview
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.podcasts.podcast_view == PodcastView::Overview && !model.podcasts.podcast_items.is_empty() && model.libview.gallery_view,
                                        #[local_ref]
                                        podcasts_gallery -> gtk::FlowBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 10,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                        },
                                    },
                                    adw::StatusPage {
                                        set_icon_name: Some("microphone-symbolic"),
                                        set_title: &gettext("No podcasts"),
                                        set_description: Some(&gettext("Subscribe to a podcast via its feed address (RSS).")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.podcasts.podcast_view == PodcastView::Overview && model.podcasts.podcast_items.is_empty(),
                                    },
                                },
                            add_titled_with_icon[Some("streaming"), &gettext("Streaming"), "audio-x-generic-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    // Tab switcher: channels / recordings + "+" for a new channel.
                                    gtk::Box {
                                        set_spacing: 6,
                                        // Same spacing above/below the tab menu as the YouTube tabs.
                                        set_margin_top: 2,
                                        set_margin_bottom: 4,
                                        set_margin_start: 12,
                                        set_margin_end: 12,
                                        add_css_class: "linked",
                                        gtk::ToggleButton {
                                            set_label: &gettext("Stations"),
                                            set_hexpand: true,
                                            #[watch]
                                            set_active: model.streaming.stream_view == StreamView::Channels,
                                            connect_clicked => Msg::SetStreamView(StreamView::Channels),
                                        },
                                        gtk::ToggleButton {
                                            set_label: &gettext("Recordings"),
                                            set_hexpand: true,
                                            #[watch]
                                            set_active: model.streaming.stream_view == StreamView::Recordings,
                                            connect_clicked => Msg::SetStreamView(StreamView::Recordings),
                                        },
                                        gtk::Button {
                                            set_icon_name: "list-add-symbolic",
                                            set_tooltip_text: Some(&gettext("Add station")),
                                            add_css_class: "flat",
                                            connect_clicked => Msg::StreamAdd,
                                        },
                                    },

                                    // Channels.
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.streaming.stream_view == StreamView::Channels && !model.streaming.stream_items.is_empty(),
                                        #[local_ref]
                                        streams_list -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            // Same gap below the tab menu as the YouTube/Channels lists.
                                            set_margin_top: 10,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },
                                    adw::StatusPage {
                                        set_icon_name: Some("audio-x-generic-symbolic"),
                                        set_title: &gettext("No stations"),
                                        set_description: Some(&gettext("Add a stream address or search for a station worldwide.")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.streaming.stream_view == StreamView::Channels && model.streaming.stream_items.is_empty(),
                                    },

                                    // Recordings.
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.streaming.stream_view == StreamView::Recordings && (!model.streaming.recording_items.is_empty() || model.streaming.record_state.is_some()),
                                        #[local_ref]
                                        recordings_list -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            // Same gap below the tab menu as the YouTube/Channels lists.
                                            set_margin_top: 10,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },
                                    adw::StatusPage {
                                        set_icon_name: Some("media-record-symbolic"),
                                        set_title: &gettext("No recordings"),
                                        set_description: Some(&gettext("Record the current song while a station plays.")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.streaming.stream_view == StreamView::Recordings && model.streaming.recording_items.is_empty() && model.streaming.record_state.is_none(),
                                    },
                                },
                            add_titled_with_icon[Some("favorites"), &gettext("Favorites"), "emilia-favorite-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.favorites.favorite_items.is_empty(),
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

                                    adw::StatusPage {
                                        set_icon_name: Some("emilia-favorite-symbolic"),
                                        set_title: &gettext("No favorites"),
                                        set_description: Some(&gettext("Mark tracks, albums or artists with the star under \"More info\".")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.favorites.favorite_items.is_empty(),
                                    },
                                },
                            add_titled_with_icon[Some("youtube"), &gettext("YouTube"), "im-youtube-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    // Warning when yt-dlp can no longer parse YouTube (the recurring
                                    // "cat and mouse" breakage); refreshed on entering the section and
                                    // after each extraction. The button opens the settings (yt-dlp update).
                                    adw::Banner {
                                        // Fully hidden (not just collapsed) when fine, so its
                                        // intrinsic min width doesn't widen the view stack on a phone.
                                        #[watch]
                                        set_visible: model.youtube.ytdlp_broken,
                                        #[watch]
                                        set_revealed: model.youtube.ytdlp_broken,
                                        set_title: &gettext("YouTube isn't working right now – update yt-dlp in the settings, or wait for a newer release."),
                                        set_button_label: Some(&gettext("Settings")),
                                        connect_button_clicked => Msg::OpenSettings,
                                    },

                                    // Header: "Newest videos" / "Channels" switcher + "+" to search/subscribe.
                                    gtk::Box {
                                        set_orientation: gtk::Orientation::Horizontal,
                                        set_spacing: 6,
                                        set_margin_top: 2,
                                        set_margin_bottom: 4,
                                        set_margin_start: 12,
                                        set_margin_end: 12,
                                        add_css_class: "linked",

                                        gtk::ToggleButton {
                                            set_label: &gettext("Recent"),
                                            set_hexpand: true,
                                            #[watch]
                                            set_active: model.youtube.yt_view == YtView::Recent,
                                            connect_clicked => Msg::SetYtView(YtView::Recent),
                                        },
                                        gtk::ToggleButton {
                                            set_label: &gettext("Newest"),
                                            set_hexpand: true,
                                            #[watch]
                                            set_active: model.youtube.yt_view == YtView::Newest,
                                            connect_clicked => Msg::SetYtView(YtView::Newest),
                                        },
                                        gtk::ToggleButton {
                                            set_label: &gettext("Subscriptions"),
                                            set_hexpand: true,
                                            #[watch]
                                            set_active: model.youtube.yt_view == YtView::Channels,
                                            connect_clicked => Msg::SetYtView(YtView::Channels),
                                        },
                                        gtk::Button {
                                            set_icon_name: "list-add-symbolic",
                                            set_tooltip_text: Some(&gettext("Search YouTube")),
                                            add_css_class: "flat",
                                            connect_clicked => Msg::YtSubscribe,
                                        },
                                    },

                                    // "Newest": newest across all subscribed channels.
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.youtube.yt_view == YtView::Newest && !model.youtube.newest_items.is_empty(),
                                        #[local_ref]
                                        yt_newest_list -> gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 6,
                                            set_valign: gtk::Align::Start,
                                            // Same gap below the tab switcher as the Channels list.
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
                                        set_visible: model.youtube.yt_view == YtView::Newest && model.youtube.newest_items.is_empty(),
                                    },

                                    // "Recent": recently played videos (history).
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.youtube.yt_view == YtView::Recent && !model.youtube.recent_items.is_empty(),
                                        #[local_ref]
                                        yt_recent_list -> gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 6,
                                            set_valign: gtk::Align::Start,
                                            // Same gap below the tab switcher as the Channels list.
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
                                        set_visible: model.youtube.yt_view == YtView::Recent && model.youtube.recent_items.is_empty(),
                                    },

                                    // "Channels": subscribed channels (list / gallery).
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.youtube.yt_view == YtView::Channels && !model.youtube.channel_items.is_empty() && !model.libview.gallery_view,
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
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.youtube.yt_view == YtView::Channels && !model.youtube.channel_items.is_empty() && model.libview.gallery_view,
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
                                        set_visible: model.youtube.yt_view == YtView::Channels && model.youtube.channel_items.is_empty(),
                                    },
                                },
                            add_titled_with_icon[Some("audiobooks"), &gettext("Audiobooks"), "emilia-audiobook-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.favorites.audiobook_items.is_empty() && !model.libview.gallery_view,
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
                                    // Gallery variant of the audiobooks
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.favorites.audiobook_items.is_empty() && model.libview.gallery_view,
                                        #[local_ref]
                                        audiobooks_gallery -> gtk::FlowBox {
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
                            set_can_target: false,
                            add_css_class: "emilia-loading",
                            #[watch]
                            set_visible: model.overlay_visible(),

                            gtk::Spinner {
                                set_spinning: true,
                                set_size_request: (48, 48),
                            },
                            gtk::Label {
                                #[watch]
                                set_label: &model.overlay_text(),
                                add_css_class: "dim-label",
                            },
                        },
                    },

                    // Mini player at the bottom with transport controls. The bar stays
                    // always visible; without a selected track only the
                    // song row (title + seek bar) is hidden and the
                    // transport buttons are grayed out.
                    add_bottom_bar = &gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 2,
                        set_margin_top: 4,
                        set_margin_bottom: 6,
                        set_margin_start: 10,
                        set_margin_end: 10,

                        gtk::Button {
                            add_css_class: "flat",
                            set_tooltip_text: Some(&gettext("Long press for details of the current track")),
                            // Place song/artist a bit lower (more compact bar).
                            set_margin_top: 5,
                            // Without a selected track, hide entirely (frees up space).
                            #[watch]
                            set_visible: model.mini.now_playing.is_some(),
                            // Long press (not a short tap) opens the track detail view –
                            // consistent with the album/artist/track rows and so an
                            // accidental tap on the bar no longer pops the detail sheet.
                            add_controller = gtk::GestureLongPress {
                                connect_pressed[sender] => move |gesture, _, _| {
                                    gesture.set_state(gtk::EventSequenceState::Claimed);
                                    sender.input(Msg::OpenNowPlaying);
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
                                    set_label: "EQ",
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
                                                    sender.input(Msg::SetPlaybackRate(s.value()));
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
                                    connect_clicked => Msg::ToggleShuffle,
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
                                    connect_clicked => Msg::Prev,
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
                                    connect_clicked => Msg::TogglePlay,
                                },
                                // Record button right next to play/pause, on the
                                // same height. Red dot; blinks during recording.
                                // Only visible when a station is running and the buffer is on.
                                gtk::Button {
                                    set_icon_name: "media-record-symbolic",
                                    set_tooltip_text: Some(&gettext("Record")),
                                    set_valign: gtk::Align::Center,
                                    #[watch]
                                    set_visible: model.streaming.playing_stream.is_some()
                                        && model.streaming.recording_buffer_minutes > 0,
                                    #[watch]
                                    set_css_classes: if model.streaming.record_state.is_some() {
                                        &["flat", "circular", "emilia-record-dot", "emilia-recording"]
                                    } else {
                                        &["flat", "circular", "emilia-record-dot"]
                                    },
                                    connect_clicked => Msg::TransportRecordToggle,
                                },
                                gtk::Button {
                                    set_icon_name: "media-skip-forward-symbolic",
                                    set_tooltip_text: Some(&gettext("Forward")),
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.mini.now_playing.is_some(),
                                    connect_clicked => Msg::Next,
                                },
                            },
                            // Bottom right: repeat (centered between "next" and the
                            // queue) and the queue.
                            #[wrap(Some)]
                            set_end_widget = &gtk::Box {
                                set_spacing: 6,
                                set_valign: gtk::Align::Center,
                                // Repeat (loop): at the end of the queue or
                                // of the single track, start over. Active = white, off = gray.
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
                                    connect_clicked => Msg::ToggleRepeat,
                                },
                                gtk::Button {
                                    set_icon_name: "list-high-priority-symbolic",
                                    set_tooltip_text: Some(&gettext("Queue")),
                                    set_valign: gtk::Align::Center,
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.mini.now_playing.is_some(),
                                    connect_clicked => Msg::ShowQueue,
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
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Make custom app icons (e.g. the concert mic) discoverable.
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::IconTheme::for_display(&display)
                .add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
            // App icon (logo.png under the app id name) for window/taskbar –
            // takes effect even without an installed .desktop file (e.g. `cargo run`).
            gtk::Window::set_default_icon_name("de.cais.Emilia");

            // Covers/photos in the album/artist list flush left (no indentation).
            let css = gtk::CssProvider::new();
            css.load_from_string(
                "row.emilia-flush > box.header { padding-left: 0px; margin-left: 0px; }\
                 row.emilia-flush > box.header > box.prefixes { margin-left: 0px; margin-right: 8px; }\
                 button.sync-connected { color: @success_color; }\
                 button.emilia-bigplay, button.emilia-record-dot { min-width: 46px; min-height: 46px; padding: 0px; }\
                 button.emilia-bigplay image, button.emilia-record-dot image { -gtk-icon-size: 34px; }\
                 button.emilia-record-dot image { color: @error_color; }\
                 image.emilia-record-dot { color: @error_color; }\
                 @keyframes emilia-blink { 0% { opacity: 1; } 50% { opacity: 0.25; } 100% { opacity: 1; } }\
                 button.emilia-recording image { animation: emilia-blink 1.1s ease-in-out infinite; }\
                 image.emilia-recording { animation: emilia-blink 1.1s ease-in-out infinite; }\
                 button.emilia-nav-btn:checked image { color: @accent_color; }\
                 box.emilia-step { background-color: alpha(@window_fg_color, 0.12); border-radius: 999px; }\
                 box.emilia-step label { font-weight: bold; }\
                 box.emilia-step-active { background-color: @accent_bg_color; }\
                 box.emilia-step-active label { color: @accent_fg_color; }\
                 scrolledwindow.emilia-nav-scroller scrollbar { opacity: 0; min-width: 0px; min-height: 0px; }\
                 scrolledwindow.emilia-nav-scroller button.emilia-nav-btn { padding-left: 6px; padding-right: 6px; min-width: 0px; }\
                 image.emilia-offline { color: white; background-color: @error_color; border-radius: 999px; padding: 2px; margin: 2px; }\
                 box.emilia-loading { background-color: alpha(@window_bg_color, 0.85); border-radius: 18px; padding: 22px 30px; }\
                 progressbar.emilia-hourbar, progressbar.emilia-hourbar > trough, progressbar.emilia-hourbar > trough > progress { min-width: 0px; }\
                 label.emilia-gallery-title { background-color: alpha(black, 0.55); color: white; padding: 3px 8px; border-bottom-left-radius: 6px; border-bottom-right-radius: 6px; }\
                 flowbox.emilia-gallery > flowboxchild { padding: 0px; border-radius: 6px; }\
                 flowbox.emilia-gallery > flowboxchild:selected { background: none; }",
            );
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

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
        // Apply the color scheme (default: system) immediately.
        apply_color_scheme(
            library
                .get_setting("color_scheme")
                .ok()
                .flatten()
                .as_deref()
                .unwrap_or("system"),
        );
        let music_dir = library.get_setting("music_dir").ok().flatten();
        let root_dir = music_dir.as_ref().map(PathBuf::from);
        // Restore the most recently opened folder – only if it still exists
        // and lies under the start folder; otherwise the start folder itself.
        let browse_dir = library
            .get_setting("browse_dir")
            .ok()
            .flatten()
            .map(PathBuf::from)
            .filter(|p| root_dir.as_ref().is_some_and(|r| p.starts_with(r)) && p.is_dir())
            .or_else(|| root_dir.clone());

        // Additional music sources (local secondary folder / Nextcloud) for the tabs.
        let sources = library.list_sources().unwrap_or_default();

        // First-run setup: shown once when nothing is configured yet. Existing
        // installations (a music folder or sources already set) are silently
        // marked complete instead, so the assistant never appears for them.
        let setup_done = matches!(
            library.get_setting("setup_complete").ok().flatten().as_deref(),
            Some("1")
        );
        let first_run = !setup_done && music_dir.is_none() && sources.is_empty();
        if !setup_done && !first_run {
            let _ = library.set_setting("setup_complete", "1");
        }

        // Most recently saved window size / maximization.
        let saved_w = library
            .get_setting("win_width")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i32>().ok());
        let saved_h = library
            .get_setting("win_height")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i32>().ok());
        let saved_max = matches!(
            library
                .get_setting("win_maximized")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        // Concert options.
        let concert_hint_dismissed = matches!(
            library
                .get_setting("concert_hint_dismissed")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        // Hidden menu items (comma-separated). The old key
        // "concerts_hidden=1" is still honored.
        let mut hidden_sections: std::collections::HashSet<String> = library
            .get_setting("hidden_sections")
            .ok()
            .flatten()
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if matches!(
            library
                .get_setting("concerts_hidden")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        ) {
            hidden_sections.insert("concerts".to_string());
        }
        // YouTube is an opt-in feature (off by default, restricted in some
        // countries, extractor not bundled). When disabled, hide its section –
        // toggling the setting adds/removes "youtube" from `hidden_sections`.
        let youtube_enabled = matches!(
            library
                .get_setting("youtube_enabled")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        if !youtube_enabled {
            hidden_sections.insert("youtube".to_string());
        }
        // Menu order (comma-separated stack names). Unknown names are
        // discarded, new sections appended at the end in default order – so
        // future menu items appear automatically.
        let mut section_order: Vec<&'static str> = library
            .get_setting("section_order")
            .ok()
            .flatten()
            .map(|s| {
                s.split(',')
                    .filter_map(|name| {
                        SECTIONS
                            .iter()
                            .find(|(n, _, _)| *n == name.trim())
                            .map(|(n, _, _)| *n)
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (name, _, _) in SECTIONS {
            if !section_order.contains(&name) {
                section_order.push(name);
            }
        }
        // Automatic online fetch (default: on; only "0" turns it off).
        let auto_enrich = !matches!(
            library.get_setting("auto_enrich").ok().flatten().as_deref(),
            Some("0")
        );
        // Repeat state (default: off).
        let repeat_on = matches!(
            library.get_setting("repeat").ok().flatten().as_deref(),
            Some("1")
        );
        // Display language (default: system locale). It already took effect
        // at startup in `main` via `i18n::init`; here only for the display in
        // the settings switcher.
        let ui_language = library
            .get_setting("ui_language")
            .ok()
            .flatten()
            .unwrap_or_else(|| "system".to_string());
        // Gallery view (default: off) and tiles/row (default: 3 mobile / 4 desktop).
        let gallery_view = matches!(
            library
                .get_setting("gallery_view")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        // Tiles per row (2–8). Initial default depends on the form factor:
        // 3 on phone-sized screens, 4 on the desktop (see `initial_gallery_columns`).
        let gallery_columns = library
            .get_setting("gallery_columns")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or_else(initial_gallery_columns)
            .clamp(2, 8);
        // Timeshift buffer for stations in minutes (default 5, 0 = off, max. 60).
        let recording_buffer_minutes = library
            .get_setting("recording_buffer_minutes")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(5)
            .min(60);
        // Most recently open navigation item (only allow valid section names).
        let saved_section = library
            .get_setting("active_section")
            .ok()
            .flatten()
            .filter(|s| SECTIONS.iter().any(|(name, _, _)| name == s));

        let entries = FactoryVecDeque::builder()
            .launch(gtk::ListBox::default())
            .forward(sender.input_sender(), |out| match out {
                FsOutput::Activated(index) => Msg::Activate(index.current_index()),
                FsOutput::LongPress(index) => Msg::ShowContextMenu(index.current_index()),
                FsOutput::DoubleClick(index) => Msg::ToggleQueue(index.current_index()),
            });

        let albums = FactoryVecDeque::builder()
            .launch(gtk::ListBox::default())
            .forward(sender.input_sender(), |out| match out {
                AlbumOutput::Activated(index) => Msg::ShowAlbumTracks(index.current_index()),
                AlbumOutput::LongPress(index) => Msg::ShowAlbumDetail(index.current_index()),
            });

        let artists = FactoryVecDeque::builder()
            .launch(gtk::ListBox::default())
            .forward(sender.input_sender(), |out| match out {
                ArtistOutput::Activated(index) => Msg::OpenArtistTracks(index.current_index()),
                ArtistOutput::LongPress(index) => Msg::ShowArtistDetail(index.current_index()),
            });

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
                    move || sender.input(Msg::TrackFinished)
                },
                {
                    let sender = sender.clone();
                    move |title| sender.input(Msg::StreamTitle(title))
                },
                {
                    let sender = sender.clone();
                    move || sender.input(Msg::PlaybackError)
                },
                move || sender.input(Msg::PlaybackReady),
            );
        }

        // During playback, regularly save the resume position, so that
        // an audio drama also resumes there after a crash/close.
        {
            let sender = sender.clone();
            gtk::glib::timeout_add_seconds_local(5, move || {
                sender.input(Msg::PersistResume);
                gtk::glib::ControlFlow::Continue
            });
        }

        // Per-second tick for the seek bar (update position/duration).
        {
            let sender = sender.clone();
            gtk::glib::timeout_add_seconds_local(1, move || {
                sender.input(Msg::Tick);
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
            sender.input(Msg::CheckSources);
            gtk::glib::timeout_add_seconds_local(45, move || {
                sender.input(Msg::CheckSources);
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
        let podcasts_list = gtk::ListBox::new();
        let streams_list = gtk::ListBox::new();
        let recordings_list = gtk::ListBox::new();
        let newest_list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let yt_channels_list = gtk::ListBox::new();
        let yt_newest_list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let yt_recent_list = gtk::Box::new(gtk::Orientation::Vertical, 6);
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
                crate::ui::cloud_page::CloudOutput::SourcesChanged => Msg::SourcesChanged,
                crate::ui::cloud_page::CloudOutput::Indexed => Msg::CloudIndexed,
            });
        let setup_page = crate::ui::setup::SetupPage::builder()
            .launch(())
            .forward(sender.input_sender(), |out| match out {
                crate::ui::setup::SetupOutput::Finished {
                    lang_code,
                    music_dir,
                    enabled_sections,
                } => Msg::SetupFinished {
                    lang_code,
                    music_dir,
                    enabled_sections,
                },
            });

        let mut model = App {
            library,
            player,
            mpris,
            input: sender.input_sender().clone(),
            libview: LibView {
                entries,
                albums,
                albums_gallery: gtk::FlowBox::new(),
                albums_overview: Vec::new(),
                album_count: 0,
                artists,
                artists_gallery: gtk::FlowBox::new(),
                artists_overview: Vec::new(),
                artist_count: 0,
                gallery_view,
                gallery_columns,
                loading: false,
                loading_label: None,
                gallery_tried: std::cell::RefCell::new(std::collections::HashSet::new()),
                gallery_hooked: std::cell::RefCell::new(std::collections::HashSet::new()),
            },
            refresh_pending: 0,
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
                last_prev: None,
                interrupted_queue: None,
                nav_stack: Vec::new(),
                prev_ctx: None,
                playing_path: None,
                close_resume: std::rc::Rc::new(std::cell::RefCell::new(None)),
                play_session: None,
                close_session: std::rc::Rc::new(std::cell::RefCell::new(None)),
                queue_list: queue_list.clone(),
                skip_count: 0,
                forced_start_ms: None,
            },
            mini: MiniState {
                now_playing: None,
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
            toast_overlay: toast_overlay.clone(),
            concerts: ConcertsState {
                concert_items: Vec::new(),
                concerts_list: concerts_list.clone(),
                concerts_gallery: gtk::FlowBox::new(),
                concert_hint_dismissed,
            },
            favorites: FavoritesState {
                favorite_items: Vec::new(),
                favorites_list: favorites_list.clone(),
                audiobook_items: Vec::new(),
                audiobooks_list: audiobooks_list.clone(),
                audiobooks_gallery: gtk::FlowBox::new(),
            },
            playlists: PlaylistsState {
                playlist_items: Vec::new(),
                playlists_list: playlists_list.clone(),
            },
            podcasts: PodcastsState {
                podcast_items: Vec::new(),
                podcasts_list: podcasts_list.clone(),
                podcasts_gallery: gtk::FlowBox::new(),
                podcast_view: PodcastView::Newest,
                newest_items: Vec::new(),
                newest_list: newest_list.clone(),
                podcast_search_results: Vec::new(),
                podcast_search: std::rc::Rc::new(std::cell::RefCell::new(None)),
                playing_episode_url: None,
                episode_play_buttons: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
                ctx_episode_play: std::rc::Rc::new(std::cell::RefCell::new(None)),
                ctx_episode_download: std::rc::Rc::new(std::cell::RefCell::new(None)),
                downloading_episodes: std::collections::HashSet::new(),
            },
            streaming: StreamingState {
                stream_view: StreamView::Channels,
                stream_items: Vec::new(),
                streams_list: streams_list.clone(),
                stream_search_results: Vec::new(),
                stream_search: std::rc::Rc::new(std::cell::RefCell::new(None)),
                playing_stream: None,
                stream_title: None,
                recorder: None,
                record_state: None,
                recording_buffer_minutes,
                recording_items: Vec::new(),
                recordings_list: recordings_list.clone(),
                stream_play_buttons: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
                rec_play_buttons: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            },
            youtube: YoutubeState {
                enabled: youtube_enabled,
                ytdlp_version: None,
                settings_status: std::rc::Rc::new(std::cell::RefCell::new(None)),
                settings_dl_btn: std::rc::Rc::new(std::cell::RefCell::new(None)),
                ytdlp_busy: false,
                ytdlp_broken: false,
                yt_view: YtView::Recent,
                channel_items: Vec::new(),
                channels_list: yt_channels_list.clone(),
                channels_gallery: gtk::FlowBox::new(),
                newest_items: Vec::new(),
                newest_list: yt_newest_list.clone(),
                recent_items: Vec::new(),
                recent_list: yt_recent_list.clone(),
                search_results: Vec::new(),
                search: std::rc::Rc::new(std::cell::RefCell::new(None)),
                playing_video_id: None,
                video_play_buttons: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
                ctx_video_play: std::rc::Rc::new(std::cell::RefCell::new(None)),
                ctx_video_download: std::rc::Rc::new(std::cell::RefCell::new(None)),
                ctx_video_meta: std::rc::Rc::new(std::cell::RefCell::new(None)),
                downloading_videos: std::collections::HashSet::new(),
                video_titles: std::collections::HashMap::new(),
                playing_playlist: false,
                progress_toast: std::rc::Rc::new(std::cell::RefCell::new(None)),
                playlist_songs_cache: std::collections::HashMap::new(),
                pl_cover_slots: Vec::new(),
            },
            settings_src_list: std::rc::Rc::new(std::cell::RefCell::new(None)),
            offline_sources: std::collections::HashSet::new(),
            stats_page,
            nav: NavState {
                split: adw::OverlaySplitView::new(),
                view_stack: adw::ViewStack::new(),
                nav_view: adw::NavigationView::new(),
                sidebar_nav: gtk::Box::new(gtk::Orientation::Vertical, 0),
                top_nav: gtk::Box::new(gtk::Orientation::Horizontal, 0),
                nav_buttons: Vec::new(),
                section_order,
                hidden_sections,
                context_target: None,
                ctx_play: std::rc::Rc::new(std::cell::RefCell::new(None)),
                overview_scroll: std::rc::Rc::new(std::cell::RefCell::new(None)),
                narrow: std::rc::Rc::new(std::cell::Cell::new(false)),
                nav_hidden: std::rc::Rc::new(std::cell::Cell::new(false)),
                apply_chrome: std::rc::Rc::new(|| {}),
            },
            sync_page,
            sync_connected: false,
            cloud_page,
            setup_page,
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
        model.reload_podcasts(&sender);
        model.reload_streams(&sender);
        model.reload_recordings(&sender);
        // (Statistics build themselves in the StatsPage component's init.)
        // Cache the podcast feed images once in the background, then rebuild
        // the list so that the covers appear (no UI block at startup).
        sender.spawn_oneshot_command(|| {
            if let Ok(lib) = Library::open() {
                for (_, _, image, _) in lib.podcasts().unwrap_or_default() {
                    if let Some(url) = image {
                        crate::core::online::cache_podcast_image(&url);
                    }
                }
            }
            Cmd::ReloadPodcasts
        });
        // Likewise cache the station logos once in the background.
        sender.spawn_oneshot_command(|| {
            if let Ok(lib) = Library::open() {
                for st in lib.streams().unwrap_or_default() {
                    if let Some(url) = st.favicon {
                        crate::core::online::cache_station_image(&url);
                    }
                }
            }
            Cmd::ReloadStreams
        });
        // YouTube (optional, opt-in): load subscribed channels, and – on a
        // connection – verify/refresh yt-dlp and the newest videos in the
        // background. yt-dlp is re-fetched once per new app version (YouTube
        // changes frequently break older versions).
        if model.youtube.enabled {
            model.reload_channels(&sender);
            let online = online_available();
            sender.spawn_oneshot_command(move || {
                let Ok(lib) = Library::open() else {
                    return Cmd::ReloadChannels;
                };
                let prev = lib.get_setting("yt_dlp_app_version").ok().flatten();
                let cur = env!("CARGO_PKG_VERSION");
                if online && crate::core::youtube::available() && prev.as_deref() != Some(cur) {
                    let _ = crate::core::youtube::update_ytdlp();
                }
                let _ = lib.set_setting("yt_dlp_app_version", cur);
                if online && crate::core::youtube::available() {
                    for (id, title, url, thumb, _) in lib.channels().unwrap_or_default() {
                        if let Some(t) = thumb.as_deref() {
                            crate::core::online::cache_youtube_thumb(t);
                        }
                        let _ = crate::ui::app_youtube::refresh_channel_videos(id, &title, &url);
                    }
                }
                Cmd::ReloadChannels
            });
        }
        // Automatically read the library at startup and – on Wi-Fi/LAN and
        // with the switch enabled – fetch missing covers/metadata in the background.
        model.start_scan(&sender, true, false);
        // Also check the remote sources for new content in the background (silent,
        // non-manual: respects the auto-enrich setting). Skipped when offline so a
        // launch without a connection does not spin up a pointless re-index worker.
        if online_available() {
            model.reindex_cloud_sources(&sender, false);
        }

        let entries_box = model.libview.entries.widget();
        let albums_box = model.libview.albums.widget();
        let artists_box = model.libview.artists.widget();
        let albums_gallery = model.libview.albums_gallery.clone();
        let artists_gallery = model.libview.artists_gallery.clone();
        let concerts_gallery = model.concerts.concerts_gallery.clone();
        let audiobooks_gallery = model.favorites.audiobooks_gallery.clone();
        let podcasts_gallery = model.podcasts.podcasts_gallery.clone();
        let yt_channels_gallery = model.youtube.channels_gallery.clone();
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
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, root: &Self::Root) {
        match msg {
            Msg::Activate(index) => {
                let entry = self
                    .libview
                    .entries
                    .guard()
                    .get(index)
                    .map(|r| r.entry.clone());
                let Some(entry) = entry else {
                    return;
                };
                // Remote entries (Nextcloud) go through their own path.
                if let crate::ui::fs_row::FsEntry::RemoteDir { rel_path, .. } = &entry {
                    self.files.remote_browse = Some(rel_path.clone());
                    self.load_dir(&sender);
                    return;
                }
                if let crate::ui::fs_row::FsEntry::RemoteFile { rel_path, .. } = &entry {
                    let rel = rel_path.clone();
                    self.activate_remote(&rel);
                    return;
                }
                {
                    if entry.is_dir() {
                        let Some(p) = entry.path().cloned() else {
                            return;
                        };
                        self.files.browse_dir = Some(p);
                        self.load_dir(&sender);
                    } else {
                        let Some(path) = entry.path().cloned() else {
                            return;
                        };
                        // Tapping the active song again → toggle playback
                        // (pause/resume), instead of restarting.
                        if !self.toggle_if_active_file(&path) {
                            // Is a real queue currently running? Then slip the
                            // single song in between and resume the queue
                            // afterwards at its spot (it stays intact).
                            if self.mini.playing
                                && self.transport.queue.len() > 1
                                && self.transport.interrupted_queue.is_none()
                            {
                                self.transport.interrupted_queue =
                                    Some((self.transport.queue.clone(), self.transport.queue_pos));
                            }
                            self.transport.queue = vec![path];
                            self.transport.queue_pos = 0;
                            self.play_current();
                            self.refresh_queue_icons();
                        }
                    }
                }
            }
            Msg::ToggleQueue(index) => {
                // Local files use their path, remote (NC) files their synthetic
                // nc: path (resolved via `entry_files`), so both can be queued.
                let entry = self
                    .libview
                    .entries
                    .guard()
                    .get(index)
                    .filter(|r| !r.entry.is_dir())
                    .map(|r| r.entry.clone());
                let path = entry.and_then(|e| self.entry_files(&e).into_iter().next());
                if let Some(path) = path {
                    // Toggle membership in the user queue (never the active
                    // context): a second tap removes it again.
                    if let Some(pos) = self.transport.user_queue.iter().position(|p| *p == path) {
                        self.transport.user_queue.remove(pos);
                        self.toast(&gettext("Removed from queue"));
                    } else {
                        self.transport.user_queue.push(path);
                        self.toast(&gettext("Will play next"));
                    }
                    self.reload_queue_list();
                    self.refresh_queue_icons();
                    self.save_queue();
                }
            }
            Msg::ShowContextMenu(index) => {
                let entry = self
                    .libview
                    .entries
                    .guard()
                    .get(index)
                    .map(|r| CtxTarget::Fs(r.entry.clone()));
                if entry.is_some() {
                    self.nav.context_target = entry;
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::ShowArtistDetail(index) => {
                let meta = self
                    .libview
                    .artists
                    .guard()
                    .get(index)
                    .map(|c| c.meta.clone())
                    .or_else(|| self.libview.artists_overview.get(index).cloned());
                if let Some(meta) = meta {
                    // Fetch the photo of the opened artist with priority.
                    self.fetch_focus_artist(&sender, &meta.name);
                    self.nav.context_target = Some(CtxTarget::Artist(meta));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::ShowAlbumDetail(index) => {
                let meta = self
                    .libview
                    .albums
                    .guard()
                    .get(index)
                    .map(|c| c.meta.clone())
                    .or_else(|| self.libview.albums_overview.get(index).cloned());
                if let Some(meta) = meta {
                    // Fetch the cover of the opened album with priority.
                    self.fetch_focus_album(&sender, &meta.artist, &meta.album);
                    self.nav.context_target = Some(CtxTarget::Album(meta));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::ShowAlbumDetailFor { artist, album } => {
                self.fetch_focus_album(&sender, &artist, &album);
                // Load album metadata (for cover/year), otherwise an empty entry.
                let meta = self
                    .library
                    .get_album_meta(&artist, &album)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| crate::model::AlbumMeta::pending(artist, album));
                let mut meta = meta;
                if meta
                    .cover_path
                    .as_deref()
                    .is_none_or(|p| p.trim().is_empty())
                {
                    meta.cover_path = self.album_cover_for(&meta.artist, &meta.album);
                }
                self.nav.context_target = Some(CtxTarget::Album(meta));
                self.open_context_menu(root, &sender);
            }
            Msg::ShowTrackDetail(path) => {
                self.nav.context_target = Some(CtxTarget::Fs(FsEntry::file(PathBuf::from(path))));
                self.open_context_menu(root, &sender);
            }
            Msg::ShowAlbumTracks(index) => {
                // Album overview: open by album name (artist irrelevant).
                let album = self
                    .libview
                    .albums
                    .guard()
                    .get(index)
                    .map(|c| c.meta.album.clone())
                    .or_else(|| {
                        self.libview
                            .albums_overview
                            .get(index)
                            .map(|m| m.album.clone())
                    });
                if let Some(album) = album {
                    self.open_album_by_name(&sender, &album);
                }
            }
            Msg::ShowConcertDetail(index) => {
                if let Some((scope, key, _, is_dir)) =
                    self.concerts.concert_items.get(index).cloned()
                {
                    self.nav.context_target = Some(self.entry_target(&scope, &key, is_dir));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::OpenArtistTracks(index) => {
                let meta = self
                    .libview
                    .artists
                    .guard()
                    .get(index)
                    .map(|c| c.meta.clone())
                    .or_else(|| self.libview.artists_overview.get(index).cloned());
                if let Some(meta) = meta {
                    // Fetch the photo of the opened artist with priority.
                    self.fetch_focus_artist(&sender, &meta.name);
                    self.open_artist_tracks(&sender, &meta);
                }
            }
            Msg::OpenAlbumTracks { artist, album } => {
                self.fetch_focus_album(&sender, &artist, &album);
                self.open_album_tracks(&sender, &artist, &album);
            }
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
            } => {
                // Re-tapping the song that is already playing toggles
                // pause/resume instead of restarting it.
                if self.toggle_if_active_file(&PathBuf::from(&path)) {
                    return;
                }
                let files: Vec<PathBuf> = self
                    .folder_tracks_ordered(&folder)
                    .into_iter()
                    .map(|t| PathBuf::from(t.path))
                    .collect();
                let target = PathBuf::from(&path);
                if let Some(pos) = files.iter().position(|p| *p == target) {
                    self.transport.queue = files;
                    self.transport.queue_pos = pos;
                    self.play_current();
                    self.refresh_queue_icons();
                    if close {
                        self.nav.nav_view.pop_to_tag("main");
                    }
                }
            }
            Msg::PlayArtistTrack { name, path, close } => {
                // Re-tapping the song that is already playing toggles
                // pause/resume instead of restarting it.
                if self.toggle_if_active_file(&PathBuf::from(&path)) {
                    return;
                }
                // Queue = all tracks of the artist (across albums),
                // start at the tapped track.
                let files: Vec<PathBuf> = self
                    .artist_albums(&name)
                    .into_iter()
                    .flat_map(|(_, tracks)| tracks)
                    .map(|t| PathBuf::from(t.path))
                    .collect();
                let target = PathBuf::from(&path);
                if let Some(pos) = files.iter().position(|p| *p == target) {
                    self.transport.queue = files;
                    self.transport.queue_pos = pos;
                    self.play_current();
                    self.refresh_queue_icons();
                    // Back to the main page, so that the mini player is visible.
                    if close {
                        self.nav.nav_view.pop_to_tag("main");
                    }
                }
            }
            Msg::PlayOneTrack { path, close } => {
                // Re-tapping the song that is already playing toggles
                // pause/resume instead of restarting it.
                if self.toggle_if_active_file(&PathBuf::from(&path)) {
                    return;
                }
                // Selecting a single track (album or playlist) plays *only* that
                // track – its siblings are not enqueued. Use the album/playlist
                // play button for the whole thing. A single play is logged to
                // "Recent" like any other standalone track.
                self.youtube.playing_playlist = false;
                self.transport.queue = vec![PathBuf::from(&path)];
                self.transport.queue_pos = 0;
                self.play_current();
                self.refresh_queue_icons();
                if close {
                    self.nav.nav_view.pop_to_tag("main");
                }
            }
            Msg::PlayAlbum { artist, album } => {
                // Whole album from track 1 in track order (shuffle off).
                let files: Vec<PathBuf> = self
                    .album_tracks_for_artist(&artist, &album)
                    .into_iter()
                    .map(|t| PathBuf::from(t.path))
                    .collect();
                if !files.is_empty() {
                    self.transport.shuffle = false;
                    self.transport.queue = files;
                    self.transport.queue_pos = 0;
                    self.play_current();
                    self.refresh_queue_icons();
                    self.nav.nav_view.pop_to_tag("main");
                }
            }
            Msg::CtxPlay => {
                if let Some(entry) = self.nav.context_target.clone() {
                    let files = self.ctx_files(&entry);
                    if !files.is_empty() {
                        self.transport.queue = files;
                        self.transport.queue_pos = 0;
                        self.play_current();
                        self.refresh_queue_icons();
                    }
                }
            }
            Msg::CtxPlayAlbum => {
                // Album always in track order from song 1, without shuffle; at the end
                // of the queue `play_next` stops by itself (no further song).
                if let Some((artist, album)) = self.ctx_album() {
                    let files = self.album_files(&artist, &album);
                    if !files.is_empty() {
                        self.transport.shuffle = false;
                        self.transport.queue = files;
                        self.transport.queue_pos = 0;
                        self.play_current();
                        self.refresh_queue_icons();
                    }
                }
            }
            Msg::CtxPlayArtist { newest_first } => {
                // Albums by year (oldest/newest first), each album top-down,
                // without shuffle.
                if let Some(name) = self.ctx_artist() {
                    let files = self.artist_files_ordered(&name, newest_first);
                    if !files.is_empty() {
                        self.transport.shuffle = false;
                        self.transport.queue = files;
                        self.transport.queue_pos = 0;
                        self.play_current();
                        self.refresh_queue_icons();
                    }
                }
            }
            Msg::CtxAddQueue => {
                if let Some(entry) = self.nav.context_target.clone() {
                    let mut files = self.ctx_files(&entry);
                    let n = files.len();
                    // Explicit enqueue: append to the user queue, never the active
                    // context. Playback is untouched; the tracks play next, ahead
                    // of the rest of the running album.
                    self.transport.user_queue.append(&mut files);
                    self.reload_queue_list();
                    self.refresh_queue_icons();
                    self.save_queue();
                    self.toast(&gettext_f(
                        "Added {n} tracks to the queue",
                        &[("n", &n.to_string())],
                    ));
                }
            }
            Msg::CtxAddPlaylist => self.open_add_to_playlist_dialog(root, &sender),
            Msg::PlaylistCreateAddTo(name) => {
                let name = name.trim();
                if !name.is_empty() {
                    if let Ok(id) = self.library.create_playlist(name) {
                        self.add_context_to_playlist(id, &sender);
                    }
                }
            }
            Msg::PlaylistAddTo(id) => self.add_context_to_playlist(id, &sender),
            Msg::OpenPlaylist(id) => {
                if let Some((_, name, _)) = self
                    .playlists
                    .playlist_items
                    .iter()
                    .find(|(pid, _, _)| *pid == id)
                    .cloned()
                {
                    self.open_playlist(&sender, id, &name);
                }
            }
            Msg::ShowPlaylistDetail(id) => {
                if let Some((_, name, _)) = self
                    .playlists
                    .playlist_items
                    .iter()
                    .find(|(pid, _, _)| *pid == id)
                    .cloned()
                {
                    self.open_playlist_detail(root, &sender, id, &name);
                }
            }
            Msg::PlayPlaylist(id) => {
                let paths = self.library.playlist_paths(id).unwrap_or_default();
                if !paths.is_empty() {
                    self.transport.queue = paths.into_iter().map(PathBuf::from).collect();
                    self.transport.queue_pos = 0;
                    self.transport.shuffle = false;
                    self.play_current();
                    self.refresh_queue_icons();
                }
            }
            Msg::PlayPlaylistShuffled(id) => {
                let paths = self.library.playlist_paths(id).unwrap_or_default();
                if !paths.is_empty() {
                    let len = paths.len();
                    self.transport.queue = paths.into_iter().map(PathBuf::from).collect();
                    // Random start, then a fresh random order over the rest.
                    self.transport.queue_pos = gtk::glib::random_int_range(0, len as i32) as usize;
                    self.transport.shuffle = true;
                    self.rebuild_shuffle_order();
                    self.play_current();
                    self.refresh_queue_icons();
                }
            }
            Msg::SetPlaylistCover { id, path } => {
                let _ = self.library.set_playlist_cover(id, &path);
                self.reload_playlists(&sender);
            }
            Msg::PlaylistDelete(id) => {
                self.undo_toast(
                    &sender,
                    &gettext("Playlist deleted"),
                    Msg::PlaylistDeleteConfirmed(id),
                );
            }
            Msg::PlaylistDeleteConfirmed(id) => {
                let _ = self.library.delete_playlist(id);
                self.reload_playlists(&sender);
            }
            Msg::PlaylistRenameDialog(id) => self.open_rename_playlist_dialog(root, &sender, id),
            Msg::PlaylistRename { id, name } => {
                let name = name.trim();
                if !name.is_empty() {
                    let _ = self.library.rename_playlist(id, name);
                    self.reload_playlists(&sender);
                }
            }
            Msg::PodcastSubscribe => self.open_subscribe_podcast_dialog(root, &sender),
            Msg::PodcastSearch(term) => {
                let term = term.trim().to_string();
                if !term.is_empty() {
                    self.toast(&gettext("Searching …"));
                    sender.spawn_command(move |out| {
                        let results =
                            crate::core::podcast::search_podcasts(&term).unwrap_or_default();
                        // Show hits immediately (still without covers) …
                        let _ = out.send(Cmd::PodcastSearchResults(results.clone()));
                        // … and fetch the cover thumbnails afterwards in the background.
                        for r in &results {
                            if let Some(img) = r.image_url.as_deref() {
                                crate::core::online::cache_podcast_image(img);
                            }
                        }
                        let _ = out.send(Cmd::PodcastSearchCoversReady);
                    });
                }
            }
            Msg::PodcastSubscribeUrl(url) => {
                let url = url.trim().to_string();
                if !url.is_empty() {
                    self.toast(&gettext("Loading feed …"));
                    sender.spawn_command(move |out| {
                        let _ = out.send(Cmd::PodcastFetched(fetch_and_store_podcast(&url)));
                    });
                }
            }
            Msg::PodcastRefresh(id) => {
                if let Ok(Some(url)) = self.library.podcast_feed_url(id) {
                    self.toast(&gettext("Updating feed …"));
                    sender.spawn_command(move |out| {
                        let _ = out.send(Cmd::PodcastFetched(fetch_and_store_podcast(&url)));
                    });
                }
            }
            Msg::OpenPodcast(id) => {
                if let Some((_, title, _, _)) = self
                    .podcasts
                    .podcast_items
                    .iter()
                    .find(|(pid, _, _, _)| *pid == id)
                    .cloned()
                {
                    self.open_podcast(&sender, id, &title);
                }
            }
            Msg::OpenPodcastAt(index) => {
                if let Some(id) = self.podcasts.podcast_items.get(index).map(|p| p.0) {
                    sender.input(Msg::OpenPodcast(id));
                }
            }
            Msg::ShowPodcastDetailAt(index) => {
                if let Some(id) = self.podcasts.podcast_items.get(index).map(|p| p.0) {
                    sender.input(Msg::ShowPodcastDetail(id));
                }
            }
            Msg::PodcastDelete(id) => {
                self.undo_toast(
                    &sender,
                    &gettext("Podcast removed"),
                    Msg::PodcastDeleteConfirmed(id),
                );
            }
            Msg::PodcastDeleteConfirmed(id) => {
                let _ = self.library.delete_podcast(id);
                self.reload_podcasts(&sender);
            }
            // --- Streaming (internet radio) ---
            Msg::StreamAdd => self.open_add_stream_dialog(root, &sender),
            Msg::StreamSearch(term) => self.stream_search(&sender, term),
            Msg::StreamAddResult(index) => self.add_stream_result(&sender, index),
            Msg::StreamAddUrl(url) => self.stream_add_url(&sender, url),
            Msg::ToggleStream(id) => self.toggle_stream(id),
            Msg::StreamRecordToggle(id) => self.stream_record_toggle(&sender, id),
            Msg::TransportRecordToggle => {
                if let Some(id) = self.streaming.playing_stream {
                    sender.input(Msg::StreamRecordToggle(id));
                }
            }
            Msg::StreamTitle(title) => self.stream_title(title),
            Msg::OpenStream(id) => self.open_stream(root, &sender, id),
            Msg::StreamRenameDialog(id) => self.open_rename_stream_dialog(root, &sender, id),
            Msg::StreamRename { id, name } => {
                let name = name.trim();
                if !name.is_empty() {
                    let _ = self.library.rename_stream(id, name);
                    self.reload_streams(&sender);
                }
            }
            Msg::StreamDelete(id) => {
                self.undo_toast(
                    &sender,
                    &gettext("Station removed"),
                    Msg::StreamDeleteConfirmed(id),
                );
            }
            Msg::StreamDeleteConfirmed(id) => self.stream_delete_confirmed(&sender, id),
            // --- Recording (timeshift) ---
            Msg::RecordStop => {
                if self.streaming.record_state.is_some() {
                    // Finalize the song still in progress so it isn't lost.
                    self.finalize_recording(&sender);
                    self.streaming.record_state = None;
                    self.toast(&gettext("Recording stopped"));
                    self.reload_recordings(&sender);
                }
            }
            Msg::OpenStreamReplay(id) => self.open_stream_replay(&sender, id),
            Msg::ReplayPlay { start, end } => self.replay_play(start, end),
            Msg::ReplaySave { start, end, title } => self.replay_save(&sender, start, end, title),
            Msg::PlayRecording(path) => self.play_recording(path),
            Msg::OpenRecording(id) => self.open_recording(root, &sender, id),
            Msg::RecordingDelete(id) => {
                self.undo_toast(
                    &sender,
                    &gettext("Recording deleted"),
                    Msg::RecordingDeleteConfirmed(id),
                );
            }
            Msg::RecordingDeleteConfirmed(id) => {
                if let Ok(Some(path)) = self.library.delete_recording(id) {
                    let _ = std::fs::remove_file(&path);
                }
                self.reload_recordings(&sender);
            }
            Msg::AddRecordingToLibrary(id) => self.add_recording_to_library(id),
            Msg::EditRecording(id) => self.open_recording_edit(&sender, id),
            Msg::RecordingPlayFrom { path, ms } => {
                self.transport.forced_start_ms = Some(ms);
                self.play_recording(path);
            }
            Msg::RecordingPreviewPause => {
                if self.mini.playing {
                    self.player.pause();
                    self.mini.playing = false;
                    self.mpris.set_playing(false);
                    self.refresh_queue_icons();
                }
            }
            Msg::RecordingApplyCut { id, cuts } => self.apply_recording_cut(&sender, id, cuts),
            Msg::RecordingCutDone {
                id,
                path,
                duration_ms,
            } => match path {
                Some(p) => {
                    let _ = self.library.update_recording_file(id, &p, duration_ms);
                    self.nav.nav_view.pop();
                    self.reload_recordings(&sender);
                    self.toast(&gettext("Recording edited"));
                }
                None => self.toast(&gettext("Editing the recording failed")),
            },
            Msg::SetRecordingBufferMinutes(n) => {
                self.streaming.recording_buffer_minutes = n.min(60);
                let _ = self.library.set_setting(
                    "recording_buffer_minutes",
                    &self.streaming.recording_buffer_minutes.to_string(),
                );
            }
            Msg::ToggleEpisode { url, title } => self.toggle_episode(url, title),
            Msg::EpisodeSeekTo { url, title, ms } => self.episode_seek_to(url, title, ms),
            Msg::ToggleEpisodeDownload { url, title } => {
                self.toggle_episode_download(&sender, url, title)
            }
            Msg::SetPodcastView(view) => self.podcasts.podcast_view = view,
            Msg::SetStreamView(view) => self.streaming.stream_view = view,
            Msg::ShowEpisodeDetail(index) => self.open_episode_detail(root, &sender, index),
            Msg::ShowPodcastEpisodeDetail { podcast_id, index } => {
                self.open_podcast_episode_detail(root, &sender, podcast_id, index)
            }
            Msg::ShowPodcastDetail(id) => self.open_podcast_detail(root, &sender, id),
            // --- YouTube ---
            Msg::SetYoutubeEnabled(on) => self.set_youtube_enabled(on, &sender),
            Msg::FetchYtDlp => {
                let update = self.youtube.ytdlp_version.is_some();
                self.start_ytdlp_fetch(update, &sender);
            }
            Msg::YtSubscribe => self.open_youtube_search_dialog(root, &sender),
            Msg::YtSearch(term, kind) => {
                let term = term.trim().to_string();
                if !term.is_empty() {
                    self.toast(&gettext("Searching …"));
                    sender.spawn_command(move |out| {
                        let results =
                            crate::core::youtube::search(&term, kind, 25).unwrap_or_default();
                        let _ = out.send(Cmd::YtSearchResults(results.clone()));
                        for r in &results {
                            if let Some(t) = r.thumbnail.as_deref() {
                                crate::core::online::cache_youtube_thumb(t);
                            }
                        }
                        let _ = out.send(Cmd::YtSearchThumbsReady);
                    });
                }
            }
            Msg::YtSubscribeChannel(url) => {
                if let Some(r) = self
                    .youtube
                    .search_results
                    .iter()
                    .find(|r| r.url == url && r.kind == crate::core::youtube::YtKind::Channel)
                    .cloned()
                {
                    self.toast(&gettext_f("Subscribing to {t} …", &[("t", &r.title)]));
                    sender.spawn_command(move |out| {
                        let t = crate::ui::app_youtube::fetch_and_store_channel(
                            &r.id,
                            &r.title,
                            &r.url,
                            r.thumbnail.as_deref(),
                        );
                        let _ = out.send(Cmd::YtChannelFetched(t));
                    });
                }
            }
            Msg::YtOpenChannel(id) => {
                if let Some((_, title, _, _, _)) = self
                    .youtube
                    .channel_items
                    .iter()
                    .find(|(cid, _, _, _, _)| *cid == id)
                    .cloned()
                {
                    self.open_channel(&sender, id, &title);
                }
            }
            Msg::YtOpenChannelAt(index) => {
                if let Some(id) = self.youtube.channel_items.get(index).map(|c| c.0) {
                    sender.input(Msg::YtOpenChannel(id));
                }
            }
            Msg::YtShowChannelDetail(id) => self.open_channel_detail(root, &sender, id),
            Msg::YtShowChannelDetailAt(index) => {
                if let Some(id) = self.youtube.channel_items.get(index).map(|c| c.0) {
                    sender.input(Msg::YtShowChannelDetail(id));
                }
            }
            Msg::YtRefreshChannel(id) => {
                if let Some((_, title, url, _, _)) = self
                    .youtube
                    .channel_items
                    .iter()
                    .find(|(cid, _, _, _, _)| *cid == id)
                    .cloned()
                {
                    self.toast(&gettext("Refreshing …"));
                    sender.spawn_command(move |out| {
                        let t = crate::ui::app_youtube::refresh_channel_videos(id, &title, &url);
                        let _ = out.send(Cmd::YtChannelFetched(t));
                    });
                }
            }
            Msg::YtDeleteChannel(id) => {
                self.undo_toast(
                    &sender,
                    &gettext("Channel removed"),
                    Msg::YtDeleteChannelConfirmed(id),
                );
            }
            Msg::YtDeleteChannelConfirmed(id) => {
                let _ = self.library.delete_channel(id);
                self.reload_channels(&sender);
            }
            Msg::YtPlayChannel(id) => self.yt_play_channel(id),
            Msg::YtRemoveRecent(key) => {
                let _ = self.library.delete_recent(&key);
                self.reload_yt_recent(&sender);
            }
            Msg::YtShowVideoDetail { video_id, title } => {
                self.show_video_detail(root, &sender, &video_id, &title)
            }
            Msg::YtShowNewestDetail(index) => {
                if let Some(v) = self.youtube.newest_items.get(index).cloned() {
                    self.show_video_detail(root, &sender, &v.video_id, &v.title);
                }
            }
            Msg::YtShowPlaylistDetail { url, title } => {
                self.show_playlist_detail(root, &sender, &url, &title)
            }
            Msg::SetYtView(view) => self.youtube.yt_view = view,
            Msg::YtEnriched {
                video_id,
                artist,
                cover,
            } => self.yt_enriched(&sender, video_id, artist, cover),
            Msg::YtPlayVideo { video_id, title } => self.yt_play_video(video_id, title),
            Msg::YtPlayPlaylistAt {
                url,
                title,
                index,
                close,
            } => self.yt_play_playlist_at(&sender, url, title, index, close),
            Msg::YtStartPlaylist { url, title } => self.yt_start_playlist(&sender, url, title),
            Msg::YtStreamResolved {
                video_id,
                resume,
                result,
            } => self.yt_stream_resolved(&sender, video_id, resume, result),
            Msg::YtAddToLibrary { video_id, title } => {
                self.yt_add_video_to_library(video_id, title, &sender, false)
            }
            Msg::YtAddToLibraryConfirmed { video_id, title } => {
                self.yt_add_video_to_library(video_id, title, &sender, true)
            }
            Msg::YtPlaylistToLibrary { url, title } => {
                self.yt_playlist_to_library(url, title, &sender)
            }
            Msg::YtSavePlaylist { url, title } => self.yt_save_playlist(url, title, &sender),
            Msg::YtOpenRecentPlaylist { url, title } => {
                self.yt_open_recent_playlist(&sender, url, title)
            }
            Msg::CtxEqualizer => self.open_eq_dialog(root, &sender),
            Msg::CtxShare => {
                use crate::ui::sync_page::SyncInput;
                if self.sync_connected {
                    // Paired: share the item whose detail menu this is. The
                    // SyncPage shows a short size confirmation, then sends it.
                    if let Some(target) = self.nav.context_target.clone() {
                        let selection = self.ctx_share_selection(&target);
                        if selection.song_paths.is_empty() {
                            self.toast(&gettext("Nothing here to share"));
                        } else {
                            self.sync_page.emit(SyncInput::ShareSelection {
                                window: root.clone(),
                                selection,
                            });
                        }
                    }
                } else {
                    // Not paired yet: open the pairing dialog. Once connected, the
                    // user starts the share again from the detail view.
                    self.sync_page.emit(SyncInput::Open(root.clone()));
                }
            }
            Msg::OpenSync => {
                use crate::ui::sync_page::SyncInput;
                self.sync_page.emit(SyncInput::Open(root.clone()));
            }
            Msg::SyncConnected(connected) => self.sync_connected = connected,
            Msg::SyncImported => {
                self.load_favorites(&sender);
                self.reload_playlists(&sender);
                self.reload_podcasts(&sender);
                // Received audio files were indexed into the `track` table as they
                // arrived → rebuild the artist/album overviews so they show up.
                self.reload_library_overviews();
            }
            Msg::TrackFinished => {
                if self.files.playing_remote {
                    // Remote queue: advance to the next track (or stop at the
                    // end). Runs separately from the local queue.
                    self.remote_next();
                } else if self.podcasts.playing_episode_url.is_some()
                    && self.transport.queue.is_empty()
                {
                    // A streamed episode has ended (no queue
                    // behind it): finalize its statistics session as "fully
                    // listened", then reset the playback state and marking.
                    self.finalize_play_session(true);
                    self.mini.playing = false;
                    self.podcasts.playing_episode_url = None;
                    self.mpris.set_playing(false);
                    self.refresh_queue_icons();
                } else {
                    // Listened to the end → finalize the listening session as "fully listened",
                    // before the subsequent play_current starts a new session.
                    self.finalize_play_session(true);
                    // Track finished → forget resume, next time from the start.
                    // `take()` prevents play_current from saving the (end) position again
                    // as a resume point.
                    if let Some(path) = self.transport.playing_path.take() {
                        let _ = self.library.set_resume_path(&path.to_string_lossy(), 0);
                    }
                    *self.transport.close_resume.borrow_mut() = None;
                    // If a single song was slipped in between, now resume the interrupted
                    // queue at its spot.
                    if self.transport.queue.len() == 1 && self.transport.interrupted_queue.is_some()
                    {
                        if let Some((q, pos)) = self.transport.interrupted_queue.take() {
                            self.transport.queue = q;
                            self.transport.queue_pos = pos;
                            self.play_current();
                        }
                    } else {
                        // A new (multi-part) playback discards a possibly
                        // remembered interruption.
                        self.transport.interrupted_queue = None;
                        self.play_next();
                    }
                }
            }
            Msg::PersistResume => {
                if self.mini.playing {
                    // Persist resume points on this 5 s timer (not every Tick):
                    // a hard crash loses at most ~5 s of position, while normal
                    // pause/seek/track-switch/close still save immediately.
                    self.save_resume();
                    if self.podcasts.playing_episode_url.is_some() {
                        self.save_episode_progress();
                    }
                    if let Some(pos) = self.player.position_ms() {
                        self.mpris.set_position(pos);
                    }
                }
            }
            Msg::Tick => {
                // Advance the running timeshift recording at the song boundaries.
                if self.streaming.record_state.is_some() {
                    self.drive_recording(&sender);
                }
                // Sync the play/pause and record icons of the station rows.
                self.refresh_stream_icons();
                if self.mini.playing {
                    if let Some(pos) = self.player.position_ms() {
                        self.mini.position_ms = pos;
                    }
                    if let Some(dur) = self.player.duration_ms() {
                        self.mini.track_duration_ms = dur;
                    }
                    // Carry the close snapshot along.
                    if let Some(entry) = self.transport.close_resume.borrow_mut().as_mut() {
                        entry.1 = self.mini.position_ms;
                        entry.2 = self.mini.track_duration_ms;
                    }
                    // (Episode resume is persisted on the 5 s PersistResume timer,
                    // not here — no per-second DB write on the UI thread.)
                    // Track the current chapter below the title (except while hovering).
                    self.update_current_chapter();
                    // Keep counting the listened time of the statistics session (wall clock, only
                    // during "Playing"; ~1 s per tick). Backfill the duration if needed,
                    // in case it was not yet known at the start.
                    let dur = self.mini.track_duration_ms;
                    if let Some(s) = self.transport.play_session.as_mut() {
                        s.played_ms += 1000;
                        if s.duration_ms == 0 {
                            s.duration_ms = dur;
                        }
                    }
                    if let Some(cs) = self.transport.close_session.borrow_mut().as_mut() {
                        if let Some(s) = self.transport.play_session.as_ref() {
                            cs.2 = s.played_ms;
                            cs.3 = s.duration_ms;
                        }
                    }
                }
            }
            Msg::AutoEnrichTick => {
                // Quiet backfill of missing artist photos & online covers in the
                // background (rate-limited in the worker). Only if desired, a
                // folder is set, no run is currently active and there is network.
                // If a (full) fetch is already running, the `enriching` lock takes effect and
                // this tick fizzles out – no pileup.
                if self.enrich_state.auto_enrich
                    && !self.enrich_state.enriching
                    && self.files.music_dir.is_some()
                    && online_available()
                {
                    self.run_enrich(&sender, false, true);
                }
            }
            Msg::FingerprintCurrent(path) => self.fetch_focus_track(&sender, &path),
            Msg::Seek(ms) => {
                let ms = ms.max(0);
                self.mini.position_ms = ms;
                if self.player.seek_ms(ms).is_ok() {
                    self.mpris.seeked(ms);
                }
            }
            Msg::Mpris(cmd) => self.handle_mpris(root, cmd),
            Msg::Next => {
                if self.files.playing_remote {
                    self.remote_next();
                } else {
                    self.play_next();
                }
            }
            Msg::Prev => {
                if self.files.playing_remote {
                    self.remote_prev();
                } else {
                    self.play_prev();
                }
            }
            Msg::ToggleShuffle => {
                self.transport.shuffle = !self.transport.shuffle;
                // When enabling, build a fresh random order of the whole
                // queue (running track first).
                if self.transport.shuffle {
                    self.rebuild_shuffle_order();
                }
                self.mpris.set_shuffle(self.transport.shuffle);
            }
            Msg::ToggleRepeat => {
                self.transport.repeat = !self.transport.repeat;
                let _ = self
                    .library
                    .set_setting("repeat", if self.transport.repeat { "1" } else { "0" });
                self.mpris.set_repeat(self.transport.repeat);
            }
            Msg::NavUp => {
                // Remote source: one rel segment up.
                if let Some(rel) = self.files.remote_browse.clone() {
                    if !rel.is_empty() {
                        let parent = match rel.rfind('/') {
                            Some(0) | None => String::new(),
                            Some(i) => rel[..i].to_string(),
                        };
                        self.files.remote_browse = Some(parent);
                        self.load_dir(&sender);
                    }
                    return;
                }
                if self.can_go_up() {
                    if let Some(parent) = self.files.browse_dir.as_ref().and_then(|d| d.parent()) {
                        self.files.browse_dir = Some(parent.to_path_buf());
                        self.load_dir(&sender);
                    }
                }
            }
            Msg::FilesGoStart => {
                // Remote source: back to the music root of the source.
                if self.files.remote_browse.is_some() {
                    if self.files.remote_browse.as_deref() != Some("") {
                        self.files.remote_browse = Some(String::new());
                        self.load_dir(&sender);
                    }
                    return;
                }
                if let Some(root) = self.files.root_dir.clone() {
                    if self.files.browse_dir.as_ref() != Some(&root) {
                        self.files.browse_dir = Some(root);
                        self.load_dir(&sender);
                    }
                }
            }
            Msg::Refresh => {
                self.load_dir(&sender);
                // Each helper reports whether it actually spawned a background
                // worker; we count those so the loading spinner stays up until
                // the last one reports back (see the matching `Cmd::*` arms).
                let mut pending = 0u32;
                // Re-index the cloud sources too, so their structure and covers
                // update (existing sources are only indexed when first added).
                // On completion this rebuilds the views and fetches covers.
                // `manual` → fetch online regardless of the auto-enrich setting.
                if self.reindex_cloud_sources(&sender, true) {
                    pending += 1;
                }
                // "Rescan" also updates the local library (artists/albums).
                if self.start_scan(&sender, false, true) {
                    pending += 1;
                }
                // Also pull new content for the media subscriptions: every
                // podcast feed and every YouTube channel (background workers;
                // both need a connection, so skip them when offline).
                if online_available() {
                    if self.refresh_all_podcasts(&sender) {
                        pending += 1;
                    }
                    if self.refresh_all_channels(&sender) {
                        pending += 1;
                    }
                }
                self.refresh_pending = pending;
            }
            Msg::OpenSettings => self.open_settings(root, &sender),
            Msg::OpenSearch => self.open_search_dialog(root, &sender),
            Msg::SearchPlayTrack(path) => {
                // A real local file is played directly; remote (`nc:`) hits can't
                // be played as a file, so fall back to opening their album.
                if std::path::Path::new(&path).is_file() {
                    self.play_path(&path, false);
                } else if let Some(album) = self
                    .library
                    .track_by_path(&path)
                    .ok()
                    .flatten()
                    .and_then(|t| t.album)
                    .filter(|a| !a.trim().is_empty())
                {
                    self.open_album_by_name(&sender, &album);
                }
            }
            Msg::SearchOpenAlbum(album) => self.open_album_by_name(&sender, &album),
            Msg::SearchOpenArtist(name) => {
                self.fetch_focus_artist(&sender, &name);
                let meta = self
                    .library
                    .get_artist_meta(&name)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| crate::model::ArtistMeta::pending(name.clone()));
                self.open_artist_tracks(&sender, &meta);
            }
            Msg::OpenGlobalEq => self.open_global_eq(root, &sender),
            Msg::OpenCurrentEq => {
                if let Some(path) = self.transport.queue.get(self.transport.queue_pos).cloned() {
                    let key = path.to_string_lossy().into_owned();
                    // `display_name` resolves YouTube titles (yt:<id>) and the
                    // "artist - title" of library tracks; the static helper would
                    // show the raw path for YouTube.
                    let name = self.display_name(&path);
                    self.open_eq_editor(root, &sender, "the track", &name, None, "track", key);
                }
            }
            Msg::OpenTrackEq { path, title } => {
                self.open_eq_editor(root, &sender, "the track", &title, None, "track", path);
            }
            Msg::ShowQueue => self.open_queue_dialog(root, &sender),
            Msg::PlayQueueAt { start, len } => {
                // Play this queue entry now: move its block to the front of the
                // user queue, then advance – `play_next` splices the first track
                // into the context and the rest follow track by track. Entries
                // before it stay queued and play afterwards.
                let n = self.transport.user_queue.len();
                if start < n {
                    let len = len.clamp(1, n - start);
                    let block: Vec<PathBuf> =
                        self.transport.user_queue.drain(start..start + len).collect();
                    for (i, p) in block.into_iter().enumerate() {
                        self.transport.user_queue.insert(i, p);
                    }
                    self.play_next();
                }
            }
            Msg::SetPlaybackRate(rate) => {
                let rate = (rate / 0.25).round() * 0.25;
                let rate = rate.clamp(0.25, 2.0);
                // Guard against the scale's #[watch] re-emitting the same value.
                if (rate - self.mini.playback_rate).abs() > 1e-3 {
                    self.mini.playback_rate = rate;
                    self.player.set_rate(rate);
                }
            }
            Msg::PlaybackReady => {
                // Source finished buffering → stop the loading spinner.
                if self.mini.loading {
                    self.mini.loading = false;
                }
            }
            Msg::PlaybackError => {
                // A failed start clears the loading spinner regardless of source.
                self.mini.loading = false;
                // Streams/episodes have no "next" → don't skip on their errors.
                if self.streaming.playing_stream.is_some()
                    || self.podcasts.playing_episode_url.is_some()
                {
                    return;
                }
                // Only skip when something is actually queued.
                if self.files.playing_remote || !self.transport.queue.is_empty() {
                    self.skip_current_track();
                }
            }
            Msg::QueueClear => {
                // Clear only the explicit user queue; the currently playing
                // album/track (the context) keeps running untouched.
                self.transport.user_queue.clear();
                self.reload_queue_list();
                self.refresh_queue_icons();
                self.save_queue();
                self.toast(&gettext("Queue cleared"));
            }
            Msg::QueueMoveRange { from, len, to } => {
                let n = self.transport.user_queue.len();
                // Dropping a block onto itself is a no-op.
                if from < n && len >= 1 && !(to >= from && to < from + len) {
                    let len = len.min(n - from);
                    let block: Vec<PathBuf> =
                        self.transport.user_queue.drain(from..from + len).collect();
                    // After removal everything past the block shifts left by `len`.
                    let insert_at = if to > from { to - len } else { to }
                        .min(self.transport.user_queue.len());
                    for (i, p) in block.into_iter().enumerate() {
                        self.transport.user_queue.insert(insert_at + i, p);
                    }
                    self.reload_queue_list();
                    self.refresh_queue_icons();
                    self.save_queue();
                }
            }
            Msg::SetMusicDir(path) => {
                let dir = path.to_string_lossy().into_owned();
                if let Err(e) = self.library.set_setting("music_dir", &dir) {
                    tracing::error!("Failed to save music folder: {e}");
                }
                self.files.music_dir = Some(dir);
                // Only re-root the file view if the primary tab is currently active
                // – on an additional source the user would otherwise be left stranded.
                if self.files.active_source == ActiveSource::Primary {
                    self.files.root_dir = Some(path.clone());
                    self.files.browse_dir = Some(path);
                    self.load_dir(&sender);
                }
                // Read the new folder and (Wi-Fi + switch) fetch automatically.
                self.start_scan(&sender, true, false);
            }
            Msg::SetupFinished {
                lang_code,
                music_dir,
                enabled_sections,
            } => {
                // Which menu items the user keeps. At least one must stay visible.
                let mut enabled: std::collections::HashSet<String> =
                    enabled_sections.into_iter().collect();
                if !SECTIONS.iter().any(|(n, _, _)| enabled.contains(*n)) {
                    enabled.insert("files".to_string());
                }
                let hidden_value = SECTIONS
                    .iter()
                    .map(|(n, _, _)| *n)
                    .filter(|n| !enabled.contains(*n))
                    .collect::<Vec<_>>()
                    .join(",");
                let _ = self.library.set_setting("hidden_sections", &hidden_value);
                // The YouTube section is the opt-in feature: its menu item mirrors
                // the `youtube_enabled` flag.
                let yt_on = enabled.contains("youtube");
                let _ = self
                    .library
                    .set_setting("youtube_enabled", if yt_on { "1" } else { "0" });
                self.youtube.enabled = yt_on;
                // Persist the rest before any possible restart below.
                let _ = self.library.set_setting("setup_complete", "1");
                let _ = self.library.set_setting("ui_language", &lang_code);
                self.settings.ui_language = lang_code.clone();
                let dir = music_dir.to_string_lossy().into_owned();
                let _ = self.library.set_setting("music_dir", &dir);

                if lang_code != crate::i18n::system_language_code() {
                    // The chosen language differs from the active (system) one.
                    // gettext only reads the catalog at startup, so re-launch to
                    // rebuild the UI in the chosen language; setup is complete now
                    // (persisted above), so the assistant won't reappear and the
                    // normal startup re-roots the folder and scans.
                    //
                    // Re-exec *in place* (replace this process image) rather than
                    // spawn + exit: under Flatpak this process is PID 1 of the
                    // sandbox's PID namespace, so exiting it makes the kernel kill
                    // every other process in the namespace — including a freshly
                    // spawned child, leaving the app simply gone. exec() keeps the
                    // same PID, so the sandbox stays alive and the new image starts.
                    if let Ok(exe) = std::env::current_exe() {
                        use std::os::unix::process::CommandExt;
                        let err = std::process::Command::new(&exe).exec();
                        // exec() only returns on failure; fall back to spawn.
                        tracing::error!("re-exec for language change failed: {err}");
                        let _ = std::process::Command::new(&exe).spawn();
                    }
                    std::process::exit(0);
                }

                // Same language → keep running: apply the navigation and folder now.
                self.nav.hidden_sections = SECTIONS
                    .iter()
                    .map(|(n, _, _)| *n)
                    .filter(|n| !enabled.contains(*n))
                    .map(str::to_string)
                    .collect();
                self.refresh_nav_visibility();
                let cur = self.nav.view_stack.visible_child_name();
                let on_hidden = cur
                    .as_deref()
                    .map(|c| self.nav.hidden_sections.contains(c))
                    .unwrap_or(true);
                if on_hidden {
                    if let Some(next) = self
                        .nav
                        .section_order
                        .iter()
                        .copied()
                        .find(|n| !self.nav.hidden_sections.contains(*n))
                    {
                        self.nav.view_stack.set_visible_child_name(next);
                    }
                }
                // Re-root the file view to the chosen folder and start the scan.
                sender.input(Msg::SetMusicDir(music_dir));
            }
            Msg::SelectSource(sel) => {
                if self.files.active_source != sel {
                    self.apply_source(sel, &sender);
                }
            }
            Msg::SourcesChanged => {
                self.files.sources = self.library.list_sources().unwrap_or_default();
                // If the active source is no longer valid (removed, or the
                // primary "Music" tab dropped because no music folder is set),
                // fall back to the first available folder.
                if let Some(s) = self.active_source_fallback() {
                    self.apply_source(s, &sender);
                }
                self.rebuild_source_tabs();
                // Indexed cloud tracks may have been added/removed.
                self.reload_library_overviews();
                // Refresh the "Other sources" list of the settings page, in case
                // the settings dialog is currently open (e.g. right after a
                // Nextcloud connect, which lands the source in that same list).
                let src_list = self.settings_src_list.borrow().clone();
                if let Some(list) = src_list {
                    if list.root().is_some() {
                        self.fill_src_list(&list, &sender);
                    } else {
                        *self.settings_src_list.borrow_mut() = None;
                    }
                }
            }
            Msg::CheckSources => {
                let webdavs: Vec<crate::model::Source> = self
                    .files
                    .sources
                    .iter()
                    .filter(|s| s.kind == "webdav")
                    .cloned()
                    .collect();
                if !webdavs.is_empty() {
                    sender.spawn_command(move |out| {
                        let status: Vec<(i64, bool)> = webdavs
                            .iter()
                            .map(|s| {
                                let ok = crate::core::webdav::Creds::from_source(s)
                                    .map(|c| crate::core::webdav::test_connection(&c).is_ok())
                                    .unwrap_or(false);
                                (s.id, ok)
                            })
                            .collect();
                        let _ = out.send(Cmd::SourceStatus(status));
                    });
                }
            }
            Msg::AddCloudSource => {
                use crate::ui::cloud_page::CloudInput;
                self.cloud_page.emit(CloudInput::Open {
                    window: root.clone(),
                    mobile: self.is_mobile(),
                });
            }
            Msg::CloudIndexed => {
                // Cloud tracks are in the DB → rebuild albums/artists and
                // (if desired) fetch covers/photos online.
                self.reload_library_overviews();
                if self.enrich_state.auto_enrich
                    && !self.enrich_state.enriching
                    && online_available()
                {
                    self.run_enrich(&sender, false, false);
                }
            }
            Msg::CtxDownloadRemote(rel) => {
                let Some(creds) = self.active_webdav_creds() else {
                    return;
                };
                let Some(dest) = self.remote_cache_path(&rel) else {
                    return;
                };
                self.toast(&gettext("Downloading …"));
                sender.spawn_oneshot_command(move || {
                    match crate::core::webdav::download(&creds, &rel, &dest) {
                        Ok(()) => Cmd::RemoteDownloaded(Ok((rel, dest))),
                        Err(e) => Cmd::RemoteDownloaded(Err(e.to_string())),
                    }
                });
            }
            Msg::SetAcoustidKey(key) => {
                let key = key.trim().to_string();
                let _ = self.library.set_secret_setting("acoustid_key", &key);
                self.enrich_state.acoustid_key = if key.is_empty() { None } else { Some(key) };
            }
            Msg::SetAlbumCover {
                artist,
                album,
                path,
            } => self.set_album_cover(artist, album, path),
            Msg::SetArtistImage { name, path } => self.set_artist_image(name, path),
            Msg::UploadCover => self.open_cover_upload_dialog(root, &sender),
            Msg::SetFanartKey(key) => {
                let key = key.trim().to_string();
                let _ = self.library.set_secret_setting("fanart_key", &key);
                self.enrich_state.fanart_key = if key.is_empty() { None } else { Some(key) };
            }
            Msg::SetAutoEnrich(on) => {
                self.enrich_state.auto_enrich = on;
                let _ = self
                    .library
                    .set_setting("auto_enrich", if on { "1" } else { "0" });
            }
            Msg::SetLanguage(lang) => {
                if lang != self.settings.ui_language {
                    self.settings.ui_language = lang.clone();
                    let _ = self.library.set_setting("ui_language", &lang);
                    // gettext reads the language only at startup; therefore restart
                    // the app so that the whole interface switches over.
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new(exe).spawn();
                    }
                    std::process::exit(0);
                }
            }
            Msg::SetColorScheme(scheme) => {
                apply_color_scheme(&scheme);
                let _ = self.library.set_setting("color_scheme", &scheme);
            }
            Msg::SetGalleryView(on) => {
                self.libview.gallery_view = on;
                let _ = self
                    .library
                    .set_setting("gallery_view", if on { "1" } else { "0" });
                self.rebuild_all_lists(&sender);
            }
            Msg::SetGalleryColumns(n) => {
                self.libview.gallery_columns = n.clamp(2, 8);
                let _ = self
                    .library
                    .set_setting("gallery_columns", &self.libview.gallery_columns.to_string());
                if self.libview.gallery_view {
                    self.rebuild_all_lists(&sender);
                }
            }
            Msg::SetAreas { scope, key, value } => self.set_areas(&sender, scope, key, value),
            Msg::SetEq {
                output,
                scope,
                key,
                bands,
            } => {
                let _ = self.library.set_eq(&output, scope, &key, &bands);
                // Re-resolve and apply the effective EQ of the active output
                // (audible, provided the edited level currently applies).
                self.apply_current_eq();
            }
            Msg::SetEqEnabled {
                output,
                scope,
                key,
                enabled,
            } => {
                let _ = self.library.set_eq_enabled(&output, scope, &key, enabled);
                self.apply_current_eq();
            }
            Msg::ClearEq { output, scope, key } => {
                let _ = self.library.clear_eq(&output, scope, &key);
                self.apply_current_eq();
            }
            Msg::ConcertImport => self.concert_import(&sender),
            Msg::ConcertDismissHint => {
                self.concerts.concert_hint_dismissed = true;
                let _ = self.library.set_setting("concert_hint_dismissed", "1");
            }
            Msg::ConcertHideSection => {
                self.set_section_visible("concerts", false);
                self.toast(&gettext("Hid the Concerts menu item"));
            }
            Msg::ConcertAdd(items) => self.concert_add(&sender, items),
            Msg::PlayConcert(index) => {
                if let Some((scope, key, _, is_dir)) =
                    self.concerts.concert_items.get(index).cloned()
                {
                    self.play_entry(&scope, &key, is_dir);
                }
            }
            Msg::OpenConcertEntry(index) => {
                // Gallery tap: like the list tap – album/folder opens the
                // track list, a single track is played.
                if let Some((scope, key, _, is_dir)) =
                    self.concerts.concert_items.get(index).cloned()
                {
                    if scope == "track" {
                        self.play_entry(&scope, &key, is_dir);
                    } else {
                        sender.input(Msg::OpenEntryTracks { scope, key });
                    }
                }
            }
            Msg::SetSectionVisible { section, visible } => {
                self.set_section_visible(section, visible);
            }
            Msg::MoveSection { from, to } => {
                if from < self.nav.section_order.len()
                    && to < self.nav.section_order.len()
                    && from != to
                {
                    let name = self.nav.section_order.remove(from);
                    self.nav.section_order.insert(to, name);
                    let value = self.nav.section_order.join(",");
                    let _ = self.library.set_setting("section_order", &value);
                    // Apply the order to the existing buttons.
                    self.apply_section_order();
                }
            }
            Msg::UnhideEntry { scope, key } => {
                // Delete the override → back to default (visible again).
                let _ = self.library.set_category(&scope, &key, None);
                self.reload_library_overviews();
                self.load_concerts(&sender);
                self.load_audiobooks(&sender);
                self.load_dir(&sender);
                self.toast(&gettext("Shown again"));
            }
            Msg::ToggleFavorite => self.toggle_favorite(&sender),
            Msg::PlayFavorite(index) => self.play_favorite(&sender, index),
            Msg::ShowFavoriteDetail(index) => {
                if let Some((scope, key, _, is_dir)) =
                    self.favorites.favorite_items.get(index).cloned()
                {
                    self.nav.context_target = Some(self.entry_target(&scope, &key, is_dir));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::MoveFavorite { from, to } => self.move_favorite(&sender, from, to),
            Msg::PlayAudiobook(index) => {
                if let Some((scope, key, _, is_dir)) =
                    self.favorites.audiobook_items.get(index).cloned()
                {
                    self.play_entry(&scope, &key, is_dir);
                }
            }
            Msg::OpenAudiobookEntry(index) => {
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
            Msg::ShowAudiobookDetail(index) => {
                if let Some((scope, key, _, is_dir)) =
                    self.favorites.audiobook_items.get(index).cloned()
                {
                    self.nav.context_target = Some(self.entry_target(&scope, &key, is_dir));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::TogglePlay => {
                if self.mini.playing {
                    self.save_resume();
                    self.player.pause();
                    self.mini.playing = false;
                    // Pausing during buffering stops the spinner (no longer "loading").
                    self.mini.loading = false;
                } else if self.transport.playing_path.is_some()
                    || self.streaming.playing_stream.is_some()
                    || self.podcasts.playing_episode_url.is_some()
                {
                    // Paused (file, station or episode) → resume.
                    self.player.resume();
                    self.mini.playing = true;
                } else if !self.transport.queue.is_empty() {
                    // Playback had ended → restart from the current position (rewound
                    // to 0 after the end). play_current sets
                    // playing/MPRIS/icons itself.
                    self.play_current();
                    return;
                } else if !self.transport.user_queue.is_empty() {
                    // Nothing loaded, but the user queued tracks → start the queue
                    // (play_next splices the first queued track into the context).
                    self.play_next();
                    return;
                } else {
                    return;
                }
                self.mpris.set_playing(self.mini.playing);
                // Adjust the play/pause icon of the active track in the list.
                self.refresh_queue_icons();
                self.refresh_stream_icons();
            }
            Msg::OpenNowPlaying => {
                if let Some(path) = self.transport.queue.get(self.transport.queue_pos).cloned() {
                    // A running YouTube video (synthetic `yt:<id>` path) needs its
                    // own detail (channel / URL / thumbnail) – not the file-tag
                    // based track info, which would be empty/wrong for it.
                    if let Some(video_id) =
                        path.to_str().and_then(crate::core::youtube::parse_yt_path)
                    {
                        let title = self
                            .youtube
                            .video_titles
                            .get(&video_id)
                            .cloned()
                            .or_else(|| self.library.yt_title(&video_id).ok().flatten())
                            .filter(|t| !t.trim().is_empty())
                            .or_else(|| self.mini.now_playing.clone())
                            .unwrap_or_else(|| video_id.clone());
                        self.show_video_detail(root, &sender, &video_id, &title);
                    } else {
                        // Detail view of the running track (as a file entry).
                        self.nav.context_target = Some(CtxTarget::Fs(FsEntry::file(path)));
                        self.open_context_menu(root, &sender);
                    }
                }
            }
        }
    }

    /// Process the results of the background workers.
    fn update_cmd(
        &mut self,
        msg: Self::CommandOutput,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        // Mirror the (worker-thread) extraction-broken flag into the model after
        // every command, so the YouTube warning banner reflects the latest
        // yt-dlp result (and is correct when the section is next opened).
        self.youtube.ytdlp_broken = crate::core::youtube::extraction_broken();
        match msg {
            Cmd::Entries(entries) => {
                // "Mixed album": more than one distinct artist in the folder.
                let distinct: std::collections::HashSet<String> = entries
                    .iter()
                    .filter_map(|e| e.effective_artist())
                    .collect();
                let opts = RowOpts {
                    show_artist: distinct.len() > 1,
                };
                let queue = self.transport.queue.clone();
                let mut guard = self.libview.entries.guard();
                guard.clear();
                for e in entries {
                    let queued = e.path().is_some_and(|ep| queue.iter().any(|p| p == ep));
                    guard.push_back((e, opts, queued));
                }
                drop(guard);
                self.libview.loading = false;

                // This folder is now shown; restore the remembered scroll position (from
                // the last visit) after the layout.
                self.files.shown_dir = self.files.browse_dir.clone();
                if let (Some(dir), Some(sc)) = (self.files.browse_dir.clone(), self.fs_scroller()) {
                    if let Some(&value) = self.files.fs_scroll.borrow().get(&dir) {
                        for delay in [50u64, 250] {
                            let sc = sc.clone();
                            gtk::glib::timeout_add_local_once(
                                std::time::Duration::from_millis(delay),
                                move || sc.vadjustment().set_value(value),
                            );
                        }
                    }
                }
            }
            Cmd::RemoteEntries(result, source, rel) => {
                // Discard the stale result (source/folder switched in the meantime).
                if self.files.active_source != source
                    || self.files.remote_browse.as_deref() != Some(rel.as_str())
                {
                    return;
                }
                self.libview.loading = false;
                match result {
                    Err(e) => {
                        tracing::warn!("WebDAV listing failed: {e}");
                        self.libview.entries.guard().clear();
                        self.toast(&gettext("Could not load this folder"));
                    }
                    Ok(list) => {
                        use crate::ui::app_views::natural_key;
                        let (mut dirs, mut files): (Vec<_>, Vec<_>) =
                            list.into_iter().partition(|e| e.is_dir);
                        dirs.sort_by_key(|a| natural_key(&a.name));
                        files.sort_by_key(|a| natural_key(&a.name));
                        // Source id, to read already-indexed track metadata
                        // (title/artist/duration) straight from the DB.
                        let source_id = match &source {
                            ActiveSource::Source(id) => Some(*id),
                            _ => None,
                        };
                        let mut entries: Vec<FsEntry> =
                            Vec::with_capacity(dirs.len() + files.len());
                        for d in dirs {
                            entries.push(FsEntry::remote_dir(d.rel_path, d.name));
                        }
                        for f in files {
                            let cached = self.remote_cache_path(&f.rel_path).filter(|p| p.exists());
                            // If the source was indexed, the tags already live in
                            // the DB → show them at once instead of re-reading them
                            // over the network row by row.
                            let meta = source_id.and_then(|id| {
                                self.library
                                    .track_by_path(&crate::core::webdav::nc_path(id, &f.rel_path))
                                    .ok()
                                    .flatten()
                            });
                            let (title, artist, duration_ms) = match meta {
                                Some(t) => (Some(t.title), t.artist, t.duration_ms),
                                None => (None, None, None),
                            };
                            entries.push(FsEntry::remote_file(
                                f.rel_path,
                                f.name,
                                cached,
                                title,
                                artist,
                                duration_ms,
                            ));
                        }
                        let distinct: std::collections::HashSet<String> = entries
                            .iter()
                            .filter_map(|e| e.effective_artist())
                            .collect();
                        let opts = RowOpts {
                            show_artist: distinct.len() > 1,
                        };
                        {
                            let mut guard = self.libview.entries.guard();
                            guard.clear();
                            for e in entries {
                                guard.push_back((e, opts, false));
                            }
                        }
                        self.refresh_queue_icons();
                        // Fetch the tags of the remote files in the background.
                        if let Some(src) = self.active_remote_source() {
                            self.start_remote_tag_fetch(&sender, &src);
                        }
                    }
                }
            }
            Cmd::RemoteTags(tags) => {
                // rel path → factory index, then send tags to the respective row.
                let map: std::collections::HashMap<String, usize> = {
                    let guard = self.libview.entries.guard();
                    (0..guard.len())
                        .filter_map(|i| {
                            guard.get(i).and_then(|r| match &r.entry {
                                FsEntry::RemoteFile { rel_path, .. } => Some((rel_path.clone(), i)),
                                _ => None,
                            })
                        })
                        .collect()
                };
                for (rel, title, artist, duration_ms) in tags {
                    if let Some(&i) = map.get(&rel) {
                        self.libview.entries.send(
                            i,
                            FsInput::SetTags {
                                title,
                                artist,
                                duration_ms,
                            },
                        );
                    }
                }
            }
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
            } => {
                if manual {
                    self.refresh_done();
                }
                // Library is read in → update the views.
                self.reload_library_overviews();
                // Fill in album covers from the embedded artwork in the files —
                // purely local, so they show even offline or with online
                // enrichment disabled (the online sweep below only runs when
                // connected).
                self.run_local_covers(&sender);
                // Then automatically fetch online – without user action,
                // provided it is desired, no fetch is already running and there is any
                // connection at all (on any connection, even metered). The
                // local scan already ran, so here without re-reading.
                if then_enrich
                    && self.enrich_state.auto_enrich
                    && !self.enrich_state.enriching
                    && self.files.music_dir.is_some()
                    && online_available()
                {
                    // Automatic run (without a renewed tag scan), full scope.
                    self.run_enrich(&sender, false, false);
                }
            }
            Cmd::CloudReindexed { manual } => {
                if manual {
                    self.refresh_done();
                }
                // Freshly indexed remote tracks → rebuild the library views and
                // favorites. Then fetch covers/photos (incl. the embedded covers
                // of the remote tracks). A manual refresh does this regardless of
                // the passive auto-enrich setting; the silent startup top-up only
                // when auto-enrich is on (like the local scan's `then_enrich`).
                self.reload_library_overviews();
                self.load_favorites(&sender);
                if (manual || self.enrich_state.auto_enrich)
                    && !self.enrich_state.enriching
                    && online_available()
                {
                    self.run_enrich(&sender, false, false);
                }
            }
            Cmd::Candidates(candidates) => {
                if candidates.is_empty() {
                    self.toast(&gettext("No new concert candidates found"));
                } else {
                    self.open_concert_import_dialog(root, &sender, candidates);
                }
            }
            Cmd::PodcastFetched(title) => {
                self.reload_podcasts(&sender);
                match title {
                    Some(t) => self.toast(&gettext_f("Subscribed: {t}", &[("t", &t)])),
                    None => self.toast(&gettext("Could not load feed")),
                }
            }
            Cmd::EpisodeDownloaded { url, result } => {
                self.podcasts.downloading_episodes.remove(&url);
                self.refresh_download_row();
                match result {
                    Ok(_) => self.toast(&gettext("Episode downloaded")),
                    Err(e) => {
                        tracing::warn!("Episode download failed: {e}");
                        self.toast(&gettext("Download failed"));
                    }
                }
            }
            Cmd::PodcastSearchResults(results) => {
                self.podcasts.podcast_search_results = results;
                self.rebuild_podcast_search_results(&sender);
            }
            Cmd::PodcastSearchCoversReady => self.rebuild_podcast_search_results(&sender),
            Cmd::ReloadPodcasts => self.reload_podcasts(&sender),
            Cmd::PodcastsRefreshed => {
                self.refresh_done();
                self.reload_podcasts(&sender);
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
            Cmd::YtDlpChecked(version) => {
                self.youtube.ytdlp_version = version;
                self.refresh_ytdlp_status_label();
            }
            Cmd::YtSearchResults(results) => {
                self.youtube.search_results = results;
                self.rebuild_youtube_search_results(&sender);
            }
            Cmd::YtSearchThumbsReady => self.rebuild_youtube_search_results(&sender),
            Cmd::YtChannelFetched(title) => {
                self.reload_channels(&sender);
                match title {
                    Some(t) => self.toast(&gettext_f("Subscribed: {t}", &[("t", &t)])),
                    None => self.toast(&gettext("Could not load channel")),
                }
            }
            Cmd::ReloadChannels => self.reload_channels(&sender),
            Cmd::ChannelsRefreshed => {
                self.refresh_done();
                self.reload_channels(&sender);
            }
            Cmd::YtVideoMeta {
                video_id,
                uploader,
                duration,
                cover,
            } => self.apply_video_meta(&video_id, uploader, duration, cover),
            Cmd::YtPlaylistStart { url, title, items } => {
                if items.is_empty() {
                    self.toast(&gettext("Playlist is empty"));
                } else {
                    self.youtube.video_titles.clear();
                    let mut queue = Vec::with_capacity(items.len());
                    let mut paths = Vec::with_capacity(items.len());
                    for (id, vtitle) in &items {
                        self.youtube.video_titles.insert(id.clone(), vtitle.clone());
                        // Persist the title so the playlist/queue shows names.
                        let _ = self.library.set_yt_title(id, vtitle);
                        let p = crate::core::youtube::yt_path(id);
                        paths.push(p.clone());
                        queue.push(PathBuf::from(p));
                    }
                    // Log the playlist as one "Recent" entry (not the videos).
                    self.youtube.playing_playlist = true;
                    let _ = self
                        .library
                        .add_recent_playlist(&url, &title, items.len() as i64);
                    // Recent playlist cover = its first video's thumbnail.
                    if let Some((id, _)) = items.first() {
                        let _ = self
                            .library
                            .set_recent_thumb(&url, &crate::core::youtube::thumbnail_url(id));
                    }
                    // Mirror the playlist into the Playlists section (keyed by
                    // its URL, so a same-named user playlist is left untouched).
                    let _ = self.library.replace_yt_playlist(&url, &title, &paths);
                    self.transport.queue = queue;
                    self.transport.queue_pos = 0;
                    self.play_current();
                    self.reload_yt_recent(&sender);
                    self.reload_playlists(&sender);
                }
            }
            Cmd::YtPlaylistSaved(result) => match result {
                Ok(n) => {
                    self.reload_playlists(&sender);
                    self.yt_progress_done(&gettext_f(
                        "Saved {n} track(s) to Playlists",
                        &[("n", &n.to_string())],
                    ));
                }
                Err(e) => {
                    tracing::warn!("yt save playlist failed: {e}");
                    self.yt_progress_done(&gettext("Could not save playlist"));
                }
            },
            Cmd::YtLibraryProgress { done, total } => {
                self.yt_progress(&gettext_f(
                    "Adding to library {done}/{total} …",
                    &[("done", &done.to_string()), ("total", &total.to_string())],
                ));
            }
            Cmd::YtLibraryAdded { video_id, result } => {
                if let Some(vid) = &video_id {
                    self.youtube.downloading_videos.remove(vid);
                    self.refresh_yt_download_row();
                }
                match result {
                    Ok(n) => {
                        self.reload_library_overviews();
                        self.yt_progress_done(&gettext_f(
                            "Added {n} track(s) to your library",
                            &[("n", &n.to_string())],
                        ));
                    }
                    Err(e) => {
                        tracing::warn!("yt library add failed: {e}");
                        self.yt_progress_done(&gettext("Could not add to library"));
                    }
                }
            }
            Cmd::YtLibraryExists {
                video_id,
                title,
                dest,
            } => {
                self.youtube.downloading_videos.remove(&video_id);
                self.refresh_yt_download_row();
                self.yt_progress_done(&gettext("Song already exists"));
                // Never overwrite a different song silently – let the user decide.
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
                            sender.input(Msg::YtAddToLibraryConfirmed {
                                video_id: video_id.clone(),
                                title: title.clone(),
                            });
                        }
                    });
                }
                confirm.present(Some(root));
            }
            Cmd::YtPlaylistSongs { url, title, result } => {
                // Hide the loading overlay (covers both success and failure).
                self.libview.loading = false;
                self.libview.loading_label = None;
                match result {
                    Ok(videos) => {
                        self.youtube
                            .playlist_songs_cache
                            .insert(url.clone(), videos.clone());
                        self.show_yt_playlist_songs(&sender, &url, &title, videos);
                    }
                    Err(e) => {
                        tracing::warn!("yt playlist load failed: {e}");
                        self.toast(&gettext("Could not load playlist"));
                    }
                }
            }
            Cmd::YtPlaylistCoversReady => {
                // Fill the pending cover frames whose thumbnails are now cached.
                // Keep any still-uncached ones (a later batch may complete them);
                // drop frames whose row is no longer on screen.
                self.youtube.pl_cover_slots.retain(|(thumb_url, frame)| {
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
            Cmd::StreamSearchResults(results) => {
                self.streaming.stream_search_results = results;
                self.rebuild_stream_search_results(&sender);
            }
            Cmd::StreamSearchCoversReady => self.rebuild_stream_search_results(&sender),
            Cmd::ReloadStreams => self.reload_streams(&sender),
            Cmd::ReloadRecordings => self.reload_recordings(&sender),
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
    }
}

impl App {
    /// One background worker of a manual refresh reported back → decrement the
    /// pending counter (saturating, so a stray completion can never wrap it).
    /// When it hits zero the loading overlay hides itself again (see the view).
    pub(crate) fn refresh_done(&mut self) {
        self.refresh_pending = self.refresh_pending.saturating_sub(1);
    }

    /// Whether the loading overlay should be shown: either a folder/list load is
    /// in progress or a manual refresh still has background workers running.
    pub(crate) fn overlay_visible(&self) -> bool {
        self.libview.loading || self.refresh_pending > 0
    }

    /// Text beneath the overlay spinner. A specific load label (e.g. a YouTube
    /// playlist) wins; otherwise a manual refresh shows "Updating …", and
    /// finally the default "reading data" of a plain folder/list load.
    pub(crate) fn overlay_text(&self) -> String {
        if let Some(label) = &self.libview.loading_label {
            label.clone()
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
        self.reload_podcasts(sender);
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
        if self.is_mobile() {
            dialog.set_presentation_mode(adw::DialogPresentationMode::BottomSheet);
        }
    }

    /// Fetches the **photo of the currently opened artist** immediately in the background
    /// – so that what the user is looking at appears first (priority over the
    /// running bulk sync). Additionally fetches – if a fanart.tv key is present –
    /// the **image gallery** of the artist (multiple photos), which exists only in the
    /// detail view and is therefore loaded only here (on demand).
    /// Does nothing without network; the single photo is skipped if a photo is already assigned
    /// or after too many attempts, the gallery if it is already present or has
    /// already been attempted in this session. On success: `Cmd::ReloadViews`.
    pub(crate) fn fetch_focus_artist(&self, sender: &ComponentSender<Self>, name: &str) {
        let name = name.trim().to_string();
        if name.is_empty() || !online_available() {
            return;
        }
        // (a) Single photo (Deezer): skip if already assigned or exhausted.
        let matched = self
            .library
            .get_artist_meta(&name)
            .ok()
            .flatten()
            .is_some_and(|m| m.status == "matched");
        let need_image =
            !matched && self.library.artist_attempts(&name) < crate::ui::enrich::MAX_ATTEMPTS;
        // (b) Gallery (fanart.tv): only with a key, if none is present yet and not yet
        // attempted in this session (galleries have no attempt limit).
        let fkey = self
            .enrich_state
            .fanart_key
            .clone()
            .filter(|k| !k.is_empty());
        let need_gallery = fkey.is_some()
            && self
                .library
                .artist_images(&name)
                .map(|imgs| imgs.is_empty())
                .unwrap_or(false)
            && self
                .libview
                .gallery_tried
                .borrow_mut()
                .insert(format!("a\u{1}{name}"));
        if !need_image && !need_gallery {
            return;
        }
        let fkey = fkey.filter(|_| need_gallery);
        sender.spawn_oneshot_command(move || {
            if let Ok(lib) = Library::open() {
                let client = crate::core::online::OnlineClient::new();
                if need_image {
                    let (image, errored) = match client.fetch_artist_image(&name) {
                        Ok(img) => (img, false),
                        Err(_) => (None, true),
                    };
                    let meta = crate::core::online::store_artist_image(&name, image, errored);
                    let _ = lib.upsert_artist_meta(&meta);
                }
                if let Some(key) = fkey {
                    let _ = crate::core::online::enrich_artist_gallery(&client, &lib, &name, &key);
                }
            }
            Cmd::ReloadViews
        });
    }

    /// Like [`Self::fetch_focus_artist`], only for the **currently opened album**: fetches
    /// the single cover (MusicBrainz + Cover Art Archive) and – if none is there yet –
    /// the **cover gallery** of the album. The single cover is skipped if one is already
    /// present or too many attempts failed; the gallery if it is already
    /// present or was attempted in this session. It needs the MBID set during the
    /// cover fetch – at the very first open this is just being created,
    /// so the gallery may only take effect on the next open.
    /// Populates album covers from the embedded artwork in the files in the
    /// background (purely local, no network, independent of the auto-enrich
    /// setting) and reloads the album/artist views when done. This is why the
    /// embedded cover the user put into the files shows up everywhere — grid,
    /// song list and detail — not only after an online enrichment run.
    pub(crate) fn run_local_covers(&self, sender: &ComponentSender<Self>) {
        sender.spawn_oneshot_command(|| {
            if let Ok(lib) = Library::open() {
                crate::ui::enrich::populate_local_covers(&lib);
            }
            Cmd::ReloadViews
        });
    }

    pub(crate) fn fetch_focus_album(
        &self,
        sender: &ComponentSender<Self>,
        artist: &str,
        album: &str,
    ) {
        let artist = artist.trim().to_string();
        let album = album.trim().to_string();
        if artist.is_empty() || album.is_empty() || !online_available() {
            return;
        }
        let has_cover = self
            .library
            .get_album_meta(&artist, &album)
            .ok()
            .flatten()
            .is_some_and(|m| {
                m.cover_path
                    .as_deref()
                    .is_some_and(|p| !p.trim().is_empty())
            });
        let need_cover = !has_cover
            && self.library.album_attempts(&artist, &album) < crate::ui::enrich::MAX_ATTEMPTS;
        let need_gallery = self
            .library
            .album_images(&artist, &album)
            .map(|imgs| imgs.is_empty())
            .unwrap_or(false)
            && self
                .libview
                .gallery_tried
                .borrow_mut()
                .insert(format!("b\u{1}{artist}\u{1}{album}"));
        if !need_cover && !need_gallery {
            return;
        }
        sender.spawn_oneshot_command(move || {
            if let Ok(lib) = Library::open() {
                let client = crate::core::online::OnlineClient::new();
                // No cover at all → full online match (sets cover + MBID).
                if need_cover {
                    let _ = crate::core::online::enrich_album(&client, &lib, &artist, &album);
                }
                if need_gallery {
                    // The gallery needs an MBID. If the album already shows the
                    // user's embedded cover (so `need_cover` was false), match the
                    // MBID **without** overwriting that cover, so the online images
                    // are offered as alternatives rather than replacing it.
                    if !need_cover {
                        let _ =
                            crate::core::online::match_album_mbid(&client, &lib, &artist, &album);
                    }
                    let _ =
                        crate::core::online::enrich_album_gallery(&client, &lib, &artist, &album);
                }
            }
            Cmd::ReloadViews
        });
    }

    /// On-demand **fingerprint track recognition** (Chromaprint → AcoustID) for
    /// the just started track. Runs only with an AcoustID key + `fpcalc` + network,
    /// only for not-yet-assigned and not-exhausted tracks. Replaces the
    /// earlier bulk run: what is actually played gets recognized.
    pub(crate) fn fetch_focus_track(&self, sender: &ComponentSender<Self>, path: &std::path::Path) {
        if !online_available() {
            return;
        }
        let Some(key) = self
            .enrich_state
            .acoustid_key
            .clone()
            .filter(|k| !k.is_empty())
        else {
            return;
        };
        if !crate::core::online::fingerprint_available() {
            return;
        }
        let path_str = path.to_string_lossy().to_string();
        let matched = self
            .library
            .get_track_meta(&path_str)
            .ok()
            .flatten()
            .is_some_and(|m| m.status == "matched");
        if matched || self.library.track_attempts(&path_str) >= crate::ui::enrich::MAX_ATTEMPTS {
            return;
        }
        let path = path.to_path_buf();
        sender.spawn_oneshot_command(move || {
            if let Ok(lib) = Library::open() {
                let client = crate::core::online::OnlineClient::new();
                let _ = crate::core::online::enrich_track_fingerprint(&client, &lib, &key, &path);
            }
            Cmd::ReloadViews
        });
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

    /// Shows/hides a navigation menu item: updates the state,
    /// saves it, toggles all associated buttons (sidebar +
    /// top bar) and, when hiding the active item, switches to the
    /// first visible one.
    pub(crate) fn set_section_visible(&mut self, section: &str, visible: bool) {
        // At least one menu item must stay visible.
        if !visible {
            let visible_count = SECTIONS
                .iter()
                .filter(|(n, _, _)| !self.nav.hidden_sections.contains(*n))
                .count();
            if visible_count <= 1 {
                return;
            }
        }
        if visible {
            self.nav.hidden_sections.remove(section);
        } else {
            self.nav.hidden_sections.insert(section.to_string());
        }
        let value = SECTIONS
            .iter()
            .map(|(n, _, _)| *n)
            .filter(|n| self.nav.hidden_sections.contains(*n))
            .collect::<Vec<_>>()
            .join(",");
        let _ = self.library.set_setting("hidden_sections", &value);

        // Re-apply button visibility and, when only one menu item is left,
        // suppress the navigation entirely (Settings then sits in the title bar).
        self.refresh_nav_visibility();

        // If the currently visible section is hidden, switch to the first
        // visible menu item (in the chosen order).
        if !visible {
            let cur = self.nav.view_stack.visible_child_name();
            if cur.as_deref() == Some(section) {
                if let Some(next) = self
                    .nav
                    .section_order
                    .iter()
                    .copied()
                    .find(|n| !self.nav.hidden_sections.contains(*n))
                {
                    self.nav.view_stack.set_visible_child_name(next);
                }
            }
        }
    }

    /// Re-applies the navigation visibility: hides the buttons of hidden
    /// sections, and when only a single menu item remains visible suppresses the
    /// whole navigation (sidebar + top bar) and moves Settings into the title
    /// bar (via [`NavState::apply_chrome`]).
    pub(crate) fn refresh_nav_visibility(&self) {
        let visible_count = SECTIONS
            .iter()
            .filter(|(n, _, _)| !self.nav.hidden_sections.contains(*n))
            .count();
        let single = visible_count <= 1;
        self.nav.nav_hidden.set(single);
        for (name, _is_sidebar, btn) in &self.nav.nav_buttons {
            btn.set_visible(!self.nav.hidden_sections.contains(*name) && !single);
        }
        (self.nav.apply_chrome)();
    }

    /// Applies `section_order` to the navigation containers by reordering the
    /// existing buttons (sidebar buttons before the
    /// spacer + "Settings", which stay untouched at the end).
    pub(crate) fn apply_section_order(&self) {
        for sidebar in [true, false] {
            let container = if sidebar {
                &self.nav.sidebar_nav
            } else {
                &self.nav.top_nav
            };
            let mut prev: Option<gtk::Widget> = None;
            for &name in &self.nav.section_order {
                if let Some((_, _, btn)) = self
                    .nav
                    .nav_buttons
                    .iter()
                    .find(|(n, s, _)| *n == name && *s == sidebar)
                {
                    container.reorder_child_after(btn, prev.as_ref());
                    prev = Some(btn.clone().upcast());
                }
            }
        }
    }
}
