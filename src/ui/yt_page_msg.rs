//! Messages of the [`YtPage`](crate::ui::yt_page::YtPage) component: what the
//! page takes ([`YtInput`]), what it asks the parent for ([`YtOutput`]) and
//! what its worker threads report back ([`YtCmd`]), plus the search-kind
//! choice of the search dialog. Re-exported from [`crate::ui::yt_page`], so
//! callers keep reaching them as `crate::ui::yt_page::…`.

use relm4::adw;

use crate::core::youtube::{self, YtKind, YtResult};
use crate::ui::app::{SortCrit, YtView};

/// What the search dialog looks for: one of the regular result kinds, or
/// streams that are live right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchKind {
    Yt(YtKind),
    Live,
}

#[derive(Debug)]
pub(crate) enum YtInput {
    /// A video's play button / "Play" was tapped: forwarded to the transport,
    /// noting whether it came from the "Recently" list.
    PlayVideo {
        video_id: String,
        title: String,
    },
    // --- driven by the parent ---
    Reload,
    RefreshAll,
    ReloadRecent,
    PlaybackStateChanged {
        playing_video_id: Option<String>,
        playing: bool,
    },
    /// Per-second position of the running long-form video (from the transport):
    /// update its rows' progress widgets in place.
    VideoProgressTick {
        video_id: String,
        position_ms: i64,
        duration_ms: i64,
    },
    /// The video played to its end → show its rows as "Listened" right away.
    VideoFinished {
        video_id: String,
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
    /// Toggle the gallery tiles' title ("Show description").
    SetGalleryDesc(bool),
    SetGalleryView(bool),
    SetGalleryColumns(u32),
    SetMobile(bool),
    SetWindow(adw::ApplicationWindow),
    // --- view-internal ---
    /// Banner button → ask the parent to open the settings (yt-dlp update).
    OpenSettings,
    Subscribe,
    Search(String, SearchKind),
    /// Save the live search hit at this index to the Live tab.
    AddLive(usize),
    ShowLiveDetail(String),
    RemoveLive(String),
    SubscribeChannel(String),
    OpenChannel(i64),
    OpenChannelAt(usize),
    ShowChannelDetail(i64),
    ShowChannelDetailAt(usize),
    RefreshChannel(i64),
    /// Detail views' refresh: fetch the metadata (artist, cover, description,
    /// song list, avatar) again, then reopen the detail view.
    RefreshVideo {
        video_id: String,
        title: String,
    },
    RefreshChannelDetail(i64),
    RefreshPlaylist {
        url: String,
        title: String,
    },
    RefreshLive(String),
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
        /// Artist from a better source than YouTube (e.g. a radio stream's song
        /// recognition), used as the hint for the online metadata lookup.
        artist: Option<String>,
    },
    AddToLibraryConfirmed {
        video_id: String,
        title: String,
        artist: Option<String>,
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
        /// Tapped in the "Recently" list: keep its place there.
        keep_recent_order: bool,
    },
    /// Play a video from one of its jump marks (chapter list / a timestamp in
    /// the description), like tapping a timestamp in podcast shownotes.
    PlayVideoAt {
        video_id: String,
        title: String,
        ms: i64,
    },
    /// Transport: play/pause a saved live stream (streamed only, like a station).
    PlayLive {
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
    /// Open the equalizer dialog of a live stream (Live tab detail).
    OpenLiveEq {
        video_id: String,
        title: String,
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
    /// Live progress of the running "refresh all" (channel `done` of `total`,
    /// name of the channel being fetched) for the overlay's progress bar.
    RefreshProgress {
        done: usize,
        total: usize,
        label: String,
    },
    /// Outcome of a refresh, shown briefly in the overlay — informational toasts
    /// are disabled app-wide, so this is the only feedback channel left.
    RefreshSummary(String),
    /// Share a selection (a YouTube channel or video) over device sync.
    Share(Box<crate::core::sync::share::Selection>),
    /// The sort slot was rebuilt → the parent refreshes the shared title-bar
    /// sort button (if the YouTube section is showing).
    SortChanged,
}

#[derive(Debug)]
pub(crate) enum YtCmd {
    /// A detail refresh finished → reopen that detail view with the new data.
    VideoRefreshed {
        video_id: String,
        title: String,
    },
    ChannelDetailRefreshed(i64),
    PlaylistRefreshed {
        url: String,
        title: String,
        result: Result<Vec<YtResult>, String>,
    },
    LiveRefreshed {
        video_id: String,
        details: Option<youtube::YtResult>,
        thumbnail: Option<String>,
    },
    SearchResults(u64, Vec<YtResult>),
    SearchFailed(u64),
    SearchThumbsReady(u64),
    ChannelFetched(Option<String>),
    /// A newly subscribed channel's video cache finished filling in the
    /// background — the lists can show its videos now.
    ChannelVideosReady,
    /// One channel of a "refresh all" is about to be fetched.
    RefreshProgress {
        done: usize,
        total: usize,
        title: String,
    },
    /// All channels re-fetched, with what the run brought in.
    ChannelsRefreshed {
        updated: usize,
        failed: usize,
        new_videos: usize,
    },
    /// The refresh worker found no usable yt-dlp — nothing was fetched.
    RefreshUnavailable,
    VideoMeta {
        video_id: String,
        uploader: Option<String>,
        duration: Option<i64>,
        cover: Option<String>,
        /// Jump marks from the description (empty when the video has none).
        chapters: Vec<(i64, String)>,
    },
    LibraryProgress {
        done: usize,
        total: usize,
    },
    /// Live phase/percentage of a single-video library add, for the popup.
    AddLibProgress {
        video_id: String,
        progress: youtube::AddProgress,
    },
    LibraryAdded {
        video_id: Option<String>,
        result: Result<usize, String>,
    },
    LibraryExists {
        video_id: String,
        title: String,
        /// Carried through so "Overwrite" reruns the import with the same
        /// artist hint the first attempt had.
        artist: Option<String>,
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
    /// Startup channel-thumbnail cache finished; `true` if it brought in a
    /// thumbnail that was missing → redraw.
    CoversCached(bool),
}
