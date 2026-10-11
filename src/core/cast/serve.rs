//! LAN HTTP server the renderers fetch from. A renderer cannot read Emilia's
//! `file://` tracks, the loopback [`crate::core::media_proxy`] or many `https`
//! hosts, so every item played on a renderer is **published** here first and
//! the renderer gets a `http://<lan-ip>:<port>/<secret>/<token>/<name>` URL.
//!
//! Three kinds of item:
//! * a local **file** — served with `Range` support, so the renderer can seek;
//! * a **relay** of a network URL (podcast, radio, YouTube, the media proxy)
//!   — `Range` is passed through to the origin;
//! * a **transcode** for a format the renderer does not take (Ogg/Opus on most
//!   DLNA radios): a GStreamer pipeline per request decodes and re-encodes to
//!   MP3 (320 kbit/s CBR — every renderer plays it), starting at an offset (a
//!   seek is a new request). FLAC would be lossless, but `flacenc` puts out
//!   nothing after the start seek and only Ogg-mapped headers in its caps.
//!
//! Only published items can be fetched: the path must carry the per-process
//! secret and a random per-item token, and the registry keeps just the most
//! recent items. Nothing else of the file system is reachable.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::core::http::{ConnPermit, HttpReq, read_head, write_status};

const MAX_CONNECTIONS: usize = 10;
const MAX_ITEMS: usize = 64;
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const COPY_CHUNK: usize = 64 * 1024;

/// Target format of a transcode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Enc {
    Mp3,
}

impl Enc {
    pub fn mime(self) -> &'static str {
        "audio/mpeg"
    }

    fn ext(self) -> &'static str {
        "mp3"
    }

    fn element(self) -> &'static str {
        "lamemp3enc"
    }

    /// Whether GStreamer can build this encoder here.
    pub fn available(self) -> bool {
        gst::ElementFactory::find(self.element()).is_some()
    }
}

#[derive(Debug, Clone)]
pub enum Item {
    File(PathBuf),
    Relay(String),
    /// `uri` is anything `uridecodebin` opens (`file://`, `http(s)://`).
    Transcode {
        uri: String,
        start_ms: i64,
        enc: Enc,
    },
}

struct Server {
    port: u16,
    secret: String,
    items: Mutex<VecDeque<(String, Item)>>,
}

static SERVER: OnceLock<std::result::Result<Server, String>> = OnceLock::new();

fn random_hex(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).map_err(|e| anyhow!("no randomness: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn server() -> Result<&'static Server> {
    SERVER
        .get_or_init(|| start().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| anyhow!("cast server unavailable: {e}"))
}

fn start() -> Result<Server> {
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    let port = listener.local_addr()?.port();
    std::thread::Builder::new()
        .name("cast-serve".into())
        .spawn(move || accept_loop(listener))?;
    tracing::info!("cast server listening on port {port}");
    Ok(Server {
        port,
        secret: random_hex(16)?,
        items: Mutex::new(VecDeque::new()),
    })
}

/// The local address the renderer at `host` can reach us on: the source
/// address the routing table picks toward it (no packet is sent).
fn lan_ip_toward(host: &str) -> Result<String> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.connect((host, 1900))?;
    Ok(sock.local_addr()?.ip().to_string())
}

/// A URL path segment of only safe characters, keeping the extension (some
/// renderers pick the decoder by it).
fn safe_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() { "item".into() } else { s }
}

/// Publishes `item` for the renderer at `host`; returns (URL, token). `name`
/// becomes the last path segment (a transcode gets the target's extension).
pub fn publish(item: Item, name: &str, host: &str) -> Result<(String, String)> {
    let srv = server()?;
    let token = random_hex(12)?;
    let name = match &item {
        Item::Transcode { enc, .. } => {
            let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
            format!("{}.{}", safe_name(stem), enc.ext())
        }
        _ => safe_name(name),
    };
    let ip = lan_ip_toward(host)?;
    if let Ok(mut items) = srv.items.lock() {
        items.push_back((token.clone(), item));
        while items.len() > MAX_ITEMS {
            items.pop_front();
        }
    }
    Ok((
        format!("http://{ip}:{}/{}/{token}/{name}", srv.port, srv.secret),
        token,
    ))
}

fn lookup(path: &str) -> Option<Item> {
    let srv = server().ok()?;
    let mut parts = path.trim_start_matches('/').splitn(3, '/');
    if parts.next()? != srv.secret {
        return None;
    }
    let token = parts.next()?;
    let items = srv.items.lock().ok()?;
    items
        .iter()
        .find(|(t, _)| t == token)
        .map(|(_, i)| i.clone())
}

fn accept_loop(listener: TcpListener) {
    let active = Arc::new(AtomicUsize::new(0));
    for sock in listener.incoming() {
        let Ok(mut sock) = sock else { continue };
        let Some(permit) = ConnPermit::acquire(&active, MAX_CONNECTIONS) else {
            write_status(&mut sock, 503);
            continue;
        };
        let _ = std::thread::Builder::new()
            .name("cast-serve-conn".into())
            .spawn(move || {
                let _permit = permit;
                handle(sock);
            });
    }
}

fn handle(mut sock: TcpStream) {
    let _ = sock.set_read_timeout(Some(IO_TIMEOUT));
    let _ = sock.set_write_timeout(Some(IO_TIMEOUT));
    let Ok(req) = read_head(&mut sock) else {
        return;
    };
    if req.method != "GET" && req.method != "HEAD" {
        write_status(&mut sock, 405);
        return;
    }
    let Some(item) = lookup(&req.path) else {
        write_status(&mut sock, 404);
        return;
    };
    let res = match item {
        Item::File(path) => serve_file(&mut sock, &req, &path),
        Item::Relay(url) => serve_relay(&mut sock, &req, &url),
        Item::Transcode { uri, start_ms, enc } => {
            serve_transcode(&mut sock, &req, &uri, start_ms, enc)
        }
    };
    if let Err(e) = res {
        tracing::debug!(
            "cast serve {}: {e}",
            req.path.rsplit('/').next().unwrap_or("")
        );
    }
}

/// MIME type by extension: the audio types of the media proxy plus covers.
pub fn mime_for(name: &str) -> &'static str {
    match name
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("m4b") => "audio/mp4",
        _ => crate::core::media_proxy::content_type(name),
    }
}

/// Headers DLNA renderers look for (Samsung/LG refuse a stream without the
/// transfer mode; `DLNA.ORG_OP=01` announces byte seeking).
fn dlna_headers(seekable: bool) -> String {
    format!(
        "transferMode.dlna.org: Streaming\r\n\
         contentFeatures.dlna.org: DLNA.ORG_OP={};DLNA.ORG_CI=0;\
         DLNA.ORG_FLAGS=01700000000000000000000000000000\r\n",
        if seekable { "01" } else { "00" }
    )
}

fn serve_file(out: &mut TcpStream, req: &HttpReq, path: &PathBuf) -> std::io::Result<()> {
    let mut f = std::fs::File::open(path)?;
    let total = f.metadata()?.len();
    let range = req
        .header("Range")
        .and_then(|h| crate::core::media_proxy::parse_range(h, Some(total)));
    let (start, end) = match range {
        Some((s, e)) => (
            s,
            e.unwrap_or(total.saturating_sub(1))
                .min(total.saturating_sub(1)),
        ),
        None => (0, total.saturating_sub(1)),
    };
    if range.is_some() && start >= total {
        let head = format!(
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return out.write_all(head.as_bytes());
    }
    let len = if total == 0 { 0 } else { end - start + 1 };
    let mut head = if range.is_some() {
        format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\n")
    } else {
        "HTTP/1.1 200 OK\r\n".to_string()
    };
    head.push_str(&format!(
        "Content-Type: {}\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\n{}\
         Connection: close\r\n\r\n",
        mime_for(&path.to_string_lossy()),
        dlna_headers(true)
    ));
    out.write_all(head.as_bytes())?;
    if req.method == "HEAD" {
        return out.flush();
    }
    f.seek(SeekFrom::Start(start))?;
    let mut left = len;
    let mut buf = vec![0u8; COPY_CHUNK];
    while left > 0 {
        let want = buf.len().min(left as usize);
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        left -= n as u64;
    }
    out.flush()
}

fn relay_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .build()
}

fn serve_relay(out: &mut TcpStream, req: &HttpReq, url: &str) -> std::io::Result<()> {
    let mut up = relay_agent()
        .get(url)
        .set("User-Agent", crate::core::net::BROWSER_UA);
    if let Some(r) = req.header("Range") {
        up = up.set("Range", r);
    }
    let resp = match up.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => {
            write_status(out, if code == 416 { 416 } else { 502 });
            return Ok(());
        }
        Err(e) => {
            tracing::debug!("cast relay: {e}");
            write_status(out, 502);
            return Ok(());
        }
    };
    let status = resp.status();
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\n",
        if status == 206 {
            "Partial Content"
        } else {
            "OK"
        }
    );
    for h in [
        "Content-Type",
        "Content-Length",
        "Content-Range",
        "Accept-Ranges",
    ] {
        if let Some(v) = resp.header(h) {
            head.push_str(&format!("{h}: {v}\r\n"));
        }
    }
    head.push_str(&dlna_headers(
        resp.header("Accept-Ranges")
            .is_some_and(|v| v.contains("bytes")),
    ));
    head.push_str("Connection: close\r\n\r\n");
    out.write_all(head.as_bytes())?;
    if req.method == "HEAD" {
        return out.flush();
    }
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; COPY_CHUNK];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        // A write error means the renderer hung up (seek / stop / next track).
        out.write_all(&buf[..n])?;
    }
    out.flush()
}

/// Decode → convert → encode → appsink, starting at `start_ms`.
fn transcode_pipeline(
    uri: &str,
    start_ms: i64,
    enc: Enc,
) -> Result<(gst::Pipeline, gst_app::AppSink)> {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("uridecodebin")
        .property("uri", uri)
        .build()?;
    let conv = gst::ElementFactory::make("audioconvert").build()?;
    let resample = gst::ElementFactory::make("audioresample").build()?;
    let encoder = gst::ElementFactory::make(enc.element()).build()?;
    encoder.set_property_from_str("target", "bitrate");
    encoder.set_property("bitrate", 320i32);
    encoder.set_property("cbr", true);
    let sink = gst_app::AppSink::builder().sync(false).build();
    pipeline.add_many([&src, &conv, &resample, &encoder, sink.upcast_ref()])?;
    gst::Element::link_many([&conv, &resample, &encoder, sink.upcast_ref()])?;
    let conv_weak = conv.downgrade();
    src.connect_pad_added(move |_, pad| {
        let Some(conv) = conv_weak.upgrade() else {
            return;
        };
        let is_audio = pad
            .current_caps()
            .and_then(|c| c.structure(0).map(|s| s.name().starts_with("audio/")))
            .unwrap_or(false);
        if let Some(sinkpad) = conv.static_pad("sink")
            && is_audio
            && !sinkpad.is_linked()
        {
            let _ = pad.link(&sinkpad);
        }
    });
    if start_ms > 0 {
        let ok = pipeline.set_state(gst::State::Paused)?;
        if ok != gst::StateChangeSuccess::NoPreroll {
            let (res, _, _) = pipeline.state(gst::ClockTime::from_seconds(15));
            res?;
            pipeline.seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::ClockTime::from_mseconds(start_ms as u64),
            )?;
        }
    }
    pipeline.set_state(gst::State::Playing)?;
    Ok((pipeline, sink))
}

fn serve_transcode(
    out: &mut TcpStream,
    req: &HttpReq,
    uri: &str,
    start_ms: i64,
    enc: Enc,
) -> std::io::Result<()> {
    // No length is known up front: the body is delimited by closing the
    // connection, which every renderer accepts for a stream.
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\n{}Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        enc.mime(),
        dlna_headers(false)
    );
    if req.method == "HEAD" {
        out.write_all(head.as_bytes())?;
        return out.flush();
    }
    let (pipeline, sink) = match transcode_pipeline(uri, start_ms, enc) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("cast transcode: {e}");
            write_status(out, 500);
            return Ok(());
        }
    };
    out.write_all(head.as_bytes())?;
    let res = (|| {
        while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(20)) {
            let Some(buf) = sample.buffer() else { continue };
            let Ok(map) = buf.map_readable() else {
                continue;
            };
            out.write_all(map.as_slice())?;
        }
        out.flush()
    })();
    let _ = pipeline.set_state(gst::State::Null);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_url_safe() {
        assert_eq!(safe_name("Rock & Roll.flac"), "Rock___Roll.flac");
        assert_eq!(safe_name("../../etc"), "_.._etc");
        assert_eq!(safe_name(""), "item");
        assert_eq!(safe_name("Ünïcode.mp3"), "_n_code.mp3");
    }

    #[test]
    fn mime_covers_art_and_audio() {
        assert_eq!(mime_for("/a/cover.JPG"), "image/jpeg");
        assert_eq!(mime_for("/a/b.m4b"), "audio/mp4");
        assert_eq!(mime_for("/a/b.flac"), "audio/flac");
    }
}
