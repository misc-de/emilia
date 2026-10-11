//! Playing on network speakers ("cast"): UPnP/DLNA MediaRenderers, Sonos
//! and Google Cast.
//!
//! The renderer pulls the audio itself over HTTP — Emilia only tells it what
//! to fetch (SOAP `SetAVTransportURI` + `Play`) and serves the item from its
//! LAN server ([`serve`]). While a session runs, the [`crate::core::player`]
//! routes every transport call (load, pause, seek, volume, position) to the
//! session instead of its GStreamer decks, so the whole app — queue,
//! podcasts, radio, YouTube, MPRIS — plays on the speaker without knowing.
//!
//! * [`discover`] — SSDP search + device description (on demand only).
//! * [`upnp`] — description parser, SOAP calls, DIDL-Lite metadata.
//! * [`serve`] — the LAN HTTP server (files, relays, transcodes).
//! * [`session`] — the worker thread driving one renderer.
//!
//! Pure Rust on blocking threads, no tokio: the phone build has it too.

pub mod backend;
pub mod castv2;
pub mod discover;
pub mod dmr;
pub mod mdns;
pub mod serve;
pub mod session;
pub mod upnp;

use std::sync::Mutex;

pub use discover::discover;
pub use session::{Event, Session, Transport};
pub use upnp::Meta;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Sonos,
    Dlna,
    /// Google Cast (Chromecast, Nest speakers, Cast-enabled TVs/speakers).
    Cast,
}

/// A UPnP service of a renderer: its exact type (with the version the device
/// implements — the SOAP action header must name it) and control endpoint.
#[derive(Debug, Clone, PartialEq)]
pub struct Service {
    pub service_type: String,
    pub control_url: String,
}

/// A renderer found on the LAN.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    /// UDN without the `uuid:` prefix (Sonos: `RINCON_…`).
    pub id: String,
    pub name: String,
    /// Manufacturer + model, for the picker's subtitle.
    pub model: String,
    pub kind: Kind,
    pub location: String,
    pub host: String,
    /// Cast: the CASTV2 port (8009, other ports for speaker groups).
    pub port: u16,
    /// UPnP only (Cast devices have none of the services).
    pub avt: Option<Service>,
    pub rc: Option<Service>,
    pub cm: Option<Service>,
    /// Sonos: group volume on the coordinator.
    pub grc: Option<Service>,
    /// Sonos: household topology (to fold speakers into room groups).
    pub zgt: Option<Service>,
}

/// What is playing, as the app last told the lock screen. The player reads it
/// when it hands an item to a renderer, so the speaker's display (and the
/// Sonos app) show title, artist and cover instead of a file name.
#[derive(Debug, Clone, Default)]
pub struct NowPlaying {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub length_ms: Option<i64>,
    pub art_path: Option<String>,
}

static NOW_PLAYING: Mutex<Option<NowPlaying>> = Mutex::new(None);

/// Records the current track's metadata (fed from the MPRIS update, which
/// every playback path already calls).
pub fn set_now_playing(np: NowPlaying) {
    if let Ok(mut g) = NOW_PLAYING.lock() {
        *g = Some(np);
    }
}

pub fn now_playing() -> Option<NowPlaying> {
    NOW_PLAYING.lock().ok()?.clone()
}
