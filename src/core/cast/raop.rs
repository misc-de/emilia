//! AirPlay 1 **sender** (RAOP client): plays on AirPlay speakers (AirPort
//! Express, AirPlay-enabled receivers and speakers, shairport-sync, another
//! Emilia).
//!
//! Unlike DLNA and Cast, an AirPlay speaker does not fetch a URL — the
//! sender pushes the audio. So the item (the LAN server URL the session
//! publishes anyway) is decoded here with GStreamer to 44.1 kHz/16-bit
//! stereo, cut into 352-frame packets, packed as uncompressed ("verbatim")
//! ALAC or L16, AES-encrypted when the speaker insists, and sent over RTP in
//! real time with a small lead. The RTSP session (ANNOUNCE, SETUP, RECORD,
//! SET_PARAMETER, FLUSH, TEARDOWN) carries control, volume and metadata.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aes::Aes128;
use aes::cipher::{BlockCipherEncrypt, KeyInit};
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use super::airplay::{airport_key, ntp_now, timing_reply};
use super::backend::{Backend, LoadItem, Poll};
use super::session::Transport;

const RATE: u64 = 44100;
const FRAMES: usize = 352;
const PCM_BYTES: usize = FRAMES * 4;
/// How far ahead of real time we keep the speaker supplied.
const LEAD_FRAMES: u64 = RATE / 2;

/// What the speaker's TXT record allows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Caps {
    pub alac: bool,
    pub encrypt: bool,
}

impl Caps {
    /// From the `cn` (codecs: 0 PCM, 1 ALAC) and `et` (encryption: 0 none,
    /// 1 RSA) TXT values. Unencrypted is preferred when allowed.
    pub(crate) fn from_txt(cn: Option<&str>, et: Option<&str>) -> Self {
        let has =
            |list: Option<&str>, v: &str| list.is_some_and(|l| l.split(',').any(|x| x.trim() == v));
        Self {
            alac: cn.is_none() || has(cn, "1"),
            encrypt: !(et.is_none() || has(et, "0")) && has(et, "1"),
        }
    }
}

// --- Packet building ---------------------------------------------------------------

struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl BitWriter {
    fn new(cap: usize) -> Self {
        Self {
            out: Vec::with_capacity(cap),
            acc: 0,
            n: 0,
        }
    }

    fn put(&mut self, value: u32, bits: u32) {
        for i in (0..bits).rev() {
            self.acc = (self.acc << 1) | ((value >> i) & 1);
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.acc as u8);
                self.acc = 0;
                self.n = 0;
            }
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push((self.acc << (8 - self.n)) as u8);
        }
        self.out
    }
}

/// One 352-frame block of S16LE stereo as an uncompressed ALAC frame: a
/// stereo element (`ID_CPE`) with the "not compressed" escape, the samples
/// verbatim (16 bit big-endian, L/R interleaved), then `ID_END`.
fn alac_verbatim(pcm: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::new(pcm.len() + 8);
    w.put(1, 3); // element type: channel pair
    w.put(0, 4); // element instance tag
    w.put(0, 12); // unused
    w.put(0, 1); // has_size: frame length from the cookie (352)
    w.put(0, 2); // extra bits
    w.put(1, 1); // not compressed
    for s in pcm.as_chunks::<2>().0 {
        w.put(u32::from(u16::from_le_bytes(*s)), 16);
    }
    w.put(7, 3); // ID_END
    w.finish()
}

/// S16LE → S16BE (L16 is big-endian on the wire).
fn l16(pcm: &[u8]) -> Vec<u8> {
    pcm.as_chunks::<2>()
        .0
        .iter()
        .flat_map(|s| [s[1], s[0]])
        .collect()
}

/// Encrypts whole 16-byte blocks with AES-128-CBC (fresh IV per packet).
fn encrypt(payload: &mut [u8], key: &[u8; 16], iv: &[u8; 16]) {
    let c = Aes128::new(key.into());
    let mut prev = *iv;
    for block in payload.as_chunks_mut::<16>().0 {
        for (x, p) in block.iter_mut().zip(prev.iter()) {
            *x ^= p;
        }
        c.encrypt_block(block.into());
        prev = *block;
    }
}

fn rtp(marker: bool, seq: u16, ts: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(12 + payload.len());
    p.push(0x80);
    p.push(if marker { 0xe0 } else { 0x60 });
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&0x0e1d_a1aau32.to_be_bytes()); // SSRC
    p.extend_from_slice(payload);
    p
}

/// A sync packet: "the sample at `now_ts - latency` plays now".
fn sync_packet(first: bool, play_ts: u32, next_ts: u32) -> Vec<u8> {
    let mut p = vec![if first { 0x90 } else { 0x80 }, 0xd4, 0x00, 0x07];
    p.extend_from_slice(&play_ts.to_be_bytes());
    p.extend_from_slice(&ntp_now());
    p.extend_from_slice(&next_ts.to_be_bytes());
    p
}

fn dmap(tag: &str, value: &[u8]) -> Vec<u8> {
    let mut v = tag.as_bytes().to_vec();
    v.extend_from_slice(&(value.len() as u32).to_be_bytes());
    v.extend_from_slice(value);
    v
}

// --- Streaming ---------------------------------------------------------------------

/// State the streaming thread shares with the backend.
#[derive(Default)]
struct Shared {
    /// Frames sent since the current item (re)started.
    sent: u64,
    started: bool,
    paused: bool,
    /// The decoder ran out and the lead has played.
    ended: bool,
    failed: bool,
    seq: u16,
    ts: u32,
}

struct Streamer {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Streamer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn decoder(url: &str, start_ms: i64) -> Result<(gst::Pipeline, gst_app::AppSink)> {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("uridecodebin")
        .property("uri", url)
        .build()?;
    let conv = gst::ElementFactory::make("audioconvert").build()?;
    let res = gst::ElementFactory::make("audioresample").build()?;
    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("layout", "interleaved")
        .field("rate", RATE as i32)
        .field("channels", 2i32)
        .build();
    let sink = gst_app::AppSink::builder().caps(&caps).sync(false).build();
    pipeline.add_many([&src, &conv, &res, sink.upcast_ref()])?;
    gst::Element::link_many([&conv, &res, sink.upcast_ref()])?;
    let conv_weak = conv.downgrade();
    src.connect_pad_added(move |_, pad| {
        let Some(conv) = conv_weak.upgrade() else {
            return;
        };
        let audio = pad
            .current_caps()
            .and_then(|c| c.structure(0).map(|s| s.name().starts_with("audio/")))
            .unwrap_or(false);
        if let Some(sinkpad) = conv.static_pad("sink")
            && audio
            && !sinkpad.is_linked()
        {
            let _ = pad.link(&sinkpad);
        }
    });
    // A live source does not preroll and cannot seek anyway.
    if start_ms > 0 && pipeline.set_state(gst::State::Paused)? != gst::StateChangeSuccess::NoPreroll
    {
        pipeline.state(gst::ClockTime::from_seconds(15)).0?;
        pipeline.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::ClockTime::from_mseconds(start_ms as u64),
        )?;
    }
    pipeline.set_state(gst::State::Playing)?;
    Ok((pipeline, sink))
}

struct StreamCfg {
    url: String,
    start_ms: i64,
    caps: Caps,
    key: [u8; 16],
    iv: [u8; 16],
    data: UdpSocket,
    control: UdpSocket,
    server: SocketAddr,
    control_to: SocketAddr,
    latency_frames: u64,
}

fn stream(cfg: StreamCfg, shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>) {
    let (pipeline, sink) = match decoder(&cfg.url, cfg.start_ms) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("AirPlay send: decoder: {e:#}");
            if let Ok(mut s) = shared.lock() {
                s.failed = true;
            }
            return;
        }
    };
    let mut pcm: Vec<u8> = Vec::with_capacity(PCM_BYTES * 4);
    let mut eos = false;
    let mut clock = Instant::now();
    let mut clock_frames = 0u64;
    let mut last_sync = Instant::now() - Duration::from_secs(2);
    let mut first_packet = true;
    let mut first_sync = true;
    while !stop.load(Ordering::Relaxed) {
        let paused = shared.lock().map(|s| s.paused).unwrap_or(true);
        if paused {
            std::thread::sleep(Duration::from_millis(50));
            // Real time stands still while paused.
            clock = Instant::now();
            clock_frames = shared.lock().map(|s| s.sent).unwrap_or(0);
            continue;
        }
        let sent = shared.lock().map(|s| s.sent).unwrap_or(0);
        let due =
            clock_frames + (clock.elapsed().as_micros() as u64 * RATE / 1_000_000) + LEAD_FRAMES;
        if last_sync.elapsed() >= Duration::from_secs(1) {
            let (ts, _) = shared.lock().map(|s| (s.ts, s.seq)).unwrap_or_default();
            let lead = (sent.saturating_sub(
                clock_frames + clock.elapsed().as_micros() as u64 * RATE / 1_000_000,
            )) as u32;
            let playing = ts
                .wrapping_sub(lead)
                .wrapping_sub(cfg.latency_frames as u32);
            let _ = cfg
                .control
                .send_to(&sync_packet(first_sync, playing, ts), cfg.control_to);
            first_sync = false;
            last_sync = Instant::now();
        }
        if sent >= due {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        // Fill one packet's worth of PCM.
        while pcm.len() < PCM_BYTES && !eos {
            match sink.try_pull_sample(gst::ClockTime::from_mseconds(200)) {
                Some(sample) => {
                    if let Some(buf) = sample.buffer()
                        && let Ok(map) = buf.map_readable()
                    {
                        pcm.extend_from_slice(map.as_slice());
                    }
                }
                None => {
                    if sink.is_eos() {
                        eos = true;
                    } else if stop.load(Ordering::Relaxed) {
                        break;
                    }
                }
            }
        }
        if pcm.is_empty() && eos {
            // Let the lead and the speaker's buffer play out, then report.
            let tail = Duration::from_millis((LEAD_FRAMES + cfg.latency_frames) * 1000 / RATE);
            let until = Instant::now() + tail;
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
            if let Ok(mut s) = shared.lock() {
                s.ended = true;
            }
            break;
        }
        if pcm.len() < PCM_BYTES {
            pcm.resize(PCM_BYTES, 0); // the last, short block: pad with silence
        }
        let block: Vec<u8> = pcm.drain(..PCM_BYTES).collect();
        let mut payload = if cfg.caps.alac {
            alac_verbatim(&block)
        } else {
            l16(&block)
        };
        if cfg.caps.encrypt {
            encrypt(&mut payload, &cfg.key, &cfg.iv);
        }
        let (seq, ts) = match shared.lock() {
            Ok(mut s) => {
                let v = (s.seq, s.ts);
                s.seq = s.seq.wrapping_add(1);
                s.ts = s.ts.wrapping_add(FRAMES as u32);
                s.sent += FRAMES as u64;
                s.started = true;
                v
            }
            Err(_) => break,
        };
        let _ = cfg
            .data
            .send_to(&rtp(first_packet, seq, ts, &payload), cfg.server);
        first_packet = false;
    }
    let _ = pipeline.set_state(gst::State::Null);
}

// --- RTSP + backend ----------------------------------------------------------------

pub(crate) struct RaopBackend {
    rtsp: TcpStream,
    rbuf: Vec<u8>,
    cseq: u32,
    url: String,
    session: Option<String>,
    host: IpAddr,
    caps: Caps,
    key: [u8; 16],
    iv: [u8; 16],
    data: UdpSocket,
    control: UdpSocket,
    server: Option<SocketAddr>,
    control_to: Option<SocketAddr>,
    latency_frames: u64,
    shared: Arc<Mutex<Shared>>,
    streamer: Option<Streamer>,
    start_ms: i64,
    item_url: String,
    volume: u32,
    lost: bool,
    timing_stop: Arc<AtomicBool>,
}

impl Drop for RaopBackend {
    fn drop(&mut self) {
        self.timing_stop.store(true, Ordering::Relaxed);
    }
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    let _ = getrandom::getrandom(&mut b);
    b
}

impl RaopBackend {
    pub(crate) fn connect(host: &str, port: u16, caps: Caps) -> Result<Self> {
        let ip: IpAddr = host
            .parse()
            .map_err(|_| anyhow!("bad AirPlay address {host}"))?;
        let rtsp = TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_secs(5))?;
        rtsp.set_read_timeout(Some(Duration::from_secs(10)))?;
        let local = rtsp.local_addr()?.ip();
        let sid = u32::from_be_bytes(random::<4>());
        let data = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        let control = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        let timing = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        timing.set_read_timeout(Some(Duration::from_millis(500)))?;
        let timing_port = timing.local_addr()?.port();
        let timing_stop = Arc::new(AtomicBool::new(false));
        {
            // Speakers ask our clock for the time to sync playback.
            let stop = timing_stop.clone();
            std::thread::Builder::new()
                .name("raop-timing".into())
                .spawn(move || {
                    let mut buf = [0u8; 128];
                    while !stop.load(Ordering::Relaxed) {
                        if let Ok((n, from)) = timing.recv_from(&mut buf)
                            && let Some(r) = timing_reply(&buf[..n])
                        {
                            let _ = timing.send_to(&r, from);
                        }
                    }
                })?;
        }
        let mut me = Self {
            rtsp,
            rbuf: Vec::new(),
            cseq: 0,
            url: format!("rtsp://{local}/{sid}"),
            session: None,
            host: ip,
            caps,
            key: random(),
            iv: random(),
            control,
            data,
            server: None,
            control_to: None,
            latency_frames: RATE * 2,
            shared: Arc::new(Mutex::new(Shared {
                seq: u16::from_be_bytes(random::<2>()),
                ts: u32::from_be_bytes(random::<4>()),
                ..Default::default()
            })),
            streamer: None,
            start_ms: 0,
            item_url: String::new(),
            volume: 50,
            lost: false,
            timing_stop,
        };
        me.setup(timing_port, local)?;
        Ok(me)
    }

    /// Sends one request and returns (status code, headers).
    fn request(
        &mut self,
        method: &str,
        headers: &[(&str, String)],
        body: Option<(&str, &[u8])>,
    ) -> Result<(u16, Vec<(String, String)>)> {
        self.cseq += 1;
        let target = if method == "OPTIONS" {
            "*".to_string()
        } else {
            self.url.clone()
        };
        let mut req = format!(
            "{method} {target} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: Emilia/{}\r\nClient-Instance: 56B29BB6CB904862\r\n",
            self.cseq,
            env!("CARGO_PKG_VERSION")
        );
        if let Some(s) = &self.session {
            req.push_str(&format!("Session: {s}\r\n"));
        }
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some((ty, b)) = body {
            req.push_str(&format!(
                "Content-Type: {ty}\r\nContent-Length: {}\r\n\r\n",
                b.len()
            ));
            self.rtsp.write_all(req.as_bytes())?;
            self.rtsp.write_all(b)?;
        } else {
            req.push_str("\r\n");
            self.rtsp.write_all(req.as_bytes())?;
        }
        self.read_reply()
    }

    fn read_reply(&mut self) -> Result<(u16, Vec<(String, String)>)> {
        let mut tmp = [0u8; 4096];
        let end = loop {
            if let Some(p) = self.rbuf.windows(4).position(|w| w == b"\r\n\r\n") {
                break p + 4;
            }
            let n = self.rtsp.read(&mut tmp)?;
            if n == 0 {
                self.lost = true;
                bail!("AirPlay speaker closed the connection");
            }
            self.rbuf.extend_from_slice(&tmp[..n]);
        };
        let head = String::from_utf8_lossy(&self.rbuf[..end]).into_owned();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        let len: usize = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        while self.rbuf.len() < end + len {
            let n = self.rtsp.read(&mut tmp)?;
            if n == 0 {
                break;
            }
            self.rbuf.extend_from_slice(&tmp[..n]);
        }
        self.rbuf.drain(..(end + len).min(self.rbuf.len()));
        Ok((status, headers))
    }

    fn setup(&mut self, timing_port: u16, local: IpAddr) -> Result<()> {
        let (st, _) = self.request("OPTIONS", &[], None)?;
        if st == 401 || st == 403 {
            bail!("the AirPlay speaker needs a password or pairing (AirPlay 2)");
        }
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD_NO_PAD.encode(b);
        let (rtpmap, fmtp) = if self.caps.alac {
            ("96 AppleLossless", "96 352 0 16 40 10 14 2 255 0 0 44100")
        } else {
            ("96 L16/44100/2", "96 352 0 16 40 10 14 2 255 0 0 44100")
        };
        let mut sdp = format!(
            "v=0\r\no=iTunes {} 0 IN IP4 {local}\r\ns=iTunes\r\nc=IN IP4 {}\r\nt=0 0\r\n\
             m=audio 0 RTP/AVP 96\r\na=rtpmap:{rtpmap}\r\na=fmtp:{fmtp}\r\n",
            u32::from_be_bytes(random::<4>()),
            self.host
        );
        if self.caps.encrypt {
            let public = rsa::RsaPublicKey::from(&airport_key()?);
            let wrapped = public.encrypt(&mut SysRng, rsa::Oaep::new::<sha1::Sha1>(), &self.key)?;
            sdp.push_str(&format!(
                "a=rsaaeskey:{}\r\na=aesiv:{}\r\n",
                b64(&wrapped),
                b64(&self.iv)
            ));
        }
        let (st, _) = self.request("ANNOUNCE", &[], Some(("application/sdp", sdp.as_bytes())))?;
        if st != 200 {
            bail!("the AirPlay speaker refused the stream (ANNOUNCE {st})");
        }
        let transport = format!(
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={timing_port}",
            self.control.local_addr()?.port()
        );
        let (st, h) = self.request("SETUP", &[("Transport", transport)], None)?;
        if st != 200 {
            bail!("AirPlay SETUP failed ({st})");
        }
        let get = |name: &str| {
            h.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        };
        self.session = get("Session").map(|s| s.split(';').next().unwrap_or("").to_string());
        let t = get("Transport").unwrap_or_default();
        let port_of = |key: &str| {
            t.split(';')
                .find_map(|p| p.strip_prefix(&format!("{key}=")))
                .and_then(|v| v.parse::<u16>().ok())
        };
        let server =
            port_of("server_port").ok_or_else(|| anyhow!("no server_port from the speaker"))?;
        self.server = Some(SocketAddr::new(self.host, server));
        self.control_to = Some(SocketAddr::new(
            self.host,
            port_of("control_port").unwrap_or(server),
        ));
        let (seq, ts) = self
            .shared
            .lock()
            .map(|s| (s.seq, s.ts))
            .unwrap_or_default();
        let (st, h) = self.request(
            "RECORD",
            &[
                ("Range", "npt=0-".into()),
                ("RTP-Info", format!("seq={seq};rtptime={ts}")),
            ],
            None,
        )?;
        if st != 200 {
            bail!("AirPlay RECORD failed ({st})");
        }
        if let Some(l) = h
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Audio-Latency"))
            .and_then(|(_, v)| v.parse::<u64>().ok())
        {
            self.latency_frames = l;
        }
        Ok(())
    }

    fn rtp_info(&self) -> String {
        let (seq, ts) = self
            .shared
            .lock()
            .map(|s| (s.seq, s.ts))
            .unwrap_or_default();
        format!("seq={seq};rtptime={ts}")
    }

    fn flush(&mut self) -> Result<()> {
        let info = self.rtp_info();
        self.request("FLUSH", &[("RTP-Info", info)], None)
            .map(|_| ())
    }

    /// (Re)starts streaming the current item from `start_ms`.
    fn start_stream(&mut self, start_ms: i64, paused: bool) -> Result<()> {
        self.streamer = None;
        let (Some(server), Some(control_to)) = (self.server, self.control_to) else {
            bail!("AirPlay session not set up");
        };
        if let Ok(mut s) = self.shared.lock() {
            s.sent = 0;
            s.started = false;
            s.ended = false;
            s.failed = false;
            s.paused = paused;
        }
        self.start_ms = start_ms;
        let cfg = StreamCfg {
            url: self.item_url.clone(),
            start_ms,
            caps: self.caps,
            key: self.key,
            iv: self.iv,
            data: self.data.try_clone()?,
            control: self.control.try_clone()?,
            server,
            control_to,
            latency_frames: self.latency_frames,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let shared = self.shared.clone();
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("raop-stream".into())
                .spawn(move || stream(cfg, shared, stop))?
        };
        self.streamer = Some(Streamer {
            stop,
            thread: Some(thread),
        });
        Ok(())
    }

    fn send_meta(&mut self, item: &LoadItem) {
        let mut inner = dmap("minm", item.meta.title.as_bytes());
        if let Some(a) = &item.meta.artist {
            inner.extend(dmap("asar", a.as_bytes()));
        }
        if let Some(a) = &item.meta.album {
            inner.extend(dmap("asal", a.as_bytes()));
        }
        let body = dmap("mlit", &inner);
        let info = self.rtp_info();
        let _ = self.request(
            "SET_PARAMETER",
            &[("RTP-Info", info)],
            Some(("application/x-dmap-tagged", &body)),
        );
    }
}

/// System randomness for RSA-OAEP.
struct SysRng;

impl rsa::rand_core::RngCore for SysRng {
    fn next_u32(&mut self) -> u32 {
        u32::from_le_bytes(random::<4>())
    }
    fn next_u64(&mut self) -> u64 {
        u64::from_le_bytes(random::<8>())
    }
    fn fill_bytes(&mut self, dst: &mut [u8]) {
        let _ = getrandom::getrandom(dst);
    }
}

impl rsa::rand_core::CryptoRng for SysRng {}

impl Backend for RaopBackend {
    fn accepts(&mut self) -> Option<Vec<String>> {
        // We decode ourselves: anything GStreamer opens.
        Some(vec!["*".into()])
    }

    fn volume(&mut self) -> Result<u32> {
        Ok(self.volume)
    }

    fn set_volume(&mut self, v: u32) -> Result<()> {
        self.volume = v.min(100);
        // AirPlay volume: −30…0 dB, −144 = mute.
        let db = if self.volume == 0 {
            -144.0
        } else {
            -30.0 + 30.0 * f64::from(self.volume) / 100.0
        };
        let body = format!("volume: {db:.6}\r\n");
        self.request(
            "SET_PARAMETER",
            &[],
            Some(("text/parameters", body.as_bytes())),
        )
        .map(|_| ())
    }

    fn load(&mut self, item: &LoadItem) -> Result<bool> {
        if self.streamer.is_some() {
            self.streamer = None;
            self.flush()?;
        }
        self.item_url = item.url.to_string();
        self.start_stream(item.start_ms, !item.play)?;
        self.send_meta(item);
        Ok(true)
    }

    fn play(&mut self) -> Result<()> {
        if let Ok(mut s) = self.shared.lock() {
            s.paused = false;
        }
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        if let Ok(mut s) = self.shared.lock() {
            s.paused = true;
        }
        self.flush()
    }

    fn stop(&mut self) -> Result<()> {
        self.streamer = None;
        let _ = self.request("TEARDOWN", &[], None);
        Ok(())
    }

    fn seek(&mut self, ms: i64) -> Result<()> {
        let paused = self.shared.lock().map(|s| s.paused).unwrap_or(false);
        self.streamer = None;
        self.flush()?;
        self.start_stream(ms, paused)
    }

    fn poll(&mut self, _active: bool) -> Result<Poll> {
        // A closed RTSP connection means another sender took the speaker.
        self.rtsp.set_nonblocking(true)?;
        let mut probe = [0u8; 1];
        match self.rtsp.peek(&mut probe) {
            Ok(0) => self.lost = true,
            Ok(_) => {
                // Unsolicited data (rare): drop it.
                let mut sink = [0u8; 4096];
                let _ = self.rtsp.read(&mut sink);
            }
            Err(_) => {}
        }
        self.rtsp.set_nonblocking(false)?;
        let s = self
            .shared
            .lock()
            .map_err(|_| anyhow!("stream state poisoned"))?;
        let heard = s.sent.saturating_sub(LEAD_FRAMES + self.latency_frames);
        let state = if s.ended || self.streamer.is_none() {
            Transport::Stopped
        } else if s.paused {
            Transport::Paused
        } else if s.started {
            Transport::Playing
        } else {
            Transport::Transitioning
        };
        Ok(Poll {
            state,
            pos: Some(self.start_ms + (heard * 1000 / RATE) as i64),
            dur: None,
            current: String::new(),
            failed: s.failed,
            lost: self.lost,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_prefer_plain_alac() {
        assert_eq!(
            Caps::from_txt(Some("0,1"), Some("0,1")),
            Caps {
                alac: true,
                encrypt: false
            }
        );
        assert_eq!(
            Caps::from_txt(Some("1"), Some("1")),
            Caps {
                alac: true,
                encrypt: true
            }
        );
        assert_eq!(
            Caps::from_txt(Some("0"), None),
            Caps {
                alac: false,
                encrypt: false
            }
        );
    }

    #[test]
    fn verbatim_alac_layout() {
        let mut pcm = vec![0u8; PCM_BYTES];
        pcm[0] = 0x34; // first left sample 0x1234 (LE)
        pcm[1] = 0x12;
        let f = alac_verbatim(&pcm);
        // 23 header bits + 352×2×16 sample bits + 3 end bits → 1412 bytes.
        assert_eq!(f.len(), 1412);
        // Bits: 001 0000 000000000000 0 00 1, then the first sample 0x1234
        // from bit 23 on: byte 2 = 0000001|0, byte 3 = the next 8 sample bits.
        assert_eq!(&f[..4], &[0x20, 0x00, 0x02, 0x24]);
    }

    /// Our verbatim ALAC frames decode bit-exactly with FFmpeg's ALAC decoder
    /// and the same cookie a receiver builds from our SDP. Skipped without
    /// gst-libav.
    #[test]
    fn verbatim_alac_decodes_bit_exact() {
        gst::init().unwrap();
        if gst::ElementFactory::find("avdec_alac").is_none() {
            return;
        }
        // A ramp with negative values, so sign and bit order both matter.
        let pcm: Vec<u8> = (0..FRAMES * 2 * 8)
            .map(|i| ((i as i32 * 37) % 65536 - 32768) as i16)
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let fmtp = [352, 0, 16, 40, 10, 14, 2, 255, 0, 0, 44100];
        let caps = gst::Caps::builder("audio/x-alac")
            .field("rate", 44100i32)
            .field("channels", 2i32)
            .field(
                "codec_data",
                gst::Buffer::from_slice(super::super::airplay::alac_cookie(&fmtp)),
            )
            .build();
        let dec = gst::parse::launch("appsrc name=in ! avdec_alac ! audioconvert ! audio/x-raw,format=S16LE,layout=interleaved ! appsink name=out sync=false")
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
        for (i, block) in pcm.as_chunks::<PCM_BYTES>().0.iter().enumerate() {
            let mut b = gst::Buffer::from_slice(alac_verbatim(block));
            b.get_mut().unwrap().set_pts(gst::ClockTime::from_nseconds(
                i as u64 * 352 * 1_000_000_000 / 44100,
            ));
            src.push_buffer(b).unwrap();
        }
        src.end_of_stream().unwrap();
        let mut decoded = Vec::new();
        while let Some(s) = out.try_pull_sample(gst::ClockTime::from_seconds(5)) {
            decoded.extend_from_slice(&s.buffer().unwrap().map_readable().unwrap());
        }
        dec.set_state(gst::State::Null).unwrap();
        assert_eq!(decoded.len(), pcm.len());
        assert!(decoded == pcm, "decoded PCM differs");
    }

    #[test]
    fn sender_encryption_matches_the_receiver() {
        let (key, iv) = ([0x11u8; 16], [0x22u8; 16]);
        let plain = alac_verbatim(&vec![7u8; PCM_BYTES]);
        let mut wire = plain.clone();
        encrypt(&mut wire, &key, &iv);
        assert_ne!(wire[..16], plain[..16]);
        // The 4-byte tail past the last whole block stays plain.
        assert_eq!(wire[1408..], plain[1408..]);
        super::super::airplay::decrypt(&mut wire, &key, &iv);
        assert_eq!(wire, plain);
    }

    #[test]
    fn rtp_and_sync_headers() {
        let p = rtp(true, 7, 99, &[1, 2]);
        assert_eq!(&p[..4], &[0x80, 0xe0, 0, 7]);
        assert_eq!(&p[4..8], &99u32.to_be_bytes());
        assert_eq!(&p[12..], &[1, 2]);
        let s = sync_packet(true, 10, 20);
        assert_eq!(&s[..4], &[0x90, 0xd4, 0x00, 0x07]);
        assert_eq!(s.len(), 20);
    }
}
