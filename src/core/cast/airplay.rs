//! Emilia as an **AirPlay 1** speaker (RAOP — "Remote Audio Output
//! Protocol"): iPhones, iPads, Macs and iTunes list it under AirPlay and
//! stream to it.
//!
//! * mDNS announces `<MAC>@<name>._raop._tcp` ([`super::mdns::advertise`]).
//! * An RTSP server takes the session: OPTIONS (answering the
//!   `Apple-Challenge` with the AirPort Express key, which senders verify),
//!   ANNOUNCE (SDP: codec, the AES key wrapped with RSA-OAEP, the IV),
//!   SETUP (our UDP ports), RECORD, SET_PARAMETER (volume, DMAP metadata,
//!   cover), FLUSH (sender paused/seeked) and TEARDOWN.
//! * Audio arrives as RTP over UDP: AES-128-CBC encrypted ALAC frames (or
//!   L16 PCM), decrypted here and played through a GStreamer pipeline
//!   (`appsrc ! avdec_alac ! … ! autoaudiosink`), timed by the RTP
//!   timestamps plus a fixed latency so network jitter is absorbed.
//!
//! AirPlay 2 needs HomeKit pairing and is out of reach; every Apple device
//! still streams to AirPlay 1 receivers.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aes::Aes128;
use aes::cipher::{BlockCipherDecrypt, KeyInit};
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use super::mdns;

/// The AirPort Express private key (public knowledge since 2011; every
/// AirPlay 1 receiver uses it — senders check the challenge response).
const AIRPORT_KEY: &str = include_str!("airport_key.pem");
const SAMPLE_RATE: u64 = 44100;
/// Playback delay behind the sender's clock: absorbs Wi-Fi jitter.
const LATENCY_MS: u64 = 500;
const MAX_REQUEST: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum AirPlayEvent {
    /// A sender started streaming.
    Started,
    Meta {
        title: String,
        artist: Option<String>,
        album: Option<String>,
    },
    /// The stream ended (sender stopped or disconnected).
    Stopped,
}

/// A running AirPlay receiver; dropping it stops it and withdraws the
/// announcement.
pub struct AirPlayHandle {
    stop: Arc<AtomicBool>,
    _advert: mdns::Advert,
    port: u16,
}

impl Drop for AirPlayHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect_timeout(
            &(Ipv4Addr::LOCALHOST, self.port).into(),
            Duration::from_millis(200),
        );
    }
}

struct Ctx {
    key: rsa::RsaPrivateKey,
    mac: [u8; 6],
    events: async_channel::Sender<AirPlayEvent>,
    stop: Arc<AtomicBool>,
    /// Bumped for every new session; an older session stops itself.
    generation: AtomicU64,
}

/// The AirPort key from its PEM (base64 PKCS#1 DER).
fn airport_key() -> Result<rsa::RsaPrivateKey> {
    use rsa::pkcs1::DecodeRsaPrivateKey;
    let b64: String = AIRPORT_KEY
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect();
    let der = base64::engine::general_purpose::STANDARD.decode(b64.trim())?;
    Ok(rsa::RsaPrivateKey::from_pkcs1_der(&der)?)
}

/// A MAC-like id derived from `seed`, stable across starts (senders
/// remember receivers by it).
fn device_mac(seed: &str) -> [u8; 6] {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(format!("emilia-airplay:{seed}").as_bytes());
    let mut m = [0u8; 6];
    m.copy_from_slice(&h[..6]);
    m[0] = (m[0] & 0xfe) | 0x02; // locally administered, unicast
    m
}

impl AirPlayHandle {
    pub fn start(name: &str, seed: &str) -> Result<(Self, async_channel::Receiver<AirPlayEvent>)> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        let port = listener.local_addr()?.port();
        let (tx, rx) = async_channel::unbounded();
        let mac = device_mac(seed);
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = Arc::new(Ctx {
            key: airport_key()?,
            mac,
            events: tx,
            stop: stop.clone(),
            generation: AtomicU64::new(0),
        });
        let mac_hex: String = mac.iter().map(|b| format!("{b:02X}")).collect();
        let txt = [
            "txtvers=1",
            "ch=2",
            "cn=0,1",
            "et=0,1",
            "ek=1",
            "sv=false",
            "da=true",
            "sr=44100",
            "ss=16",
            "pw=false",
            "vn=3",
            "tp=UDP",
            "md=0,1,2",
            "vs=130.14",
            "am=Emilia",
            "sf=0x4",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let host = format!("emilia-{}", mac_hex[6..].to_lowercase());
        let advert = mdns::advertise(
            "_raop._tcp.local",
            &format!("{mac_hex}@{name}"),
            &host,
            port,
            txt,
        )?;
        std::thread::Builder::new()
            .name("airplay-rtsp".into())
            .spawn(move || {
                for sock in listener.incoming() {
                    if ctx.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(sock) = sock else { continue };
                    let ctx = ctx.clone();
                    let _ = std::thread::Builder::new()
                        .name("airplay-conn".into())
                        .spawn(move || {
                            if let Err(e) = serve_connection(sock, &ctx) {
                                tracing::debug!("AirPlay connection ended: {e:#}");
                            }
                        });
                }
            })?;
        tracing::info!("AirPlay receiver \"{name}\" on port {port}");
        Ok((
            Self {
                stop,
                _advert: advert,
                port,
            },
            rx,
        ))
    }
}

// --- RTSP --------------------------------------------------------------------------

struct Request {
    method: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Reads one RTSP request (`METHOD uri RTSP/1.0`, headers, body by
/// Content-Length). `None` when the peer closed.
fn read_request(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Result<Option<Request>> {
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        if buf.len() > MAX_REQUEST {
            bail!("RTSP head too large");
        }
        let n = sock.read(&mut tmp)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let method = lines
        .next()
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or("")
        .to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_REQUEST {
        bail!("RTSP body too large");
    }
    while buf.len() < head_end + len {
        let n = sock.read(&mut tmp)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = buf[head_end..head_end + len].to_vec();
    buf.drain(..head_end + len);
    Ok(Some(Request {
        method,
        headers,
        body,
    }))
}

fn reply(
    sock: &mut TcpStream,
    req: &Request,
    status: &str,
    extra: &[(&str, String)],
) -> Result<()> {
    let mut s = format!(
        "RTSP/1.0 {status}\r\nCSeq: {}\r\nServer: AirTunes/105.1\r\nAudio-Jack-Status: connected; type=analog\r\n",
        req.header("CSeq").unwrap_or("0")
    );
    for (k, v) in extra {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str("Content-Length: 0\r\n\r\n");
    sock.write_all(s.as_bytes())?;
    Ok(())
}

/// The `Apple-Response`: the challenge, our IP and MAC, zero-padded to 32
/// bytes and signed raw (PKCS#1 v1.5, no digest prefix).
fn apple_response(
    key: &rsa::RsaPrivateKey,
    challenge_b64: &str,
    ip: IpAddr,
    mac: &[u8; 6],
) -> Result<String> {
    let challenge = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(challenge_b64.trim().trim_end_matches('='))?;
    let mut data = challenge;
    match ip {
        IpAddr::V4(v4) => data.extend_from_slice(&v4.octets()),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => data.extend_from_slice(&v4.octets()),
            None => data.extend_from_slice(&v6.octets()),
        },
    }
    data.extend_from_slice(mac);
    if data.len() < 32 {
        data.resize(32, 0);
    }
    let sig = key.sign(rsa::Pkcs1v15Sign::new_unprefixed(), &data)?;
    Ok(base64::engine::general_purpose::STANDARD_NO_PAD.encode(sig))
}

/// What ANNOUNCE told us about the stream.
#[derive(Debug, Clone, Default)]
struct Format {
    alac: bool,
    /// ALAC parameters from `a=fmtp:` (frame length, …, sample rate).
    fmtp: Vec<u32>,
    aes_key: Option<[u8; 16]>,
    aes_iv: [u8; 16],
}

fn parse_sdp(sdp: &str, key: &rsa::RsaPrivateKey) -> Result<Format> {
    let mut f = Format::default();
    let b64 = |s: &str| {
        base64::engine::general_purpose::STANDARD_NO_PAD.decode(s.trim().trim_end_matches('='))
    };
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("a=rtpmap:") {
            f.alac = v.contains("AppleLossless");
            if !f.alac && !v.contains("L16") {
                bail!("unsupported AirPlay codec: {v}");
            }
        } else if let Some(v) = line.strip_prefix("a=fmtp:") {
            f.fmtp = v
                .split_whitespace()
                .skip(1)
                .filter_map(|n| n.parse().ok())
                .collect();
        } else if let Some(v) = line.strip_prefix("a=rsaaeskey:") {
            let wrapped = b64(v)?;
            let raw = key.decrypt(rsa::Oaep::new::<sha1::Sha1>(), &wrapped)?;
            f.aes_key = Some(
                raw.try_into()
                    .map_err(|_| anyhow!("AES key is not 16 bytes"))?,
            );
        } else if let Some(v) = line.strip_prefix("a=aesiv:") {
            f.aes_iv = b64(v)?
                .try_into()
                .map_err(|_| anyhow!("AES IV is not 16 bytes"))?;
        }
    }
    Ok(f)
}

/// The ALAC "magic cookie" FFmpeg's decoder wants: a 12-byte `alac` atom
/// header plus the 24-byte ALACSpecificConfig built from the fmtp values
/// (`352 0 16 40 10 14 2 255 0 0 44100`).
fn alac_cookie(fmtp: &[u32]) -> Vec<u8> {
    let g = |i: usize, d: u32| fmtp.get(i).copied().unwrap_or(d);
    let mut c = Vec::with_capacity(36);
    c.extend_from_slice(&36u32.to_be_bytes());
    c.extend_from_slice(b"alac");
    c.extend_from_slice(&0u32.to_be_bytes());
    c.extend_from_slice(&g(0, 352).to_be_bytes()); // frame length
    c.push(g(1, 0) as u8); // compatible version
    c.push(g(2, 16) as u8); // bit depth
    c.push(g(3, 40) as u8); // rice history mult
    c.push(g(4, 10) as u8); // rice initial history
    c.push(g(5, 14) as u8); // rice limit
    c.push(g(6, 2) as u8); // channels
    c.extend_from_slice(&(g(7, 255) as u16).to_be_bytes()); // max run
    c.extend_from_slice(&g(8, 0).to_be_bytes()); // max frame bytes
    c.extend_from_slice(&g(9, 0).to_be_bytes()); // average bitrate
    c.extend_from_slice(&g(10, 44100).to_be_bytes()); // sample rate
    c
}

/// AirPlay volume (dB, −30…0, −144 = mute) → linear gain.
fn volume_gain(db: f64) -> f64 {
    if db <= -100.0 {
        0.0
    } else {
        10f64.powf(db.clamp(-30.0, 0.0) / 20.0)
    }
}

/// The DMAP strings we care about from a SET_PARAMETER body.
fn dmap_meta(b: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut pos = 0;
    while pos + 8 <= b.len() {
        let tag = String::from_utf8_lossy(&b[pos..pos + 4]).into_owned();
        let len = u32::from_be_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
        let Some(data) = b.get(pos + 8..pos + 8 + len) else {
            break;
        };
        match tag.as_str() {
            "mlit" => out.extend(dmap_meta(data)),
            "minm" | "asar" | "asal" => {
                out.insert(tag, String::from_utf8_lossy(data).into_owned());
            }
            _ => {}
        }
        pos += 8 + len;
    }
    out
}

// --- Session -----------------------------------------------------------------------

/// The playback side of one streaming session.
struct Stream {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    volume: gst::Element,
}

impl Stream {
    fn new(fmt: &Format) -> Result<Self> {
        let pipeline = gst::Pipeline::new();
        let caps = if fmt.alac {
            let cookie = gst::Buffer::from_slice(alac_cookie(&fmt.fmtp));
            gst::Caps::builder("audio/x-alac")
                .field("rate", SAMPLE_RATE as i32)
                .field("channels", 2i32)
                .field("codec_data", cookie)
                .build()
        } else {
            gst::Caps::builder("audio/x-raw")
                .field("format", "S16BE")
                .field("layout", "interleaved")
                .field("rate", SAMPLE_RATE as i32)
                .field("channels", 2i32)
                .build()
        };
        let src = gst_app::AppSrc::builder()
            .caps(&caps)
            .format(gst::Format::Time)
            .is_live(true)
            .max_bytes(8 * 1024 * 1024)
            .build();
        let mut chain: Vec<gst::Element> = vec![src.clone().upcast()];
        if fmt.alac {
            chain.push(
                gst::ElementFactory::make("avdec_alac")
                    .build()
                    .map_err(|_| anyhow!("avdec_alac missing (gst-libav)"))?,
            );
        }
        let volume = gst::ElementFactory::make("volume").build()?;
        for name in ["audioconvert", "audioresample"] {
            chain.push(gst::ElementFactory::make(name).build()?);
        }
        chain.push(volume.clone());
        chain.push(gst::ElementFactory::make("autoaudiosink").build()?);
        pipeline.add_many(&chain)?;
        gst::Element::link_many(&chain)?;
        pipeline.set_state(gst::State::Playing)?;
        Ok(Self {
            pipeline,
            src,
            volume,
        })
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Decrypts one audio payload in place: whole 16-byte blocks are AES-CBC
/// (the IV restarts with every packet), the tail is plain.
fn decrypt(payload: &mut [u8], key: &[u8; 16], iv: &[u8; 16]) {
    let cipher = Aes128::new(key.into());
    let mut prev = *iv;
    let (blocks, _tail) = payload.as_chunks_mut::<16>();
    for block in blocks {
        let cur = *block;
        cipher.decrypt_block(block.into());
        for (x, p) in block.iter_mut().zip(prev.iter()) {
            *x ^= p;
        }
        prev = cur;
    }
}

/// Shared between the RTSP thread (FLUSH, volume) and the UDP thread.
struct Live {
    stream: Option<Stream>,
    /// RTP timestamp ↔ running time anchor of the current run.
    anchor: Option<(u32, gst::ClockTime)>,
    last_ts: Option<u32>,
}

fn push_audio(live: &Mutex<Live>, packet: &[u8], fmt: &Format) {
    if packet.len() < 12 {
        return;
    }
    let ts = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
    let mut payload = packet[12..].to_vec();
    if let Some(key) = &fmt.aes_key {
        decrypt(&mut payload, key, &fmt.aes_iv);
    }
    let Ok(mut l) = live.lock() else { return };
    // A late retransmit or a reordered packet: its slot has passed.
    if let Some(last) = l.last_ts
        && ts.wrapping_sub(last) > u32::MAX / 2
    {
        return;
    }
    let now = match &l.stream {
        Some(stream) => stream
            .pipeline
            .current_running_time()
            .unwrap_or(gst::ClockTime::ZERO),
        None => return,
    };
    let (ts0, run0) = *l
        .anchor
        .get_or_insert((ts, now + gst::ClockTime::from_mseconds(LATENCY_MS)));
    let offset = gst::ClockTime::from_nseconds(
        u64::from(ts.wrapping_sub(ts0)) * 1_000_000_000 / SAMPLE_RATE,
    );
    let mut buf = gst::Buffer::from_mut_slice(payload);
    if let Some(b) = buf.get_mut() {
        b.set_pts(run0 + offset);
    }
    if let Some(stream) = &l.stream {
        let _ = stream.src.push_buffer(buf);
    }
    l.last_ts = Some(ts);
}

/// Answers a timing request (NTP-style, packet type 0x52) on the timing port.
fn timing_reply(req: &[u8]) -> Option<Vec<u8>> {
    if req.len() < 32 || req[1] & 0x7f != 0x52 {
        return None;
    }
    let now = ntp_now();
    let mut r = vec![0x80, 0xd3, req[2], req[3], 0, 0, 0, 0];
    r.extend_from_slice(&req[24..32]); // origin = their transmit time
    r.extend_from_slice(&now);
    r.extend_from_slice(&now);
    Some(r)
}

fn ntp_now() -> [u8; 8] {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() + 2_208_988_800;
    let frac = (u64::from(d.subsec_nanos()) << 32) / 1_000_000_000;
    let mut o = [0u8; 8];
    o[..4].copy_from_slice(&(secs as u32).to_be_bytes());
    o[4..].copy_from_slice(&(frac as u32).to_be_bytes());
    o
}

fn udp() -> Result<UdpSocket> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    Ok(s)
}

fn serve_connection(mut sock: TcpStream, ctx: &Ctx) -> Result<()> {
    sock.set_read_timeout(Some(Duration::from_secs(120)))?;
    let local_ip = sock.local_addr()?.ip();
    let mut buf = Vec::new();
    let mut fmt = Format::default();
    let live = Arc::new(Mutex::new(Live {
        stream: None,
        anchor: None,
        last_ts: None,
    }));
    let mut generation = 0;
    let mut streaming = false;
    let mut volume = 1.0;
    let result = (|| -> Result<()> {
        while let Some(req) = read_request(&mut sock, &mut buf)? {
            if ctx.stop.load(Ordering::Relaxed) {
                break;
            }
            // A newer sender took over: this session yields.
            if generation != 0 && ctx.generation.load(Ordering::Relaxed) != generation {
                break;
            }
            tracing::debug!("AirPlay RTSP {}", req.method);
            let mut extra: Vec<(&str, String)> = Vec::new();
            if let Some(ch) = req.header("Apple-Challenge") {
                extra.push((
                    "Apple-Response",
                    apple_response(&ctx.key, ch, local_ip, &ctx.mac)?,
                ));
            }
            match req.method.as_str() {
                "OPTIONS" => extra.push((
                    "Public",
                    "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER".into(),
                )),
                "ANNOUNCE" => {
                    fmt = parse_sdp(&String::from_utf8_lossy(&req.body), &ctx.key)?;
                    generation = ctx.generation.fetch_add(1, Ordering::Relaxed) + 1;
                }
                "SETUP" => {
                    let (data, control, timing) = (udp()?, udp()?, udp()?);
                    extra.push((
                        "Transport",
                        format!(
                            "RTP/AVP/UDP;unicast;mode=record;server_port={};control_port={};timing_port={}",
                            data.local_addr()?.port(),
                            control.local_addr()?.port(),
                            timing.local_addr()?.port()
                        ),
                    ));
                    extra.push(("Session", "1".into()));
                    spawn_udp(data, control, timing, live.clone(), fmt.clone(), ctx.stop.clone())?;
                }
                "RECORD" => {
                    let stream = Stream::new(&fmt)?;
                    tracing::debug!("AirPlay pipeline up");
                    stream.volume.set_property("volume", volume);
                    if let Ok(mut l) = live.lock() {
                        l.stream = Some(stream);
                        l.anchor = None;
                        l.last_ts = None;
                    }
                    streaming = true;
                    let _ = ctx.events.send_blocking(AirPlayEvent::Started);
                    extra.push(("Audio-Latency", (LATENCY_MS * SAMPLE_RATE / 1000).to_string()));
                }
                "FLUSH" => {
                    // Paused or seeked on the sender: drop what is queued and
                    // restart the timeline with the next packet.
                    if let Ok(mut l) = live.lock() {
                        l.stream = Stream::new(&fmt).ok();
                        if let Some(s) = &l.stream {
                            s.volume.set_property("volume", volume);
                        }
                        l.anchor = None;
                        l.last_ts = None;
                    }
                }
                "SET_PARAMETER" => {
                    let ty = req.header("Content-Type").unwrap_or("");
                    if ty.starts_with("text/parameters") {
                        let text = String::from_utf8_lossy(&req.body);
                        if let Some(db) = text
                            .lines()
                            .find_map(|l| l.strip_prefix("volume:"))
                            .and_then(|v| v.trim().parse::<f64>().ok())
                        {
                            volume = volume_gain(db);
                            if let Ok(l) = live.lock()
                                && let Some(s) = &l.stream
                            {
                                s.volume.set_property("volume", volume);
                            }
                        }
                    } else if ty.starts_with("application/x-dmap-tagged") {
                        let m = dmap_meta(&req.body);
                        if let Some(title) = m.get("minm").filter(|t| !t.is_empty()) {
                            let _ = ctx.events.send_blocking(AirPlayEvent::Meta {
                                title: title.clone(),
                                artist: m.get("asar").cloned().filter(|s| !s.is_empty()),
                                album: m.get("asal").cloned().filter(|s| !s.is_empty()),
                            });
                        }
                    }
                }
                "GET_PARAMETER" => {}
                "TEARDOWN" => {
                    reply(&mut sock, &req, "200 OK", &extra)?;
                    break;
                }
                _ => {
                    reply(&mut sock, &req, "501 Not Implemented", &extra)?;
                    continue;
                }
            }
            reply(&mut sock, &req, "200 OK", &extra)?;
        }
        Ok(())
    })();
    tracing::debug!("AirPlay session ended: {:?}", result.as_ref().err());
    if let Ok(mut l) = live.lock() {
        l.stream = None;
    }
    // Stop the UDP threads of this session.
    drop(live);
    if streaming && (generation == 0 || ctx.generation.load(Ordering::Relaxed) == generation) {
        let _ = ctx.events.send_blocking(AirPlayEvent::Stopped);
    }
    result
}

/// Audio (data port), retransmits/sync (control port) and timing requests.
/// The threads end when the session's `Live` is dropped.
fn spawn_udp(
    data: UdpSocket,
    control: UdpSocket,
    timing: UdpSocket,
    live: Arc<Mutex<Live>>,
    fmt: Format,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let weak = Arc::downgrade(&live);
    drop(live);
    {
        let weak = weak.clone();
        let fmt = fmt.clone();
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("airplay-audio".into())
            .spawn(move || {
                let mut buf = [0u8; 2048];
                while !stop.load(Ordering::Relaxed) {
                    let Some(live) = weak.upgrade() else { break };
                    if let Ok((n, _)) = data.recv_from(&mut buf) {
                        push_audio(&live, &buf[..n], &fmt);
                    }
                }
            })?;
    }
    {
        let weak = weak.clone();
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("airplay-control".into())
            .spawn(move || {
                let mut buf = [0u8; 2048];
                while !stop.load(Ordering::Relaxed) {
                    let Some(live) = weak.upgrade() else { break };
                    if let Ok((n, _)) = control.recv_from(&mut buf) {
                        // 0x56: a retransmitted audio packet behind a 4-byte header.
                        if n > 16 && buf[1] & 0x7f == 0x56 {
                            push_audio(&live, &buf[4..n], &fmt);
                        }
                    }
                }
            })?;
    }
    std::thread::Builder::new()
        .name("airplay-timing".into())
        .spawn(move || {
            let mut buf = [0u8; 128];
            while !stop.load(Ordering::Relaxed) && weak.strong_count() > 0 {
                if let Ok((n, from)) = timing.recv_from(&mut buf)
                    && let Some(r) = timing_reply(&buf[..n])
                {
                    let _ = timing.send_to(&r, from);
                }
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn airport_key_loads_and_answers_a_challenge() {
        let key = airport_key().expect("AirPort key");
        let resp = apple_response(
            &key,
            "x9fqdSbUZBnvsvq0hu9Ttg==",
            IpAddr::V4(Ipv4Addr::new(192, 168, 0, 2)),
            &[1, 2, 3, 4, 5, 6],
        )
        .unwrap();
        // A 2048-bit signature, base64 without padding.
        assert_eq!(resp.len(), 342);
        assert!(!resp.ends_with('='));
        // The public half verifies it (what the sender does).
        let public = rsa::RsaPublicKey::from(&key);
        let sig = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(&resp)
            .unwrap();
        let mut data = base64::engine::general_purpose::STANDARD
            .decode("x9fqdSbUZBnvsvq0hu9Ttg==")
            .unwrap();
        data.extend_from_slice(&[192, 168, 0, 2, 1, 2, 3, 4, 5, 6]);
        data.resize(32, 0);
        public
            .verify(rsa::Pkcs1v15Sign::new_unprefixed(), &data, &sig)
            .unwrap();
    }

    /// System randomness for the test's RSA encryption.
    struct TestRng;
    impl rsa::rand_core::RngCore for TestRng {
        fn next_u32(&mut self) -> u32 {
            let mut b = [0u8; 4];
            self.fill_bytes(&mut b);
            u32::from_le_bytes(b)
        }
        fn next_u64(&mut self) -> u64 {
            let mut b = [0u8; 8];
            self.fill_bytes(&mut b);
            u64::from_le_bytes(b)
        }
        fn fill_bytes(&mut self, dst: &mut [u8]) {
            getrandom::getrandom(dst).unwrap();
        }
    }
    impl rsa::rand_core::CryptoRng for TestRng {}

    #[test]
    fn sdp_unwraps_the_aes_key() {
        let key = airport_key().unwrap();
        let public = rsa::RsaPublicKey::from(&key);
        let aes_key = [7u8; 16];
        let mut rng = TestRng;
        let wrapped = public
            .encrypt(&mut rng, rsa::Oaep::new::<sha1::Sha1>(), &aes_key)
            .unwrap();
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD_NO_PAD.encode(b);
        let sdp = format!(
            "v=0\r\nm=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\n\
             a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\na=rsaaeskey:{}\r\na=aesiv:{}\r\n",
            b64(&wrapped),
            b64(&[9u8; 16])
        );
        let f = parse_sdp(&sdp, &key).unwrap();
        assert!(f.alac);
        assert_eq!(f.aes_key, Some(aes_key));
        assert_eq!(f.aes_iv, [9u8; 16]);
        assert_eq!(f.fmtp, vec![352, 0, 16, 40, 10, 14, 2, 255, 0, 0, 44100]);
        let cookie = alac_cookie(&f.fmtp);
        assert_eq!(cookie.len(), 36);
        assert_eq!(&cookie[4..8], b"alac");
        assert_eq!(&cookie[12..16], &352u32.to_be_bytes());
    }

    #[test]
    fn cbc_decrypts_whole_blocks_only() {
        use aes::cipher::BlockCipherEncrypt;
        let key = [3u8; 16];
        let iv = [5u8; 16];
        let plain: Vec<u8> = (0..40u8).collect();
        // Encrypt the first two blocks with CBC, leave the 8-byte tail plain.
        let c = Aes128::new((&key).into());
        let mut enc = plain.clone();
        let mut prev = iv;
        for block in enc[..32].as_chunks_mut::<16>().0 {
            for (x, p) in block.iter_mut().zip(prev.iter()) {
                *x ^= p;
            }
            c.encrypt_block(block.into());
            prev = *block;
        }
        decrypt(&mut enc, &key, &iv);
        assert_eq!(enc, plain);
    }

    /// ALAC frames from FFmpeg's encoder decode with our hand-built cookie
    /// (what an iPhone's stream needs). Skipped without gst-libav.
    #[test]
    fn alac_frames_decode_with_our_cookie() {
        gst::init().unwrap();
        if gst::ElementFactory::find("avenc_alac").is_none()
            || gst::ElementFactory::find("avdec_alac").is_none()
        {
            return;
        }
        let enc = gst::parse::launch(
            "audiotestsrc num-buffers=40 samplesperbuffer=4096 wave=sine ! \
             audio/x-raw,format=S16LE,rate=44100,channels=2 ! avenc_alac ! appsink name=out sync=false",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let out = enc
            .by_name("out")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        enc.set_state(gst::State::Playing).unwrap();
        let mut frames = Vec::new();
        while let Some(s) = out.try_pull_sample(gst::ClockTime::from_seconds(5)) {
            frames.push(s.buffer().unwrap().map_readable().unwrap().to_vec());
        }
        enc.set_state(gst::State::Null).unwrap();
        assert!(
            frames.len() >= 30,
            "encoder produced {} frames",
            frames.len()
        );

        let fmtp = [4096, 0, 16, 40, 10, 14, 2, 255, 0, 0, 44100];
        let caps = gst::Caps::builder("audio/x-alac")
            .field("rate", 44100i32)
            .field("channels", 2i32)
            .field("codec_data", gst::Buffer::from_slice(alac_cookie(&fmtp)))
            .build();
        let dec = gst::parse::launch("appsrc name=in ! avdec_alac ! audioconvert ! audio/x-raw,format=S16LE ! appsink name=out sync=false")
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        let src = dec
            .by_name("in")
            .unwrap()
            .downcast::<gst_app::AppSrc>()
            .unwrap();
        src.set_caps(Some(&caps));
        src.set_format(gst::Format::Time);
        let out = dec
            .by_name("out")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        dec.set_state(gst::State::Playing).unwrap();
        for (i, f) in frames.iter().enumerate() {
            let mut b = gst::Buffer::from_slice(f.clone());
            b.get_mut().unwrap().set_pts(gst::ClockTime::from_nseconds(
                i as u64 * 4096 * 1_000_000_000 / 44100,
            ));
            src.push_buffer(b).unwrap();
        }
        src.end_of_stream().unwrap();
        let mut bytes = 0;
        while let Some(s) = out.try_pull_sample(gst::ClockTime::from_seconds(5)) {
            bytes += s.buffer().unwrap().size();
        }
        let bus_error = dec.bus().unwrap().pop_filtered(&[gst::MessageType::Error]);
        dec.set_state(gst::State::Null).unwrap();
        assert!(bus_error.is_none(), "decoder error: {bus_error:?}");
        assert_eq!(bytes, frames.len() * 4096 * 4);
    }

    #[test]
    fn volume_and_metadata() {
        assert_eq!(volume_gain(0.0), 1.0);
        assert_eq!(volume_gain(-144.0), 0.0);
        assert!((volume_gain(-20.0) - 0.1).abs() < 1e-9);
        let mut item = Vec::new();
        for (tag, val) in [("minm", "Song"), ("asar", "Band")] {
            item.extend_from_slice(tag.as_bytes());
            item.extend_from_slice(&(val.len() as u32).to_be_bytes());
            item.extend_from_slice(val.as_bytes());
        }
        let mut mlit = b"mlit".to_vec();
        mlit.extend_from_slice(&(item.len() as u32).to_be_bytes());
        mlit.extend_from_slice(&item);
        let m = dmap_meta(&mlit);
        assert_eq!(m.get("minm").map(String::as_str), Some("Song"));
        assert_eq!(m.get("asar").map(String::as_str), Some("Band"));
    }
}
