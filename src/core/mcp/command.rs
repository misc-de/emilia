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
    /// Arm the sleep timer for this many minutes; `0` turns it off.
    SetSleepTimer(u32),
}

/// Installed by the UI; invoked from the MCP server thread (any thread, hence
/// `Send + Sync`). Fire-and-forget — the tool answers optimistically.
pub type ControlFn = Arc<dyn Fn(McpCommand) + Send + Sync>;
