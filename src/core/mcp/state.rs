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
    /// What kind of item is loaded: `track`, `episode`, `station`, `youtube`,
    /// `youtube_live` or `remote`; `None` when nothing is.
    pub kind: Option<&'static str>,
    /// Its identifier in the terms of the matching tools: a library path, an
    /// episode URL, a station id, a video id or a remote path.
    pub id: Option<String>,
    /// The play context (library paths / `yt:<id>`), and where in it we are.
    pub queue: Vec<String>,
    pub queue_pos: usize,
    /// Tracks explicitly enqueued ("play next"), consumed as they play.
    pub user_queue: Vec<String>,
    pub shuffle: bool,
    pub repeat: bool,
    pub playback_rate: f64,
    /// A voice memo is being recorded from the microphone.
    pub memo_recording: bool,
    /// When the running memo recording started (Unix seconds).
    pub memo_started_at: Option<i64>,
    /// Bumped on every memo event (started, failed to start, saved, failed to
    /// save), so a tool can wait for the outcome of its own request.
    pub memo_seq: u64,
    /// Outcome of the latest memo event: the saved memo, or the error.
    pub last_memo: Option<SavedMemo>,
    pub memo_error: Option<String>,
}

/// A memo the recorder just saved.
#[derive(Debug, Clone)]
pub struct SavedMemo {
    pub id: i64,
    pub title: String,
    pub path: String,
    pub duration_ms: i64,
    pub category_id: Option<i64>,
}

impl NowPlaying {
    /// Live items (a radio station, a YouTube live stream) have no position to
    /// seek to and no duration.
    pub fn is_live(&self) -> bool {
        matches!(self.kind, Some("station" | "youtube_live"))
    }
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
