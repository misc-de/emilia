//! Emilia as a network speaker: a UPnP/DLNA **MediaRenderer** that other apps
//! (BubbleUPnP, Home Assistant, Windows "Play to", another Emilia) can send
//! music to.
//!
//! Three parts, all on blocking threads:
//! * SSDP — answers searches on 239.255.255.250:1900 and announces itself
//!   (`ssdp:alive` on start and every few minutes, `ssdp:byebye` on stop);
//! * HTTP — the device description, the service descriptions (SCPD), SOAP
//!   control and GENA event subscriptions;
//! * a notifier that sends `LastChange` events to subscribers whenever the
//!   published state changes.
//!
//! The renderer itself does not play anything: commands go to the app as
//! [`DmrCommand`]s, and the app publishes what it is doing via
//! [`DmrHandle::publish`]. Off by default — anyone in the LAN could make the
//! app play a URL while it is on.

use std::collections::HashMap;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::upnp::{self, esc, fmt_hms};
use crate::core::http::{
    ConnPermit, HttpReq, read_body_fully, read_head, write_response, write_status,
};

const SSDP_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const MAX_AGE: u64 = 1800;
const ANNOUNCE_EVERY: Duration = Duration::from_secs(300);
const MAX_CONNECTIONS: usize = 16;
const MAX_BODY: usize = 256 * 1024;
const SUB_TIMEOUT: u64 = 1800;
const MAX_SUBSCRIBERS: usize = 32;

const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const AVT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RC: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const CM: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

/// Formats Emilia's player takes over HTTP (announced as the sink list).
const SINK_MIMES: [&str; 12] = [
    "audio/mpeg",
    "audio/mp3",
    "audio/mp4",
    "audio/x-m4a",
    "audio/aac",
    "audio/flac",
    "audio/x-flac",
    "audio/ogg",
    "application/ogg",
    "audio/opus",
    "audio/wav",
    "audio/x-wav",
];

/// What a controller asked for, handed to the app.
#[derive(Debug, Clone, PartialEq)]
pub enum DmrCommand {
    /// `SetAVTransportURI` followed by `Play` (the app starts on `Play`).
    Load {
        uri: String,
        title: String,
        artist: Option<String>,
    },
    Play,
    Pause,
    Stop,
    Seek(i64),
    /// 0.0–1.0
    Volume(f64),
}

/// The app's playback as the renderer reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DmrStatus {
    /// The controller's item is what plays right now.
    pub ours: bool,
    pub playing: bool,
    /// Something is loaded (paused when not playing).
    pub loaded: bool,
    pub pos_ms: i64,
    pub dur_ms: Option<i64>,
    pub volume: f64,
}

/// Renderer state: what the controller set plus the app's published status.
#[derive(Debug, Default)]
struct State {
    uri: String,
    meta: String,
    /// The URI waits for `Play`.
    pending: bool,
    status: DmrStatus,
    muted_from: Option<f64>,
}

impl State {
    fn transport(&self) -> &'static str {
        if self.uri.is_empty() {
            "NO_MEDIA_PRESENT"
        } else if self.pending || !self.status.ours || !self.status.loaded {
            "STOPPED"
        } else if self.status.playing {
            "PLAYING"
        } else {
            "PAUSED_PLAYBACK"
        }
    }

    fn rel_time(&self) -> String {
        if self.status.ours && !self.pending {
            fmt_hms(self.status.pos_ms)
        } else {
            fmt_hms(0)
        }
    }

    fn duration(&self) -> String {
        match self.status.dur_ms.filter(|_| self.status.ours) {
            Some(d) => fmt_hms(d),
            None => fmt_hms(0),
        }
    }

    fn volume(&self) -> u32 {
        (self.status.volume.clamp(0.0, 1.0) * 100.0).round() as u32
    }
}

struct Subscriber {
    sid: String,
    service: &'static str,
    callbacks: Vec<String>,
    seq: u32,
    expires: Instant,
}

struct Shared {
    name: String,
    udn: String,
    port: u16,
    state: Mutex<State>,
    subs: Mutex<Vec<Subscriber>>,
    commands: async_channel::Sender<DmrCommand>,
    stop: AtomicBool,
}

/// A running renderer. Dropping it says goodbye on the LAN and stops all
/// threads.
pub struct DmrHandle {
    shared: Arc<Shared>,
}

impl DmrHandle {
    /// Starts the renderer under `name`. Commands arrive on the returned
    /// receiver. `udn_seed` keeps the device id stable across starts, so
    /// controllers recognise the same speaker again.
    pub fn start(
        name: &str,
        udn_seed: &str,
    ) -> Result<(Self, async_channel::Receiver<DmrCommand>)> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        let port = listener.local_addr()?.port();
        let (tx, rx) = async_channel::unbounded();
        let shared = Arc::new(Shared {
            name: name.to_string(),
            udn: format!("uuid:{}", stable_uuid(udn_seed)),
            port,
            state: Mutex::new(State {
                status: DmrStatus {
                    volume: 1.0,
                    ..Default::default()
                },
                ..Default::default()
            }),
            subs: Mutex::new(Vec::new()),
            commands: tx,
            stop: AtomicBool::new(false),
        });
        let ssdp = ssdp_socket()?;
        {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("dmr-http".into())
                .spawn(move || http_loop(listener, shared))?;
        }
        {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("dmr-ssdp".into())
                .spawn(move || ssdp_loop(ssdp, shared))?;
        }
        {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("dmr-notify".into())
                .spawn(move || notify_loop(shared))?;
        }
        tracing::info!("DLNA renderer \"{name}\" on port {port}");
        Ok((Self { shared }, rx))
    }

    /// The app's current playback (call on every change and from the tick).
    pub fn publish(&self, status: DmrStatus) {
        if let Ok(mut s) = self.shared.state.lock() {
            s.status = status;
        }
    }

    /// The renderer's device id (UDN without `uuid:`), as discovery reports it.
    pub fn id(&self) -> &str {
        self.shared.udn.trim_start_matches("uuid:")
    }

    /// The URI the controller loaded, to tell whether it is what plays.
    pub fn current_uri(&self) -> String {
        self.shared
            .state
            .lock()
            .map(|s| s.uri.clone())
            .unwrap_or_default()
    }
}

impl Drop for DmrHandle {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        announce(&self.shared, "ssdp:byebye");
        // Unblock the accept loop.
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, self.shared.port)),
            Duration::from_millis(200),
        );
    }
}

/// A UUID-shaped id derived from `seed` (sha256), stable for the same seed.
fn stable_uuid(seed: &str) -> String {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(format!("emilia-dmr:{seed}").as_bytes());
    let x: String = h.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &x[..8],
        &x[8..12],
        &x[12..16],
        &x[16..20],
        &x[20..32]
    )
}

// --- SSDP ----------------------------------------------------------------------

fn ssdp_socket() -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 1900).into())?;
    s.join_multicast_v4(&SSDP_GROUP, &Ipv4Addr::UNSPECIFIED)?;
    s.set_multicast_ttl_v4(2)?;
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    Ok(s.into())
}

/// The (NT/ST, USN) pairs the renderer answers to / announces.
fn targets(udn: &str) -> Vec<(String, String)> {
    let mut v = vec![
        (
            "upnp:rootdevice".to_string(),
            format!("{udn}::upnp:rootdevice"),
        ),
        (udn.to_string(), udn.to_string()),
    ];
    for t in [DEVICE_TYPE, AVT, RC, CM] {
        v.push((t.to_string(), format!("{udn}::{t}")));
    }
    v
}

fn location(shared: &Shared, ip: &str) -> String {
    format!("http://{ip}:{}/dmr/desc.xml", shared.port)
}

fn server_header() -> String {
    format!("Linux UPnP/1.0 Emilia/{}", env!("CARGO_PKG_VERSION"))
}

/// Our address on the interface the default route uses.
fn local_ip() -> String {
    crate::core::sync::local_ip()
}

fn announce(shared: &Shared, nts: &str) {
    let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return;
    };
    let _ = sock.set_multicast_ttl_v4(2);
    let loc = location(shared, &local_ip());
    for (nt, usn) in targets(&shared.udn) {
        let msg = format!(
            "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n\
             CACHE-CONTROL: max-age={MAX_AGE}\r\nLOCATION: {loc}\r\nNT: {nt}\r\n\
             NTS: {nts}\r\nSERVER: {}\r\nUSN: {usn}\r\n\r\n",
            server_header()
        );
        let _ = sock.send_to(msg.as_bytes(), (SSDP_GROUP, 1900));
    }
}

/// The search target of an `M-SEARCH`, if it is one.
fn search_target(msg: &str) -> Option<String> {
    let mut lines = msg.lines();
    if !lines.next()?.starts_with("M-SEARCH") {
        return None;
    }
    lines.find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("st")
            .then(|| v.trim().to_string())
    })
}

fn ssdp_loop(sock: UdpSocket, shared: Arc<Shared>) {
    announce(&shared, "ssdp:alive");
    let mut last = Instant::now();
    let mut buf = [0u8; 2048];
    while !shared.stop.load(Ordering::Relaxed) {
        if last.elapsed() > ANNOUNCE_EVERY {
            announce(&shared, "ssdp:alive");
            last = Instant::now();
        }
        let Ok((n, from)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let Some(st) = search_target(&String::from_utf8_lossy(&buf[..n])) else {
            continue;
        };
        let all = targets(&shared.udn);
        let matches: Vec<_> = all
            .iter()
            .filter(|(t, _)| st == "ssdp:all" || *t == st)
            .collect();
        if matches.is_empty() {
            continue;
        }
        let ip = reply_ip(from).unwrap_or_else(local_ip);
        let loc = location(&shared, &ip);
        let Ok(out) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
            continue;
        };
        for (t, usn) in matches {
            let msg = format!(
                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age={MAX_AGE}\r\nEXT:\r\n\
                 LOCATION: {loc}\r\nSERVER: {}\r\nST: {t}\r\nUSN: {usn}\r\n\r\n",
                server_header()
            );
            let _ = out.send_to(msg.as_bytes(), from);
        }
    }
}

/// The local address the asker can reach us on.
fn reply_ip(to: SocketAddr) -> Option<String> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect(to).ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

// --- HTTP ----------------------------------------------------------------------

fn http_loop(listener: TcpListener, shared: Arc<Shared>) {
    let active = Arc::new(AtomicUsize::new(0));
    for sock in listener.incoming() {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(mut sock) = sock else { continue };
        let Some(permit) = ConnPermit::acquire(&active, MAX_CONNECTIONS) else {
            write_status(&mut sock, 503);
            continue;
        };
        let shared = shared.clone();
        let _ = std::thread::Builder::new()
            .name("dmr-conn".into())
            .spawn(move || {
                let _permit = permit;
                handle(sock, &shared);
            });
    }
}

fn handle(mut sock: TcpStream, shared: &Shared) {
    let _ = sock.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = sock.set_write_timeout(Some(Duration::from_secs(10)));
    let Ok(mut req) = read_head(&mut sock) else {
        return;
    };
    let service = match req.path.as_str() {
        p if p.starts_with("/dmr/avt") => Some(AVT),
        p if p.starts_with("/dmr/rc") => Some(RC),
        p if p.starts_with("/dmr/cm") => Some(CM),
        _ => None,
    };
    match (req.method.as_str(), req.path.as_str()) {
        ("GET" | "HEAD", "/dmr/desc.xml") => xml(&mut sock, &description(shared)),
        ("GET" | "HEAD", "/dmr/avt.xml") => xml(&mut sock, &scpd(AVT_ACTIONS, AVT_VARS)),
        ("GET" | "HEAD", "/dmr/rc.xml") => xml(&mut sock, &scpd(RC_ACTIONS, RC_VARS)),
        ("GET" | "HEAD", "/dmr/cm.xml") => xml(&mut sock, &scpd(CM_ACTIONS, CM_VARS)),
        ("POST", p) if p.ends_with("/ctl") => {
            if read_body_fully(&mut sock, &mut req, MAX_BODY).is_err() {
                return;
            }
            let Some(service) = service else {
                write_status(&mut sock, 404);
                return;
            };
            soap_reply(&mut sock, shared, service, &req);
        }
        ("SUBSCRIBE", p) if p.ends_with("/evt") => match service {
            Some(service) => subscribe(&mut sock, shared, service, &req),
            None => write_status(&mut sock, 404),
        },
        ("UNSUBSCRIBE", _) => {
            if let (Some(sid), Ok(mut subs)) = (req.header("SID"), shared.subs.lock()) {
                subs.retain(|s| s.sid != sid);
            }
            write_status(&mut sock, 200);
        }
        _ => write_status(&mut sock, 404),
    }
}

fn xml(out: &mut TcpStream, body: &str) {
    write_response(out, 200, "text/xml; charset=\"utf-8\"", body.as_bytes());
}

fn description(shared: &Shared) -> String {
    let svc = |t: &str, id: &str, key: &str| {
        format!(
            "<service><serviceType>{t}</serviceType><serviceId>urn:upnp-org:serviceId:{id}</serviceId>\
             <SCPDURL>/dmr/{key}.xml</SCPDURL><controlURL>/dmr/{key}/ctl</controlURL>\
             <eventSubURL>/dmr/{key}/evt</eventSubURL></service>"
        )
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <root xmlns=\"urn:schemas-upnp-org:device-1-0\" xmlns:dlna=\"urn:schemas-dlna-org:device-1-0\">\
         <specVersion><major>1</major><minor>0</minor></specVersion><device>\
         <deviceType>{DEVICE_TYPE}</deviceType><friendlyName>{}</friendlyName>\
         <manufacturer>Emilia</manufacturer><manufacturerURL>https://github.com/misc-de/emilia</manufacturerURL>\
         <modelName>Emilia</modelName><modelNumber>{}</modelNumber>\
         <dlna:X_DLNADOC>DMR-1.50</dlna:X_DLNADOC><UDN>{}</UDN><serviceList>{}{}{}</serviceList>\
         </device></root>",
        esc(&shared.name),
        env!("CARGO_PKG_VERSION"),
        shared.udn,
        svc(AVT, "AVTransport", "avt"),
        svc(RC, "RenderingControl", "rc"),
        svc(CM, "ConnectionManager", "cm"),
    )
}

// --- SCPD ----------------------------------------------------------------------

/// (action, [(argument, "in"/"out", related state variable)])
type Actions = &'static [(
    &'static str,
    &'static [(&'static str, &'static str, &'static str)],
)];
/// (state variable, data type, sends events)
type Vars = &'static [(&'static str, &'static str, bool)];

const AVT_ACTIONS: Actions = &[
    (
        "SetAVTransportURI",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("CurrentURI", "in", "AVTransportURI"),
            ("CurrentURIMetaData", "in", "AVTransportURIMetaData"),
        ],
    ),
    (
        "GetMediaInfo",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("NrTracks", "out", "NumberOfTracks"),
            ("MediaDuration", "out", "CurrentMediaDuration"),
            ("CurrentURI", "out", "AVTransportURI"),
            ("CurrentURIMetaData", "out", "AVTransportURIMetaData"),
            ("NextURI", "out", "NextAVTransportURI"),
            ("NextURIMetaData", "out", "NextAVTransportURIMetaData"),
            ("PlayMedium", "out", "PlaybackStorageMedium"),
            ("RecordMedium", "out", "RecordStorageMedium"),
            ("WriteStatus", "out", "RecordMediumWriteStatus"),
        ],
    ),
    (
        "GetTransportInfo",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("CurrentTransportState", "out", "TransportState"),
            ("CurrentTransportStatus", "out", "TransportStatus"),
            ("CurrentSpeed", "out", "TransportPlaySpeed"),
        ],
    ),
    (
        "GetPositionInfo",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Track", "out", "CurrentTrack"),
            ("TrackDuration", "out", "CurrentTrackDuration"),
            ("TrackMetaData", "out", "CurrentTrackMetaData"),
            ("TrackURI", "out", "CurrentTrackURI"),
            ("RelTime", "out", "RelativeTimePosition"),
            ("AbsTime", "out", "AbsoluteTimePosition"),
            ("RelCount", "out", "RelativeCounterPosition"),
            ("AbsCount", "out", "AbsoluteCounterPosition"),
        ],
    ),
    (
        "GetDeviceCapabilities",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("PlayMedia", "out", "PossiblePlaybackStorageMedia"),
            ("RecMedia", "out", "PossibleRecordStorageMedia"),
            ("RecQualityModes", "out", "PossibleRecordQualityModes"),
        ],
    ),
    (
        "GetTransportSettings",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("PlayMode", "out", "CurrentPlayMode"),
            ("RecQualityMode", "out", "CurrentRecordQualityMode"),
        ],
    ),
    (
        "GetCurrentTransportActions",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Actions", "out", "CurrentTransportActions"),
        ],
    ),
    ("Stop", &[("InstanceID", "in", "A_ARG_TYPE_InstanceID")]),
    (
        "Play",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Speed", "in", "TransportPlaySpeed"),
        ],
    ),
    ("Pause", &[("InstanceID", "in", "A_ARG_TYPE_InstanceID")]),
    (
        "Seek",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Unit", "in", "A_ARG_TYPE_SeekMode"),
            ("Target", "in", "A_ARG_TYPE_SeekTarget"),
        ],
    ),
    ("Next", &[("InstanceID", "in", "A_ARG_TYPE_InstanceID")]),
    ("Previous", &[("InstanceID", "in", "A_ARG_TYPE_InstanceID")]),
];

const AVT_VARS: Vars = &[
    ("TransportState", "string", false),
    ("TransportStatus", "string", false),
    ("PlaybackStorageMedium", "string", false),
    ("RecordStorageMedium", "string", false),
    ("PossiblePlaybackStorageMedia", "string", false),
    ("PossibleRecordStorageMedia", "string", false),
    ("CurrentPlayMode", "string", false),
    ("TransportPlaySpeed", "string", false),
    ("RecordMediumWriteStatus", "string", false),
    ("CurrentRecordQualityMode", "string", false),
    ("PossibleRecordQualityModes", "string", false),
    ("NumberOfTracks", "ui4", false),
    ("CurrentTrack", "ui4", false),
    ("CurrentTrackDuration", "string", false),
    ("CurrentMediaDuration", "string", false),
    ("CurrentTrackMetaData", "string", false),
    ("CurrentTrackURI", "string", false),
    ("AVTransportURI", "string", false),
    ("AVTransportURIMetaData", "string", false),
    ("NextAVTransportURI", "string", false),
    ("NextAVTransportURIMetaData", "string", false),
    ("RelativeTimePosition", "string", false),
    ("AbsoluteTimePosition", "string", false),
    ("RelativeCounterPosition", "i4", false),
    ("AbsoluteCounterPosition", "i4", false),
    ("CurrentTransportActions", "string", false),
    ("LastChange", "string", true),
    ("A_ARG_TYPE_SeekMode", "string", false),
    ("A_ARG_TYPE_SeekTarget", "string", false),
    ("A_ARG_TYPE_InstanceID", "ui4", false),
];

const RC_ACTIONS: Actions = &[
    (
        "ListPresets",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("CurrentPresetNameList", "out", "PresetNameList"),
        ],
    ),
    (
        "SelectPreset",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("PresetName", "in", "A_ARG_TYPE_PresetName"),
        ],
    ),
    (
        "GetVolume",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Channel", "in", "A_ARG_TYPE_Channel"),
            ("CurrentVolume", "out", "Volume"),
        ],
    ),
    (
        "SetVolume",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Channel", "in", "A_ARG_TYPE_Channel"),
            ("DesiredVolume", "in", "Volume"),
        ],
    ),
    (
        "GetMute",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Channel", "in", "A_ARG_TYPE_Channel"),
            ("CurrentMute", "out", "Mute"),
        ],
    ),
    (
        "SetMute",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Channel", "in", "A_ARG_TYPE_Channel"),
            ("DesiredMute", "in", "Mute"),
        ],
    ),
];

const RC_VARS: Vars = &[
    ("PresetNameList", "string", false),
    ("Volume", "ui2", false),
    ("Mute", "boolean", false),
    ("LastChange", "string", true),
    ("A_ARG_TYPE_Channel", "string", false),
    ("A_ARG_TYPE_InstanceID", "ui4", false),
    ("A_ARG_TYPE_PresetName", "string", false),
];

const CM_ACTIONS: Actions = &[
    (
        "GetProtocolInfo",
        &[
            ("Source", "out", "SourceProtocolInfo"),
            ("Sink", "out", "SinkProtocolInfo"),
        ],
    ),
    (
        "GetCurrentConnectionIDs",
        &[("ConnectionIDs", "out", "CurrentConnectionIDs")],
    ),
    (
        "GetCurrentConnectionInfo",
        &[
            ("ConnectionID", "in", "A_ARG_TYPE_ConnectionID"),
            ("RcsID", "out", "A_ARG_TYPE_RcsID"),
            ("AVTransportID", "out", "A_ARG_TYPE_AVTransportID"),
            ("ProtocolInfo", "out", "A_ARG_TYPE_ProtocolInfo"),
            (
                "PeerConnectionManager",
                "out",
                "A_ARG_TYPE_ConnectionManager",
            ),
            ("PeerConnectionID", "out", "A_ARG_TYPE_ConnectionID"),
            ("Direction", "out", "A_ARG_TYPE_Direction"),
            ("Status", "out", "A_ARG_TYPE_ConnectionStatus"),
        ],
    ),
];

const CM_VARS: Vars = &[
    ("SourceProtocolInfo", "string", true),
    ("SinkProtocolInfo", "string", true),
    ("CurrentConnectionIDs", "string", true),
    ("A_ARG_TYPE_ConnectionStatus", "string", false),
    ("A_ARG_TYPE_ConnectionManager", "string", false),
    ("A_ARG_TYPE_Direction", "string", false),
    ("A_ARG_TYPE_ProtocolInfo", "string", false),
    ("A_ARG_TYPE_ConnectionID", "i4", false),
    ("A_ARG_TYPE_AVTransportID", "i4", false),
    ("A_ARG_TYPE_RcsID", "i4", false),
];

fn scpd(actions: Actions, vars: Vars) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><scpd xmlns=\"urn:schemas-upnp-org:service-1-0\">\
         <specVersion><major>1</major><minor>0</minor></specVersion><actionList>",
    );
    for (name, args) in actions {
        s.push_str(&format!("<action><name>{name}</name><argumentList>"));
        for (arg, dir, var) in *args {
            s.push_str(&format!(
                "<argument><name>{arg}</name><direction>{dir}</direction>\
                 <relatedStateVariable>{var}</relatedStateVariable></argument>"
            ));
        }
        s.push_str("</argumentList></action>");
    }
    s.push_str("</actionList><serviceStateTable>");
    for (name, ty, events) in vars {
        s.push_str(&format!(
            "<stateVariable sendEvents=\"{}\"><name>{name}</name><dataType>{ty}</dataType></stateVariable>",
            if *events { "yes" } else { "no" }
        ));
    }
    s.push_str("</serviceStateTable></scpd>");
    s
}

// --- SOAP ----------------------------------------------------------------------

fn sink_protocol_info() -> String {
    SINK_MIMES
        .iter()
        .map(|m| format!("http-get:*:{m}:*"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Title / artist of a DIDL-Lite item.
fn didl_title(meta: &str, uri: &str) -> (String, Option<String>) {
    let v = upnp::leaf_values(meta);
    let artist = v
        .get("artist")
        .or_else(|| v.get("creator"))
        .filter(|s| !s.trim().is_empty())
        .cloned();
    let title = v
        .get("title")
        .filter(|s| !s.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| {
            uri.split(['?', '#'])
                .next()
                .and_then(|p| p.rsplit('/').next())
                .filter(|s| !s.is_empty())
                .unwrap_or(uri)
                .to_string()
        });
    (title, artist)
}

/// Runs one SOAP action. `Ok` carries the (escaped) output arguments.
fn run_action(
    shared: &Shared,
    service: &str,
    action: &str,
    args: &HashMap<String, String>,
) -> Result<String, u16> {
    let arg = |k: &str| args.get(k).map(String::as_str).unwrap_or("");
    let send = |c: DmrCommand| {
        let _ = shared.commands.send_blocking(c);
    };
    let mut st = shared.state.lock().map_err(|_| 501u16)?;
    let out = match (service, action) {
        (AVT, "SetAVTransportURI") => {
            let uri = arg("CurrentURI").trim().to_string();
            if !(uri.starts_with("http://") || uri.starts_with("https://")) {
                return Err(714); // Illegal MIME-type / unsupported resource
            }
            st.uri = uri;
            st.meta = arg("CurrentURIMetaData").to_string();
            st.pending = true;
            String::new()
        }
        (AVT, "Play") => {
            if st.uri.is_empty() {
                return Err(701); // Transition not available
            }
            if st.pending || !st.status.ours {
                st.pending = false;
                let (title, artist) = didl_title(&st.meta, &st.uri);
                let uri = st.uri.clone();
                drop(st);
                send(DmrCommand::Load { uri, title, artist });
            } else {
                drop(st);
                send(DmrCommand::Play);
            }
            return Ok(String::new());
        }
        (AVT, "Pause") => {
            drop(st);
            send(DmrCommand::Pause);
            return Ok(String::new());
        }
        (AVT, "Stop") => {
            st.pending = true;
            drop(st);
            send(DmrCommand::Stop);
            return Ok(String::new());
        }
        (AVT, "Seek") => {
            let target = arg("Target");
            let ms = upnp::parse_hms(target).ok_or(711u16)?; // Illegal seek target
            drop(st);
            send(DmrCommand::Seek(ms));
            return Ok(String::new());
        }
        (AVT, "Next" | "Previous") => String::new(),
        (AVT, "GetTransportInfo") => format!(
            "<CurrentTransportState>{}</CurrentTransportState>\
             <CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>",
            st.transport()
        ),
        (AVT, "GetPositionInfo") => format!(
            "<Track>{}</Track><TrackDuration>{}</TrackDuration><TrackMetaData>{}</TrackMetaData>\
             <TrackURI>{}</TrackURI><RelTime>{}</RelTime><AbsTime>{}</AbsTime>\
             <RelCount>2147483647</RelCount><AbsCount>2147483647</AbsCount>",
            u8::from(!st.uri.is_empty()),
            st.duration(),
            esc(&st.meta),
            esc(&st.uri),
            st.rel_time(),
            st.rel_time(),
        ),
        (AVT, "GetMediaInfo") => format!(
            "<NrTracks>{}</NrTracks><MediaDuration>{}</MediaDuration><CurrentURI>{}</CurrentURI>\
             <CurrentURIMetaData>{}</CurrentURIMetaData><NextURI></NextURI><NextURIMetaData></NextURIMetaData>\
             <PlayMedium>NETWORK</PlayMedium><RecordMedium>NOT_IMPLEMENTED</RecordMedium>\
             <WriteStatus>NOT_IMPLEMENTED</WriteStatus>",
            u8::from(!st.uri.is_empty()),
            st.duration(),
            esc(&st.uri),
            esc(&st.meta),
        ),
        (AVT, "GetDeviceCapabilities") => "<PlayMedia>NETWORK</PlayMedia>\
             <RecMedia>NOT_IMPLEMENTED</RecMedia><RecQualityModes>NOT_IMPLEMENTED</RecQualityModes>"
            .to_string(),
        (AVT, "GetTransportSettings") => {
            "<PlayMode>NORMAL</PlayMode><RecQualityMode>NOT_IMPLEMENTED</RecQualityMode>"
                .to_string()
        }
        (AVT, "GetCurrentTransportActions") => {
            let actions = match st.transport() {
                "PLAYING" => "Pause,Stop,Seek",
                "PAUSED_PLAYBACK" => "Play,Stop,Seek",
                "STOPPED" => "Play",
                _ => "",
            };
            format!("<Actions>{actions}</Actions>")
        }
        (RC, "GetVolume") => format!("<CurrentVolume>{}</CurrentVolume>", st.volume()),
        (RC, "SetVolume") => {
            let v: u32 = arg("DesiredVolume").trim().parse().map_err(|_| 402u16)?;
            let vol = f64::from(v.min(100)) / 100.0;
            st.status.volume = vol;
            st.muted_from = None;
            drop(st);
            send(DmrCommand::Volume(vol));
            return Ok(String::new());
        }
        (RC, "GetMute") => format!(
            "<CurrentMute>{}</CurrentMute>",
            u8::from(st.muted_from.is_some())
        ),
        (RC, "SetMute") => {
            let mute = matches!(arg("DesiredMute").trim(), "1" | "true" | "True");
            let vol = if mute {
                if st.muted_from.is_none() {
                    st.muted_from = Some(st.status.volume);
                }
                0.0
            } else {
                st.muted_from.take().unwrap_or(st.status.volume)
            };
            st.status.volume = vol;
            drop(st);
            send(DmrCommand::Volume(vol));
            return Ok(String::new());
        }
        (RC, "ListPresets") => {
            "<CurrentPresetNameList>FactoryDefaults</CurrentPresetNameList>".to_string()
        }
        (RC, "SelectPreset") => String::new(),
        (CM, "GetProtocolInfo") => {
            format!("<Source></Source><Sink>{}</Sink>", sink_protocol_info())
        }
        (CM, "GetCurrentConnectionIDs") => "<ConnectionIDs>0</ConnectionIDs>".to_string(),
        (CM, "GetCurrentConnectionInfo") => "<RcsID>0</RcsID><AVTransportID>0</AVTransportID>\
             <ProtocolInfo></ProtocolInfo><PeerConnectionManager></PeerConnectionManager>\
             <PeerConnectionID>-1</PeerConnectionID><Direction>Input</Direction><Status>OK</Status>"
            .to_string(),
        _ => return Err(401), // Invalid action
    };
    Ok(out)
}

fn soap_reply(out: &mut TcpStream, shared: &Shared, service: &'static str, req: &HttpReq) {
    let action = req
        .header("SOAPACTION")
        .and_then(|h| {
            h.trim()
                .trim_matches('"')
                .rsplit_once('#')
                .map(|(_, a)| a.to_string())
        })
        .unwrap_or_default();
    let body = String::from_utf8_lossy(&req.body);
    let args = upnp::leaf_values(&body);
    match run_action(shared, service, &action, &args) {
        Ok(inner) => {
            let x = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
                 <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
                 s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
                 <u:{action}Response xmlns:u=\"{service}\">{inner}</u:{action}Response></s:Body></s:Envelope>"
            );
            xml(out, &x);
        }
        Err(code) => {
            tracing::debug!("DLNA renderer: {action} → UPnP error {code}");
            let x = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
                 <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
                 s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><s:Fault>\
                 <faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail>\
                 <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode>\
                 <errorDescription>Action failed</errorDescription></UPnPError></detail></s:Fault>\
                 </s:Body></s:Envelope>"
            );
            write_response(out, 500, "text/xml; charset=\"utf-8\"", x.as_bytes());
        }
    }
}

// --- GENA ----------------------------------------------------------------------

fn subscribe(out: &mut TcpStream, shared: &Shared, service: &'static str, req: &HttpReq) {
    let Ok(mut subs) = shared.subs.lock() else {
        write_status(out, 500);
        return;
    };
    let now = Instant::now();
    subs.retain(|s| s.expires > now);
    let sid = if let Some(sid) = req.header("SID") {
        // Renewal.
        let Some(s) = subs.iter_mut().find(|s| s.sid == sid) else {
            write_status(out, 412);
            return;
        };
        s.expires = now + Duration::from_secs(SUB_TIMEOUT);
        sid.to_string()
    } else {
        let callbacks: Vec<String> = req
            .header("CALLBACK")
            .unwrap_or("")
            .split(['<', '>'])
            .map(str::trim)
            .filter(|u| u.starts_with("http://"))
            .map(str::to_string)
            .collect();
        if callbacks.is_empty() || subs.len() >= MAX_SUBSCRIBERS {
            write_status(out, 412);
            return;
        }
        let sid = format!(
            "uuid:{}",
            stable_uuid(&format!("{:?}{}", now, callbacks[0]))
        );
        subs.push(Subscriber {
            sid: sid.clone(),
            service,
            callbacks,
            seq: 0,
            expires: now + Duration::from_secs(SUB_TIMEOUT),
        });
        sid
    };
    drop(subs);
    let head = format!(
        "HTTP/1.1 200 OK\r\nSID: {sid}\r\nTIMEOUT: Second-{SUB_TIMEOUT}\r\nSERVER: {}\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n",
        server_header()
    );
    let _ = out.write_all(head.as_bytes());
    let _ = out.flush();
    // The initial event (SEQ 0) carries the full state; sent by the notifier
    // on its next round (it sees `seq == 0`).
}

/// The event body of `service` for the current state.
fn event_body(service: &str, st: &State) -> String {
    let props = match service {
        AVT => {
            let last = format!(
                "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\"><InstanceID val=\"0\">\
                 <TransportState val=\"{}\"/><TransportStatus val=\"OK\"/>\
                 <CurrentTrackURI val=\"{uri}\"/><AVTransportURI val=\"{uri}\"/>\
                 <CurrentTrackDuration val=\"{dur}\"/><CurrentMediaDuration val=\"{dur}\"/>\
                 <CurrentTrackMetaData val=\"{meta}\"/><AVTransportURIMetaData val=\"{meta}\"/>\
                 <NumberOfTracks val=\"{n}\"/><CurrentTrack val=\"{n}\"/></InstanceID></Event>",
                st.transport(),
                uri = esc(&st.uri),
                dur = st.duration(),
                meta = esc(&st.meta),
                n = u8::from(!st.uri.is_empty()),
            );
            format!(
                "<e:property><LastChange>{}</LastChange></e:property>",
                esc(&last)
            )
        }
        RC => {
            let last = format!(
                "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/RCS/\"><InstanceID val=\"0\">\
                 <Volume channel=\"Master\" val=\"{}\"/><Mute channel=\"Master\" val=\"{}\"/>\
                 </InstanceID></Event>",
                st.volume(),
                u8::from(st.muted_from.is_some())
            );
            format!(
                "<e:property><LastChange>{}</LastChange></e:property>",
                esc(&last)
            )
        }
        _ => format!(
            "<e:property><SourceProtocolInfo></SourceProtocolInfo></e:property>\
             <e:property><SinkProtocolInfo>{}</SinkProtocolInfo></e:property>\
             <e:property><CurrentConnectionIDs>0</CurrentConnectionIDs></e:property>",
            sink_protocol_info()
        ),
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">{props}</e:propertyset>"
    )
}

/// Polls the state and sends events: the initial one to new subscribers,
/// then one per change (position ticks are not events in UPnP).
fn notify_loop(shared: Arc<Shared>) {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(3))
        .build();
    let mut last: HashMap<&'static str, String> = HashMap::new();
    while !shared.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(400));
        let bodies: HashMap<&'static str, String> = match shared.state.lock() {
            Ok(st) => [AVT, RC, CM]
                .into_iter()
                .map(|s| (s, event_body(s, &st)))
                .collect(),
            Err(_) => continue,
        };
        let changed: Vec<&'static str> = bodies
            .iter()
            .filter(|(s, b)| last.get(*s) != Some(*b))
            .map(|(s, _)| *s)
            .collect();
        // Collect the deliveries under the lock, send them outside it.
        let mut deliveries = Vec::new();
        if let Ok(mut subs) = shared.subs.lock() {
            let now = Instant::now();
            subs.retain(|s| s.expires > now);
            for s in subs.iter_mut() {
                if s.seq == 0 || changed.contains(&s.service) {
                    deliveries.push((
                        s.callbacks.clone(),
                        s.sid.clone(),
                        s.seq,
                        bodies[s.service].clone(),
                    ));
                    s.seq = s.seq.wrapping_add(1).max(1);
                }
            }
        }
        last = bodies;
        for (callbacks, sid, seq, body) in deliveries {
            for cb in callbacks {
                let ok = agent
                    .request("NOTIFY", &cb)
                    .set("Content-Type", "text/xml; charset=\"utf-8\"")
                    .set("NT", "upnp:event")
                    .set("NTS", "upnp:propchange")
                    .set("SID", &sid)
                    .set("SEQ", &seq.to_string())
                    .send_string(&body)
                    .is_ok();
                if ok {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_is_stable_and_shaped() {
        let a = stable_uuid("x");
        assert_eq!(a, stable_uuid("x"));
        assert_ne!(a, stable_uuid("y"));
        assert_eq!(a.len(), 36);
        assert_eq!(a.matches('-').count(), 4);
    }

    #[test]
    fn search_targets_are_read() {
        let m = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\n\
                 st: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
        assert_eq!(search_target(m).as_deref(), Some(DEVICE_TYPE));
        assert_eq!(search_target("NOTIFY * HTTP/1.1\r\nST: x\r\n\r\n"), None);
    }

    #[test]
    fn transport_follows_the_app() {
        let mut st = State::default();
        assert_eq!(st.transport(), "NO_MEDIA_PRESENT");
        st.uri = "http://h/a.mp3".into();
        st.pending = true;
        assert_eq!(st.transport(), "STOPPED");
        st.pending = false;
        st.status = DmrStatus {
            ours: true,
            loaded: true,
            playing: true,
            ..Default::default()
        };
        assert_eq!(st.transport(), "PLAYING");
        st.status.playing = false;
        assert_eq!(st.transport(), "PAUSED_PLAYBACK");
        st.status.ours = false;
        assert_eq!(st.transport(), "STOPPED");
    }

    #[test]
    fn didl_gives_title_and_artist() {
        let meta = "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
                    xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
                    <item><dc:title>Song &amp; Dance</dc:title><upnp:artist>Band</upnp:artist></item></DIDL-Lite>";
        assert_eq!(
            didl_title(meta, "http://h/x.mp3"),
            ("Song & Dance".to_string(), Some("Band".to_string()))
        );
        assert_eq!(
            didl_title("", "http://h/dir/x.mp3?a=1"),
            ("x.mp3".to_string(), None)
        );
    }

    #[test]
    fn scpd_lists_actions_and_variables() {
        let s = scpd(RC_ACTIONS, RC_VARS);
        assert!(s.contains("<name>SetVolume</name>"));
        assert!(s.contains("<stateVariable sendEvents=\"yes\"><name>LastChange</name>"));
        // Every related state variable is declared.
        for (_, args) in AVT_ACTIONS.iter().chain(RC_ACTIONS).chain(CM_ACTIONS) {
            for (_, _, var) in *args {
                let declared = AVT_VARS
                    .iter()
                    .chain(RC_VARS)
                    .chain(CM_VARS)
                    .any(|(n, _, _)| n == var);
                assert!(declared, "{var} not declared");
            }
        }
    }

    #[test]
    fn controller_round_trip() {
        // A real renderer on real sockets, driven by the cast client code.
        let Ok((h, rx)) = DmrHandle::start("Test", "round-trip") else {
            return; // no multicast here (sandboxed CI)
        };
        let agent = upnp::agent();
        let loc = format!("http://127.0.0.1:{}/dmr/desc.xml", h.shared.port);
        let dev = upnp::describe(&agent, &loc).expect("description");
        let avt = dev.avt.clone().expect("AVTransport");
        assert_eq!(dev.name, "Test");
        assert_eq!(dev.id, h.id());
        assert_eq!(
            upnp::transport_info(&agent, &avt).unwrap(),
            "NO_MEDIA_PRESENT"
        );
        let mimes = upnp::sink_mimes(&agent, &dev).unwrap();
        assert!(mimes.contains(&"audio/flac".to_string()));
        let meta = upnp::Meta {
            title: "Song".into(),
            artist: Some("Band".into()),
            ..Default::default()
        };
        let didl = upnp::didl("http://h/a.mp3", "audio/mpeg", &meta, None, false);
        upnp::set_uri(&agent, &avt, "http://h/a.mp3", &didl).unwrap();
        assert_eq!(upnp::transport_info(&agent, &avt).unwrap(), "STOPPED");
        upnp::play(&agent, &avt).unwrap();
        assert_eq!(
            rx.recv_blocking().unwrap(),
            DmrCommand::Load {
                uri: "http://h/a.mp3".into(),
                title: "Song".into(),
                artist: Some("Band".into())
            }
        );
        h.publish(DmrStatus {
            ours: true,
            playing: true,
            loaded: true,
            pos_ms: 61_000,
            dur_ms: Some(180_000),
            volume: 0.5,
        });
        assert_eq!(upnp::transport_info(&agent, &avt).unwrap(), "PLAYING");
        let (pos, dur, uri) = upnp::position_info(&agent, &avt).unwrap();
        assert_eq!(
            (pos, dur, uri.as_str()),
            (Some(61_000), Some(180_000), "http://h/a.mp3")
        );
        assert_eq!(upnp::get_volume(&agent, &dev).unwrap(), 50);
        upnp::set_volume(&agent, &dev, 30).unwrap();
        assert_eq!(rx.recv_blocking().unwrap(), DmrCommand::Volume(0.3));
        upnp::seek(&agent, &avt, 90_000).unwrap();
        assert_eq!(rx.recv_blocking().unwrap(), DmrCommand::Seek(90_000));
        upnp::pause(&agent, &avt).unwrap();
        assert_eq!(rx.recv_blocking().unwrap(), DmrCommand::Pause);
        // A local file is refused: the renderer only fetches over HTTP.
        assert!(upnp::set_uri(&agent, &avt, "file:///etc/passwd", "").is_err());
    }
}
