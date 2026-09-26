//! Abstract playback/control commands the MCP server issues into the app.
//!
//! The MCP layer lives in `core` and must stay independent of the GTK/relm4 UI.
//! Rather than referencing the UI `Msg` enum directly (which would couple
//! `core` to `ui`), a tool maps to one of these backend-agnostic
//! [`McpCommand`]s. The UI installs a [`ControlFn`] at startup that translates
//! each command into the matching `Msg` and posts it to the relm4 main loop
//! (see `src/ui/app_init.rs`). This keeps `core::mcp` UI-free and unit-testable.

use std::sync::Arc;

/// A single control action requested by an MCP tool. Deliberately coarse: the
/// UI decides how each maps onto its own playback model.
#[derive(Debug, Clone, PartialEq)]
pub enum McpCommand {
    /// Resume playback (no-op if already playing).
    Play,
    /// Pause playback.
    Pause,
    /// Toggle play/pause.
    TogglePlay,
    /// Skip to the next track.
    Next,
    /// Skip to the previous track.
    Prev,
    /// Seek to an absolute position (milliseconds) in the current track.
    Seek(i64),
    /// Play a whole album in track order.
    PlayAlbum {
        artist: String,
        album: String,
    },
    /// Play all tracks of an artist.
    PlayArtist(String),
    /// Play a single track by its library path.
    PlayTrack(String),
    /// Stream a podcast episode by its audio URL (`title` for display).
    PlayEpisode {
        url: String,
        title: String,
    },
    /// Play a voice memo / recording by its file path.
    PlayMemo(String),
    /// Play a YouTube video by its id (`title` for display).
    PlayYoutube {
        video_id: String,
        title: String,
    },
    /// Play a playlist by id, optionally shuffled.
    PlayPlaylist {
        id: i64,
        shuffle: bool,
    },
    /// Rename a playlist.
    RenamePlaylist {
        id: i64,
        name: String,
    },
    /// Delete a playlist (destructive; the tool gate requires confirmation).
    DeletePlaylist(i64),
    /// Set a playlist's cover image from a file path.
    SetPlaylistCover {
        id: i64,
        path: String,
    },
    /// Append tracks (by library path) to the user queue (play next).
    Enqueue(Vec<String>),
    /// Toggle a podcast episode's listened/unlistened state.
    ToggleEpisodeListened {
        url: String,
        title: String,
    },
    /// Delete a voice memo by id (destructive; gated by confirmation).
    DeleteMemo(i64),
    /// Delete a stream recording by id (destructive; gated by confirmation).
    DeleteRecording(i64),
    /// Set an album's cover image from a file path.
    SetAlbumCover {
        artist: String,
        album: String,
        path: String,
    },
    /// Set an artist's photo from a file path.
    SetArtistImage {
        name: String,
        path: String,
    },
    /// Set the areas (properties) an item appears in. `scope` ∈ {track, album,
    /// artist}; `value` is a comma-separated area list (empty = hidden).
    SetAreas {
        scope: String,
        key: String,
        value: String,
    },
    /// Arm the sleep timer for this many minutes; `0` turns it off.
    SetSleepTimer(u32),
    /// Start a saved radio station (by its db id); idempotent when it already runs.
    PlayStation(i64),
    /// Start/stop the timeshift recording of a station.
    ToggleStationRecording(i64),
    /// Remove a saved station (destructive; the tool gate requires confirmation).
    /// Stops it first if it is playing.
    DeleteStation(i64),
    /// The station list changed in the DB (added/renamed over MCP) → redraw it.
    ReloadStations,
    /// Unsubscribe a podcast (destructive; gated by confirmation).
    DeletePodcast(i64),
    /// The podcast list changed in the DB (subscribed over MCP) → redraw it.
    ReloadPodcasts,
    /// Re-fetch every subscribed feed (the page's "refresh all").
    RefreshPodcasts,
    /// Track rows changed in the DB (tags edited / files deleted over MCP) →
    /// rebuild the artist/album overviews.
    LibraryChanged,
    /// Play a YouTube playlist by its URL (`title` for display).
    PlayYoutubePlaylist {
        url: String,
        title: String,
    },
    /// Play/resume a saved YouTube live stream (idempotent when it already runs).
    PlayLive {
        video_id: String,
        title: String,
    },
    /// Play a subscribed YouTube channel's cached videos as the queue.
    PlayYoutubeChannel(i64),
    /// Re-fetch one subscribed channel (`Some(id)`) or all of them (`None`).
    RefreshYoutube(Option<i64>),
    /// Remove a channel subscription (destructive; gated by confirmation).
    DeleteYoutubeChannel(i64),
    /// YouTube data changed in the DB (subscriptions / live streams) → redraw.
    ReloadYoutube,
    /// Play a favorite / audiobook / concert entry as its list row would.
    PlayEntry {
        scope: String,
        key: String,
        is_dir: bool,
    },
    /// The favorites list changed in the DB → redraw it.
    ReloadFavorites,
    /// Rename a voice memo.
    RenameMemo {
        id: i64,
        title: String,
    },
    /// Move a memo to a category (`None` = "General").
    SetMemoCategory {
        id: i64,
        category_id: Option<i64>,
    },
    /// Rename a memo category.
    RenameMemoCategory {
        id: i64,
        name: String,
    },
    /// Remove a memo category; `with_memos` also deletes its memos (destructive;
    /// gated by confirmation), otherwise they move to "General".
    DeleteMemoCategory {
        id: i64,
        with_memos: bool,
    },
    /// Start recording a voice memo from the microphone. `stop_after_s` stops
    /// it automatically; `title` / `category_id` are applied when it is saved.
    StartMemo {
        stop_after_s: Option<u32>,
        title: Option<String>,
        category_id: Option<i64>,
    },
    /// Stop the running memo recording and save it (`title` / `category_id`
    /// override what the start asked for).
    StopMemo {
        title: Option<String>,
        category_id: Option<i64>,
    },
    /// Memo data changed in the DB (a category was added) → redraw.
    ReloadMemos,
    /// Empty the play queue (context and user queue).
    ClearQueue,
    /// Turn shuffle / repeat on or off (idempotent).
    SetShuffle(bool),
    SetRepeat(bool),
    /// Playback speed (0.25–2.0).
    SetPlaybackRate(f64),
    /// Save (`Some`) or reset (`None`) an equalizer level of the default
    /// output and apply it. `scope` ∈ global/artist/album/track/stream/podcast/
    /// episode; the key follows the app's convention for that scope.
    SetEqualizer {
        scope: String,
        key: String,
        bands: Option<[f64; 10]>,
    },
    /// Device sync: start the pairing server (offer a connection).
    SyncStartServer,
    /// Device sync: connect to a peer's pairing code (`emilia://pair?…`).
    SyncPair(String),
    /// Device sync: share a selection with the paired peer, sending without the
    /// size-confirmation step. Boxed: `Selection` is large.
    SyncShare(Box<crate::core::sync::share::Selection>),
    /// Device sync: accept (everything new) or reject the pending incoming offer.
    SyncRespond {
        accept: bool,
    },
    /// Device sync: end the live pairing.
    SyncDisconnect,
}

/// Installed by the UI; invoked from the MCP server thread (any thread, hence
/// `Send + Sync`). Fire-and-forget — the tool answers optimistically.
pub type ControlFn = Arc<dyn Fn(McpCommand) + Send + Sync>;
