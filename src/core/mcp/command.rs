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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    PlayAlbum { artist: String, album: String },
    /// Play all tracks of an artist.
    PlayArtist(String),
    /// Play a single track by its library path.
    PlayTrack(String),
    /// Stream a podcast episode by its audio URL (`title` for display).
    PlayEpisode { url: String, title: String },
    /// Play a voice memo / recording by its file path.
    PlayMemo(String),
    /// Play a YouTube video by its id (`title` for display).
    PlayYoutube { video_id: String, title: String },
    /// Play a playlist by id, optionally shuffled.
    PlayPlaylist { id: i64, shuffle: bool },
    /// Rename a playlist.
    RenamePlaylist { id: i64, name: String },
    /// Delete a playlist (destructive; the tool gate requires confirmation).
    DeletePlaylist(i64),
    /// Set a playlist's cover image from a file path.
    SetPlaylistCover { id: i64, path: String },
    /// Append tracks (by library path) to the user queue (play next).
    Enqueue(Vec<String>),
    /// Toggle a podcast episode's listened/unlistened state.
    ToggleEpisodeListened { url: String, title: String },
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
    SetArtistImage { name: String, path: String },
    /// Set the areas (properties) an item appears in. `scope` ∈ {track, album,
    /// artist}; `value` is a comma-separated area list (empty = hidden).
    SetAreas {
        scope: String,
        key: String,
        value: String,
    },
    /// Arm the sleep timer for this many minutes; `0` turns it off.
    SetSleepTimer(u32),
}

/// Installed by the UI; invoked from the MCP server thread (any thread, hence
/// `Send + Sync`). Fire-and-forget — the tool answers optimistically.
pub type ControlFn = Arc<dyn Fn(McpCommand) + Send + Sync>;
