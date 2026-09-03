//! Local HTTP **range proxy** so GStreamer can stream from sources it cannot
//! read itself: SMB shares and Google Drive (a bearer token cannot ride on a
//! playbin URI). `souphttpsrc` fetches `http://127.0.0.1:<port>/<secret>/
//! <source-id>/<rel>` with `Range` requests, which are answered from
//! [`Backend::open_range`] — so seeking, buffering and gapless pre-rolling all
//! work exactly as for a podcast episode.
//!
//! One listener per process, bound lazily to a random loopback port. A random
//! per-process secret in the path keeps other local users from using the proxy
//! as a credential-free gateway to the shares. One request per connection,
//! one thread per connection (capped), sources resolved from the DB on each
//! request so a renamed/removed source needs no cache invalidation.

use std::io::Write;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Result};
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};

use crate::core::db::Library;
use crate::core::http::{read_head, write_status, HttpReq};
use crate::core::remote::Backend;

/// Simultaneous connections served (a seeking player opens a few in a row).
const MAX_CONNECTIONS: usize = 12;
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const COPY_CHUNK: usize = 128 * 1024;

struct Proxy {
    port: u16,
    secret: String,
}

static PROXY: OnceLock<std::result::Result<Proxy, String>> = OnceLock::new();

/// The playable URI of a remote file through the proxy (starts the proxy on
/// first use).
pub fn stream_uri(source_id: i64, rel: &str) -> Result<String> {
    let proxy = PROXY
        .get_or_init(|| start().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| anyhow!("media proxy unavailable: {e}"))?;
    Ok(format!(
        "http://127.0.0.1:{}/{}/{}/{}",
        proxy.port,
        proxy.secret,
        source_id,
        utf8_percent_encode(rel, NON_ALPHANUMERIC)
    ))
}

fn start() -> Result<Proxy> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let mut buf = [0u8; 24];
    getrandom::getrandom(&mut buf).map_err(|e| anyhow!("no randomness: {e}"))?;
    let secret = buf.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let secret_for_thread = secret.clone();
    std::thread::Builder::new()
        .name("media-proxy".into())
        .spawn(move || serve(listener, secret_for_thread))?;
    tracing::info!("media proxy listening on 127.0.0.1:{port}");
    Ok(Proxy { port, secret })
}

fn serve(listener: TcpListener, secret: String) {
    let active = Arc::new(AtomicUsize::new(0));
    let secret = Arc::new(secret);
    for sock in listener.incoming() {
        let Ok(sock) = sock else { continue };
        if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            let mut sock = sock;
            write_status(&mut sock, 503);
            continue;
        }
        active.fetch_add(1, Ordering::Relaxed);
        let active = active.clone();
        let secret = secret.clone();
        let _ = std::thread::Builder::new()
            .name("media-proxy-conn".into())
            .spawn(move || {
                handle(sock, &secret);
                active.fetch_sub(1, Ordering::Relaxed);
            });
    }
}

/// `/<secret>/<source-id>/<encoded rel>` → (source id, rel).
fn parse_target(path: &str, secret: &str) -> Option<(i64, String)> {
    let mut parts = path.trim_start_matches('/').splitn(3, '/');
    let got_secret = parts.next()?;
    // Constant-time-ish compare is overkill for a per-process loopback secret,
    // but an exact match is required.
    if got_secret != secret {
        return None;
    }
    let id: i64 = parts.next()?.parse().ok()?;
    let rel = percent_decode_str(parts.next().unwrap_or(""))
        .decode_utf8_lossy()
        .into_owned();
    Some((id, rel))
}

/// Parses a single `bytes=` range (`a-b`, `a-`, `-n`) into
/// `(start, inclusive end)`; `None` for no/unsupported ranges (→ full body).
/// `total` is needed for suffix ranges and may be unknown (`None`).
fn parse_range(header: &str, total: Option<u64>) -> Option<(u64, Option<u64>)> {
    let spec = header.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None; // multi-range → serve everything
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        // Suffix range: the last n bytes.
        let n: u64 = b.parse().ok()?;
        let total = total?;
        if n == 0 {
            return None;
        }
        return Some((total.saturating_sub(n), None));
    }
    let start: u64 = a.parse().ok()?;
    let end = if b.is_empty() {
        None
    } else {
        Some(b.parse::<u64>().ok()?)
    };
    Some((start, end))
}

fn resolve(source_id: i64) -> Option<Backend> {
    let lib = Library::open().ok()?;
    let src = lib
        .list_sources()
        .ok()?
        .into_iter()
        .find(|s| s.id == source_id)?;
    Backend::from_source(&src)
}

fn content_type(rel: &str) -> &'static str {
    match rel
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("ogg" | "oga" | "opus") => "audio/ogg",
        Some("m4a" | "mp4" | "aac") => "audio/mp4",
        Some("wav") => "audio/wav",
        Some("wma") => "audio/x-ms-wma",
        _ => "application/octet-stream",
    }
}

fn handle(mut sock: TcpStream, secret: &str) {
    let _ = sock.set_read_timeout(Some(IO_TIMEOUT));
    let _ = sock.set_write_timeout(Some(IO_TIMEOUT));
    let Ok(req) = read_head(&mut sock) else {
        return;
    };
    if req.method != "GET" && req.method != "HEAD" {
        write_status(&mut sock, 405);
        return;
    }
    let Some((source_id, rel)) = parse_target(&req.path, secret) else {
        write_status(&mut sock, 404);
        return;
    };
    let Some(backend) = resolve(source_id) else {
        write_status(&mut sock, 404);
        return;
    };
    let range = req.header("Range").and_then(|h| parse_range(h, None));
    let (start, end) = range.unwrap_or((0, None));
    let body = match backend.open_range(&rel, start, end) {
        Ok(b) => b,
        Err(e) => {
            let msg = e.to_string();
            tracing::warn!("media proxy: {rel}: {msg}");
            let status = if msg.contains("not satisfiable") {
                416
            } else {
                502
            };
            write_status(&mut sock, status);
            return;
        }
    };
    let _ = write_body(&mut sock, &req, &rel, body);
}

fn write_body(
    out: &mut TcpStream,
    req: &HttpReq,
    rel: &str,
    body: crate::core::remote::RangeBody,
) -> std::io::Result<()> {
    let partial = req.header("Range").is_some();
    let (status, reason) = if partial {
        (206, "Partial Content")
    } else {
        (200, "OK")
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {}\r\nAccept-Ranges: bytes\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        content_type(rel),
        body.byte_len()
    );
    if partial {
        head.push_str(&format!(
            "Content-Range: bytes {}-{}/{}\r\n",
            body.start, body.end, body.total
        ));
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes())?;
    if req.method == "HEAD" {
        return out.flush();
    }
    let mut reader = body.body;
    let mut buf = vec![0u8; COPY_CHUNK];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                tracing::debug!("media proxy: read ended early for {rel}: {e}");
                break;
            }
        };
        // A write error means the player closed the connection (seek/stop):
        // stop pulling from the backend.
        out.write_all(&buf[..n])?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ranges() {
        assert_eq!(parse_range("bytes=0-", None), Some((0, None)));
        assert_eq!(parse_range("bytes=100-199", None), Some((100, Some(199))));
        assert_eq!(parse_range(" bytes = 5-", None), None);
        assert_eq!(parse_range("bytes=-100", Some(1000)), Some((900, None)));
        assert_eq!(parse_range("bytes=-100", None), None);
        assert_eq!(parse_range("bytes=0-1,5-6", None), None);
        assert_eq!(parse_range("items=0-1", None), None);
    }

    #[test]
    fn targets_need_the_secret() {
        assert_eq!(
            parse_target("/s3cret/7/%2FAlben%2FX%2Fa%20b.mp3", "s3cret"),
            Some((7, "/Alben/X/a b.mp3".to_string()))
        );
        assert_eq!(parse_target("/wrong/7/x", "s3cret"), None);
        assert_eq!(parse_target("/s3cret/abc/x", "s3cret"), None);
        assert_eq!(
            parse_target("/s3cret/7", "s3cret"),
            Some((7, String::new()))
        );
    }

    #[test]
    fn stream_uri_round_trips_through_parse_target() {
        let rel = "/Rock & Roll/Best of A & B.flac";
        let encoded = utf8_percent_encode(rel, NON_ALPHANUMERIC).to_string();
        let path = format!("/sec/3/{encoded}");
        assert_eq!(parse_target(&path, "sec"), Some((3, rel.to_string())));
    }

    #[test]
    fn content_types_follow_the_extension() {
        assert_eq!(content_type("/a/b.MP3"), "audio/mpeg");
        assert_eq!(content_type("/a/b.flac"), "audio/flac");
        assert_eq!(content_type("/a/b"), "application/octet-stream");
    }
}
