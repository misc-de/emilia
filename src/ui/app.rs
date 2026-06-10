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
    album_subtitle, apply_color_scheme, artist_count_subtitle, attach_swipe_back, cover_widget,
    duration_label, find_scroller, fmt_duration, fmt_rate, guarded_resume, initial_gallery_columns,
    most_common_artist, on_secondary_click, online_available, read_entries, save_window_state,
    unix_now,
};
use crate::ui::app_init::InitState;
use crate::ui::artist_row::{ArtistCard, ArtistOutput};
use crate::ui::fs_row::{FsEntry, FsInput, FsOutput, FsRow};

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
pub(crate) const SECTIONS: [(&str, &str, &str); 12] = [
    ("favorites", "Favorites", "emilia-favorite-symbolic"),
    ("files", "Files", "folder-symbolic"),
    ("artists", "Artists", "avatar-default-symbolic"),
    ("albums", "Albums", "media-optical-symbolic"),
    ("concerts", "Concerts", "ticket-special-symbolic"),
    ("podcasts", "Podcasts", "podcast-symbolic"),
    ("streaming", "Streaming", "internet-radio-symbolic"),
    ("youtube", "YouTube", "im-youtube-symbolic"),
    ("audiobooks", "Audiobooks", "emilia-audiobook-symbolic"),
    ("playlists", "Playlists", "view-list-symbolic"),
    ("memo", "Memo", "audio-input-microphone-symbolic"),
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

/// One- to two-sentence description of a menu section (English `msgid`; translate
/// at the display site with `gettext()`). Shown as the secondary text of each row
/// in the setup assistant and the Settings → Menu list.
pub(crate) fn section_description(name: &str) -> &'static str {
    match name {
        "favorites" => "Quick access to the tracks, albums and artists you starred.",
        "files" => "Browse your music folder — and any extra sources — as a file tree.",
        "artists" => "Every artist in your library, each opening to their albums and tracks.",
        "albums" => "Every album in your library, sortable and grouped by initial or year.",
        "concerts" => "Live and concert recordings you marked, kept apart from your albums.",
        "podcasts" => "Subscribe to podcast feeds and play or download their episodes.",
        "streaming" => "Internet radio stations, with an optional buffer to record what just played.",
        "youtube" => "Search and play YouTube, follow channels and keep videos offline. Needs the yt-dlp tool.",
        "audiobooks" => "Albums, folders or tracks you marked as audiobooks, resuming where you left off.",
        "playlists" => "Your own playlists, arranged in any order you like.",
        "memo" => "Quick voice notes recorded with the microphone.",
        "stats" => "Listening statistics and your most-played artists and tracks.",
        _ => "",
    }
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

/// Re-exec the app in place (replace the process image) so gettext re-reads the
/// chosen UI language at startup — the language can only be picked up on a fresh
/// start. Uses `exec()` rather than spawn + exit because under Flatpak this
/// process is PID 1 of the sandbox's PID namespace: exiting it makes the kernel
/// kill every other process in the namespace, including a freshly spawned child,
/// leaving the app simply gone. `exec()` keeps the same PID, so the sandbox
/// stays alive and the new image starts. Only returns (via the spawn fallback)
/// if `exec()` itself fails; otherwise it never returns.
pub(crate) fn relaunch_for_language_change() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&exe).exec();
        // exec() only returns on failure; fall back to spawn.
        tracing::error!("re-exec for language change failed: {err}");
        let _ = std::process::Command::new(&exe).spawn();
    }
    std::process::exit(0);
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

/// What the (shared) waveform editor is currently editing. The editor body is
/// generic over "an audio file with a path"; this only distinguishes where the
/// item is looked up and where the cut result is written back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditKind {
    /// A radio timeshift recording (`recording` table).
    Recording,
    /// A voice memo (`memo` table).
    Memo,
}

/// Which view the Memo page shows (tab switcher): a flat "Recent" list or a
/// "Category" tree (categories alphanumeric, their memos nested underneath).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoView {
    Recent,
    Category,
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

/// A sort criterion of a library overview, chosen via the sort popover in the
/// title bar. Not every category offers every criterion (see
/// [`section_sort_criteria`]); the direction (asc/desc) is tracked per category
/// in [`LibView::sort`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortCrit {
    /// By name/title (natural order).
    Name,
    /// By the summed playback length of all tracks.
    Length,
    /// By the release year.
    Release,
    /// By the number of songs.
    Songs,
}

impl SortCrit {
    /// Stable token for persisting the choice in the settings DB.
    pub(crate) fn as_key(self) -> &'static str {
        match self {
            SortCrit::Name => "name",
            SortCrit::Length => "length",
            SortCrit::Release => "release",
            SortCrit::Songs => "songs",
        }
    }

    /// Parse the persisted token; falls back to [`SortCrit::Name`].
    pub(crate) fn from_key(s: &str) -> Self {
        match s {
            "length" => SortCrit::Length,
            "release" => SortCrit::Release,
            "songs" => SortCrit::Songs,
            _ => SortCrit::Name,
        }
    }

    /// Localized label shown in the sort popover.
    pub(crate) fn label(self) -> String {
        match self {
            SortCrit::Name => gettext("Name"),
            SortCrit::Length => gettext("Length"),
            // Release year; sorting by it groups the album list under year headings.
            SortCrit::Release => gettext("Date"),
            SortCrit::Songs => gettext("Number of songs"),
        }
    }
}

/// The library sections that offer a sort control (with their own remembered
/// criterion + direction). Other sections (Files/Podcasts/YouTube/Stats) don't.
pub(crate) const SORTABLE_SECTIONS: &[&str] = &["artists", "albums", "concerts", "audiobooks"];

/// The criteria a given section offers, in popover order. Category-appropriate:
/// artists carry no single release year, so they omit [`SortCrit::Release`];
/// albums/concerts/audiobooks derive a year from their tracks' tag metadata.
pub(crate) fn section_sort_criteria(section: &str) -> &'static [SortCrit] {
    use SortCrit::*;
    match section {
        "albums" | "concerts" | "audiobooks" => &[Name, Length, Release, Songs],
        "artists" => &[Name, Songs, Length],
        _ => &[],
    }
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
    pub(crate) artists: FactoryVecDeque<ArtistCard>,
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
    /// Per-section sort state (criterion + `desc` direction), keyed by the
    /// view-stack section name. Only the [`SORTABLE_SECTIONS`] have an entry;
    /// a missing entry means the default (by name, ascending).
    pub(crate) sort: std::collections::HashMap<&'static str, (SortCrit, bool)>,
    /// Per-section "no grouping" flag: when set, the overview is sorted but not
    /// split into section headings (the flat look from before grouping existed).
    /// Keyed like [`Self::sort`]; a missing/`false` entry means grouped.
    pub(crate) no_group: std::collections::HashMap<&'static str, bool>,
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
    /// Inline list filter: the title-bar toggle button, its search bar and the
    /// search entry. Shown only on list sections (Files / Artists / Albums in
    /// list mode); filters the visible `ListBox` live (see [`crate::ui::app_filter`]).
    pub(crate) filter_btn: gtk::ToggleButton,
    pub(crate) filter_bar: gtk::SearchBar,
    pub(crate) filter_entry: gtk::SearchEntry,
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
    /// Audiobooks: (scope, key, title, is_dir).
    pub(crate) audiobook_items: Vec<(String, String, String, bool)>,
    pub(crate) audiobooks_list: gtk::ListBox,
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
}

/// Concerts page state, grouped off the `App` god-object.
pub(crate) struct ConcertsState {
    /// Concerts/audiobooks entries: (scope, key, title, is_dir) – like favorites.
    pub(crate) concert_items: Vec<(String, String, String, bool)>,
    pub(crate) concerts_list: gtk::ListBox,
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
    /// A first/initial library scan is running (the music folder is being read
    /// for the very first time, so the views are still empty). Drives the
    /// loading overlay with an explanatory text so the app does not look frozen.
    /// Cleared when the scan reports back (`Cmd::ScanDone`).
    pub(crate) scanning: bool,
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
    /// Podcasts page, extracted into its own relm4 component (list + dialogs +
    /// feed workers). Playback stays in the parent transport; the page reaches it
    /// via `PodcastsOutput` and is told the state back via `PlaybackStateChanged`.
    pub(crate) podcasts_page: relm4::Controller<crate::ui::podcasts_page::PodcastsPage>,
    /// Hand-off slot for episode subpages built by the PodcastsPage component
    /// (read in `Msg::PushPodcastSubpage`, then pushed onto the shared nav).
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
    /// Play the album folder at this file-browser row index (its play button).
    PlayFsAlbum(usize),
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
    /// Detail view's refresh button: re-fetch the cover/metadata of the open
    /// target and rebuild the detail view.
    CtxRefresh,
    /// Share a ready-made selection over device sync (from the station / podcast
    /// / playlist / YouTube detail views). Same path as the music "Share": size
    /// confirmation when paired, otherwise open pairing.
    ShareItems(crate::core::sync::share::Selection),
    /// Header sync icon → open the pairing / connection-status dialog (no item).
    OpenSync,
    // --- Device synchronization (handled by the SyncPage component) ---
    /// The sync component paired/disconnected → tint the header icon.
    SyncConnected(bool),
    /// The sync component imported metadata → reload the affected views.
    SyncImported,
    TrackFinished,
    /// The active deck moved to the next queue track **gaplessly** (driven by
    /// `playbin3`'s `about-to-finish`); advance the app's state to match.
    GaplessAdvanced,
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
    /// Load lyrics for the just-started track: check embedded tags + the DB
    /// cache, then fetch from LRCLIB in the background if needed.
    LoadLyrics(PathBuf),
    /// Open the karaoke dialog for the running track's synced lyrics.
    ShowLyrics,
    /// Fine-grained karaoke tick: refresh the highlighted line while the dialog
    /// is open (no-op otherwise).
    LyricsTick,
    /// The karaoke dialog was closed: stop its timer and drop the view.
    LyricsClosed,
    /// Seek the song to a clicked karaoke line (its LRC timestamp in ms; the
    /// current delay offset is applied by the handler).
    LyricsSeek(i64),
    /// Toggle timed karaoke highlighting for the running track (persisted).
    LyricsToggleKaraoke,
    /// Nudge the karaoke timing offset by the given ms (+ = later); persisted.
    LyricsDelayAdjust(i64),
    /// Online lyrics for an open file-info dialog returned (path + lyrics).
    FileLyricsFetched {
        path: String,
        lyrics: Option<crate::core::lyrics::Lyrics>,
    },
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
    /// Set or clear the sleep timer (from the header zzz popover).
    SetSleepTimer(SleepChoice),
    /// Live inline-filter text changed (filters the visible list).
    InlineFilter(String),
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
    /// Open the song page of the album currently playing (player-bar shortcut).
    ShowCurrentAlbum,
    /// Back arrow in the shared header: pop the current subpage.
    NavBack,
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
    /// Remove an extra source (local folder / Nextcloud) by id, after the user
    /// confirmed it in the settings list. Then reloads sources + tabs.
    DeleteSource(i64),
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
    /// Gapless playback on/off (settings); persisted + pushed to the player.
    SetGapless(bool),
    /// Crossfade window in seconds (settings); persisted + pushed to the player.
    SetCrossfade(f64),
    /// Gallery view (cover grid) on/off; rebuilds the lists.
    SetGalleryView(bool),
    /// Tiles per row in the gallery view (2–8); rebuilds the lists.
    SetGalleryColumns(u32),
    /// Rebuild the title-bar sort popover for the current section (or hide it).
    /// Emitted when the visible section changes.
    SortMenuRefresh,
    /// Change the sort criterion of the current section; persists and re-sorts.
    SetSortCrit(SortCrit),
    /// Change the sort direction of the current section (`true` = descending).
    SetSortDir(bool),
    /// Toggle section grouping for the current section (`true` = no grouping).
    SetSortNoGroup(bool),
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
    // Podcasts (episode playback only; the page lives in the PodcastsPage
    // component — these two are mapped from its `Output`).
    /// Toggle an episode: start or – if already the running one – pause/resume.
    ToggleEpisode {
        url: String,
        title: String,
    },
    /// Click on a time-jump mark in the show notes: jump to the spot (start the
    /// episode there if needed).
    EpisodeSeekTo {
        url: String,
        title: String,
        ms: i64,
    },
    // --- Bridge from the PodcastsPage component to the shared parent chrome ---
    /// The page parked a built episode subpage in `podcast_subpage`; push it onto
    /// the shared NavigationView. Unit so `Msg` stays `Send` (the `!Send` widget
    /// travels through the shared slot, not the message).
    PushPodcastSubpage,
    /// Informational toast requested by the page.
    PodcastToast(String),
    /// The page confirmed a removal → show the "Podcast removed" undo toast.
    PodcastUndoToast(i64),
    /// Undo window elapsed → tell the page to actually delete the podcast.
    PodcastReallyDelete(i64),
    /// The page started/finished a "refresh all" worker → drive the spinner.
    PodcastRefreshStarted(bool),
    PodcastRefreshFinished,
    // YouTube (optional feature). Enabling/disabling is driven by the "youtube"
    // menu switch (see `Msg::SetSectionVisible`), not a dedicated settings toggle.
    /// Fetch yt-dlp (settings button): installs it, or re-downloads the latest
    /// when one is already present. The download/update choice is decided from the
    /// cached version at handling time, so the button works even before the
    /// background version probe has resolved.
    FetchYtDlp,
    /// Background tick (startup + slow timer): silently re-download the managed
    /// yt-dlp when it has gone stale, so YouTube keeps working hands-off.
    YtDlpAutoUpdate,
    // --- transport, requested by the YtPage component (or a worker result) ---
    /// Play a subscribed channel's cached videos as the queue.
    YtPlayChannel(i64),
    /// Start playing a whole playlist (loads its videos as the queue).
    YtStartPlaylist {
        url: String,
        title: String,
    },
    /// Play a cached playlist (videos handed in) starting at `index`; `close`
    /// pops the song-list subpage afterwards.
    YtStartPlaylistAt {
        url: String,
        title: String,
        index: usize,
        close: bool,
        videos: Vec<(String, String, Option<i64>)>,
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
    // --- bridge from the YtPage component to the shared parent chrome ---
    /// Open a mirrored playlist in the Playlists section.
    YtOpenPlaylist {
        id: i64,
        name: String,
    },
    /// Open a video's detail dialog (from a playlist row or the now-playing bar,
    /// which only have the parent's sender) → forwarded to the YtPage component.
    YtShowVideoDetail {
        video_id: String,
        title: String,
    },
    /// Informational toast requested by the page.
    YtToast(String),
    /// Show/update the persistent add-to-library progress toast.
    YtProgress(String),
    /// Finish the progress toast with a short final message.
    YtProgressDone(String),
    /// Set/clear the central loading overlay.
    YtSetLoading(Option<String>),
    /// A track/playlist was added → reload artist/album overviews.
    YtLibraryChanged,
    /// A playlist was saved → reload the Playlists section.
    YtPlaylistsChanged,
    /// The page parked a built subpage in `yt_subpage`; push it onto the nav.
    PushYtSubpage,
    /// The page confirmed a channel removal → show the "channel removed" undo toast.
    YtChannelUndo(i64),
    /// Undo window elapsed → tell the page to actually delete the channel.
    YtChannelReallyDelete(i64),
    /// The page started/finished a "refresh all" worker → drive the spinner.
    YtRefreshStarted(bool),
    YtRefreshFinished,
    // Streaming (internet radio) — transport; the page lives in the StreamPage
    // component and reaches the transport through these.
    /// Tap a station: starts it, toggle pause/resume on a running station.
    ToggleStream(i64),
    /// Record button of a station row: starts/stops the continuous recording.
    StreamRecordToggle(i64),
    /// Shared player-bar record button: records a voice memo (Memo section) or
    /// toggles the running station's timeshift recording (Streaming section).
    RecordToggle,
    /// Title tag from the playback (for stations: the running ICY title).
    StreamTitle(String),
    /// Actually remove a station (after the undo toast; stops the player/recorder).
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
    // --- bridge from the StreamPage component to the shared parent chrome ---
    /// The page confirmed a station removal → show the "station removed" undo toast.
    StreamDeleteUndo(i64),
    /// The page confirmed a recording deletion → show the "recording deleted" undo toast.
    RecordingDeleteUndo(i64),
    /// Undo window elapsed → tell the page to actually delete the recording.
    StreamRecordingReallyDelete(i64),
    /// A recording was copied into the music library → reload artist/album views.
    StreamLibraryChanged,
    /// Informational toast requested by the page.
    StreamToast(String),
    /// Open the waveform editor subpage for a recording (id).
    EditRecording(i64),
    /// Open the waveform editor subpage for a voice memo (id).
    EditMemo(i64),
    /// Preview a recording/memo file from a chosen position (ms) – editor playhead.
    RecordingPlayFrom {
        path: String,
        ms: i64,
    },
    /// Pause the editor preview (pauses the main player it plays through).
    RecordingPreviewPause,
    /// Apply the editor's cut ranges (seconds) to a recording/memo and overwrite it.
    EditApplyCut {
        kind: EditKind,
        id: i64,
        cuts: Vec<(f64, f64)>,
    },
    /// Result of the background cut: new path (`None` = failed) + new duration.
    EditCutDone {
        kind: EditKind,
        id: i64,
        path: Option<String>,
        duration_ms: i64,
    },
    /// Set the size of the timeshift buffer in minutes (0–60).
    SetRecordingBufferMinutes(u32),

    // ---- Voice memos ----
    /// A finished recording was finalized off-thread: new file path (`None` =
    /// failed) + its duration. Creates the memo row.
    MemoRecordSaved {
        path: Option<String>,
        duration_ms: i64,
    },
    /// Switch the memo view: Recent list or Category tree.
    SetMemoView(MemoView),
    /// Open a memo's detail dialog (id) – via long press.
    OpenMemo(i64),
    /// Rename a memo.
    MemoRename {
        id: i64,
        title: String,
    },
    /// Assign (or clear, with `None`) a memo's category.
    MemoSetCategory {
        id: i64,
        category_id: Option<i64>,
    },
    /// Delete a memo (id) – undo toast; deferred to `MemoDeleteConfirmed`.
    MemoDelete(i64),
    /// Actually delete a memo (after the undo toast expires).
    MemoDeleteConfirmed(i64),
    /// Open the "new category" text prompt (the "+" in the tab bar).
    MemoCategoryAddPrompt,
    /// Add a new memo category (confirmed name).
    MemoCategoryAdd(String),
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
    /// A timeshift recording's cover/segment finished (worker) → tell the
    /// StreamPage component to rebuild the recordings list.
    ReloadRecordings,
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
                        // Inline list filter: reveals the search bar below the
                        // header to filter the visible list (Files / Artists /
                        // Albums). Only shown on list sections; wired in
                        // `setup_inline_filter`. Separate from the global search
                        // dialog (the magnifier on the left).
                        #[name = "filter_btn"]
                        pack_end = &gtk::ToggleButton {
                            set_icon_name: "emilia-filter-symbolic",
                            set_tooltip_text: Some(&gettext("Filter list")),
                            set_visible: false,
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
                            // Mobile menu strip: 5px higher than before (top 12 → 7)
                            // with 5px more breathing room below (bottom 2 → 7).
                            set_margin_top: 7,
                            set_margin_bottom: 7,
                            // Center the icon strip when it fits; it still scrolls
                            // (left-aligned) once the icons overflow the width.
                            set_halign: gtk::Align::Center,
                        },
                    },

                    // Inline filter bar: revealed by the header filter button on
                    // list sections to filter the visible list live. Collapsed
                    // (zero height) while inactive, so it costs nothing elsewhere.
                    #[name = "filter_bar"]
                    add_top_bar = &gtk::SearchBar {
                        #[wrap(Some)]
                        #[name = "filter_entry"]
                        set_child = &gtk::SearchEntry {
                            set_hexpand: true,
                            set_placeholder_text: Some(&gettext("Filter the list …")),
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
                                    // Gallery variant (photo grid). The box holds either
                                    // a single grid or alphabetically grouped sections.
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.artist_count > 0 && model.libview.gallery_view,
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
                                    // Gallery variant (cover grid). The box holds either
                                    // a single grid or year-grouped sections (date sort).
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.libview.album_count > 0 && model.libview.gallery_view,
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
                                    // Gallery variant of the concerts. The box holds
                                    // either a single grid or alphabetically grouped
                                    // sections (name sort).
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.concerts.concert_items.is_empty() && model.libview.gallery_view,
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
                                    // Gallery variant of the audiobooks. The box holds
                                    // either a single grid or alphabetically grouped
                                    // sections (name sort).
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.favorites.audiobook_items.is_empty() && model.libview.gallery_view,
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

                                        gtk::ToggleButton {
                                            set_label: &gettext("Recent"),
                                            set_hexpand: true,
                                            #[watch]
                                            set_active: model.memo.view == MemoView::Recent,
                                            connect_clicked => Msg::SetMemoView(MemoView::Recent),
                                        },
                                        gtk::ToggleButton {
                                            set_label: &gettext("Category"),
                                            set_hexpand: true,
                                            #[watch]
                                            set_active: model.memo.view == MemoView::Category,
                                            connect_clicked => Msg::SetMemoView(MemoView::Category),
                                        },
                                        gtk::Button {
                                            set_icon_name: "list-add-symbolic",
                                            set_tooltip_text: Some(&gettext("Add category")),
                                            add_css_class: "flat",
                                            connect_clicked => Msg::MemoCategoryAddPrompt,
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
                        set_margin_bottom: 3,
                        set_margin_start: 10,
                        set_margin_end: 10,

                        gtk::Button {
                            add_css_class: "flat",
                            // Trim ~5px of vertical padding above/below the song line
                            // (CSS, see `init`), on top of the small top margin.
                            add_css_class: "emilia-songline",
                            set_tooltip_text: Some(&gettext("Show details of the current track")),
                            // 5px more breathing room above the song name (2 → 7).
                            set_margin_top: 7,
                            // Without a selected track, hide entirely (frees up space).
                            #[watch]
                            set_visible: model.mini.now_playing.is_some(),
                            // A plain tap on the song display opens the track detail view.
                            connect_clicked[sender] => move |_| {
                                sender.input(Msg::OpenNowPlaying);
                            },
                            // Long press (touch) keeps working too; it claims the sequence
                            // so the button's own click won't also fire.
                            add_controller = gtk::GestureLongPress {
                                connect_pressed[sender] => move |gesture, _, _| {
                                    gesture.set_state(gtk::EventSequenceState::Claimed);
                                    sender.input(Msg::OpenNowPlaying);
                                },
                            },
                            // Right click (classic mouse): same detail view.
                            add_controller = gtk::GestureClick {
                                set_button: gtk::gdk::BUTTON_SECONDARY,
                                connect_pressed[sender] => move |gesture, _, _, _| {
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
                                    connect_clicked => Msg::ToggleRepeat,
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
                                // Shared record button, same size as play/pause
                                // (emilia-bigplay); blinks red while recording. On the
                                // Memo section it records a voice memo; in Streaming it
                                // toggles the timeshift recording of the running
                                // station. Shown only in those contexts.
                                gtk::Button {
                                    set_valign: gtk::Align::Center,
                                    #[watch]
                                    set_visible: model.nav.view_stack.visible_child_name().as_deref() == Some("memo")
                                        || (model.nav.view_stack.visible_child_name().as_deref() == Some("streaming")
                                            && model.streaming.playing_stream.is_some()
                                            && model.streaming.recording_buffer_minutes > 0),
                                    #[watch]
                                    set_icon_name: if model.nav.view_stack.visible_child_name().as_deref() == Some("streaming") {
                                        "media-record-symbolic"
                                    } else {
                                        "audio-input-microphone-symbolic"
                                    },
                                    #[watch]
                                    set_tooltip_text: Some(&if model.nav.view_stack.visible_child_name().as_deref() == Some("streaming") {
                                        if model.streaming.record_state.is_some() {
                                            gettext("Stop recording")
                                        } else {
                                            gettext("Record")
                                        }
                                    } else if model.memo.recording {
                                        gettext("Stop the voice memo")
                                    } else {
                                        gettext("Record a voice memo")
                                    }),
                                    #[watch]
                                    set_css_classes: if (model.nav.view_stack.visible_child_name().as_deref() == Some("streaming")
                                        && model.streaming.record_state.is_some())
                                        || (model.nav.view_stack.visible_child_name().as_deref() != Some("streaming")
                                            && model.memo.recording)
                                    {
                                        &["circular", "emilia-bigplay", "emilia-record-dot", "emilia-recording"]
                                    } else {
                                        // Red even when idle; only pulses while recording.
                                        &["circular", "emilia-bigplay", "emilia-record-dot"]
                                    },
                                    connect_clicked => Msg::RecordToggle,
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
                                    connect_clicked => Msg::ShowLyrics,
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
                                    connect_clicked => Msg::ShowCurrentAlbum,
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
        // Apply the color scheme (default: system) immediately.
        apply_color_scheme(
            library
                .get_setting("color_scheme")
                .ok()
                .flatten()
                .as_deref()
                .unwrap_or("system"),
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
            gallery_columns,
            recording_buffer_minutes,
            saved_section,
        } = Self::read_init_state(&library);

        let entries = FactoryVecDeque::builder()
            .launch(gtk::ListBox::default())
            .forward(sender.input_sender(), |out| match out {
                FsOutput::Activated(index) => Msg::Activate(index.current_index()),
                FsOutput::LongPress(index) => Msg::ShowContextMenu(index.current_index()),
                FsOutput::DoubleClick(index) => Msg::ToggleQueue(index.current_index()),
                FsOutput::PlayDir(index) => Msg::PlayFsAlbum(index.current_index()),
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
                {
                    let sender = sender.clone();
                    move || sender.input(Msg::PlaybackReady)
                },
                move || sender.input(Msg::GaplessAdvanced),
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

        // Keep the managed yt-dlp fresh hands-off: check once at startup and then
        // every 12 h. The handler is a no-op unless YouTube is on and the copy is
        // actually stale (so it costs nothing on most ticks).
        {
            let sender = sender.clone();
            sender.input(Msg::YtDlpAutoUpdate);
            gtk::glib::timeout_add_seconds_local(12 * 60 * 60, move || {
                sender.input(Msg::YtDlpAutoUpdate);
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
        // Shared hand-off slot for episode subpages built by the component (its
        // `!Send` widget can't ride on the parent's `Send` `Msg`).
        let podcast_subpage: std::rc::Rc<std::cell::RefCell<Option<(String, gtk::Box)>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let podcasts_page = crate::ui::podcasts_page::PodcastsPage::builder()
            .launch(podcast_subpage.clone())
            .forward(sender.input_sender(), |out| {
                use crate::ui::podcasts_page::PodcastsOutput as O;
                match out {
                    O::ToggleEpisode { url, title } => Msg::ToggleEpisode { url, title },
                    O::EpisodeSeekTo { url, title, ms } => Msg::EpisodeSeekTo { url, title, ms },
                    O::PushSubpage => Msg::PushPodcastSubpage,
                    O::Share(sel) => Msg::ShareItems(sel),
                    O::Toast(s) => Msg::PodcastToast(s),
                    O::DeletedUndoToast(id) => Msg::PodcastUndoToast(id),
                    O::RefreshStarted(b) => Msg::PodcastRefreshStarted(b),
                    O::RefreshFinished => Msg::PodcastRefreshFinished,
                }
            });
        let yt_subpage: std::rc::Rc<std::cell::RefCell<Option<(String, gtk::Box)>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let yt_page = crate::ui::yt_page::YtPage::builder()
            .launch(yt_subpage.clone())
            .forward(sender.input_sender(), |out| {
                use crate::ui::yt_page::YtOutput as O;
                match out {
                    O::PlayVideo { video_id, title } => Msg::YtPlayVideo { video_id, title },
                    O::PlayChannel(id) => Msg::YtPlayChannel(id),
                    O::StartPlaylist { url, title } => Msg::YtStartPlaylist { url, title },
                    O::StartPlaylistAt {
                        url,
                        title,
                        index,
                        close,
                        videos,
                    } => Msg::YtStartPlaylistAt {
                        url,
                        title,
                        index,
                        close,
                        videos,
                    },
                    O::OpenTrackEq { path, title } => Msg::OpenTrackEq { path, title },
                    O::OpenPlaylist { id, name } => Msg::YtOpenPlaylist { id, name },
                    O::OpenSettings => Msg::OpenSettings,
                    O::Toast(s) => Msg::YtToast(s),
                    O::Progress(s) => Msg::YtProgress(s),
                    O::ProgressDone(s) => Msg::YtProgressDone(s),
                    O::SetLoading(o) => Msg::YtSetLoading(o),
                    O::LibraryChanged => Msg::YtLibraryChanged,
                    O::PlaylistsChanged => Msg::YtPlaylistsChanged,
                    O::PushSubpage => Msg::PushYtSubpage,
                    O::DeleteChannelUndo(id) => Msg::YtChannelUndo(id),
                    O::RefreshStarted(b) => Msg::YtRefreshStarted(b),
                    O::RefreshFinished => Msg::YtRefreshFinished,
                    O::Share(sel) => Msg::ShareItems(sel),
                }
            });
        let stream_page = crate::ui::stream_page::StreamPage::builder()
            .launch(())
            .forward(sender.input_sender(), |out| {
                use crate::ui::stream_page::StreamOutput as O;
                match out {
                    O::ToggleStream(id) => Msg::ToggleStream(id),
                    O::PlayRecording(path) => Msg::PlayRecording(path),
                    O::OpenReplay(id) => Msg::OpenStreamReplay(id),
                    O::EditRecording(id) => Msg::EditRecording(id),
                    O::StreamDeleteUndo(id) => Msg::StreamDeleteUndo(id),
                    O::RecordingDeleteUndo(id) => Msg::RecordingDeleteUndo(id),
                    O::LibraryChanged => Msg::StreamLibraryChanged,
                    O::Share(sel) => Msg::ShareItems(sel),
                    O::Toast(s) => Msg::StreamToast(s),
                }
            });
        let setup_page = crate::ui::setup::SetupPage::builder().launch(()).forward(
            sender.input_sender(),
            |out| match out {
                crate::ui::setup::SetupOutput::Finished {
                    lang_code,
                    music_dir,
                    enabled_sections,
                } => Msg::SetupFinished {
                    lang_code,
                    music_dir,
                    enabled_sections,
                },
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
            library,
            player,
            mpris,
            input: sender.input_sender().clone(),
            libview: LibView {
                entries,
                albums,
                albums_gallery: gtk::FlowBox::new(),
                albums_gallery_box: gtk::Box::new(gtk::Orientation::Vertical, 6),
                album_headers: std::rc::Rc::new(std::cell::RefCell::new(None)),
                albums_overview: Vec::new(),
                album_count: 0,
                artists,
                artists_gallery: gtk::FlowBox::new(),
                artists_gallery_box: gtk::Box::new(gtk::Orientation::Vertical, 6),
                artist_headers: std::rc::Rc::new(std::cell::RefCell::new(None)),
                artists_overview: Vec::new(),
                artist_count: 0,
                concert_headers: std::rc::Rc::new(std::cell::RefCell::new(None)),
                audiobook_headers: std::rc::Rc::new(std::cell::RefCell::new(None)),
                sort,
                no_group,
                gallery_view,
                gallery_columns,
                loading: false,
                loading_label: None,
                gallery_tried: std::cell::RefCell::new(std::collections::HashSet::new()),
                gallery_hooked: std::cell::RefCell::new(std::collections::HashSet::new()),
            },
            refresh_pending: 0,
            scanning: false,
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
                concerts_list: concerts_list.clone(),
                concerts_gallery: gtk::FlowBox::new(),
                concerts_gallery_box: gtk::Box::new(gtk::Orientation::Vertical, 6),
                concert_hint_dismissed,
            },
            favorites: FavoritesState {
                favorite_items: Vec::new(),
                favorites_list: favorites_list.clone(),
                audiobook_items: Vec::new(),
                audiobooks_list: audiobooks_list.clone(),
                audiobooks_gallery: gtk::FlowBox::new(),
                audiobooks_gallery_box: gtk::Box::new(gtk::Orientation::Vertical, 6),
            },
            playlists: PlaylistsState {
                playlist_items: Vec::new(),
                playlists_list: playlists_list.clone(),
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
            },
            memo: crate::ui::app_memo::MemoState::new(memos_list.clone()),
            youtube: YoutubeState {
                enabled: youtube_enabled,
                ytdlp_version: None,
                settings_status: std::rc::Rc::new(std::cell::RefCell::new(None)),
                settings_dl_btn: std::rc::Rc::new(std::cell::RefCell::new(None)),
                ytdlp_busy: false,
                playing_video_id: None,
                video_titles: std::collections::HashMap::new(),
                playing_playlist: false,
                progress_toast: std::rc::Rc::new(std::cell::RefCell::new(None)),
            },
            settings_src_list: std::rc::Rc::new(std::cell::RefCell::new(None)),
            offline_sources: std::collections::HashSet::new(),
            stats_page,
            nav: NavState {
                split: adw::OverlaySplitView::new(),
                view_stack: adw::ViewStack::new(),
                sort_btn: gtk::MenuButton::new(),
                filter_btn: gtk::ToggleButton::new(),
                filter_bar: gtk::SearchBar::new(),
                filter_entry: gtk::SearchEntry::new(),
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
            podcasts_page,
            podcast_subpage,
            yt_page,
            yt_subpage,
            stream_page,
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
        // Bring the PodcastsPage component up to the current chrome state. The
        // final `SetGalleryView` triggers exactly one (correctly-moded) reload.
        {
            use crate::ui::podcasts_page::PodcastsInput as PI;
            model.podcasts_page.emit(PI::SetWindow(root.clone()));
            model.podcasts_page.emit(PI::SetMobile(model.is_mobile()));
            model
                .podcasts_page
                .emit(PI::SetGalleryColumns(model.libview.gallery_columns));
            model
                .podcasts_page
                .emit(PI::SetGalleryView(model.libview.gallery_view));
        }
        // Same for the YtPage component.
        {
            use crate::ui::yt_page::YtInput as YI;
            model.yt_page.emit(YI::SetWindow(root.clone()));
            model.yt_page.emit(YI::SetMobile(model.is_mobile()));
            model
                .yt_page
                .emit(YI::SetGalleryColumns(model.libview.gallery_columns));
            model
                .yt_page
                .emit(YI::SetGalleryView(model.libview.gallery_view));
        }
        // Bring the StreamPage component up to the current chrome state + load.
        {
            use crate::ui::stream_page::StreamInput as SI;
            model.stream_page.emit(SI::SetWindow(root.clone()));
            model.stream_page.emit(SI::SetMobile(model.is_mobile()));
            model.stream_page.emit(SI::SetBufferMinutes(
                model.streaming.recording_buffer_minutes,
            ));
            model.stream_page.emit(SI::Reload);
            model.stream_page.emit(SI::ReloadRecordings);
        }
        // Seed the starter memo categories once (localized; i18n is ready here),
        // then load categories + memos for the Memo page.
        {
            let names = [gettext("Idea"), gettext("Task"), gettext("Note")];
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let _ = model.library.seed_memo_categories(&refs);
            // One-time: the former default "Music" category was dropped from the
            // seed set; remove an existing one (its memos fall back to General).
            if model
                .library
                .get_setting("memo_music_default_removed")
                .ok()
                .flatten()
                .is_none()
            {
                let music = gettext("Music");
                for c in model
                    .library
                    .memo_categories()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|c| c.name == music)
                {
                    let _ = model.library.delete_memo_category(c.id);
                }
                let _ = model.library.set_setting("memo_music_default_removed", "1");
            }
        }
        model.reload_memo_categories(&sender);
        model.reload_memos(&sender);
        // (Statistics build themselves in the StatsPage component's init; the
        // podcast feed-image cache runs in the PodcastsPage component's init; the
        // station-logo cache runs in the StreamPage component's init.)
        // YouTube (optional, opt-in): load subscribed channels, and – on a
        // connection – verify/refresh yt-dlp and the newest videos in the
        // background. yt-dlp is re-fetched once per new app version (YouTube
        // changes frequently break older versions).
        if model.youtube.enabled {
            model.yt_page.emit(crate::ui::yt_page::YtInput::Reload);
            let online = online_available();
            sender.spawn_oneshot_command(move || {
                let Ok(lib) = Library::open() else {
                    return Cmd::YtReload;
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
                        let _ = crate::ui::yt_page::refresh_channel_videos(id, &title, &url);
                    }
                }
                Cmd::YtReload
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
        let albums_gallery_box = model.libview.albums_gallery_box.clone();
        let artists_gallery_box = model.libview.artists_gallery_box.clone();
        let concerts_gallery_box = model.concerts.concerts_gallery_box.clone();
        let audiobooks_gallery_box = model.favorites.audiobooks_gallery_box.clone();
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
            Msg::ShowConcertDetail(index) => self.on_show_concert_detail(index, root, &sender),
            Msg::OpenArtistTracks(index) => self.on_open_artist_tracks(index, &sender),
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
            Msg::CtxPlay => self.on_ctx_play(),
            Msg::CtxPlayAlbum => self.on_ctx_play_album(),
            Msg::CtxPlayArtist { newest_first } => self.on_ctx_play_artist(newest_first),
            Msg::CtxAddQueue => self.on_ctx_add_queue(),
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
            // --- Streaming (internet radio) ---
            // --- Streaming transport (requested by the StreamPage component) ---
            Msg::ToggleStream(id) => self.toggle_stream(id),
            Msg::StreamRecordToggle(id) => self.stream_record_toggle(&sender, id),
            Msg::RecordToggle => {
                // Context decides the action: timeshift in Streaming, memo elsewhere.
                if self.nav.view_stack.visible_child_name().as_deref() == Some("streaming") {
                    if let Some(id) = self.streaming.playing_stream {
                        sender.input(Msg::StreamRecordToggle(id));
                    }
                } else {
                    self.toggle_memo_record(&sender);
                }
            }
            Msg::StreamTitle(title) => self.stream_title(title),
            Msg::StreamDeleteConfirmed(id) => self.stream_delete_confirmed(id),
            // --- Recording (timeshift) ---
            Msg::RecordStop => {
                if self.streaming.record_state.is_some() {
                    // Finalize the song still in progress so it isn't lost.
                    self.finalize_recording(&sender);
                    self.streaming.record_state = None;
                    self.toast(&gettext("Recording stopped"));
                    self.sync_live_recording();
                }
            }
            Msg::OpenStreamReplay(id) => self.open_stream_replay(&sender, id),
            Msg::ReplayPlay { start, end } => self.replay_play(start, end),
            Msg::ReplaySave { start, end, title } => self.replay_save(&sender, start, end, title),
            Msg::PlayRecording(path) => self.play_recording(path),
            // --- bridge from the StreamPage component to the shared parent chrome ---
            Msg::StreamDeleteUndo(id) => {
                self.undo_toast(
                    &sender,
                    &gettext("Station removed"),
                    Msg::StreamDeleteConfirmed(id),
                );
            }
            Msg::RecordingDeleteUndo(id) => {
                self.undo_toast(
                    &sender,
                    &gettext("Recording deleted"),
                    Msg::StreamRecordingReallyDelete(id),
                );
            }
            Msg::StreamRecordingReallyDelete(id) => self
                .stream_page
                .emit(crate::ui::stream_page::StreamInput::RecordingDeleteConfirmed(id)),
            Msg::StreamLibraryChanged => self.reload_library_overviews(),
            Msg::StreamToast(s) => self.toast(&s),
            Msg::EditRecording(id) => self.open_recording_edit(&sender, EditKind::Recording, id),
            Msg::EditMemo(id) => self.open_recording_edit(&sender, EditKind::Memo, id),
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
            Msg::EditApplyCut { kind, id, cuts } => {
                self.apply_recording_cut(&sender, kind, id, cuts)
            }
            Msg::EditCutDone {
                kind,
                id,
                path,
                duration_ms,
            } => match path {
                Some(p) => {
                    self.nav.nav_view.pop();
                    match kind {
                        EditKind::Recording => {
                            let _ = self.library.update_recording_file(id, &p, duration_ms);
                            // A recording lives under <Music>/Streaming, so it is
                            // also a normal library track. Re-read its tags into
                            // the library DB and rebuild the overviews; otherwise
                            // the album/song lists keep the old (longer) duration
                            // after a cut. (The file browser re-reads from disk on
                            // its own when navigated to.)
                            crate::core::scanner::ingest_file(
                                &self.library,
                                std::path::Path::new(&p),
                            );
                            self.stream_page
                                .emit(crate::ui::stream_page::StreamInput::ReloadRecordings);
                            self.reload_library_overviews();
                            self.toast(&gettext("Recording edited"));
                        }
                        EditKind::Memo => {
                            let _ = self.library.update_memo_file(id, &p, duration_ms);
                            self.reload_memos(&sender);
                            self.toast(&gettext("Memo edited"));
                        }
                    }
                }
                None => self.toast(&gettext("Editing failed")),
            },
            Msg::SetRecordingBufferMinutes(n) => {
                self.streaming.recording_buffer_minutes = n.min(60);
                let _ = self.library.set_setting(
                    "recording_buffer_minutes",
                    &self.streaming.recording_buffer_minutes.to_string(),
                );
                self.stream_page
                    .emit(crate::ui::stream_page::StreamInput::SetBufferMinutes(
                        self.streaming.recording_buffer_minutes,
                    ));
            }
            // --- Voice memos ---
            Msg::MemoRecordSaved { path, duration_ms } => match path {
                Some(p) => {
                    let title = crate::ui::app_memo::memo_default_title();
                    let _ = self.library.add_memo(&p, &title, None, duration_ms);
                    self.reload_memos(&sender);
                    self.toast(&gettext("Memo saved"));
                }
                None => self.toast(&gettext("Recording failed")),
            },
            Msg::SetMemoView(view) => {
                if self.memo.view != view {
                    self.memo.view = view;
                    self.reload_memos(&sender);
                }
            }
            Msg::OpenMemo(id) => self.open_memo(root, &sender, id),
            Msg::MemoRename { id, title } => {
                let _ = self.library.rename_memo(id, &title);
                self.reload_memos(&sender);
            }
            Msg::MemoSetCategory { id, category_id } => {
                let _ = self.library.set_memo_category(id, category_id);
                self.reload_memos(&sender);
            }
            Msg::MemoDelete(id) => {
                self.undo_toast(
                    &sender,
                    &gettext("Memo deleted"),
                    Msg::MemoDeleteConfirmed(id),
                );
            }
            Msg::MemoDeleteConfirmed(id) => {
                if let Ok(Some(path)) = self.library.delete_memo(id) {
                    let _ = std::fs::remove_file(&path);
                }
                self.reload_memos(&sender);
            }
            Msg::MemoCategoryAddPrompt => self.prompt_new_memo_category(root, &sender),
            Msg::MemoCategoryAdd(name) => {
                let _ = self.library.add_memo_category(&name);
                self.reload_memo_categories(&sender);
            }
            Msg::ToggleEpisode { url, title } => self.toggle_episode(url, title),
            Msg::EpisodeSeekTo { url, title, ms } => self.episode_seek_to(url, title, ms),
            Msg::PushPodcastSubpage => {
                if let Some((title, content)) = self.podcast_subpage.borrow_mut().take() {
                    self.push_subpage(&title, &content);
                    // The episode rows are now realized → let the page set their
                    // play/pause icons to the current state.
                    self.podcasts_page.emit(
                        crate::ui::podcasts_page::PodcastsInput::PlaybackStateChanged {
                            playing_url: self.podcasts.playing_episode_url.clone(),
                            playing: self.mini.playing,
                        },
                    );
                }
            }
            Msg::PodcastToast(s) => self.toast(&s),
            Msg::PodcastUndoToast(id) => {
                self.undo_toast(
                    &sender,
                    &gettext("Podcast removed"),
                    Msg::PodcastReallyDelete(id),
                );
            }
            Msg::PodcastReallyDelete(id) => {
                self.podcasts_page
                    .emit(crate::ui::podcasts_page::PodcastsInput::DeleteConfirmed(id));
            }
            Msg::PodcastRefreshStarted(started) => {
                if started {
                    self.refresh_pending += 1;
                }
            }
            Msg::PodcastRefreshFinished => self.refresh_done(),
            // --- YouTube ---
            Msg::FetchYtDlp => {
                let update = self.youtube.ytdlp_version.is_some();
                self.start_ytdlp_fetch(update, &sender);
            }
            Msg::YtDlpAutoUpdate => self.maybe_auto_update_ytdlp(&sender),
            // --- transport (requested by the YtPage component / worker results) ---
            Msg::YtPlayChannel(id) => self.yt_play_channel(id),
            Msg::YtStartPlaylist { url, title } => self.yt_start_playlist(&sender, url, title),
            Msg::YtStartPlaylistAt {
                url,
                title,
                index,
                close,
                videos,
            } => self.yt_start_playlist_at(url, title, index, close, videos),
            Msg::YtPlayVideo { video_id, title } => self.yt_play_video(video_id, title),
            Msg::YtStreamResolved {
                video_id,
                resume,
                result,
            } => self.yt_stream_resolved(&sender, video_id, resume, result),
            Msg::YtEnriched {
                video_id,
                artist,
                cover,
            } => self.yt_enriched(video_id, artist, cover),
            // --- bridge from the YtPage component to the shared parent chrome ---
            Msg::YtOpenPlaylist { id, name } => self.open_playlist(&sender, id, &name),
            Msg::YtShowVideoDetail { video_id, title } => self
                .yt_page
                .emit(crate::ui::yt_page::YtInput::ShowVideoDetail { video_id, title }),
            Msg::YtToast(s) => self.toast(&s),
            Msg::YtProgress(s) => self.yt_progress(&s),
            Msg::YtProgressDone(s) => self.yt_progress_done(&s),
            Msg::YtSetLoading(o) => {
                self.libview.loading = o.is_some();
                self.libview.loading_label = o;
            }
            Msg::YtLibraryChanged => self.reload_library_overviews(),
            Msg::YtPlaylistsChanged => self.reload_playlists(&sender),
            Msg::PushYtSubpage => {
                if let Some((title, content)) = self.yt_subpage.borrow_mut().take() {
                    self.push_subpage(&title, &content);
                    // The video rows are now realized → set their play/pause icons.
                    self.yt_page
                        .emit(crate::ui::yt_page::YtInput::PlaybackStateChanged {
                            playing_video_id: self.youtube.playing_video_id.clone(),
                            playing: self.mini.playing,
                        });
                }
            }
            Msg::YtChannelUndo(id) => {
                self.undo_toast(
                    &sender,
                    &gettext("Channel removed"),
                    Msg::YtChannelReallyDelete(id),
                );
            }
            Msg::YtChannelReallyDelete(id) => {
                self.yt_page
                    .emit(crate::ui::yt_page::YtInput::DeleteChannelConfirmed(id));
            }
            Msg::YtRefreshStarted(started) => {
                if started {
                    self.refresh_pending += 1;
                }
            }
            Msg::YtRefreshFinished => self.refresh_done(),
            Msg::CtxEqualizer => self.open_eq_dialog(root, &sender),
            Msg::CtxShare => self.on_ctx_share(root),
            Msg::ShareItems(selection) => self.share_items(selection, root),
            Msg::CtxRefresh => self.on_ctx_refresh(root, &sender),
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
            Msg::TrackFinished => self.on_track_finished(),
            Msg::GaplessAdvanced => self.on_gapless_advanced(),
            Msg::PersistResume => self.on_persist_resume(),
            Msg::Tick => self.on_tick(&sender),
            Msg::AutoEnrichTick => self.on_auto_enrich_tick(&sender),
            Msg::FingerprintCurrent(path) => self.fetch_focus_track(&sender, &path),
            Msg::LoadLyrics(path) => self.load_lyrics(&sender, path),
            Msg::ShowLyrics => self.show_lyrics(),
            Msg::LyricsTick => self.update_lyrics_highlight(),
            Msg::LyricsSeek(ts) => {
                // Jump to the clicked line (its LRC time shifted by the delay).
                let delay = self.lyrics.view.as_ref().map(|v| v.delay_ms).unwrap_or(0);
                let target = (ts + delay).max(0);
                self.mini.position_ms = target;
                if self.player.seek_ms(target).is_ok() {
                    self.mpris.seeked(target);
                }
                self.update_lyrics_highlight();
            }
            Msg::LyricsClosed => self.close_lyrics_view(),
            Msg::LyricsToggleKaraoke => self.toggle_lyrics_karaoke(),
            Msg::LyricsDelayAdjust(step) => self.adjust_lyrics_delay(step),
            Msg::FileLyricsFetched { path, lyrics } => self.on_file_lyrics_fetched(path, lyrics),
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
                // Shuffle changes the "next" track → re-arm (or clear) gapless.
                self.arm_gapless();
            }
            Msg::ToggleRepeat => {
                self.transport.repeat = !self.transport.repeat;
                let _ = self
                    .library
                    .set_setting("repeat", if self.transport.repeat { "1" } else { "0" });
                self.mpris.set_repeat(self.transport.repeat);
            }
            Msg::NavUp => self.on_nav_up(&sender),
            Msg::FilesGoStart => self.on_files_go_start(&sender),
            Msg::Refresh => self.on_refresh(&sender),
            Msg::OpenSettings => self.open_settings(root, &sender),
            Msg::SetSleepTimer(choice) => self.on_set_sleep_timer(choice),
            Msg::InlineFilter(text) => self.apply_inline_filter(&text),
            Msg::OpenSearch => self.open_search_dialog(root, &sender),
            Msg::SearchPlayTrack(path) => self.on_search_play_track(path, &sender),
            Msg::SearchOpenAlbum(album) => self.open_album_by_name(&sender, &album),
            Msg::SearchOpenArtist(name) => self.on_search_open_artist(name, &sender),
            Msg::OpenGlobalEq => self.open_global_eq(root, &sender),
            Msg::OpenCurrentEq => self.on_open_current_eq(root, &sender),
            Msg::OpenTrackEq { path, title } => {
                self.open_eq_editor(root, &sender, "the track", &title, None, "track", path);
            }
            Msg::ShowQueue => self.open_queue_dialog(root, &sender),
            Msg::ShowCurrentAlbum => {
                if let Some(album) = self.mini.current_album.clone() {
                    self.open_album_by_name(&sender, &album);
                }
            }
            Msg::NavBack => {
                self.nav.nav_view.pop();
            }
            Msg::PlayQueueAt { start, len } => self.on_play_queue_at(start, len),
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
            Msg::QueueClear => self.on_queue_clear(),
            Msg::QueueMoveRange { from, len, to } => self.on_queue_move_range(from, len, to),
            Msg::SetMusicDir(path) => self.on_set_music_dir(path, &sender),
            Msg::SetupFinished {
                lang_code,
                music_dir,
                enabled_sections,
            } => self.on_setup_finished(lang_code, music_dir, enabled_sections, &sender),
            Msg::SelectSource(sel) => {
                if self.files.active_source != sel {
                    self.apply_source(sel, &sender);
                }
            }
            Msg::SourcesChanged => self.on_sources_changed(&sender),
            Msg::DeleteSource(id) => {
                let _ = self.library.delete_source(id);
                self.on_sources_changed(&sender);
            }
            Msg::CheckSources => self.on_check_sources(&sender),
            Msg::AddCloudSource => {
                use crate::ui::cloud_page::CloudInput;
                self.cloud_page.emit(CloudInput::Open {
                    window: root.clone(),
                    mobile: self.is_mobile(),
                });
            }
            Msg::CloudIndexed => self.on_cloud_indexed(&sender),
            Msg::CtxDownloadRemote(rel) => self.on_ctx_download_remote(rel, &sender),
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
            Msg::SetLanguage(lang) => self.on_set_language(lang, root),
            Msg::SetColorScheme(scheme) => {
                apply_color_scheme(&scheme);
                let _ = self.library.set_setting("color_scheme", &scheme);
            }
            Msg::SetGapless(on) => {
                self.settings.gapless = on;
                let _ = self
                    .library
                    .set_setting("gapless", if on { "1" } else { "0" });
                self.apply_playback_prefs();
            }
            Msg::SetCrossfade(secs) => {
                self.settings.crossfade_secs = secs.clamp(0.0, 12.0);
                let _ = self
                    .library
                    .set_setting("crossfade_secs", &self.settings.crossfade_secs.to_string());
                self.apply_playback_prefs();
            }
            Msg::SortMenuRefresh => self.rebuild_sort_menu(),
            Msg::SetSortCrit(crit) => {
                let Some(section) = self.current_section() else {
                    return;
                };
                let (cur, desc) = self.libview.sort_for(&section);
                if cur != crit {
                    self.set_section_sort(&section, crit, desc, &sender);
                }
            }
            Msg::SetSortDir(desc) => {
                let Some(section) = self.current_section() else {
                    return;
                };
                let (crit, cur) = self.libview.sort_for(&section);
                if cur != desc {
                    self.set_section_sort(&section, crit, desc, &sender);
                }
            }
            Msg::SetSortNoGroup(off) => {
                let Some(section) = self.current_section() else {
                    return;
                };
                if self.libview.grouping_off(&section) != off {
                    self.set_section_grouping(&section, off, &sender);
                }
            }
            Msg::SetGalleryView(on) => {
                self.libview.gallery_view = on;
                let _ = self
                    .library
                    .set_setting("gallery_view", if on { "1" } else { "0" });
                self.rebuild_all_lists(&sender);
                self.podcasts_page
                    .emit(crate::ui::podcasts_page::PodcastsInput::SetGalleryView(on));
                self.yt_page
                    .emit(crate::ui::yt_page::YtInput::SetGalleryView(on));
                // Artists/Albums gallery tiles aren't filtered → update the
                // funnel button's visibility for the new (list/gallery) mode.
                self.update_filter_chrome();
            }
            Msg::SetGalleryColumns(n) => {
                self.libview.gallery_columns = n.clamp(2, 8);
                let _ = self
                    .library
                    .set_setting("gallery_columns", &self.libview.gallery_columns.to_string());
                if self.libview.gallery_view {
                    self.rebuild_all_lists(&sender);
                }
                self.podcasts_page.emit(
                    crate::ui::podcasts_page::PodcastsInput::SetGalleryColumns(
                        self.libview.gallery_columns,
                    ),
                );
                self.yt_page
                    .emit(crate::ui::yt_page::YtInput::SetGalleryColumns(
                        self.libview.gallery_columns,
                    ));
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
                // The YouTube section is the opt-in feature; its menu switch is now
                // the single enable/disable control, so route it through
                // `set_youtube_enabled` (keeps the `youtube_enabled` flag + the
                // background channel load in step). All other sections just toggle
                // their menu visibility.
                if section == "youtube" {
                    self.set_youtube_enabled(visible, &sender);
                } else {
                    self.set_section_visible(section, visible);
                }
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
            Msg::TogglePlay => self.on_toggle_play(),
            Msg::OpenNowPlaying => self.on_open_now_playing(root, &sender),
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
            Cmd::ReloadRecordings => self
                .stream_page
                .emit(crate::ui::stream_page::StreamInput::ReloadRecordings),
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
        self.libview.loading || self.refresh_pending > 0 || self.scanning
    }

    /// Text beneath the overlay spinner. A specific load label (e.g. a YouTube
    /// playlist) wins; otherwise a manual refresh shows "Updating …", and
    /// finally the default "reading data" of a plain folder/list load.
    pub(crate) fn overlay_text(&self) -> String {
        if let Some(label) = &self.libview.loading_label {
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
