use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use adw::prelude::*;
use relm4::factory::FactoryVecDeque;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::player::Player;
use crate::core::scanner;
use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::model::{AlbumMeta, ArtistMeta, Source, Track};
use crate::ui::album_row::{AlbumCard, AlbumOutput};
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
pub(crate) const SECTIONS: [(&str, &str, &str); 10] = [
    ("favorites", "Favorites", "emilia-favorite-symbolic"),
    ("files", "Files", "folder-symbolic"),
    ("artists", "Artists", "avatar-default-symbolic"),
    ("albums", "Albums", "media-optical-symbolic"),
    ("concerts", "Concerts", "emilia-concert-symbolic"),
    ("podcasts", "Podcasts", "microphone-symbolic"),
    ("streaming", "Streaming", "audio-x-generic-symbolic"),
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

/// Before this position no resume is remembered (too close to the start).
const RESUME_MIN_POS_MS: i64 = 5_000;
/// This close to the end the track counts as finished → reset resume to 0.
const RESUME_END_GUARD_MS: i64 = 10_000;
/// Cadence of the quiet background backfill of missing artist photos & covers.
/// Deliberately low (~1 min) so new users quickly get an enriched overview;
/// the worker throttles the actual network requests itself.
const AUTO_ENRICH_INTERVAL_SECS: u32 = 60;

/// Resume position with guards: near start or end it is set to 0,
/// so a nearly finished track starts from the beginning next time.
/// Current time in Unix seconds (for the listening statistics timestamps).
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Applies the color scheme ("system"/"dark"/"light") via the global
/// libadwaita StyleManager. "system" follows the desktop setting.
pub(crate) fn apply_color_scheme(code: &str) {
    let scheme = match code {
        "dark" => adw::ColorScheme::ForceDark,
        "light" => adw::ColorScheme::ForceLight,
        _ => adw::ColorScheme::Default,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

pub(crate) fn guarded_resume(pos_ms: i64, dur_ms: i64) -> i64 {
    if pos_ms < RESUME_MIN_POS_MS {
        0
    } else if dur_ms > 0 && pos_ms > dur_ms - RESUME_END_GUARD_MS {
        0
    } else {
        pos_ms
    }
}

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
    /// Galleries (artist/album) for which an on-demand fetch already ran this session.
    pub(crate) gallery_tried: std::cell::RefCell<std::collections::HashSet<String>>,
    /// Gallery FlowBoxes whose resize hook has already been connected once.
    pub(crate) gallery_hooked: std::cell::RefCell<std::collections::HashSet<usize>>,
}

/// Playback transport: queue, shuffle order, history, resume/stats sessions.
pub(crate) struct TransportState {
    pub(crate) queue: Vec<PathBuf>,
    pub(crate) queue_pos: usize,
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
}

/// Mini-player / now-playing strip state, grouped off the `App` god-object.
pub(crate) struct MiniState {
    /// Title shown in the player bar; `None` when nothing is loaded.
    pub(crate) now_playing: Option<String>,
    pub(crate) playing: bool,
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
    pub(crate) stream_search:
        std::rc::Rc<std::cell::RefCell<Option<(adw::Dialog, gtk::ListBox)>>>,
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
    pub(crate) podcast_search:
        std::rc::Rc<std::cell::RefCell<Option<(adw::Dialog, gtk::ListBox)>>>,
    /// URL of the currently loaded podcast episode (play/pause row marker);
    /// `None` when music is playing or no episode is running.
    pub(crate) playing_episode_url: Option<String>,
    /// Play/pause buttons of the visible episode rows (audio URL → button), to
    /// refresh their icon on playback-state changes.
    pub(crate) episode_play_buttons:
        std::rc::Rc<std::cell::RefCell<Vec<(String, gtk::Button)>>>,
    /// "Play" row of an open episode detail dialog (row, audio URL) – hidden
    /// while exactly this episode is playing.
    pub(crate) ctx_episode_play:
        std::rc::Rc<std::cell::RefCell<Option<(adw::ActionRow, String)>>>,
    /// "Download" row of an open episode detail dialog: (row, icon, spinner,
    /// audio URL). Its label/icon reflect the offline state and are refreshed
    /// when a background download starts or finishes.
    pub(crate) ctx_episode_download:
        std::rc::Rc<std::cell::RefCell<Option<(adw::ActionRow, gtk::Image, gtk::Spinner, String)>>>,
    /// Audio URLs of episodes whose download is currently running (to show a
    /// spinner and ignore repeated taps).
    pub(crate) downloading_episodes: std::collections::HashSet<String>,
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
    /// "Connected" list of the Nextcloud page in the open settings dialog, so that
    /// it can be updated immediately after a successful connect.
    pub(crate) settings_nc_list: std::rc::Rc<std::cell::RefCell<Option<gtk::ListBox>>>,
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
}

#[derive(Debug)]
pub enum Msg {
    Activate(usize),
    ToggleQueue(usize),
    ShowContextMenu(usize),
    ShowArtistDetail(usize),
    ShowAlbumDetail(usize),
    /// Open the detail page of an album via (artist, album) (from subpages).
    ShowAlbumDetailFor { artist: String, album: String },
    /// Open the detail page of a single song via its path.
    ShowTrackDetail(String),
    /// Open the songs subpage of an album from the album overview (short tap).
    ShowAlbumTracks(usize),
    ShowConcertDetail(usize),
    /// Short tap on an artist: list its albums & songs.
    OpenArtistTracks(usize),
    /// Tap on an album in the artist subpage: list its tracks as
    /// a further subpage.
    OpenAlbumTracks { artist: String, album: String },
    /// Play a track from the artist overview (queue = all tracks
    /// of the artist, start at the tapped one).
    PlayArtistTrack { name: String, path: String },
    /// Play a track from the album subpage (queue = whole album in
    /// track order, start at the tapped one).
    PlayAlbumTrack { artist: String, album: String, path: String },
    /// Like `PlayAlbumTrack`, but across artists (album overview):
    /// queue = all tracks of the album name.
    PlayAlbumByNameTrack { album: String, path: String },
    /// Tap on an album/folder entry in concerts/audiobooks: list its
    /// tracks as a subpage (instead of playing directly).
    OpenEntryTracks { scope: String, key: String },
    /// Play a track of a folder audiobook/concert (queue = folder in
    /// order, start at the tapped one).
    PlayFolderTrack { folder: String, path: String },
    /// Play the whole album in track order (play button of the album row).
    PlayAlbum { artist: String, album: String },
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
    ShareHost,
    ShareScan,
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
    /// Check for a newer Flatpak version (only as Flatpak; in the background).
    CheckForUpdates,
    /// Apply a found update via the Flatpak portal.
    InstallFlatpakUpdate,
    /// Result of the Flatpak update (`Ok` = done, restart needed).
    FlatpakUpdateFinished(Result<(), String>),
    OpenGlobalEq,
    /// Open the equalizer for the currently running track.
    OpenCurrentEq,
    /// Open the queue dialog.
    ShowQueue,
    /// Start playback at a specific queue entry (queue index).
    PlayQueueAt(usize),
    /// Set the playback speed (0.25–2.0, in 0.25 steps).
    SetPlaybackRate(f64),
    /// The current track failed to play (missing file/mount, unreachable
    /// Nextcloud, …) → skip to the next entry.
    PlaybackError,
    /// Clear the entire queue (after confirmation) and stop playback.
    QueueClear,
    /// Move a queue entry (queue indices).
    QueueMove { from: usize, to: usize },
    SetMusicDir(PathBuf),
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
    SetAlbumCover { artist: String, album: String, path: String },
    /// Set the primary photo of an artist (last shown in the gallery carousel).
    SetArtistImage { name: String, path: String },
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
    MoveFavorite { from: usize, to: usize },
    // Audiobooks
    /// Play an audiobook (index in `audiobook_items`).
    PlayAudiobook(usize),
    /// Open gallery audiobook (index): album/folder → track list, track → play.
    OpenAudiobookEntry(usize),
    /// Open the detail view of an audiobook.
    ShowAudiobookDetail(usize),
    // Playlists
    /// Open the "New playlist" dialog.
    PlaylistNew,
    /// Create a playlist with this name.
    PlaylistCreate(String),
    /// Create a playlist and add the current context files.
    PlaylistCreateAddTo(String),
    /// Open the tracks subpage of a playlist.
    OpenPlaylist(i64),
    /// Play the whole playlist.
    PlayPlaylist(i64),
    /// Delete a playlist.
    PlaylistDelete(i64),
    /// Add the current context files to this playlist.
    PlaylistAddTo(i64),
    /// Play a track from a playlist (queue = whole playlist).
    PlaylistTrack { id: i64, path: String },
    /// Remove a track from a playlist.
    PlaylistRemoveTrack { id: i64, path: String },
    /// Open the rename dialog of a playlist.
    PlaylistRenameDialog(i64),
    /// Rename a playlist.
    PlaylistRename { id: i64, name: String },
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
    /// Remove a podcast.
    PodcastDelete(i64),
    /// Reload the feed of a podcast.
    PodcastRefresh(i64),
    /// Toggle an entry (episode): start or – if already the running one –
    /// pause/resume. From tapping the row and from the play/pause button.
    ToggleEpisode { url: String, title: String },
    /// Switch the podcast view (Newest / Overview).
    SetPodcastView(PodcastView),
    /// Switch the streaming view (channels/recordings).
    SetStreamView(StreamView),
    /// Detail view of an entry (episode) from the "Newest" list (index).
    ShowEpisodeDetail(usize),
    /// Detail view of an episode from the episode list of a podcast.
    ShowPodcastEpisodeDetail { podcast_id: i64, index: usize },
    /// Click on a time-jump mark in the show notes: jump to the spot
    /// (start the episode there if needed).
    EpisodeSeekTo { url: String, title: String, ms: i64 },
    /// Download row in the episode detail: if not downloaded, fetch the audio
    /// for offline playback (background); if already downloaded, delete the
    /// local copy. `title` is only used for the toast.
    ToggleEpisodeDownload { url: String, title: String },
    /// Detail view/management of a subscription (podcast id) – refresh/remove.
    ShowPodcastDetail(i64),
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
    /// Remove a station.
    StreamDelete(i64),
    // Recording (timeshift)
    /// Stop the running recording.
    RecordStop,
    /// Open the replay subpage of a station.
    OpenStreamReplay(i64),
    /// Preview a buffered song (absolute byte range).
    ReplayPlay { start: u64, end: u64 },
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
    /// Delete a recording (id).
    RecordingDelete(i64),
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
    EnrichDone { changed: bool },
    /// Intermediate state: reload albums/artists view (e.g. after a phase).
    ReloadViews,
    /// Local library scan finished; `then_enrich` = possibly fetch online afterwards.
    ScanDone { then_enrich: bool },
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
    /// Result of the update check (determined in the background).
    UpdateChecked(crate::core::update::CheckResult),
    /// Cloud sources were re-indexed (refresh button) → rebuild views + covers.
    CloudReindexed,
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
                #[name = "nav_view"]
                set_child = &adw::NavigationView {
                    // Root page: the actual app (navigation, content, mini player).
                    // Artist/album subpages are pushed onto it.
                    adw::NavigationPage {
                        set_title: "Emilia",
                        set_tag: Some("main"),
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

                #[wrap(Some)]
                #[name = "content_view"]
                set_content = &adw::ToolbarView {
                    add_top_bar = &adw::HeaderBar {
                        #[wrap(Some)]
                        #[name = "win_title"]
                        set_title_widget = &adw::WindowTitle::new("Emilia", ""),
                        // Settings at the top only in narrow (mobile) mode – in
                        // desktop mode the item sits at the bottom of the sidebar.
                        #[name = "settings_top_btn"]
                        pack_start = &gtk::Button {
                            set_icon_name: "emblem-system-symbolic",
                            set_tooltip_text: Some(&gettext("Settings")),
                            set_visible: false,
                            connect_clicked => Msg::OpenSettings,
                        },
                        pack_start = &gtk::Button {
                            set_icon_name: "view-refresh-symbolic",
                            set_tooltip_text: Some(&gettext("Rescan folder")),
                            connect_clicked => Msg::Refresh,
                        },
                        // Device synchronization: opens the same "Share" dialog
                        // as the action in the detail menu (no separate popover). With
                        // an existing pairing the icon is rendered green
                        // (CSS class, see below).
                        #[name = "sync_btn"]
                        pack_start = &gtk::Button {
                            set_icon_name: "emilia-share-symbolic",
                            set_tooltip_text: Some(&gettext("Share")),
                            connect_clicked => Msg::CtxShare,
                            #[watch]
                            set_css_classes: if model.sync_connected {
                                &["sync-connected"]
                            } else {
                                &[]
                            },
                        },
                    },

                    // Top navigation (icon-only) – only in narrow (mobile) mode
                    #[name = "top_nav"]
                    add_top_bar = &gtk::Box {
                        set_halign: gtk::Align::Center,
                        set_spacing: 6,
                        set_visible: false,
                        set_margin_top: 2,
                        set_margin_bottom: 2,
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
                                            set_visible: !model.files.sources.is_empty(),
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
                                                set_margin_top: 0,
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
                            add_titled_with_icon[Some("concerts"), &gettext("Concerts"), "emilia-concert-symbolic"] =
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
                                        set_icon_name: Some("emilia-concert-symbolic"),
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
                                        set_icon_name: Some("emilia-concert-symbolic"),
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

                                    // Action at the very bottom.
                                    gtk::Box {
                                        set_halign: gtk::Align::Center,
                                        set_margin_top: 6,
                                        set_margin_bottom: 10,
                                        gtk::Button {
                                            set_label: &gettext("New playlist"),
                                            set_css_classes: &["suggested-action", "pill"],
                                            connect_clicked => Msg::PlaylistNew,
                                        },
                                    },
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
                                        // Flush to the top like the Artists/Albums lists.
                                        set_margin_top: 0,
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
                                            set_margin_top: 4,
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
                                            set_margin_top: 4,
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
                            set_visible: model.libview.loading,

                            gtk::Spinner {
                                set_spinning: true,
                                set_size_request: (48, 48),
                            },
                            gtk::Label {
                                set_label: &gettext("Reading music data"),
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
                                    #[watch]
                                    set_icon_name: if model.mini.playing {
                                        "media-playback-pause-symbolic"
                                    } else {
                                        "media-playback-start-symbolic"
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
                                    set_sensitive: model.mini.now_playing.is_some() || !model.transport.queue.is_empty(),
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

        let library = Library::open().expect("Failed to open the library database");
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
            library.get_setting("win_maximized").ok().flatten().as_deref(),
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
            library.get_setting("concerts_hidden").ok().flatten().as_deref(),
            Some("1")
        ) {
            hidden_sections.insert("concerts".to_string());
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
                        SECTIONS.iter().find(|(n, _, _)| *n == name.trim()).map(|(n, _, _)| *n)
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
        // Gallery view (default: off) and tiles/row (default: 3, 2–8).
        let gallery_view = matches!(
            library.get_setting("gallery_view").ok().flatten().as_deref(),
            Some("1")
        );
        let gallery_columns = library
            .get_setting("gallery_columns")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(3)
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
                move || sender.input(Msg::PlaybackError),
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
                gallery_tried: std::cell::RefCell::new(std::collections::HashSet::new()),
                gallery_hooked: std::cell::RefCell::new(std::collections::HashSet::new()),
            },
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
                fs_scroll: std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new())),
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
            },
            mini: MiniState {
                now_playing: None,
                playing: false,
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
            },
            settings_nc_list: std::rc::Rc::new(std::cell::RefCell::new(None)),
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
            },
            sync_page,
            sync_connected: false,
            cloud_page,
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

        model.load_dir(&sender);
        model.reload_albums();
        model.reload_artists();
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
        // Automatically read the library at startup and – on Wi-Fi/LAN and
        // with the switch enabled – fetch missing covers/metadata in the background.
        model.start_scan(&sender, true);

        let entries_box = model.libview.entries.widget();
        let albums_box = model.libview.albums.widget();
        let artists_box = model.libview.artists.widget();
        let albums_gallery = model.libview.albums_gallery.clone();
        let artists_gallery = model.libview.artists_gallery.clone();
        let concerts_gallery = model.concerts.concerts_gallery.clone();
        let audiobooks_gallery = model.favorites.audiobooks_gallery.clone();
        let podcasts_gallery = model.podcasts.podcasts_gallery.clone();
        let widgets = view_output!();
        model.nav.view_stack = widgets.view_stack.clone();
        model.nav.nav_view = widgets.nav_view.clone();
        model.nav.split = widgets.split.clone();
        model.mini.seek_scale = widgets.seek_scale.clone();
        model.mini.chapter_label = widgets.chapter_label.clone();
        model.files.source_tabs = widgets.source_tabs.clone();
        model.rebuild_source_tabs();

        // Hover over the seek bar → temporarily show the hovered chapter below the
        // title; on leaving, back to the current chapter (at the
        // playback position). Updates only the label (no view rebuild).
        // A small helper function sets the label from a time value.
        fn show_chapter_at(
            label: &gtk::Label,
            chapters: &std::cell::RefCell<Vec<(i64, String)>>,
            val_ms: i64,
        ) {
            let chaps = chapters.borrow();
            let name = chaps
                .iter()
                .rev()
                .find(|(ms, _)| *ms <= val_ms)
                .map(|(_, n)| n.clone())
                .filter(|n| !n.is_empty());
            match name {
                Some(n) => {
                    label.set_text(&n);
                    label.set_visible(true);
                }
                None => label.set_visible(false),
            }
        }
        {
            let chapters = model.mini.chapters.clone();
            let hovering = model.mini.hovering_seek.clone();
            let scale = widgets.seek_scale.clone();
            let label = widgets.chapter_label.clone();
            let motion = gtk::EventControllerMotion::new();
            {
                let (chapters, scale, label, hovering) =
                    (chapters.clone(), scale.clone(), label.clone(), hovering.clone());
                motion.connect_motion(move |_, x, _| {
                    if chapters.borrow().is_empty() {
                        return;
                    }
                    let adj = scale.adjustment();
                    let w = scale.width() as f64;
                    let span = adj.upper() - adj.lower();
                    if w <= 0.0 || span <= 0.0 {
                        return;
                    }
                    hovering.set(true);
                    let val = adj.lower() + (x / w).clamp(0.0, 1.0) * span;
                    show_chapter_at(&label, &chapters, val as i64);
                });
            }
            motion.connect_leave(move |_| {
                hovering.set(false);
                // Back to the chapter at the current playback position.
                let pos = scale.adjustment().value() as i64;
                show_chapter_at(&label, &chapters, pos);
            });
            widgets.seek_scale.add_controller(motion);
        }

        // Seek bar: dragging/clicking jumps to the position in the running track.
        // `change-value` fires only on user interaction (not on the
        // programmatic `set_value` of the tick), so there is no tug-of-war.
        {
            let sender = sender.clone();
            widgets.seek_scale.connect_change_value(move |_, _, value| {
                sender.input(Msg::Seek(value as i64));
                gtk::glib::Propagation::Proceed
            });
        }

        // Preserve the scroll position of the overview across navigation:
        // `adw::NavigationView` resets the position to 0 when shown again.
        // Therefore, when returning to the root page, restore the remembered value
        // (slightly delayed, after the re-layout).
        {
            let saved = model.nav.overview_scroll.clone();
            widgets.nav_view.connect_popped(move |nav, _page| {
                // Only when we return to the root overview.
                let is_root = nav
                    .visible_page()
                    .and_then(|p| p.tag())
                    .is_some_and(|t| t == "main");
                if !is_root {
                    return;
                }
                if let Some((sc, value)) = saved.borrow().clone() {
                    // Restore with a short delay (only after the re-layout, which
                    // otherwise resets the scroller to 0); second attempt as
                    // a safeguard against timing fluctuations.
                    for delay in [50u64, 250] {
                        let sc = sc.clone();
                        gtk::glib::timeout_add_local_once(
                            std::time::Duration::from_millis(delay),
                            move || sc.vadjustment().set_value(value),
                        );
                    }
                }
            });
        }

        // Adaptive: only at mobile (narrow) width collapse the sidebar and
        // show the top nav. On the desktop the left sidebar remains initially.
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            550.0,
            adw::LengthUnit::Sp,
        ));
        let yes = true.to_value();
        breakpoint.add_setter(&widgets.split, "collapsed", Some(&yes));
        breakpoint.add_setter(&widgets.top_nav, "visible", Some(&yes));
        // Show settings at the top only in narrow mode (desktop: sidebar).
        breakpoint.add_setter(&widgets.settings_top_btn, "visible", Some(&yes));
        // The desktop spacing between title bar and content is dropped in narrow mode.
        breakpoint.add_setter(&widgets.content_overlay, "margin-top", Some(&0i32.to_value()));
        // The transport bar would otherwise overflow on narrow phones: hide the
        // EQ button there (still reachable via the track's context menu).
        breakpoint.add_setter(&widgets.eq_btn, "visible", Some(&false.to_value()));
        root.add_breakpoint(breakpoint);

        // Create the icon-only navigation (sidebar + top) in the **saved
        // order** and couple it to the stack. All buttons
        // are created; hidden menu items are merely invisible.
        model.nav.sidebar_nav = widgets.sidebar_nav.clone();
        model.nav.top_nav = widgets.top_nav.clone();
        let mut nav_buttons: Vec<(&'static str, bool, gtk::ToggleButton)> = Vec::new();
        for (is_sidebar, container) in [
            (true, widgets.sidebar_nav.clone()),
            (false, widgets.top_nav.clone()),
        ] {
            let mut group_leader: Option<gtk::ToggleButton> = None;
            for &name in &model.nav.section_order {
                let Some((label, icon)) = section_meta(name) else {
                    continue;
                };
                let btn = gtk::ToggleButton::builder().build();
                btn.set_visible(!model.nav.hidden_sections.contains(name));
                btn.add_css_class("flat");
                // Highlight the active menu item blue on the icon (CSS `:checked`).
                btn.add_css_class("emilia-nav-btn");
                if is_sidebar {
                    // Desktop sidebar: icon **with label**. A slightly
                    // larger icon (clearly visible, never smaller than the default).
                    let inner = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                    let img = gtk::Image::from_icon_name(icon);
                    img.set_pixel_size(22);
                    inner.append(&img);
                    inner.append(&gtk::Label::new(Some(&gettext(label))));
                    btn.set_child(Some(&inner));
                    btn.set_hexpand(true);
                } else {
                    // Mobile top bar: icon only, noticeably larger (≈1.6×) than the
                    // default size – never smaller than now.
                    let img = gtk::Image::from_icon_name(icon);
                    img.set_pixel_size(26);
                    btn.set_child(Some(&img));
                    btn.set_tooltip_text(Some(&gettext(label)));
                }
                match &group_leader {
                    Some(leader) => btn.set_group(Some(leader)),
                    None => group_leader = Some(btn.clone()),
                }
                {
                    let stack = widgets.view_stack.clone();
                    let sender = sender.clone();
                    btn.connect_clicked(move |b| {
                        if b.is_active() {
                            stack.set_visible_child_name(name);
                            // Click on the menu item = to the start of the section.
                            if name == "files" {
                                sender.input(Msg::FilesGoStart);
                            }
                        }
                    });
                }
                container.append(&btn);
                nav_buttons.push((name, is_sidebar, btn));
            }
        }
        model.nav.nav_buttons = nav_buttons.clone();

        // Desktop sidebar: "Settings" at the very bottom – layout/design like
        // the menu items above (icon + label). A stretchable spacer
        // pushes the button to the bottom end.
        let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spacer.set_vexpand(true);
        widgets.sidebar_nav.append(&spacer);
        let settings_btn = gtk::Button::builder().build();
        settings_btn.add_css_class("flat");
        settings_btn.set_hexpand(true);
        let settings_inner = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        settings_inner.append(&gtk::Image::from_icon_name("emblem-system-symbolic"));
        settings_inner.append(&gtk::Label::new(Some(&gettext("Settings"))));
        settings_btn.set_child(Some(&settings_inner));
        {
            let sender = sender.clone();
            settings_btn.connect_clicked(move |_| sender.input(Msg::OpenSettings));
        }
        widgets.sidebar_nav.append(&settings_btn);

        // Set the active button to match the visible stack page and show the name
        // of the menu item discreetly as the subtitle of the header.
        let win_title = widgets.win_title.clone();
        let sync_active =
            move |stack: &adw::ViewStack, buttons: &[(&'static str, bool, gtk::ToggleButton)]| {
                let cur = stack.visible_child_name();
                let cur = cur.as_deref().unwrap_or("files");
                for (name, _is_sidebar, btn) in buttons {
                    btn.set_active(*name == cur);
                }
                win_title.set_subtitle(
                    &section_meta(cur)
                        .map(|(l, _)| gettext(l))
                        .unwrap_or_default(),
                );
            };
        // Restore the most recently open navigation item – but not a
        // hidden one. As a fallback, fall to the first visible menu item (in the
        // chosen order).
        let restore = saved_section
            .as_deref()
            .filter(|s| !model.nav.hidden_sections.contains(*s))
            .or_else(|| {
                model
                    .nav
                    .section_order
                    .iter()
                    .copied()
                    .find(|n| !model.nav.hidden_sections.contains(*n))
            });
        if let Some(section) = restore {
            widgets.view_stack.set_visible_child_name(section);
        }
        sync_active(&widgets.view_stack, &nav_buttons);
        {
            let stats_sender = model.stats_page.sender().clone();
            widgets
                .view_stack
                .connect_visible_child_notify(move |stack| {
                    sync_active(stack, &nav_buttons);
                    // Recompute the statistics fresh when opening the section.
                    if stack.visible_child_name().as_deref() == Some("stats") {
                        stats_sender.emit(crate::ui::stats_page::StatsInput::Refresh);
                    }
                });
        }

        // Swipe gesture on the whole file system page: to the right = back.
        let swipe = gtk::GestureSwipe::new();
        swipe.set_touch_only(false);
        {
            let sender = sender.clone();
            swipe.connect_swipe(move |_, vx, vy| {
                if vx > 300.0 && vx.abs() > vy.abs() * 1.5 {
                    sender.input(Msg::NavUp);
                }
            });
        }
        widgets.files_page.add_controller(swipe);

        // Restore the window size and save it on close.
        if let (Some(w), Some(h)) = (saved_w, saved_h) {
            root.set_default_size(w, h);
        }
        if saved_max {
            root.maximize();
        }
        let stack_for_close = widgets.view_stack.clone();
        let close_resume = model.transport.close_resume.clone();
        let close_session = model.transport.close_session.clone();
        root.connect_close_request(move |win| {
            // Save the last listening position (covers the gap to the 5-s save).
            if let Some((path, pos, dur)) = close_resume.borrow().clone() {
                if let Ok(lib) = Library::open() {
                    let _ = lib.set_resume_path(&path, guarded_resume(pos, dur));
                }
            }
            // Save the running listening session as the last event (otherwise the
            // currently playing track would be lost on a hard exit).
            if let Some((path, started_at, played_ms, dur)) = close_session.borrow().clone() {
                if played_ms > 0 {
                    if let Ok(lib) = Library::open() {
                        let _ = lib.log_play(&path, started_at, played_ms, dur, false, None);
                    }
                }
            }
            let section = stack_for_close.visible_child_name();
            save_window_state(
                win.default_width(),
                win.default_height(),
                win.is_maximized(),
                section.as_deref(),
            );
            gtk::glib::Propagation::Proceed
        });

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, root: &Self::Root) {
        match msg {
            Msg::Activate(index) => {
                let entry = self.libview.entries.guard().get(index).map(|r| r.entry.clone());
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
                        let is_active = self.mini.now_playing.is_some()
                            && self.transport.queue.get(self.transport.queue_pos) == Some(&path);
                        if is_active {
                            if self.mini.playing {
                                self.save_resume();
                                self.player.pause();
                            } else {
                                self.player.resume();
                            }
                            self.mini.playing = !self.mini.playing;
                            self.mpris.set_playing(self.mini.playing);
                            self.refresh_queue_icons();
                        } else {
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
                    if let Some(pos) = self.transport.queue.iter().position(|p| *p == path) {
                        self.transport.queue.remove(pos);
                        if self.transport.queue_pos > pos {
                            self.transport.queue_pos -= 1;
                        }
                        self.toast(&gettext("Removed from queue"));
                    } else {
                        self.transport.queue.push(path);
                        self.toast(&gettext("Will play next"));
                    }
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
                self.nav.context_target = Some(CtxTarget::Album(meta));
                self.open_context_menu(root, &sender);
            }
            Msg::ShowTrackDetail(path) => {
                self.nav.context_target =
                    Some(CtxTarget::Fs(FsEntry::file(PathBuf::from(path))));
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
                    .or_else(|| self.libview.albums_overview.get(index).map(|m| m.album.clone()));
                if let Some(album) = album {
                    self.open_album_by_name(&sender, &album);
                }
            }
            Msg::ShowConcertDetail(index) => {
                if let Some((scope, key, _, is_dir)) = self.concerts.concert_items.get(index).cloned() {
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
            Msg::PlayFolderTrack { folder, path } => {
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
                    self.nav.nav_view.pop_to_tag("main");
                }
            }
            Msg::PlayArtistTrack { name, path } => {
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
                    self.nav.nav_view.pop_to_tag("main");
                }
            }
            Msg::PlayAlbumTrack { artist, album, path } => {
                // Queue = whole album in track order, start at the tapped one.
                // `artist` here is the (page) artist – the same set of tracks
                // as on the album subpage.
                let files: Vec<PathBuf> = self
                    .album_tracks_for_artist(&artist, &album)
                    .into_iter()
                    .map(|t| PathBuf::from(t.path))
                    .collect();
                let target = PathBuf::from(&path);
                if let Some(pos) = files.iter().position(|p| *p == target) {
                    self.transport.queue = files;
                    self.transport.queue_pos = pos;
                    self.play_current();
                    self.refresh_queue_icons();
                    self.nav.nav_view.pop_to_tag("main");
                }
            }
            Msg::PlayAlbumByNameTrack { album, path } => {
                // Queue = all tracks of the album name (across artists),
                // start at the tapped one – matching the album overview.
                let files: Vec<PathBuf> = self
                    .album_tracks_by_name(&album)
                    .into_iter()
                    .map(|t| PathBuf::from(t.path))
                    .collect();
                let target = PathBuf::from(&path);
                if let Some(pos) = files.iter().position(|p| *p == target) {
                    self.transport.queue = files;
                    self.transport.queue_pos = pos;
                    self.play_current();
                    self.refresh_queue_icons();
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
                    let was_empty = self.transport.queue.is_empty();
                    self.transport.queue.append(&mut files);
                    // Don't auto-start: just enqueue. If the queue was empty,
                    // point the position at the first added track so the play
                    // button starts there when the user decides to play.
                    if was_empty {
                        self.transport.queue_pos = 0;
                    }
                    self.refresh_queue_icons();
                    self.save_queue();
                    self.toast(&gettext_f(
                        "Added {n} tracks to the queue",
                        &[("n", &n.to_string())],
                    ));
                }
            }
            Msg::CtxAddPlaylist => self.open_add_to_playlist_dialog(root, &sender),
            Msg::PlaylistNew => self.open_new_playlist_dialog(root, &sender),
            Msg::PlaylistCreate(name) => {
                let name = name.trim();
                if !name.is_empty() {
                    let _ = self.library.create_playlist(name);
                    self.reload_playlists(&sender);
                    self.toast(&gettext("Playlist created"));
                }
            }
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
                if let Some((_, name, _)) =
                    self.playlists.playlist_items.iter().find(|(pid, _, _)| *pid == id).cloned()
                {
                    self.open_playlist(&sender, id, &name);
                }
            }
            Msg::PlayPlaylist(id) => {
                let paths = self.library.playlist_paths(id).unwrap_or_default();
                if !paths.is_empty() {
                    self.transport.queue = paths.into_iter().map(PathBuf::from).collect();
                    self.transport.queue_pos = 0;
                    self.play_current();
                    self.refresh_queue_icons();
                }
            }
            Msg::PlaylistTrack { id, path } => {
                let queue: Vec<PathBuf> = self
                    .library
                    .playlist_paths(id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(PathBuf::from)
                    .collect();
                if let Some(pos) = queue.iter().position(|p| p.to_string_lossy() == path) {
                    self.transport.queue = queue;
                    self.transport.queue_pos = pos;
                    self.play_current();
                    self.refresh_queue_icons();
                }
            }
            Msg::PlaylistDelete(id) => {
                let _ = self.library.delete_playlist(id);
                self.reload_playlists(&sender);
                self.toast(&gettext("Playlist deleted"));
            }
            Msg::PlaylistRemoveTrack { id, path } => {
                let _ = self.library.remove_from_playlist(id, &path);
                self.reload_playlists(&sender);
                // Rebuild the subpage (replace the old one).
                self.nav.nav_view.pop();
                if let Some((_, name, _)) =
                    self.playlists.playlist_items.iter().find(|(pid, _, _)| *pid == id).cloned()
                {
                    self.open_playlist(&sender, id, &name);
                }
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
                if let Some((_, title, _, _)) =
                    self.podcasts.podcast_items.iter().find(|(pid, _, _, _)| *pid == id).cloned()
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
                let _ = self.library.delete_podcast(id);
                self.reload_podcasts(&sender);
                self.toast(&gettext("Podcast removed"));
            }
            // --- Streaming (internet radio) ---
            Msg::StreamAdd => self.open_add_stream_dialog(root, &sender),
            Msg::StreamSearch(term) => {
                let term = term.trim().to_string();
                if !term.is_empty() {
                    self.toast(&gettext("Searching …"));
                    sender.spawn_command(move |out| {
                        let results =
                            crate::core::streaming::search_stations(&term).unwrap_or_default();
                        // Show hits immediately (still without logos) …
                        let _ = out.send(Cmd::StreamSearchResults(results.clone()));
                        // … and fetch the logos afterwards in the background.
                        for r in &results {
                            if let Some(img) = r.favicon.as_deref() {
                                crate::core::online::cache_station_image(img);
                            }
                        }
                        let _ = out.send(Cmd::StreamSearchCoversReady);
                    });
                }
            }
            Msg::StreamAddResult(index) => self.add_stream_result(&sender, index),
            Msg::StreamAddUrl(url) => {
                let url = url.trim().to_string();
                if !url.is_empty() {
                    let name = crate::core::streaming::name_from_url(&url);
                    match self
                        .library
                        .add_stream(&name, &url, None, None, None, None, None)
                    {
                        Ok(_) => {
                            self.reload_streams(&sender);
                            self.toast(&gettext("Station added"));
                        }
                        Err(_) => self.toast(&gettext("Could not add station")),
                    }
                }
            }
            Msg::ToggleStream(id) => {
                if self.streaming.playing_stream == Some(id) {
                    // Already running → toggle pause/resume (buffer keeps running).
                    if self.mini.playing {
                        self.player.pause();
                        self.mini.playing = false;
                    } else {
                        self.player.resume();
                        self.mini.playing = true;
                    }
                    self.mpris.set_playing(self.mini.playing);
                } else {
                    self.play_stream(id);
                }
                self.refresh_stream_icons();
            }
            Msg::StreamRecordToggle(id) => {
                if self.streaming.record_state.as_ref().map(|r| r.stream_id) == Some(id) {
                    // Running → stop.
                    sender.input(Msg::RecordStop);
                } else if self.streaming.recording_buffer_minutes == 0 {
                    self.toast(&gettext("Enable the recording buffer in the settings first"));
                } else {
                    // Ensure the station (with buffer), then start the continuous recording.
                    if self.streaming.playing_stream != Some(id) {
                        self.play_stream(id);
                    }
                    self.record_arm(&sender, id);
                    self.refresh_stream_icons();
                }
            }
            Msg::TransportRecordToggle => {
                if let Some(id) = self.streaming.playing_stream {
                    sender.input(Msg::StreamRecordToggle(id));
                }
            }
            Msg::StreamTitle(title) => {
                // Only relevant while a station is running (file/episode tags
                // are ignored). Shows "Station — Title" in the mini player and
                // reports the title to the lock screen/media keys.
                let title = title.trim().to_string();
                if let Some(id) = self.streaming.playing_stream {
                    if !title.is_empty() && self.streaming.stream_title.as_deref() != Some(title.as_str()) {
                        self.streaming.stream_title = Some(title.clone());
                        let station = self
                            .streaming
                            .stream_items
                            .iter()
                            .find(|s| s.id == id)
                            .map(|s| s.name.clone());
                        self.mini.now_playing = Some(match &station {
                            Some(name) => format!("{name} — {title}"),
                            None => title.clone(),
                        });
                        self.mpris
                            .set_metadata(0, &title, station.as_deref(), None, None, None);
                    }
                }
            }
            Msg::OpenStream(id) => self.open_stream(root, &sender, id),
            Msg::StreamDelete(id) => {
                if self.streaming.playing_stream == Some(id) {
                    self.player.stop();
                    self.mini.playing = false;
                    self.streaming.playing_stream = None;
                    self.mini.now_playing = None;
                    self.mpris.set_playing(false);
                    self.stop_recorder();
                }
                let _ = self.library.delete_stream(id);
                self.reload_streams(&sender);
                self.toast(&gettext("Station removed"));
            }
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
            Msg::ReplayPlay { start, end } => {
                let temp = self.streaming.recorder.as_ref().and_then(|r| r.extract_temp(start, end).ok());
                match temp {
                    Some(path) => {
                        let p = path.to_string_lossy().to_string();
                        self.player.stop();
                        match self.player.play_file(&p, 0) {
                            Ok(()) => {
                                self.mini.now_playing = Some(gettext("Replay"));
                                self.mini.playing = true;
                                self.transport.playing_path = Some(path);
                                self.podcasts.playing_episode_url = None;
                                self.streaming.playing_stream = None;
                                self.mpris.set_playing(true);
                            }
                            Err(e) => tracing::error!("Replay failed: {e}"),
                        }
                    }
                    None => self.toast(&gettext("Could not extract from buffer")),
                }
            }
            Msg::ReplaySave { start, end, title } => {
                let station = self
                    .streaming
                    .playing_stream
                    .and_then(|id| self.streaming.stream_items.iter().find(|s| s.id == id))
                    .map(|s| s.name.clone());
                if self.store_segment(&sender, start, end, &title, station.as_deref(), false) {
                    self.reload_recordings(&sender);
                } else {
                    self.toast(&gettext("Could not extract from buffer"));
                }
            }
            Msg::PlayRecording(path) => {
                let p = PathBuf::from(&path);
                if p.exists() {
                    self.stop_recorder();
                    self.transport.queue = vec![p];
                    self.transport.queue_pos = 0;
                    self.play_current();
                } else {
                    self.toast(&gettext("File not found"));
                }
            }
            Msg::OpenRecording(id) => self.open_recording(root, &sender, id),
            Msg::RecordingDelete(id) => {
                if let Ok(Some(path)) = self.library.delete_recording(id) {
                    let _ = std::fs::remove_file(&path);
                }
                self.reload_recordings(&sender);
                self.toast(&gettext("Recording deleted"));
            }
            Msg::SetRecordingBufferMinutes(n) => {
                self.streaming.recording_buffer_minutes = n.min(60);
                let _ = self.library.set_setting(
                    "recording_buffer_minutes",
                    &self.streaming.recording_buffer_minutes.to_string(),
                );
            }
            Msg::ToggleEpisode { url, title } => {
                if self.podcasts.playing_episode_url.as_deref() == Some(url.as_str()) {
                    // Already loaded episode → toggle pause/resume.
                    if self.mini.playing {
                        self.player.pause();
                    } else {
                        self.player.resume();
                    }
                    self.mini.playing = !self.mini.playing;
                    self.mpris.set_playing(self.mini.playing);
                    self.refresh_queue_icons();
                } else {
                    // Other/no episode → start this one.
                    self.play_episode(&url, &title);
                }
            }
            Msg::EpisodeSeekTo { url, title, ms } => {
                if self.podcasts.playing_episode_url.as_deref() == Some(url.as_str()) {
                    // Already running → jump directly to the spot.
                    if self.player.seek_ms(ms).is_ok() {
                        self.mini.position_ms = ms;
                        self.save_episode_progress();
                    }
                } else {
                    // Otherwise start the episode at the jump mark.
                    self.play_episode_at(&url, &title, ms);
                }
            }
            Msg::ToggleEpisodeDownload { url, title } => {
                // Ignore taps while a download for this episode is in flight.
                if self.podcasts.downloading_episodes.contains(&url) {
                    return;
                }
                // Already downloaded → delete the local copy to free space.
                // Future plays then fall back to streaming; a copy currently
                // playing keeps its open file handle until the track changes.
                if let Some(path) = self.library.delete_episode_download(&url).unwrap_or(None) {
                    let _ = std::fs::remove_file(&path);
                    self.refresh_download_row();
                    self.toast(&gettext("Download removed"));
                    return;
                }
                // Not downloaded → fetch the audio in the background.
                self.podcasts.downloading_episodes.insert(url.clone());
                self.refresh_download_row();
                self.toast(&gettext_f("Downloading “{title}” …", &[("title", &title)]));
                let dl_url = url.clone();
                sender.spawn_command(move |out| {
                    let dest = crate::core::online::episode_download_dest(&dl_url);
                    let result = match crate::core::podcast::download_episode(&dl_url, &dest) {
                        Ok(_) => {
                            let path = dest.to_string_lossy().into_owned();
                            // Persist the offline copy (worker thread, own DB).
                            if let Ok(lib) = Library::open() {
                                let _ = lib.set_episode_download(&dl_url, &path);
                            }
                            Ok(path)
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = out.send(Cmd::EpisodeDownloaded {
                        url: dl_url.clone(),
                        result,
                    });
                });
            }
            Msg::SetPodcastView(view) => self.podcasts.podcast_view = view,
            Msg::SetStreamView(view) => self.streaming.stream_view = view,
            Msg::ShowEpisodeDetail(index) => self.open_episode_detail(root, &sender, index),
            Msg::ShowPodcastEpisodeDetail { podcast_id, index } => {
                self.open_podcast_episode_detail(root, &sender, podcast_id, index)
            }
            Msg::ShowPodcastDetail(id) => self.open_podcast_detail(root, &sender, id),
            Msg::CtxEqualizer => self.open_eq_dialog(root, &sender),
            Msg::CtxShare => self.open_share_dialog(root, &sender),
            Msg::ShareHost => {
                use crate::ui::sync_page::SyncInput;
                self.sync_page.emit(SyncInput::Open(root.clone()));
                self.sync_page.emit(SyncInput::StartServer);
            }
            Msg::ShareScan => {
                use crate::ui::sync_page::SyncInput;
                self.sync_page.emit(SyncInput::Open(root.clone()));
                self.sync_page.emit(SyncInput::StartScan);
            }
            Msg::SyncConnected(connected) => self.sync_connected = connected,
            Msg::SyncImported => {
                self.load_favorites(&sender);
                self.reload_playlists(&sender);
                self.reload_podcasts(&sender);
            }
            Msg::TrackFinished => {
                if self.files.playing_remote {
                    // Remote queue: advance to the next track (or stop at the
                    // end). Runs separately from the local queue.
                    self.remote_next();
                } else if self.podcasts.playing_episode_url.is_some() && self.transport.queue.is_empty() {
                    // A streamed episode has ended (no queue
                    // behind it): reset the playback state, clear the marking.
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
                    if self.transport.queue.len() == 1 && self.transport.interrupted_queue.is_some() {
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
            Msg::Mpris(cmd) => {
                use crate::core::mpris::MprisCommand as M;
                match cmd {
                    M::PlayPause => {
                        if self.mini.now_playing.is_some() {
                            if self.mini.playing {
                                self.save_resume();
                                self.player.pause();
                            } else {
                                self.player.resume();
                            }
                            self.mini.playing = !self.mini.playing;
                            self.mpris.set_playing(self.mini.playing);
                            self.refresh_queue_icons();
                        }
                    }
                    M::Play => {
                        if self.mini.now_playing.is_some() && !self.mini.playing {
                            self.player.resume();
                            self.mini.playing = true;
                            self.mpris.set_playing(true);
                            self.refresh_queue_icons();
                        }
                    }
                    M::Pause => {
                        if self.mini.now_playing.is_some() && self.mini.playing {
                            self.save_resume();
                            self.player.pause();
                            self.mini.playing = false;
                            self.mpris.set_playing(false);
                            self.refresh_queue_icons();
                        }
                    }
                    M::Next => {
                        if self.files.playing_remote {
                            self.remote_next();
                        } else {
                            self.play_next();
                        }
                    }
                    M::Prev => {
                        if self.files.playing_remote {
                            self.remote_prev();
                        } else {
                            self.play_prev();
                        }
                    }
                    M::Stop => {
                        self.save_resume();
                        self.finalize_play_session(false);
                        self.player.stop();
                        self.mini.playing = false;
                        self.transport.playing_path = None;
                        self.mini.position_ms = 0;
                        self.mini.track_duration_ms = 0;
                        *self.transport.close_resume.borrow_mut() = None;
                        self.mpris.set_stopped();
                        self.refresh_queue_icons();
                    }
                    M::Raise => root.present(),
                    M::SeekBy(offset_us) => {
                        let cur = self.player.position_ms().unwrap_or(0);
                        let target = (cur + offset_us / 1000).max(0);
                        if self.player.seek_ms(target).is_ok() {
                            self.mpris.seeked(target);
                        }
                    }
                    M::SetPosition(pos_us) => {
                        let target = (pos_us / 1000).max(0);
                        if self.player.seek_ms(target).is_ok() {
                            self.mpris.seeked(target);
                        }
                    }
                }
            }
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
            }
            Msg::ToggleRepeat => {
                self.transport.repeat = !self.transport.repeat;
                let _ = self
                    .library
                    .set_setting("repeat", if self.transport.repeat { "1" } else { "0" });
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
                // Re-index the cloud sources too, so their structure and covers
                // update (existing sources are only indexed when first added).
                // On completion this rebuilds the views and fetches covers.
                self.reindex_cloud_sources(&sender);
                // "Rescan" also updates the local library (artists/albums).
                self.start_scan(&sender, false);
            }
            Msg::OpenSettings => self.open_settings(root, &sender),
            Msg::CheckForUpdates => {
                if crate::core::update::in_flatpak() {
                    self.toast(&gettext("Checking for updates …"));
                    sender.spawn_oneshot_command(|| {
                        Cmd::UpdateChecked(crate::core::update::check())
                    });
                } else {
                    self.toast(&gettext("Updates are only available in the Flatpak version."));
                }
            }
            Msg::InstallFlatpakUpdate => {
                self.toast(&gettext(
                    "Update started – it runs in the background. Please restart Emilia afterwards.",
                ));
                let sender2 = sender.clone();
                // Trigger via the Flatpak portal (main thread; progress via signal).
                if let Err(e) =
                    crate::core::update::install(move |res| sender2.input(Msg::FlatpakUpdateFinished(res)))
                {
                    tracing::warn!("Flatpak update failed to start: {e}");
                    sender.input(Msg::FlatpakUpdateFinished(Err(e.to_string())));
                }
            }
            Msg::FlatpakUpdateFinished(res) => match res {
                Ok(()) => self.toast(&gettext("Update installed. Please restart Emilia.")),
                Err(_) => self.toast(&gettext_f(
                    "Update failed. You can update manually: {cmd}",
                    &[("cmd", &crate::core::update::manual_command())],
                )),
            },
            Msg::OpenGlobalEq => self.open_global_eq(root, &sender),
            Msg::OpenCurrentEq => {
                if let Some(path) = self.transport.queue.get(self.transport.queue_pos).cloned() {
                    let key = path.to_string_lossy().into_owned();
                    let name = Self::track_display_name(&path);
                    self.open_eq_editor(root, &sender, "the track", &name, None, "track", key);
                }
            }
            Msg::ShowQueue => self.open_queue_dialog(root, &sender),
            Msg::PlayQueueAt(idx) => {
                if idx < self.transport.queue.len() {
                    self.transport.queue_pos = idx;
                    self.play_current();
                    self.reload_queue_list(&sender);
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
            Msg::PlaybackError => {
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
                self.player.stop();
                self.transport.queue.clear();
                self.transport.queue_pos = 0;
                self.transport.shuffle_order.clear();
                self.transport.shuffle_idx = 0;
                self.mini.playing = false;
                self.mini.now_playing = None;
                self.transport.playing_path = None;
                self.mini.position_ms = 0;
                self.mini.track_duration_ms = 0;
                *self.transport.close_resume.borrow_mut() = None;
                self.mpris.set_stopped();
                self.reload_queue_list(&sender);
                self.refresh_queue_icons();
                self.save_queue();
                self.toast(&gettext("Queue cleared"));
            }
            Msg::QueueMove { from, to } => {
                let len = self.transport.queue.len();
                if from < len && to < len && from != to {
                    let item = self.transport.queue.remove(from);
                    self.transport.queue.insert(to, item);
                    // Adjust queue_pos so that the same track keeps playing.
                    let cur = self.transport.queue_pos;
                    self.transport.queue_pos = if cur == from {
                        to
                    } else {
                        let mut p = cur;
                        if from < cur {
                            p -= 1;
                        }
                        if to <= p {
                            p += 1;
                        }
                        p
                    };
                    self.reload_queue_list(&sender);
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
                self.start_scan(&sender, true);
            }
            Msg::SelectSource(sel) => {
                if self.files.active_source != sel {
                    self.apply_source(sel, &sender);
                }
            }
            Msg::SourcesChanged => {
                self.files.sources = self.library.list_sources().unwrap_or_default();
                // If the active source was removed, back to the primary tab.
                let gone = match &self.files.active_source {
                    ActiveSource::Primary => false,
                    ActiveSource::Source(id) => !self.files.sources.iter().any(|s| s.id == *id),
                };
                if gone {
                    self.apply_source(ActiveSource::Primary, &sender);
                }
                self.rebuild_source_tabs();
                // Indexed cloud tracks may have been added/removed.
                self.reload_albums();
                self.reload_artists();
                // Refresh the "Connected" list of the Nextcloud settings page,
                // in case the settings dialog is currently open.
                let nc_list = self.settings_nc_list.borrow().clone();
                if let Some(list) = nc_list {
                    if list.root().is_some() {
                        self.fill_nc_list(&list, &sender);
                    } else {
                        *self.settings_nc_list.borrow_mut() = None;
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
                self.reload_albums();
                self.reload_artists();
                if self.enrich_state.auto_enrich && !self.enrich_state.enriching && online_available() {
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
            Msg::SetAlbumCover { artist, album, path } => {
                let mut meta = self
                    .library
                    .get_album_meta(&artist, &album)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| crate::model::AlbumMeta::pending(&artist, &album));
                // Save + refresh the views only on an actual change.
                if meta.cover_path.as_deref() != Some(path.as_str()) {
                    meta.cover_path = Some(path);
                    let _ = self.library.upsert_album_meta(&meta);
                    self.reload_albums();
                }
            }
            Msg::SetArtistImage { name, path } => {
                let mut meta = self
                    .library
                    .get_artist_meta(&name)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| crate::model::ArtistMeta::pending(&name));
                if meta.image_path.as_deref() != Some(path.as_str()) {
                    meta.image_path = Some(path);
                    let _ = self.library.upsert_artist_meta(&meta);
                    self.reload_artists();
                }
            }
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
            Msg::SetAreas { scope, key, value } => {
                if let Err(e) = self.library.set_category(scope, &key, Some(&value)) {
                    tracing::error!("Failed to save properties: {e}");
                }
                // Visibility/assignment may have changed anywhere →
                // reload the views. Concerts/audiobooks are derived live from
                // the properties (no separate reconciliation needed).
                self.reload_albums();
                self.reload_artists();
                self.load_concerts(&sender);
                self.load_audiobooks(&sender);
                self.load_dir(&sender);
            }
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
            Msg::ClearEq { output, scope, key } => {
                let _ = self.library.clear_eq(&output, scope, &key);
                self.apply_current_eq();
            }
            Msg::ConcertImport => {
                // Concert import refers to the primary library.
                let Some(root) = self.files.music_dir.as_ref().map(PathBuf::from) else {
                    self.toast(&gettext("No music folder set"));
                    return;
                };
                let existing = self.library.concert_paths().unwrap_or_default();
                self.toast(&gettext("Searching for concerts …"));
                sender.spawn_oneshot_command(move || {
                    Cmd::Candidates(crate::core::concert::scan_candidates(&root, &existing))
                });
            }
            Msg::ConcertDismissHint => {
                self.concerts.concert_hint_dismissed = true;
                let _ = self.library.set_setting("concert_hint_dismissed", "1");
            }
            Msg::ConcertHideSection => {
                self.set_section_visible("concerts", false);
                self.toast(&gettext("Hid the Concerts menu item"));
            }
            Msg::ConcertAdd(items) => {
                let n = items.len();
                for (path, title, is_dir) in &items {
                    // Table: only for the candidate filtering at the next import.
                    let _ = self.library.add_concert(path, title, *is_dir);
                    // Display/management via the properties: mark the
                    // "Concerts" area on the contained albums/tracks, so that
                    // the concert can also be removed again via it.
                    let entries = if *is_dir {
                        self.folder_albums_and_tracks(path)
                    } else {
                        vec![("track".to_string(), path.clone(), title.clone(), false)]
                    };
                    for (scope, key, _, _) in entries {
                        let _ = self.library.add_category_area(
                            &scope,
                            &key,
                            crate::core::category::Area::Concerts,
                        );
                    }
                }
                self.load_concerts(&sender);
                self.toast(&ngettext_n(
                    "Added {n} concert",
                    "Added {n} concerts",
                    n as u32,
                ));
            }
            Msg::PlayConcert(index) => {
                if let Some((scope, key, _, is_dir)) = self.concerts.concert_items.get(index).cloned() {
                    self.play_entry(&scope, &key, is_dir);
                }
            }
            Msg::OpenConcertEntry(index) => {
                // Gallery tap: like the list tap – album/folder opens the
                // track list, a single track is played.
                if let Some((scope, key, _, is_dir)) = self.concerts.concert_items.get(index).cloned() {
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
                if from < self.nav.section_order.len() && to < self.nav.section_order.len() && from != to {
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
                self.reload_albums();
                self.reload_artists();
                self.load_concerts(&sender);
                self.load_audiobooks(&sender);
                self.load_dir(&sender);
                self.toast(&gettext("Shown again"));
            }
            Msg::ToggleFavorite => {
                if let Some(target) = self.nav.context_target.clone() {
                    let (scope, key, title, is_dir) = self.favorite_ref(&target);
                    let on = !self.library.is_favorite(scope, &key);
                    let _ = self.library.set_favorite(scope, &key, &title, is_dir, on);
                    self.load_favorites(&sender);
                    self.toast(&if on {
                        gettext("Added to favorites")
                    } else {
                        gettext("Removed from favorites")
                    });
                }
            }
            Msg::PlayFavorite(index) => {
                if let Some((scope, key, _, is_dir)) = self.favorites.favorite_items.get(index).cloned() {
                    // If exactly this track is already playing, only toggle play/pause
                    // (a click on the shown pause sign pauses), instead of
                    // restarting it.
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
                        let pos = tracks.iter().position(|p| *p == PathBuf::from(&key)).unwrap_or(0);
                        self.transport.shuffle = false;
                        self.transport.queue = tracks;
                        self.transport.queue_pos = pos;
                        self.play_current();
                        self.refresh_queue_icons();
                    } else {
                        self.play_entry(&scope, &key, is_dir);
                    }
                    // Update the active marking (play/pause icon) in the favorites list.
                    self.load_favorites(&sender);
                }
            }
            Msg::ShowFavoriteDetail(index) => {
                if let Some((scope, key, _, is_dir)) = self.favorites.favorite_items.get(index).cloned() {
                    self.nav.context_target = Some(self.entry_target(&scope, &key, is_dir));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::MoveFavorite { from, to } => {
                if from < self.favorites.favorite_items.len() && to < self.favorites.favorite_items.len() && from != to {
                    let item = self.favorites.favorite_items.remove(from);
                    self.favorites.favorite_items.insert(to, item);
                    let order: Vec<(String, String)> = self
                        .favorites
                        .favorite_items
                        .iter()
                        .map(|(s, k, _, _)| (s.clone(), k.clone()))
                        .collect();
                    let _ = self.library.set_favorite_order(&order);
                    self.load_favorites(&sender);
                }
            }
            Msg::PlayAudiobook(index) => {
                if let Some((scope, key, _, is_dir)) = self.favorites.audiobook_items.get(index).cloned() {
                    self.play_entry(&scope, &key, is_dir);
                }
            }
            Msg::OpenAudiobookEntry(index) => {
                // Gallery tap: album/folder opens the track list, a single track plays.
                if let Some((scope, key, _, is_dir)) = self.favorites.audiobook_items.get(index).cloned() {
                    if scope == "track" {
                        self.play_entry(&scope, &key, is_dir);
                    } else {
                        sender.input(Msg::OpenEntryTracks { scope, key });
                    }
                }
            }
            Msg::ShowAudiobookDetail(index) => {
                if let Some((scope, key, _, is_dir)) = self.favorites.audiobook_items.get(index).cloned() {
                    self.nav.context_target = Some(self.entry_target(&scope, &key, is_dir));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::TogglePlay => {
                if self.mini.playing {
                    self.save_resume();
                    self.player.pause();
                    self.mini.playing = false;
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
                } else {
                    return;
                }
                self.mpris.set_playing(self.mini.playing);
                // Adjust the play/pause icon of the active track in the list.
                self.refresh_queue_icons();
                self.refresh_stream_icons();
            }
            Msg::OpenNowPlaying => {
                // Detail view of the running track (as a file entry).
                if let Some(path) = self.transport.queue.get(self.transport.queue_pos).cloned() {
                    self.nav.context_target = Some(CtxTarget::Fs(FsEntry::file(path)));
                    self.open_context_menu(root, &sender);
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
        match msg {
            Cmd::Entries(entries) => {
                // "Mixed album": more than one distinct artist in the folder.
                let distinct: std::collections::HashSet<String> =
                    entries.iter().filter_map(|e| e.effective_artist()).collect();
                let opts = RowOpts {
                    show_artist: distinct.len() > 1,
                };
                let queue = self.transport.queue.clone();
                let mut guard = self.libview.entries.guard();
                guard.clear();
                for e in entries {
                    let queued = e
                        .path()
                        .is_some_and(|ep| queue.iter().any(|p| p == ep));
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
                        dirs.sort_by(|a, b| natural_key(&a.name).cmp(&natural_key(&b.name)));
                        files.sort_by(|a, b| natural_key(&a.name).cmp(&natural_key(&b.name)));
                        // Source id, to read already-indexed track metadata
                        // (title/artist/duration) straight from the DB.
                        let source_id = match &source {
                            ActiveSource::Source(id) => Some(*id),
                            _ => None,
                        };
                        let mut entries: Vec<FsEntry> = Vec::with_capacity(dirs.len() + files.len());
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
                        let distinct: std::collections::HashSet<String> =
                            entries.iter().filter_map(|e| e.effective_artist()).collect();
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
                    self.reload_albums();
                    self.reload_artists();
                }
            }
            Cmd::ReloadViews => {
                self.reload_albums();
                self.reload_artists();
            }
            Cmd::ScanDone { then_enrich } => {
                // Library is read in → update the views.
                self.reload_albums();
                self.reload_artists();
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
            Cmd::CloudReindexed => {
                // Freshly indexed remote tracks → rebuild the library views and
                // favorites. Then fetch covers/photos (incl. the embedded covers
                // of the remote tracks); a manual refresh does this regardless of
                // the passive auto-enrich setting.
                self.reload_albums();
                self.reload_artists();
                self.load_favorites(&sender);
                if !self.enrich_state.enriching && online_available() {
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
                    self.reload_albums();
                    self.reload_artists();
                }
            }
            Cmd::UpdateChecked(result) => match result {
                crate::core::update::CheckResult::UpToDate => self.toast(&gettext_f(
                    "Emilia is up to date (version {v}).",
                    &[("v", env!("CARGO_PKG_VERSION"))],
                )),
                crate::core::update::CheckResult::Unknown => {
                    self.toast(&gettext("Could not check for updates (offline?)."))
                }
                crate::core::update::CheckResult::Available => {
                    // Confirmation before applying – installs via the portal.
                    let confirm = adw::AlertDialog::new(
                        Some(&gettext("Update available")),
                        Some(&gettext("A newer version of Emilia is available. Install it now?")),
                    );
                    confirm.add_response("cancel", &gettext("Cancel"));
                    confirm.add_response("update", &gettext("Update"));
                    confirm.set_response_appearance("update", adw::ResponseAppearance::Suggested);
                    confirm.set_default_response(Some("update"));
                    confirm.set_close_response("cancel");
                    let sender = sender.clone();
                    confirm.connect_response(None, move |_, resp| {
                        if resp == "update" {
                            sender.input(Msg::InstallFlatpakUpdate);
                        }
                    });
                    confirm.present(Some(root));
                }
            },
        }
    }
}

/// Saves the window size/maximization and the most recently open navigation item
/// (own short-lived DB connection, since called in the close handler).
fn save_window_state(width: i32, height: i32, maximized: bool, section: Option<&str>) {
    if let Ok(lib) = Library::open() {
        let _ = lib.set_setting("win_width", &width.to_string());
        let _ = lib.set_setting("win_height", &height.to_string());
        let _ = lib.set_setting("win_maximized", if maximized { "1" } else { "0" });
        if let Some(sec) = section {
            let _ = lib.set_setting("active_section", sec);
        }
    }
}

/// Formats milliseconds as `m:ss` or `h:mm:ss` (negative → 0).
pub(crate) fn fmt_duration(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Formats a playback rate compactly: `1.0 → "1×"`, `1.5 → "1.5×"`, `0.25 → "0.25×"`.
pub(crate) fn fmt_rate(rate: f64) -> String {
    let s = format!("{rate:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}×")
}

/// Whether an online fetch makes sense: simply whether there is any connection
/// at all. Deliberately **without** a metering check – the sync runs on any
/// connection (the user's wish). The offline check remains, so that in a
/// dead zone "failed attempts" are not logged in droves (which would lock an entry
/// permanently). Basis: `gio::NetworkMonitor` (NetworkManager).
pub(crate) fn online_available() -> bool {
    use gtk::gio::prelude::NetworkMonitorExt;
    gtk::gio::NetworkMonitor::default().is_network_available()
}

/// Most common artist designation (raw tag string) of a set of tracks – serves
/// as the display/key artist of an album (for cover & album metadata).
pub(crate) fn most_common_artist(tracks: &[Track]) -> String {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for t in tracks {
        if let Some(a) = t.artist.as_deref() {
            *counts.entry(a).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(a, _)| a.to_string())
        .unwrap_or_default()
}

/// Subtitle of an album row: "year · N songs" (year only if known).
pub(crate) fn album_subtitle(year: Option<i32>, track_count: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(y) = year {
        parts.push(y.to_string());
    }
    parts.push(ngettext_n("{n} song", "{n} songs", track_count as u32));
    parts.join(" · ")
}

/// Right-aligned, subtle duration label for a track row.
pub(crate) fn duration_label(ms: i64) -> gtk::Label {
    gtk::Label::builder()
        .label(fmt_duration(ms))
        .css_classes(["dim-label", "numeric"])
        .build()
}

/// Square 48-px cover preview from a file path – decoded **synchronously**
/// and cached; if the image is missing, the frame shows the placeholder icon. Intended for
/// the on-demand opened, short subpage lists.
/// First `ScrolledWindow` in the widget subtree (depth-first search), e.g. to find the
/// scroll position of the currently visible overview section.
pub(crate) fn find_scroller(widget: &gtk::Widget) -> Option<gtk::ScrolledWindow> {
    // Skip invisible subtrees – otherwise one grabs e.g. the internal,
    // hidden scroller of an empty `adw::StatusPage` instead of the real list.
    if !widget.is_visible() {
        return None;
    }
    if let Some(sc) = widget.downcast_ref::<gtk::ScrolledWindow>() {
        return Some(sc.clone());
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(sc) = find_scroller(&c) {
            return Some(sc);
        }
        child = c.next_sibling();
    }
    None
}

pub(crate) fn cover_widget(path: Option<&str>, placeholder: &str) -> gtk::Widget {
    let texture = path.and_then(crate::ui::widgets::thumb_cached);
    crate::ui::widgets::rounded_image(texture.as_ref(), placeholder, 48)
}

/// A **gallery tile**: square cover (or placeholder icon) with the
/// title as a semi-transparent band at the bottom (overlay). Click/long-press handlers
/// are added by the caller (FlowBox).
///
/// Does **not** decode synchronously: only an already cached cover is set
/// immediately. The returned `Picture` (if a cover path is present) is filled
/// in by the caller via background decoding ([`spawn_gallery_decode`]).
/// Square default edge length of a gallery tile, until
/// [`size_gallery_tiles`] knows the exact column width. Keeps the tile
/// square from the start (instead of following the landscape format of the cover).
const GALLERY_TILE_DEFAULT: i32 = 110;

pub(crate) fn gallery_cell(
    cover_path: Option<&str>,
    icon: &str,
    title: &str,
) -> (gtk::Overlay, Option<gtk::Picture>) {
    let overlay = gtk::Overlay::new();
    // The exact tile size (exactly 1/column count of the width) is set centrally via
    // [`size_gallery_tiles`] as the `size_request`. **No `hexpand`**: otherwise
    // the FlowBox stretches the tiles beyond their share (e.g. with few
    // entries a tile would take up more than 100%/columns of the width).
    // `halign: Start`, so that the cell never grows beyond the `size_request`.
    overlay.set_hexpand(false);
    overlay.set_halign(gtk::Align::Start);
    overlay.set_valign(gtk::Align::Start);
    // **Square default size** right from creation – so that the tile
    // stays square during the whole loading/layout phase (never landscape
    // or collapsed), no matter when/if asynchronous covers arrive. [`size_gallery_tiles`]
    // subsequently only refines the exact pixel size (column width).
    overlay.set_size_request(GALLERY_TILE_DEFAULT, GALLERY_TILE_DEFAULT);
    // Square tile frame as a simple `Box` container. Its size is set hard
    // by [`size_gallery_tiles`] to the square (width = height). Deliberately NOT
    // an `AspectFrame`: it ignored its `size_request` in height and let the
    // cell follow the landscape format of asynchronously loaded covers. A `Box` respects
    // the `size_request` reliably; the cover fills format-filling (`Cover`),
    // `overflow: Hidden` + `card` round/clip the corners.
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.set_overflow(gtk::Overflow::Hidden);
    frame.set_hexpand(false);
    frame.set_halign(gtk::Align::Fill);
    frame.set_valign(gtk::Align::Fill);
    frame.set_size_request(GALLERY_TILE_DEFAULT, GALLERY_TILE_DEFAULT);
    frame.add_css_class("card");
    let picture = match cover_path {
        Some(path) => {
            // Cover as a `Picture`. Set **only** an already cached texture
            // immediately (no synchronous decoding – that would otherwise block startup and
            // gallery construction). Otherwise the card stays as a placeholder, until the
            // cover is delivered in the background.
            let pic = gtk::Picture::new();
            pic.set_content_fit(gtk::ContentFit::Cover);
            pic.set_hexpand(true);
            pic.set_vexpand(true);
            pic.set_halign(gtk::Align::Fill);
            pic.set_valign(gtk::Align::Fill);
            if let Some(tex) = crate::ui::widgets::cached_thumb(path) {
                pic.set_paintable(Some(&tex));
            }
            frame.append(&pic);
            Some(pic)
        }
        None => {
            let img = gtk::Image::from_icon_name(icon);
            img.set_pixel_size(64);
            img.set_hexpand(true);
            img.set_vexpand(true);
            frame.append(&img);
            None
        }
    };
    overlay.set_child(Some(&frame));
    let label = gtk::Label::new(Some(title));
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_xalign(0.0);
    label.set_valign(gtk::Align::End);
    label.set_halign(gtk::Align::Fill);
    label.add_css_class("emilia-gallery-title");
    overlay.add_overlay(&label);
    (overlay, picture)
}

/// Decodes the covers (path → target `Picture`) **in a background thread**
/// and delivers the textures progressively on the UI thread. As a result, neither
/// app startup nor gallery construction blocks the image decoding. Backpressure via
/// a small, bounded channel, so that the thread does not run far ahead.
pub(crate) fn spawn_gallery_decode(items: Vec<(String, gtk::Picture)>) {
    if items.is_empty() {
        return;
    }
    let (tx, rx) = async_channel::bounded::<(usize, String, gtk::gdk::Texture)>(8);
    let paths: Vec<String> = items.iter().map(|(p, _)| p.clone()).collect();
    let targets: Vec<gtk::Picture> = items.into_iter().map(|(_, pic)| pic).collect();
    std::thread::spawn(move || {
        for (i, path) in paths.into_iter().enumerate() {
            if let Some(tex) = crate::ui::widgets::decode_thumb(&path) {
                // Aborts as soon as the receiver is gone (gallery rebuilt).
                if tx.send_blocking((i, path, tex)).is_err() {
                    break;
                }
            }
        }
    });
    gtk::glib::spawn_future_local(async move {
        while let Ok((i, path, tex)) = rx.recv().await {
            crate::ui::widgets::store_thumb(path, tex.clone());
            if let Some(pic) = targets.get(i) {
                pic.set_paintable(Some(&tex));
            }
        }
    });
}

/// Sets each gallery tile to a **square in column width**. Necessary because
/// the `FlowBox` does not stretch children beyond their natural size: without a fixed
/// `size_request` the thumbnails would stay small in wide desktop mode (the image
/// "does not scale along"), while the field gets wider. Called on every fill
/// and on every width change of the window.
pub(crate) fn size_gallery_tiles(fb: &gtk::FlowBox) {
    let cols = fb.min_children_per_line().max(1) as i32;
    let w = fb.width();
    if w <= 1 {
        return; // not yet assigned – the resize hook catches up
    }
    let spacing = fb.column_spacing() as i32;
    // Subtract `cols` times the spacing (instead of `cols-1`) as a safety buffer,
    // so that always exactly `cols` tiles fit per row and do not wrap.
    let tile = ((w - spacing * cols) / cols).max(64);
    let mut child = fb.first_child();
    while let Some(c) = child {
        let next = c.next_sibling();
        if let Some(inner) = c
            .downcast_ref::<gtk::FlowBoxChild>()
            .and_then(|f| f.child())
        {
            inner.set_size_request(tile, tile);
            // Also set the AspectFrame (main child of the overlay) hard to the square
            // – otherwise the cell height follows the aspect ratio of the (possibly
            // landscape/portrait) cover instead of the width.
            if let Some(frame) = inner.first_child() {
                frame.set_size_request(tile, tile);
            }
        }
        child = next;
    }
}

/// Reads subfolders and audio files of a folder (folders first, sorted).
/// Runs in a background thread – may therefore block.
pub(crate) fn read_entries(dir: PathBuf) -> Vec<FsEntry> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if scanner::is_audio(&path) {
                files.push(path);
            }
        }
    }
    dirs.sort();
    files.sort();

    // Properties: hide files that are not visible in the "Filesystem" area
    // (inherited from album/artist). Files without a DB entry stay
    // visible. Folders are not filtered (stay navigable).
    let lib = Library::open().ok();
    let mut out = Vec::with_capacity(dirs.len() + files.len());
    // Hide folders whose folder property does not contain "Filesystem"
    // (inherited from parent folders).
    for d in dirs {
        let visible = match &lib {
            Some(lib) => lib
                .folder_areas(&d.to_string_lossy())
                .contains(&crate::core::category::Area::Filesystem),
            None => true,
        };
        if visible {
            out.push(FsEntry::dir(d));
        }
    }
    for f in files {
        let visible = match &lib {
            Some(lib) => match lib.track_by_path(&f.to_string_lossy()).ok().flatten() {
                Some(t) => lib
                    .resolve_areas(t.artist.as_deref(), t.album.as_deref(), &t.path)
                    .contains(&crate::core::category::Area::Filesystem),
                None => true,
            },
            None => true,
        };
        if visible {
            out.push(FsEntry::file(f));
        }
    }
    out
}

impl App {
    /// Rebuilds **all** lists (after switching gallery/list or the
    /// column count). Each reload function fills – depending on `gallery_view` – the
    /// list or the gallery variant.
    pub(crate) fn rebuild_all_lists(&mut self, sender: &ComponentSender<Self>) {
        self.reload_albums();
        self.reload_artists();
        self.load_dir(sender);
        self.load_favorites(sender);
        self.load_audiobooks(sender);
        self.load_concerts(sender);
        self.reload_podcasts(sender);
    }

    /// Fills a FlowBox as a **gallery**: tiles from `(cover, icon, title)`,
    /// column count = `gallery_columns`. A single click activates (`activate(index)`),
    /// long press opens the detail (`detail(index)`). Messages go via
    /// the own input sender. On a renewed call all tiles (including
    /// their controllers) are removed – no duplicate handlers.
    pub(crate) fn fill_gallery(
        &self,
        fb: &gtk::FlowBox,
        items: &[(Option<String>, &'static str, String)],
        activate: fn(usize) -> Msg,
        detail: fn(usize) -> Msg,
    ) {
        while let Some(c) = fb.first_child() {
            fb.remove(&c);
        }
        fb.set_min_children_per_line(self.libview.gallery_columns);
        fb.set_max_children_per_line(self.libview.gallery_columns);
        // `homogeneous(true)` gives **all** tiles exactly the size set via `size_request`
        // ([`size_gallery_tiles`]) (= 1/column count of the width) and
        // does NOT stretch them to the row width. Without it the FlowBox distributes
        // the row width over the tiles actually present – with few
        // entries a tile would then take up more than 100%/columns.
        fb.set_homogeneous(true);
        fb.set_row_spacing(8);
        fb.set_column_spacing(8);
        fb.set_selection_mode(gtk::SelectionMode::None);
        // Do NOT let the FlowBox itself react to a single click – otherwise
        // it swallows the click before the tile gesture can evaluate it.
        fb.set_activate_on_single_click(false);
        if !fb.has_css_class("emilia-gallery") {
            fb.add_css_class("emilia-gallery");
        }
        // Collect non-cached covers and load them in the background after construction.
        let mut to_decode: Vec<(String, gtk::Picture)> = Vec::new();
        for (i, (cover, icon, title)) in items.iter().enumerate() {
            let (cell, pic) = gallery_cell(cover.as_deref(), icon, title);
            if let (Some(path), Some(pic)) = (cover.as_deref(), pic) {
                if crate::ui::widgets::cached_thumb(path).is_none() {
                    to_decode.push((path.to_string(), pic));
                }
            }
            // Single tap → subpage **immediately** (`activate`), long press →
            // detail view (`detail`) – exactly as in the list view. Deliberately
            // NO double tap/no delay, so that the click does not hang.
            let click = gtk::GestureClick::new();
            {
                let input = self.input.clone();
                click.connect_released(move |g, n, _, _| {
                    if n == 1 {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        let _ = input.send(activate(i));
                    }
                });
            }
            cell.add_controller(click);
            let long_press = gtk::GestureLongPress::new();
            {
                let input = self.input.clone();
                long_press.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    let _ = input.send(detail(i));
                });
            }
            cell.add_controller(long_press);
            fb.append(&cell);
        }
        // Fetch the covers of the not-yet-cached tiles in the background.
        spawn_gallery_decode(to_decode);
        // Bring the tiles immediately to a square at column width (takes effect as soon as the
        // FlowBox is allocated; at the first fill in init still w=0).
        size_gallery_tiles(fb);
        // Couple to size changes once per FlowBox. `connect_map` fires
        // only when the FlowBox is visible **and allocated in the tree** – there
        // we re-measure and couple (once) to the `page-size` of the
        // enclosing ScrolledWindow, so that the tiles scale along in desktop mode on
        // a window width change.
        if self.libview.gallery_hooked.borrow_mut().insert(fb.as_ptr() as usize) {
            let pagesize_done = std::rc::Rc::new(std::cell::Cell::new(false));
            fb.connect_map(move |fb| {
                size_gallery_tiles(fb);
                if pagesize_done.get() {
                    return;
                }
                let mut ancestor = fb.parent();
                while let Some(w) = ancestor {
                    if let Ok(sw) = w.clone().downcast::<gtk::ScrolledWindow>() {
                        let weak = fb.downgrade();
                        sw.hadjustment().connect_page_size_notify(move |_| {
                            if let Some(fb) = weak.upgrade() {
                                size_gallery_tiles(&fb);
                            }
                        });
                        pagesize_done.set(true);
                        break;
                    }
                    ancestor = w.parent();
                }
            });
        }
    }

    /// Narrow (mobile) mode? Identical to the collapsed sidebar that
    /// the breakpoint sets at low window width.
    pub(crate) fn is_mobile(&self) -> bool {
        self.nav.split.is_collapsed()
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
        let fkey = self.enrich_state.fanart_key.clone().filter(|k| !k.is_empty());
        let need_gallery = fkey.is_some()
            && self
                .library
                .artist_images(&name)
                .map(|imgs| imgs.is_empty())
                .unwrap_or(false)
            && self.libview.gallery_tried.borrow_mut().insert(format!("a\u{1}{name}"));
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

    pub(crate) fn fetch_focus_album(&self, sender: &ComponentSender<Self>, artist: &str, album: &str) {
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
            .is_some_and(|m| m.cover_path.as_deref().is_some_and(|p| !p.trim().is_empty()));
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
                        let _ = crate::core::online::match_album_mbid(&client, &lib, &artist, &album);
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
        let Some(key) = self.enrich_state.acoustid_key.clone().filter(|k| !k.is_empty()) else {
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

        for (name, _is_sidebar, btn) in &self.nav.nav_buttons {
            if *name == section {
                btn.set_visible(visible);
            }
        }

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

    /// Applies `section_order` to the navigation containers by reordering the
    /// existing buttons (sidebar buttons before the
    /// spacer + "Settings", which stay untouched at the end).
    pub(crate) fn apply_section_order(&self) {
        for sidebar in [true, false] {
            let container = if sidebar { &self.nav.sidebar_nav } else { &self.nav.top_nav };
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

#[cfg(test)]
mod tests {
    use super::{fmt_duration, guarded_resume};

    #[test]
    fn guarded_resume_clamps_start_and_end() {
        let dur = 3_600_000; // 1 h
        // In the middle → unchanged.
        assert_eq!(guarded_resume(1_000_000, dur), 1_000_000);
        // Near the start (< 5 s) → 0.
        assert_eq!(guarded_resume(3_000, dur), 0);
        // Near the end (< 10 s remaining) → 0 (next time from the start).
        assert_eq!(guarded_resume(dur - 5_000, dur), 0);
        // Unknown duration (0) → no end check, position stays.
        assert_eq!(guarded_resume(1_000_000, 0), 1_000_000);
    }

    #[test]
    fn fmt_duration_formats_minutes_and_hours() {
        assert_eq!(fmt_duration(0), "0:00");
        assert_eq!(fmt_duration(5_000), "0:05");
        assert_eq!(fmt_duration(65_000), "1:05");
        assert_eq!(fmt_duration(600_000), "10:00");
        // Audio drama lengths with hours.
        assert_eq!(fmt_duration(3_661_000), "1:01:01");
        // Negative values are clamped to 0.
        assert_eq!(fmt_duration(-1), "0:00");
    }
}
