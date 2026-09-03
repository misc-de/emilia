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

// ---- device-sync snapshot ----------------------------------------------------

/// Summary of an incoming share offer (what the peer wants to send us), kept
/// small and serialisable for the `sync_status` tool.
#[derive(Debug, Clone, Default)]
pub struct OfferSummary {
    /// Peer device name (from the manifest).
    pub from: String,
    /// Number of audio files offered / of those not yet on this device.
    pub files: usize,
    pub new_files: usize,
    pub total_size: u64,
    pub yt: usize,
    pub stations: usize,
    pub recordings: usize,
    pub memos: usize,
    pub favorites: bool,
    pub playlists: bool,
    pub podcasts: bool,
    pub categories: bool,
    pub eq: bool,
}

/// What the device-sync component is doing right now. The sync flow lives in
/// the relm4 `SyncPage` on the GTK main loop; it republishes this snapshot on
/// every event so the `sync_*` tools can report and gate on it.
#[derive(Debug, Clone, Default)]
pub struct SyncSnapshot {
    /// A live pairing exists (this device may be server or client).
    pub connected: bool,
    pub peer_name: Option<String>,
    /// `true` while this device is the offering (server) side.
    pub is_server: bool,
    /// The pairing server is up and waiting for a peer to scan/paste the code.
    pub listening: bool,
    /// Pairing code (an `emilia://pair?…` URL) while listening.
    pub pair_url: Option<String>,
    pub address: Option<String>,
    /// Coarse flow phase: `idle`, `pairing`, `preparing`, `waiting_for_peer`,
    /// `sending`, `receiving`, `offer_pending`, `done`.
    pub phase: String,
    /// An offer from the peer that still awaits our accept/reject.
    pub incoming_offer: Option<OfferSummary>,
    /// Running file transfer: (done, total, current file).
    pub progress: Option<(u64, u64, String)>,
    /// Files moved by the last finished transfer.
    pub last_transfer_files: Option<usize>,
    pub last_error: Option<String>,
}

/// Shared handle the sync component writes and the MCP tools read.
pub type SyncStateHandle = Arc<Mutex<SyncSnapshot>>;

/// A fresh, idle sync snapshot handle.
pub fn new_sync_handle() -> SyncStateHandle {
    Arc::new(Mutex::new(SyncSnapshot::default()))
}
