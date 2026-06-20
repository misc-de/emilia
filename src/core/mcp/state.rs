//! Shared, readable snapshot of "what is playing right now".
//!
//! The real playback state lives in the relm4 `App` on the GTK main loop and is
//! not reachable from the MCP server thread. The app therefore publishes a small
//! snapshot here on every track/playback change; the `now_playing` tool reads it
//! under the mutex. Cheap to clone; the UI write is a brief lock, never real work.

use std::sync::{Arc, Mutex};

/// What the player is currently doing. All fields are best-effort.
#[derive(Debug, Clone, Default)]
pub struct NowPlaying {
    pub playing: bool,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub position_ms: i64,
    pub duration_ms: i64,
}

/// Shared handle the UI writes and the MCP tools read.
pub type NowPlayingHandle = Arc<Mutex<NowPlaying>>;

/// A fresh, empty snapshot handle.
pub fn new_handle() -> NowPlayingHandle {
    Arc::new(Mutex::new(NowPlaying::default()))
}
