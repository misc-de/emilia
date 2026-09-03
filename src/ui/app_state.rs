//! Sub-state structs of the root [`App`](crate::ui::app::App) component and
//! the small value types they carry (detail targets, sources, play sessions).
//! Pure data definitions — split out of `app.rs` so the root file holds the
//! component itself (messages, `view!`, `init`/`update`) and nothing else.
//! Everything here is re-exported from `crate::ui::app`, so existing paths
//! keep working.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use relm4::factory::FactoryVecDeque;
use relm4::{adw, gtk};

use crate::i18n::gettext;
use crate::model::{AlbumMeta, ArtistMeta, Source};
use crate::ui::app_sections::SortCrit;
use crate::ui::card_list::CardList;
use crate::ui::fs_row::{FsEntry, FsRow};

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
    /// How this play was started: `Some("single")` for a single tapped song
    /// (excluded from the album stats), `None` for album/queue/history plays.
    pub(crate) source: Option<&'static str>,
}

/// Album/artist overviews + file-list factory + gallery rendering state.
pub(crate) struct LibView {
    pub(crate) entries: FactoryVecDeque<FsRow>,
    pub(crate) albums: CardList,
    /// Gallery variant of the albums (cover grid), parallel to the list factory.
    pub(crate) albums_gallery: gtk::FlowBox,
    /// Scrolled child of the album gallery. Normally holds [`Self::albums_gallery`]
    /// as a single grid; when grouping is active it holds sections (a heading +
    /// a `FlowBox` per group): alphabetical initials by name, years by date. See
    /// [`App::fill_sectioned_gallery`].
    pub(crate) albums_gallery_box: gtk::Box,
    /// Per-row section heading of the album **list** (sorted order): year strings
    /// when sorting by date, the alphabetical initial (`0–9`, `A`, `B`, …) when
    /// sorting by name. Drives the `set_header_func`; `None` = no grouping.
    pub(crate) album_headers: std::rc::Rc<std::cell::RefCell<Option<Vec<String>>>>,
    /// Album overview (same order as factory/gallery); index resolution for the gallery.
    pub(crate) albums_overview: Vec<crate::model::AlbumMeta>,
    pub(crate) album_count: usize,
    // Singles / Compilations: extra album views filtered by the matching area
    // (`albums_overview_in_area`), whose kind-aware default reflects the
    // classification. Same machinery as the album overview above.
    pub(crate) singles: CardList,
    pub(crate) singles_gallery: gtk::FlowBox,
    pub(crate) singles_gallery_box: gtk::Box,
    pub(crate) single_headers: std::rc::Rc<std::cell::RefCell<Option<Vec<String>>>>,
    pub(crate) singles_overview: Vec<crate::model::AlbumMeta>,
    pub(crate) single_count: usize,
    pub(crate) compilations: CardList,
    pub(crate) compilations_gallery: gtk::FlowBox,
    pub(crate) compilations_gallery_box: gtk::Box,
    pub(crate) compilation_headers: std::rc::Rc<std::cell::RefCell<Option<Vec<String>>>>,
    pub(crate) compilations_overview: Vec<crate::model::AlbumMeta>,
    pub(crate) compilation_count: usize,
    pub(crate) artists: CardList,
    /// Gallery variant of the artists (photo grid).
    pub(crate) artists_gallery: gtk::FlowBox,
    /// Scrolled child of the artist gallery. Normally holds [`Self::artists_gallery`]
    /// as a single grid; when sorting by name it holds alphabetically grouped
    /// sections (a heading + a `FlowBox` per initial). Mirrors the album gallery.
    pub(crate) artists_gallery_box: gtk::Box,
    /// Per-row alphabetical section heading of the artist **list** (sorted order)
    /// when sorting by name; drives the `set_header_func`. `None` = no grouping.
    pub(crate) artist_headers: std::rc::Rc<std::cell::RefCell<Option<Vec<String>>>>,
    /// Artist overview (same order); index resolution for the gallery.
    pub(crate) artists_overview: Vec<crate::model::ArtistMeta>,
    pub(crate) artist_count: usize,
    /// Per-row alphabetical section headings of the concert/audiobook **lists**
    /// (sorted order) when sorting by name; drive their `set_header_func`. `None`
    /// = no grouping. Mirrors [`Self::album_headers`] for those entry lists.
    pub(crate) concert_headers: std::rc::Rc<std::cell::RefCell<Option<Vec<String>>>>,
    pub(crate) audiobook_headers: std::rc::Rc<std::cell::RefCell<Option<Vec<String>>>>,
    /// Per-row alphabetical headings of the favorites/playlists/memo/files lists
    /// (sorted order, name sort); drive their `set_header_func`. `None` = no grouping.
    pub(crate) favorite_headers: std::rc::Rc<std::cell::RefCell<Option<Vec<String>>>>,
    pub(crate) playlist_headers: std::rc::Rc<std::cell::RefCell<Option<Vec<String>>>>,
    pub(crate) memo_headers: std::rc::Rc<std::cell::RefCell<Option<Vec<String>>>>,
    pub(crate) files_headers: std::rc::Rc<std::cell::RefCell<Option<Vec<String>>>>,
    /// Per-section sort state (criterion + `desc` direction), keyed by the
    /// view-stack section name. Only the [`SORTABLE_SECTIONS`] have an entry;
    /// a missing entry means the default (by name, ascending).
    pub(crate) sort: std::collections::HashMap<&'static str, (SortCrit, bool)>,
    /// Per-section "no grouping" flag: when set, the overview is sorted but not
    /// split into section headings (the flat look from before grouping existed).
    /// Keyed like [`Self::sort`]; a missing/`false` entry means grouped.
    pub(crate) no_group: std::collections::HashMap<&'static str, bool>,
    /// Show lists as a gallery (cover grid) instead of a list (global default).
    pub(crate) gallery_view: bool,
    /// Per-section gallery override (sort popover's "Gallery view" toggle): a
    /// section with an entry uses it instead of the global [`Self::gallery_view`];
    /// a missing entry follows the global flag. Keyed like [`Self::sort`].
    pub(crate) section_gallery: std::collections::HashMap<&'static str, bool>,
    /// Number of tiles per row in the gallery view (2–8).
    pub(crate) gallery_columns: u32,
    pub(crate) loading: bool,
    /// Custom text for the loading overlay (e.g. while a YouTube playlist loads);
    /// `None` falls back to the default "Reading music data".
    pub(crate) loading_label: Option<String>,
    /// Galleries (artist/album) for which an on-demand fetch already ran this session.
    pub(crate) gallery_tried: std::cell::RefCell<std::collections::HashSet<String>>,
    /// The album track-list subpage currently rendered. Kept so a late
    /// MusicBrainz tracklist fetch — or a freshly downloaded missing track — can
    /// refill the same content box in place (no navigation flicker). `RefCell`
    /// because the renderer runs behind a `&self`.
    pub(crate) album_page:
        std::rc::Rc<std::cell::RefCell<Option<crate::ui::app_views::AlbumPageRef>>>,
    /// Modal phase-spinner shown while a missing track is searched/downloaded;
    /// the label is updated as the phase advances, and it is closed when done.
    pub(crate) missing_busy: Option<(adw::Dialog, gtk::Label)>,
    /// Play/pause controls of the rows on the *subpages* (artist, album,
    /// playlist). One shared registry: those pages come and go with the
    /// navigation, and a control whose row was dropped with its page is
    /// discarded on the next pass, so nothing has to clear this. Keyed like the
    /// entry lists (see [`crate::ui::app_favorites::mark_key`]).
    pub(crate) page_marks: crate::ui::play_mark::Marks,
}

impl LibView {
    /// Text shown beneath the loading spinner: the custom label if set, else the
    /// default. Used by the overlay both for the local library and remote loads.
    pub(crate) fn loading_text(&self) -> String {
        self.loading_label
            .clone()
            .unwrap_or_else(|| gettext("Reading music data"))
    }

    /// The remembered sort of a section (criterion + `desc`), defaulting to
    /// name-ascending when the section has no stored choice yet.
    pub(crate) fn sort_for(&self, section: &str) -> (SortCrit, bool) {
        self.sort
            .get(section)
            .copied()
            .unwrap_or((SortCrit::Name, false))
    }

    /// Whether the user disabled section grouping for `section` (sort the rows
    /// but don't split them under headings). Defaults to grouped.
    pub(crate) fn grouping_off(&self, section: &str) -> bool {
        self.no_group.get(section).copied().unwrap_or(false)
    }

    /// Whether `section` shows the gallery (cover grid). A per-section override
    /// (set in the sort popover) wins; otherwise the global [`Self::gallery_view`]
    /// applies. Used by the sortable sections' list/gallery visibility + reloads.
    pub(crate) fn gallery_on(&self, section: &str) -> bool {
        self.section_gallery
            .get(section)
            .copied()
            .unwrap_or(self.gallery_view)
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
    /// Recently played tracks (for stepping back across playback contexts).
    pub(crate) play_history: Vec<PathBuf>,
    /// When jumping back out of history, do not write to the history again.
    pub(crate) skip_history_push: bool,
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
    /// One-shot source tag for the **next** listening session (consumed by
    /// `start_play_session`). Set right before a single-track start to mark it
    /// `"single"`, so playing one song doesn't inflate its album in the stats;
    /// `None` (whole albums, queues, history) counts towards the album normally.
    pub(crate) next_source: Option<&'static str>,
    /// Ongoing listening session for the statistics (see [`PlaySession`]).
    pub(crate) play_session: Option<PlaySession>,
    /// Snapshot of the session for close (path, start, listened, duration).
    pub(crate) close_session: std::rc::Rc<std::cell::RefCell<Option<(String, i64, i64, i64)>>>,
    /// List in the queue dialog (rebuilt on changes).
    pub(crate) queue_list: gtk::ListBox,
    /// Play controls of the queue rows, so a play/pause elsewhere flips them
    /// without the dialog being rebuilt (see [`crate::ui::play_mark::Marks`]).
    pub(crate) queue_marks: crate::ui::play_mark::Marks,
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
    /// Album of the running **local** track, if it has one — drives the album
    /// shortcut in the player bar. `None` for streams/podcasts/YouTube/cloud.
    pub(crate) current_album: Option<String>,
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

/// Sleep-timer state. When `remaining_s` is set, playback pauses once it counts
/// down to zero, fading out over the final [`crate::ui::app_sleep::SLEEP_FADE_S`]
/// seconds. `until_track_end` instead stops after the current track (no fade).
/// The countdown only advances while actually playing (see [`App::sleep_tick`]).
#[derive(Default)]
pub(crate) struct SleepState {
    /// Seconds left until playback pauses; `None` = no timed sleep armed.
    pub(crate) remaining_s: Option<i64>,
    /// Stop at the end of the current track instead of after a fixed time.
    pub(crate) until_track_end: bool,
    /// Header menu button (gets the "sleep-armed" CSS class while a timer runs).
    pub(crate) button: gtk::MenuButton,
    /// Status label inside the popover ("Off" / "Pauses in 28:30").
    pub(crate) status_label: gtk::Label,
}

/// A sleep-timer choice from the header popover.
#[derive(Debug, Clone, Copy)]
pub enum SleepChoice {
    /// Cancel any running sleep timer.
    Off,
    /// Pause after this many minutes (with a fade-out over the final stretch).
    Minutes(i64),
    /// Stop once the current track finishes (no fade).
    EndOfTrack,
}

/// Lyrics for the currently playing track + the open karaoke view, grouped off
/// the `App` god-object. See [`crate::ui::app_lyrics`].
pub(crate) struct LyricsState {
    /// Parsed lyrics of the running track, once loaded (embedded/cache/online).
    pub(crate) current: Option<crate::core::lyrics::Lyrics>,
    /// Path the `current` lyrics belong to – guards against stale async results
    /// arriving after the track has already changed.
    pub(crate) for_path: Option<String>,
    /// Live karaoke view while the lyrics dialog is open.
    pub(crate) view: Option<LyricsView>,
    /// Pending lyrics pulldown in an open file-info dialog, filled when an online
    /// fetch for that file returns: the path it was opened for plus the (hidden)
    /// label + group to reveal. `Rc<RefCell>` because the dialog is built from a
    /// `&self` method.
    pub(crate) file_pending:
        std::rc::Rc<std::cell::RefCell<Option<(String, gtk::Label, adw::PreferencesGroup)>>>,
}

/// Widgets of the open karaoke dialog, kept so each tick can move the highlight
/// and auto-scroll without rebuilding anything.
pub(crate) struct LyricsView {
    /// One label per synced line (same order/length as `current.synced`).
    pub(crate) lines: Vec<gtk::Label>,
    /// Scroller around the lines (for auto-scrolling the active line into view).
    pub(crate) scroller: gtk::ScrolledWindow,
    /// Vertical box holding the line labels (parent for bounds computation).
    pub(crate) container: gtk::Box,
    /// Currently highlighted line index (skip redundant updates).
    pub(crate) active: Option<usize>,
    /// Fine-grained timer driving the highlight; removed when the dialog closes.
    pub(crate) timer: Option<gtk::glib::SourceId>,
    /// The dialog itself, so reopening can close a stale one.
    pub(crate) dialog: adw::Dialog,
    /// Whether timed karaoke highlighting is active (off → static lyrics, no
    /// timer). Persisted per track in `lyrics_pref`.
    pub(crate) karaoke: bool,
    /// Manual karaoke timing offset in ms (+ = lyrics shown later). Persisted
    /// per track.
    pub(crate) delay_ms: i64,
    /// Header label that shows the current delay (updated by the +/− buttons).
    pub(crate) delay_label: gtk::Label,
}

/// Navigation + layout chrome, grouped off the `App` god-object.
pub(crate) struct NavState {
    /// Main split view – collapsed (`is_collapsed`) means narrow/mobile mode.
    pub(crate) split: adw::OverlaySplitView,
    pub(crate) view_stack: adw::ViewStack,
    /// Title-bar sort button; its popover is (re)built per section in
    /// [`App::rebuild_sort_menu`], and it's hidden on non-sortable sections.
    pub(crate) sort_btn: gtk::MenuButton,
    /// Sort popovers handed up from the component pages (Podcasts/Streaming/
    /// YouTube), which keep their own sort state. The shared [`Self::sort_btn`]
    /// adopts the matching one when its section is active (see
    /// [`App::apply_component_sort`]).
    pub(crate) podcast_sort: crate::ui::app_sort::SortSlot,
    pub(crate) stream_sort: crate::ui::app_sort::SortSlot,
    pub(crate) yt_sort: crate::ui::app_sort::SortSlot,
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
    /// The open context/detail dialog, so a cover/photo change can rebuild it in
    /// place (close + re-open) and the new image shows immediately.
    pub(crate) ctx_dialog: std::rc::Rc<std::cell::RefCell<Option<adw::Dialog>>>,
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
    /// Last remote (WebDAV) listing error for the active source, shown as a
    /// persistent status in the file view (so the user sees *why* it is empty,
    /// not just a blank list). `None` while a remote load is fine or pending.
    pub(crate) remote_error: Option<String>,
    /// Remote (cloud) playback queue of the most recently opened folder.
    pub(crate) remote_queue: Vec<RemoteTrack>,
    pub(crate) remote_pos: usize,
    /// Is a remote file currently playing (instead of local queue/episode/station)?
    pub(crate) playing_remote: bool,
}

/// Streaming transport + timeshift-recording state owned by `App`. The
/// internet-radio *page* (station list, dialogs, search, recordings list) lives
/// in the [`crate::ui::stream_page`] component; what stays here is the running
/// station + the background timeshift recorder, which the player bar, the 1-s
/// tick and the replay subpage all reach.
pub(crate) struct StreamingState {
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
    /// Modal spinner shown while a "Recently heard" song is being resolved
    /// online (no local copy → YouTube lookup). Closed when the result arrives.
    pub(crate) resolve_busy: Option<adw::Dialog>,
}

/// Podcast playback state owned by the transport. The podcast *page* (lists,
/// dialogs, search, downloads) now lives in [`crate::ui::podcasts_page`]; the
/// only thing the transport still owns is which episode is currently loaded.
pub(crate) struct PodcastsState {
    /// URL of the currently loaded podcast episode (the canonical "an episode is
    /// playing" marker, read across the transport); `None` when music/another
    /// source is playing or no episode is running. The page keeps a mirror of
    /// this (pushed via `PodcastsInput::PlaybackStateChanged`) for its row icons.
    pub(crate) playing_episode_url: Option<String>,
}

/// YouTube transport + yt-dlp/settings state owned by `App`. The YouTube *page*
/// (lists, dialogs, search, downloads) lives in the [`crate::ui::yt_page`]
/// component; what stays here is the transport's "now playing" markers and the
/// yt-dlp installation/settings state (driven by the settings dialog). The whole
/// section is gated behind the `youtube_enabled` setting.
pub(crate) struct YoutubeState {
    /// Whether the user enabled the YouTube feature (off by default).
    pub(crate) enabled: bool,
    /// Installed `yt-dlp` version (cached for the settings status; `None` if not
    /// installed/runnable).
    pub(crate) ytdlp_version: Option<String>,
    /// The yt-dlp row in the open settings dialog (status subtitle).
    pub(crate) settings_status: std::rc::Rc<std::cell::RefCell<Option<adw::ActionRow>>>,
    /// Download/update button of the yt-dlp row in the open settings dialog.
    pub(crate) settings_dl_btn: std::rc::Rc<std::cell::RefCell<Option<gtk::Button>>>,
    /// Whether a yt-dlp download/update is currently running (ignore repeat taps).
    pub(crate) ytdlp_busy: bool,
    /// Video id currently loaded/playing (the canonical "a video is playing"
    /// marker, read across the transport). The page keeps a mirror (pushed via
    /// `YtInput::PlaybackStateChanged`) for its row icons.
    pub(crate) playing_video_id: Option<String>,
    /// Titles for the videos in the current play context (video id → title), so
    /// `yt:` tracks not in the library show a name instead of their id.
    pub(crate) video_titles: std::collections::HashMap<String, String>,
    /// Whether the current play context is a YouTube playlist – then individual
    /// videos are not logged to "Recent" (the playlist is logged as one entry).
    pub(crate) playing_playlist: bool,
    /// Position the next start of this video should begin at (video id, ms) —
    /// set when a jump mark in the description was tapped, and preferred over
    /// the stored resume position for that one start.
    pub(crate) pending_seek: Option<(String, i64)>,
    /// Live progress toast shown while adding video(s) to the on-disk library
    /// (the page requests it via `YtOutput::Progress`; the toast lives on the
    /// parent overlay).
    pub(crate) progress_toast: std::rc::Rc<std::cell::RefCell<Option<adw::Toast>>>,
}

/// Favorites + audiobooks page state, grouped off the `App` god-object.
pub(crate) struct FavoritesState {
    /// Favorites: (scope, key, title, is_dir).
    pub(crate) favorite_items: Vec<(String, String, String, bool)>,
    pub(crate) favorites_list: gtk::ListBox,
    /// Play/pause controls of the favorites rows (see [`crate::ui::play_mark`]).
    pub(crate) favorite_marks: crate::ui::play_mark::Marks,
    /// Gallery variant of the favorites (cover grid), like the audiobooks.
    pub(crate) favorites_gallery: gtk::FlowBox,
    pub(crate) favorites_gallery_box: gtk::Box,
    /// Audiobooks: (scope, key, title, is_dir).
    pub(crate) audiobook_items: Vec<(String, String, String, bool)>,
    pub(crate) audiobooks_list: gtk::ListBox,
    /// Play/pause controls of the audiobook rows.
    pub(crate) audiobook_marks: crate::ui::play_mark::Marks,
    /// Gallery variant of the audiobooks (cover grid). The box is the scrolled
    /// child and holds either the single grid or alphabetically grouped sections
    /// (see [`App::fill_sectioned_gallery`]); the flow box is the reusable grid.
    pub(crate) audiobooks_gallery: gtk::FlowBox,
    pub(crate) audiobooks_gallery_box: gtk::Box,
}

/// Playlists page state, grouped off the `App` god-object.
pub(crate) struct PlaylistsState {
    /// (id, name, track count) per playlist.
    pub(crate) playlist_items: Vec<(i64, String, i64)>,
    pub(crate) playlists_list: gtk::ListBox,
    /// Gallery variant of the playlists (derived-cover grid), like the audiobooks.
    pub(crate) playlists_gallery: gtk::FlowBox,
    pub(crate) playlists_gallery_box: gtk::Box,
}

/// Concerts page state, grouped off the `App` god-object.
pub(crate) struct ConcertsState {
    /// Concerts/audiobooks entries: (scope, key, title, is_dir) – like favorites.
    pub(crate) concert_items: Vec<(String, String, String, bool)>,
    pub(crate) concerts_list: gtk::ListBox,
    /// Play/pause controls of the concert rows.
    pub(crate) concert_marks: crate::ui::play_mark::Marks,
    /// Gallery variant of the concerts (cover grid). The box is the scrolled
    /// child and holds either the single grid or alphabetically grouped sections
    /// (see [`App::fill_sectioned_gallery`]); the flow box is the reusable grid.
    pub(crate) concerts_gallery: gtk::FlowBox,
    pub(crate) concerts_gallery_box: gtk::Box,
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
    /// Gapless playback for sequential local queues (default on).
    pub(crate) gapless: bool,
    /// Crossfade window in seconds between tracks (0 = off, default off).
    pub(crate) crossfade_secs: f64,
}

/// Desktop tray-icon options + the running service handle. The bool prefs are
/// persisted; `handle`/`hold` are live runtime state (see `src/ui/app_tray.rs`).
pub(crate) struct TrayState {
    /// Show a StatusNotifierItem tray icon.
    pub(crate) enabled: bool,
    /// Closing the window hides it into the tray instead of quitting.
    pub(crate) close_hides: bool,
    /// Start with the window hidden (tray only).
    pub(crate) start_hidden: bool,
    /// Suppress the taskbar entry even while the window is visible (X11 only).
    pub(crate) skip_taskbar: bool,
    /// Show the tray icon desaturated (grayscale pixmap) instead of colored.
    pub(crate) icon_gray: bool,
    /// Running ksni service handle (for live menu updates); `None` when off.
    pub(crate) handle: Option<ksni::Handle<crate::core::tray::EmiliaTray>>,
    /// App-hold guard keeping the process alive while only the tray remains.
    pub(crate) hold: Option<gtk::gio::ApplicationHoldGuard>,
}

/// Embedded MCP server runtime state. `now` is the snapshot the server thread
/// reads (published from the UI thread); `stop` flags a running backend to shut
/// down. See [`crate::ui::app_mcp`].
pub(crate) struct McpState {
    pub(crate) now: crate::core::mcp::NowPlayingHandle,
    /// Background-job registry (downloads), kept across server restarts.
    pub(crate) jobs: std::sync::Arc<crate::core::mcp::jobs::Jobs>,
    pub(crate) stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Device-sync status snapshot, written by the sync component and read by
    /// the `sync_*` tools; created before the component so it can be handed in.
    pub(crate) sync: crate::core::mcp::SyncStateHandle,
}
