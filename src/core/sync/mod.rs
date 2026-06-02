//! Device-to-device sync over the LAN (port of the DrivePulse logic).
//!
//! Flow: one device starts an HTTPS server ([`server`]) and shows a
//! QR code ([`qr`]); the other scans it ([`scanner`]), pins the certificate
//! fingerprint ([`crypto`]) and pairs ([`client`]). Afterwards
//! library metadata and audio files are transferred ([`data`], [`protocol`]).

pub mod client;
pub mod crypto;
pub mod data;
pub mod protocol;
pub mod qr;
pub mod scanner;
pub mod server;

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Preferred TCP port of the sync server (falls back if taken).
pub const PORT: u16 = 8765;
/// Number of fallback ports to try if [`PORT`] is taken.
pub const PORT_ATTEMPTS: u16 = 10;
/// Session expires if the client makes no request for a while.
pub const SESSION_TIMEOUT: Duration = Duration::from_secs(30);
/// Validity period of the QR code (also the wait time for a pairing).
pub const QR_TTL: Duration = Duration::from_secs(120);
/// Accept loop: maximum blocking time, after which the stop flag is checked.
pub const ACCEPT_POLL: Duration = Duration::from_millis(500);
/// Abort after this many failed pairing attempts (brute-force protection).
pub const MAX_FAILED_PAIR: u32 = 5;

/// Events from the server thread or the client worker to the UI.
///
/// Must be `Debug + Send` (transported inside `Cmd`).
#[derive(Debug)]
pub enum SyncEvent {
    /// Server is running; `pair_url` should be shown as a QR code.
    ServerReady {
        pair_url: String,
        host: String,
        port: u16,
    },
    /// Server was stopped (timeout, stop or error after start).
    ServerStopped,
    /// Another device paired successfully.
    PeerPaired { peer_name: String },
    /// Connection was dropped (by the peer or by timeout).
    PeerDisconnected,
    /// An incoming metadata import was applied (server side).
    ImportReceived { stats: ImportStats },
    /// Metadata was sent to the peer.
    ExportSent,
    /// Progress of a file transfer.
    FileProgress { done: u64, total: u64, name: String },
    /// File transfer finished.
    TransferDone { files: usize },
    /// Error (shown as a toast).
    Error(String),
}

/// Counters of what was created/updated during an import.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportStats {
    pub favorites: usize,
    pub playlists: usize,
    pub podcasts: usize,
    pub categories: usize,
    pub eq: usize,
    pub files: usize,
}

/// Current Unix timestamp in seconds.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Local IPv4 address via a UDP "connection attempt" (no real traffic).
pub fn local_ip() -> String {
    use std::net::UdpSocket;
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|sock| {
            sock.connect("8.8.8.8:80")?;
            Ok(sock.local_addr()?.ip().to_string())
        })
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// Display name of this device (hostname, otherwise "Emilia").
pub fn default_device_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "Emilia".to_string())
}
