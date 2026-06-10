//! Device-to-device sync over the LAN (port of the DrivePulse logic).
//!
//! Flow: one device starts an HTTPS server ([`server`]) and shows a
//! QR code ([`qr`]); the other scans it ([`scanner`]), pins the certificate
//! fingerprint ([`crypto`]) and pairs ([`client`]). Afterwards
//! library metadata and audio files are transferred ([`data`], [`protocol`]).

pub mod client;
pub mod crypto;
pub mod data;
pub mod hash;
pub mod protocol;
pub mod qr;
pub mod scanner;
pub mod server;
pub mod share;

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::core::sync::protocol::Capabilities;
use crate::core::sync::share::{ShareDecision, ShareManifest};

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
    /// Another device paired successfully (with its advertised capabilities).
    PeerPaired {
        peer_name: String,
        peer_caps: Capabilities,
    },
    /// Connection was dropped (by the peer or by timeout).
    PeerDisconnected,
    /// An incoming metadata import was applied (server side).
    ImportReceived { stats: ImportStats },
    /// Metadata was sent to the peer.
    /// Not emitted yet (sender-side metadata export/import is unwired); the UI
    /// already handles it.
    #[allow(dead_code)]
    ExportSent,
    /// The peer offered a selective share → show the review screen (receiver).
    ShareOffered { manifest: ShareManifest },
    /// The peer responded to our offer with its decision (sender).
    OfferAccepted {
        // The transfer is driven elsewhere; the UI only reacts to the event.
        #[allow(dead_code)]
        decision: ShareDecision,
    },
    /// Our selection was resolved to a manifest → show the size confirmation.
    /// Carries the full manifest so the sender side can park/send it on confirm.
    ManifestReady { manifest: ShareManifest },
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
    /// Saved radio stations imported.
    #[serde(default)]
    pub stations: usize,
    /// Artist photos / album covers + years applied.
    #[serde(default)]
    pub meta: usize,
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

/// True if `rel` is a non-empty *relative* path whose every component is a plain
/// segment — no `..`, no `.`, no root or drive prefix. This is the robust
/// alternative to a `rel.contains("..")` string check, which both misses sneaky
/// inputs and needlessly rejects legitimate names that merely contain "..".
fn is_safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// Resolve `rel` to an **existing** file strictly inside `dir`. The path is
/// component-validated (no traversal) and both ends are canonicalized, so a
/// symlink inside `dir` cannot be used to escape it. Returns the canonical
/// absolute path, or `None` if invalid / outside / missing.
pub fn resolve_existing(dir: &str, rel: &str) -> Option<PathBuf> {
    if dir.is_empty() || !is_safe_rel(rel) {
        return None;
    }
    let base = std::fs::canonicalize(dir).ok()?;
    let abs = std::fs::canonicalize(base.join(rel)).ok()?;
    abs.starts_with(&base).then_some(abs)
}

/// Resolve `rel` to a (possibly **new**) file strictly inside `dir`, creating its
/// parent directory. Component-validated (no traversal); the **canonicalized
/// parent** is verified to stay within the base, so a symlinked sub-directory
/// cannot redirect the write outside `dir`. Returns the destination path (which
/// may not exist yet), or `None` if invalid / outside.
pub fn resolve_new(dir: &str, rel: &str) -> Option<PathBuf> {
    if dir.is_empty() || !is_safe_rel(rel) {
        return None;
    }
    let base = std::fs::canonicalize(dir).ok()?;
    let dest = base.join(rel);
    let parent = dest.parent()?;
    std::fs::create_dir_all(parent).ok()?;
    std::fs::canonicalize(parent)
        .ok()?
        .starts_with(&base)
        .then_some(dest)
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
