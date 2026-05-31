//! Geräte-zu-Gerät-Synchronisierung im LAN (Port der DrivePulse-Logik).
//!
//! Ablauf: ein Gerät startet einen HTTPS-Server ([`server`]) und zeigt einen
//! QR-Code ([`qr`]); das andere scannt ihn ([`scanner`]), pinnt den Zertifikat-
//! Fingerprint ([`crypto`]) und koppelt sich ([`client`]). Danach werden
//! Bibliotheks-Metadaten und Audiodateien übertragen ([`data`], [`protocol`]).

pub mod client;
pub mod crypto;
pub mod data;
pub mod protocol;
pub mod qr;
pub mod scanner;
pub mod server;

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Bevorzugter TCP-Port des Sync-Servers (weicht bei Belegung aus).
pub const PORT: u16 = 8765;
/// Anzahl ausweichender Ports, falls [`PORT`] belegt ist.
pub const PORT_ATTEMPTS: u16 = 10;
/// Sitzung läuft ab, wenn der Client länger nichts anfragt.
pub const SESSION_TIMEOUT: Duration = Duration::from_secs(30);
/// Gültigkeitsdauer des QR-Codes (zugleich die Wartezeit auf eine Kopplung).
pub const QR_TTL: Duration = Duration::from_secs(120);
/// Annahmeschleife: maximale Blockierzeit, danach wird das Stop-Flag geprüft.
pub const ACCEPT_POLL: Duration = Duration::from_millis(500);
/// Abbruch nach so vielen fehlgeschlagenen Kopplungsversuchen (Bruteforce-Schutz).
pub const MAX_FAILED_PAIR: u32 = 5;

/// Ereignisse aus dem Server-Thread bzw. dem Client-Worker an die UI.
///
/// Muss `Debug + Send` sein (wird in `Cmd` transportiert).
#[derive(Debug)]
pub enum SyncEvent {
    /// Server läuft; `pair_url` ist als QR-Code anzuzeigen.
    ServerReady {
        pair_url: String,
        host: String,
        port: u16,
    },
    /// Server wurde beendet (Timeout, Stop oder Fehler nach Start).
    ServerStopped,
    /// Ein anderes Gerät hat sich erfolgreich gekoppelt.
    PeerPaired { peer_name: String },
    /// Verbindung wurde getrennt (durch Gegenstelle oder Timeout).
    PeerDisconnected,
    /// Eingehender Metadaten-Import wurde eingespielt (Server-Seite).
    ImportReceived { stats: ImportStats },
    /// Metadaten wurden an die Gegenstelle gesendet.
    ExportSent,
    /// Fortschritt einer Dateiübertragung.
    FileProgress { done: u64, total: u64, name: String },
    /// Dateiübertragung abgeschlossen.
    TransferDone { files: usize },
    /// Fehler (Anzeige als Toast).
    Error(String),
}

/// Zähler darüber, was bei einem Import angelegt/aktualisiert wurde.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportStats {
    pub favorites: usize,
    pub playlists: usize,
    pub podcasts: usize,
    pub categories: usize,
    pub eq: usize,
    pub files: usize,
}

/// Aktueller Unix-Zeitstempel in Sekunden.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Lokale IPv4-Adresse über einen UDP-„Verbindungsversuch“ (kein echter Traffic).
pub fn local_ip() -> String {
    use std::net::UdpSocket;
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|sock| {
            sock.connect("8.8.8.8:80")?;
            Ok(sock.local_addr()?.ip().to_string())
        })
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// Anzeigename dieses Geräts (Hostname, sonst „Emilia“).
pub fn default_device_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "Emilia".to_string())
}
