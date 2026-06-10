//! HTTPS server (rustls 0.23, blocking) for device sync.
//!
//! Runs blocking in its own thread on a plain `TcpListener`; every accepted
//! connection is wrapped in a rustls TLS session (the same maintained 0.23 stack
//! as the client). The accept loop is non-blocking and regularly checks the stop
//! flag as well as the pairing/session timeouts. Every authenticated request
//! extends the session (no separate ping needed).
//!
//! HTTP/1.1 is parsed with `httparse` (request line + headers); bodies are read
//! by `Content-Length` and hard-capped. One request is served per connection
//! (`Connection: close`) — the client (`ureq`) re-dials for the next request,
//! which keeps the server free of keep-alive/pipelining edge cases. The set of
//! requests is fixed and tiny (see [`crate::core::sync::client`]).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde::Serialize;

use crate::core::db::Library;
use crate::core::sync::protocol::{self, Capabilities, PairRequest, PairResponse};
use crate::core::sync::share::{ShareDecision, ShareManifest};
use crate::core::sync::{crypto, data, SyncEvent};
use crate::core::sync::{ACCEPT_POLL, PORT, PORT_ATTEMPTS, QR_TTL, SESSION_TIMEOUT};

/// Shared UI→server channel for the selective-share handshake. The server-side
/// UI parks an outgoing offer (server-as-sender) or its decision on an incoming
/// offer (client-as-sender) here; the running server thread serves them to the
/// polling client.
#[derive(Default)]
pub struct ShareChannel {
    /// Server-side user's outgoing offer (server-as-sender) — `GET /share/offer`.
    pub outgoing: Option<ShareManifest>,
    /// Server-side user's decision on an incoming offer — `GET /share/decision`.
    pub decision: Option<ShareDecision>,
}

/// Local capabilities advertised to a peer (read from the settings DB).
pub(crate) fn local_caps() -> Capabilities {
    let youtube_enabled = Library::open()
        .ok()
        .and_then(|lib| lib.get_setting("youtube_enabled").ok().flatten())
        .as_deref()
        == Some("1");
    Capabilities {
        schema: protocol::SCHEMA_VERSION,
        youtube_enabled,
    }
}

/// Hard cap for request bodies (pairing + library import). A library export is
/// only paths/metadata, so this is generous; it exists purely to stop a peer
/// (or anyone reaching the socket before auth) from streaming gigabytes and
/// exhausting memory. The body is bounded here, *before* the bearer check on
/// `/pair`, so the cap holds pre-auth.
const MAX_BODY: usize = 64 * 1024 * 1024;
/// Cap for the request head (request line + headers).
const MAX_HEADER: usize = 64 * 1024;
/// Per-connection read/write timeout, so a slow/stuck peer cannot pin a worker.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound for a single streamed `/files/put` upload (sanity cap, not memory:
/// the body is streamed straight to disk).
const MAX_PUT: usize = 16 * 1024 * 1024 * 1024;

/// Running sync server with a fresh TLS identity and session token.
pub struct SyncServer {
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    identity: crypto::ServerIdentity,
    pairing_token: String,
    session_token: String,
    device_name: String,
    /// Capabilities advertised to the peer in the pair response.
    caps: Capabilities,
    host: String,
    port: u16,
    expires_at: u64,
    stop: Arc<AtomicBool>,
    /// UI→server share handshake state (offer / decision the UI parks).
    share: Arc<Mutex<ShareChannel>>,
}

enum Action {
    Continue,
    Stop,
}

/// A parsed HTTP/1.1 request: just what the dispatch needs.
struct HttpReq {
    method: String,
    /// Request target without the query string.
    path: String,
    /// Raw query string (after `?`), empty if none.
    query: String,
    headers: Vec<(String, String)>,
    /// Body bytes read so far (for non-streamed endpoints: the full body).
    body: Vec<u8>,
    /// `Content-Length` as advertised (not yet clamped). For the streamed
    /// `/files/put` path the body is read directly off the stream up to this.
    content_length: usize,
}

impl HttpReq {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl SyncServer {
    /// Creates the TLS identity/token and binds the server (with port fallback).
    /// The server is not yet waiting – see [`Self::run`].
    pub fn start(device_name: String, stop: Arc<AtomicBool>) -> Result<Self> {
        let identity = crypto::generate_identity()?;
        let tls = crypto::server_config(&identity)?;

        let mut bound: Option<(TcpListener, u16)> = None;
        let mut port = PORT;
        for _ in 0..PORT_ATTEMPTS {
            match TcpListener::bind(("0.0.0.0", port)) {
                Ok(listener) => {
                    bound = Some((listener, port));
                    break;
                }
                Err(_) => port = port.wrapping_add(1),
            }
        }
        let (listener, port) = bound.ok_or_else(|| anyhow!("no free port for the server"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| anyhow!("listener setup failed: {e}"))?;

        Ok(Self {
            listener,
            tls,
            pairing_token: crypto::generate_token(32),
            session_token: crypto::generate_token(32),
            device_name,
            caps: local_caps(),
            host: super::local_ip(),
            port,
            expires_at: super::now_unix() + QR_TTL.as_secs(),
            stop,
            identity,
            share: Arc::new(Mutex::new(ShareChannel::default())),
        })
    }

    /// Test server on localhost with an OS-assigned free port. This keeps the
    /// end-to-end TLS/pinning path real without depending on the app's preferred
    /// LAN port range being free on the test machine.
    #[cfg(test)]
    fn start_for_test(device_name: String, stop: Arc<AtomicBool>) -> Result<Self> {
        let identity = crypto::generate_identity()?;
        let tls = crypto::server_config(&identity)?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|e| anyhow!("test listener bind failed: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| anyhow!("test listener address failed: {e}"))?
            .port();
        listener
            .set_nonblocking(true)
            .map_err(|e| anyhow!("listener setup failed: {e}"))?;

        Ok(Self {
            listener,
            tls,
            pairing_token: crypto::generate_token(32),
            session_token: crypto::generate_token(32),
            device_name,
            caps: Capabilities {
                schema: protocol::SCHEMA_VERSION,
                youtube_enabled: true,
            },
            host: "127.0.0.1".to_string(),
            port,
            expires_at: super::now_unix() + QR_TTL.as_secs(),
            stop,
            identity,
            share: Arc::new(Mutex::new(ShareChannel::default())),
        })
    }

    /// QR/pairing URL for display.
    pub fn pair_url(&self) -> String {
        protocol::build_pair_url(
            &self.host,
            self.port,
            &self.identity.fingerprint,
            &self.pairing_token,
            self.expires_at,
        )
    }

    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Handle to the UI→server share channel, so the server-side user can park an
    /// outgoing offer or a decision that the polling client picks up.
    pub fn share_channel(&self) -> Arc<Mutex<ShareChannel>> {
        self.share.clone()
    }

    /// Blocking accept loop. Reports events via `emit`. Returns
    /// when the stop flag is set, a timeout fires or the
    /// peer drops the connection.
    pub fn run<F: FnMut(SyncEvent)>(self, mut emit: F) {
        let deadline = Instant::now() + QR_TTL; // until pairing
        let mut paired = false;
        let mut session_deadline: Option<Instant> = None;
        let mut failed: u32 = 0;
        let mut peer_name = String::new();
        let mut peer_caps = Capabilities::default();

        loop {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            if !paired && Instant::now() > deadline {
                break; // nobody paired
            }
            if let Some(dl) = session_deadline {
                if Instant::now() > dl {
                    emit(SyncEvent::PeerDisconnected);
                    break;
                }
            }

            match self.listener.accept() {
                Ok((sock, _addr)) => {
                    let action = self.serve_connection(
                        sock,
                        &mut paired,
                        &mut failed,
                        &mut peer_name,
                        &mut peer_caps,
                        &mut emit,
                    );
                    match action {
                        Action::Stop => break,
                        Action::Continue => {
                            if paired {
                                session_deadline = Some(Instant::now() + SESSION_TIMEOUT);
                            }
                        }
                    }
                }
                // No pending connection within the poll interval → re-check flags.
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(_) => break,
            }
        }
    }

    /// TLS-wraps one accepted socket, reads a single request and dispatches it.
    /// Connection-level errors (handshake failure, e.g. a client whose pinning
    /// rejected our cert; or a malformed request) simply drop the connection.
    fn serve_connection<F: FnMut(SyncEvent)>(
        &self,
        mut sock: TcpStream,
        paired: &mut bool,
        failed: &mut u32,
        peer_name: &mut String,
        peer_caps: &mut Capabilities,
        emit: &mut F,
    ) -> Action {
        // The listener is non-blocking; the accepted socket must block for the
        // handshake and request I/O. Bound it with timeouts against slow peers.
        let _ = sock.set_nonblocking(false);
        let _ = sock.set_read_timeout(Some(IO_TIMEOUT));
        let _ = sock.set_write_timeout(Some(IO_TIMEOUT));

        let mut conn = match rustls::ServerConnection::new(self.tls.clone()) {
            Ok(c) => c,
            Err(_) => return Action::Continue,
        };
        let mut tls = rustls::Stream::new(&mut conn, &mut sock);

        // Read only the head first: a `/files/put` body may be gigabytes and must
        // be streamed to disk, never buffered (unlike the small JSON bodies).
        let mut req = match read_head(&mut tls) {
            Ok(req) => req,
            Err(_) => return Action::Continue,
        };
        let action = if req.method == "POST" && req.path == "/files/put" {
            self.handle_put(&req, &mut tls)
        } else {
            if read_body_fully(&mut tls, &mut req).is_err() {
                return Action::Continue;
            }
            self.dispatch(&req, &mut tls, paired, failed, peer_name, peer_caps, emit)
        };
        // Best-effort clean TLS shutdown.
        conn.send_close_notify();
        let _ = conn.complete_io(&mut sock);
        action
    }

    fn dispatch<F: FnMut(SyncEvent)>(
        &self,
        req: &HttpReq,
        out: &mut impl Write,
        paired: &mut bool,
        failed: &mut u32,
        peer_name: &mut String,
        peer_caps: &mut Capabilities,
        emit: &mut F,
    ) -> Action {
        match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/pair") => match serde_json::from_slice::<PairRequest>(&req.body) {
                Ok(pr) if crypto::constant_eq(&pr.token, &self.pairing_token) => {
                    *paired = true;
                    *peer_name = pr.device_name.clone();
                    *peer_caps = pr.caps.clone();
                    emit(SyncEvent::PeerPaired {
                        peer_name: pr.device_name,
                        peer_caps: pr.caps,
                    });
                    write_json(
                        out,
                        200,
                        &PairResponse {
                            ok: true,
                            session_token: self.session_token.clone(),
                            device_name: self.device_name.clone(),
                            caps: self.caps.clone(),
                            error: String::new(),
                        },
                    );
                    Action::Continue
                }
                _ => {
                    *failed += 1;
                    write_json(
                        out,
                        403,
                        &PairResponse {
                            ok: false,
                            error: "invalid token".into(),
                            ..Default::default()
                        },
                    );
                    if *failed >= super::MAX_FAILED_PAIR {
                        emit(SyncEvent::Error("too many pairing attempts".into()));
                        Action::Stop
                    } else {
                        Action::Continue
                    }
                }
            },

            // From here on: bearer authentication required.
            _ if !self.bearer_ok(req) => {
                write_status(out, 401);
                Action::Continue
            }

            ("GET", "/ping") => {
                write_json(out, 200, &serde_json::json!({ "ok": true }));
                Action::Continue
            }

            ("GET", "/sync/export") => {
                match Library::open().and_then(|lib| data::export_library(&lib)) {
                    Ok(exp) => write_json(out, 200, &exp),
                    Err(e) => write_json(out, 500, &serde_json::json!({ "error": e.to_string() })),
                }
                Action::Continue
            }

            ("POST", "/sync/import") => {
                let result = serde_json::from_slice(&req.body)
                    .map_err(anyhow::Error::from)
                    .and_then(|exp| {
                        let lib = Library::open()?;
                        data::import_library(&lib, &exp)
                    });
                match result {
                    Ok(stats) => {
                        emit(SyncEvent::ImportReceived {
                            stats: stats.clone(),
                        });
                        write_json(out, 200, &stats);
                    }
                    Err(e) => write_json(out, 400, &serde_json::json!({ "error": e.to_string() })),
                }
                Action::Continue
            }

            ("GET", "/files/list") => {
                match Library::open().and_then(|lib| data::export_library(&lib)) {
                    Ok(exp) => write_json(out, 200, &serde_json::json!({ "files": exp.files })),
                    Err(e) => write_json(out, 500, &serde_json::json!({ "error": e.to_string() })),
                }
                Action::Continue
            }

            ("GET", "/files/get") => {
                self.serve_file(out, &req.query);
                Action::Continue
            }

            // --- Selective share handshake ---
            // Client polls for an offer the server-side user parked (server=sender).
            ("GET", "/share/offer") => {
                match self.share.lock().ok().and_then(|c| c.outgoing.clone()) {
                    Some(m) => write_json(out, 200, &m),
                    None => write_status(out, 204),
                }
                Action::Continue
            }
            // Client offers a share to the server-side user (client=sender).
            ("POST", "/share/offer") => {
                match serde_json::from_slice::<ShareManifest>(&req.body) {
                    Ok(m) => {
                        emit(SyncEvent::ShareOffered { manifest: m });
                        write_json(out, 200, &serde_json::json!({ "ok": true }));
                    }
                    Err(e) => write_json(out, 400, &serde_json::json!({ "error": e.to_string() })),
                }
                Action::Continue
            }
            // Client polls for the server-side user's decision on its offer.
            ("GET", "/share/decision") => {
                match self.share.lock().ok().and_then(|c| c.decision.clone()) {
                    Some(d) => write_json(out, 200, &d),
                    None => write_status(out, 204),
                }
                Action::Continue
            }
            // Client sends its decision on the server's offer (server=sender).
            ("POST", "/share/decision") => {
                match serde_json::from_slice::<ShareDecision>(&req.body) {
                    Ok(d) => {
                        // The offer has been delivered and decided on: drop it so a
                        // re-polling client can't pick up the same offer twice.
                        if let Ok(mut c) = self.share.lock() {
                            c.outgoing = None;
                        }
                        emit(SyncEvent::OfferAccepted { decision: d });
                        write_json(out, 200, &serde_json::json!({ "ok": true }));
                    }
                    Err(e) => write_json(out, 400, &serde_json::json!({ "error": e.to_string() })),
                }
                Action::Continue
            }

            // Client signals a finished share (after uploading or pulling all
            // files) so the (passive) server side can show its success screen.
            ("POST", "/share/complete") => {
                let files = serde_json::from_slice::<serde_json::Value>(&req.body)
                    .ok()
                    .and_then(|v| v.get("files").and_then(|f| f.as_u64()))
                    .unwrap_or(0) as usize;
                emit(SyncEvent::TransferDone { files });
                write_json(out, 200, &serde_json::json!({ "ok": true }));
                Action::Continue
            }

            ("POST", "/disconnect") => {
                write_json(out, 200, &serde_json::json!({ "ok": true }));
                emit(SyncEvent::PeerDisconnected);
                Action::Stop
            }

            _ => {
                write_status(out, 404);
                Action::Continue
            }
        }
    }

    fn bearer_ok(&self, req: &HttpReq) -> bool {
        let expected = format!("Bearer {}", self.session_token);
        req.header("Authorization")
            .is_some_and(|v| crypto::constant_eq(v.trim(), &expected))
    }

    /// Serves an audio file from the music folder (with path-traversal protection).
    fn serve_file(&self, out: &mut impl Write, query: &str) {
        let rel = query
            .split('&')
            .find_map(|p| p.strip_prefix("path="))
            .map(protocol::percent_decode)
            .unwrap_or_default();

        let music_dir = Library::open()
            .ok()
            .and_then(|lib| lib.get_setting("music_dir").ok().flatten())
            .unwrap_or_default();

        match crate::core::sync::resolve_existing(&music_dir, &rel) {
            Some(abs) => write_file(out, &abs),
            None => write_status(out, 403),
        }
    }

    /// Streams a `/files/put` upload body straight to a file in the music folder
    /// (bearer-checked, path-traversal protected, atomic via `.part` → rename).
    /// The body is never buffered in memory.
    fn handle_put<S: Read + Write>(&self, req: &HttpReq, stream: &mut S) -> Action {
        if !self.bearer_ok(req) {
            write_status(stream, 401);
            return Action::Continue;
        }
        let rel = req
            .query
            .split('&')
            .find_map(|p| p.strip_prefix("path="))
            .map(protocol::percent_decode)
            .unwrap_or_default();
        let lib = Library::open().ok();
        let music_dir = lib
            .as_ref()
            .and_then(|l| l.get_setting("music_dir").ok().flatten())
            .unwrap_or_default();
        let Some(dest) = crate::core::sync::resolve_new(&music_dir, &rel) else {
            write_status(stream, 403);
            return Action::Continue;
        };
        let limit = req.content_length.min(MAX_PUT);
        match stream_to_file(stream, &req.body, limit, &dest) {
            Ok(n) => {
                // Read in and sort the freshly received file into the library from
                // its own tags (same as the client-as-receiver path), so it is
                // indexed exactly like a normal scan. Memos are not music — they
                // land in the memo store and are registered by the metadata apply.
                if let Some(lib) = &lib {
                    if !rel.starts_with(crate::core::sync::MEMO_PREFIX) {
                        crate::core::scanner::ingest_file(lib, &dest);
                    }
                }
                write_json(
                    stream,
                    200,
                    &serde_json::json!({ "ok": true, "written": n }),
                )
            }
            Err(_) => write_json(stream, 400, &serde_json::json!({ "error": "write failed" })),
        }
        Action::Continue
    }
}

/// Streams up to `limit` bytes (the already-buffered `leftover` first, then more
/// from `reader`) into `dest`, atomically via a `.part` file.
fn stream_to_file(
    reader: &mut impl Read,
    leftover: &[u8],
    limit: usize,
    dest: &Path,
) -> Result<u64> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("part");
    let mut file = std::fs::File::create(&tmp)?;
    let mut written = 0usize;
    let take = leftover.len().min(limit);
    file.write_all(&leftover[..take])?;
    written += take;
    let mut buf = [0u8; 64 * 1024];
    while written < limit {
        let want = (limit - written).min(buf.len());
        let n = reader.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        written += n;
    }
    file.sync_all().ok();
    if written == 0 {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow!("empty upload"));
    }
    std::fs::rename(&tmp, dest)?;
    Ok(written as u64)
}

/// Reads the request **head** (request line + headers) from `stream` (drives the
/// TLS handshake on first read). `body` holds only the bytes already buffered
/// past the header block; `content_length` is the advertised length (unclamped).
/// The caller fills the body ([`read_body_fully`]) or streams it (`/files/put`).
fn read_head(stream: &mut impl Read) -> Result<HttpReq> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > MAX_HEADER {
            return Err(anyhow!("request header too large"));
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(anyhow!("connection closed before headers"));
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut parsed = httparse::Request::new(&mut headers);
    if parsed.parse(&buf[..head_end])?.is_partial() {
        return Err(anyhow!("incomplete request head"));
    }
    let method = parsed.method.unwrap_or("").to_string();
    let target = parsed.path.unwrap_or("");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };
    let headers: Vec<(String, String)> = parsed
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_string(),
                String::from_utf8_lossy(h.value).into_owned(),
            )
        })
        .collect();

    let content_length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);

    Ok(HttpReq {
        method,
        path,
        query,
        headers,
        body: buf[head_end..].to_vec(),
        content_length,
    })
}

/// Fills `req.body` up to `Content-Length`, clamped to `MAX_BODY` (small JSON
/// bodies). For the streamed `/files/put` path this is **not** called.
fn read_body_fully(stream: &mut impl Read, req: &mut HttpReq) -> Result<()> {
    let target = req.content_length.min(MAX_BODY);
    if req.body.len() > target {
        req.body.truncate(target);
    }
    let mut tmp = [0u8; 4096];
    while req.body.len() < target {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        let take = (target - req.body.len()).min(n);
        req.body.extend_from_slice(&tmp[..take]);
    }
    Ok(())
}

/// Full read of a request (head + body) — used by the tests and any caller that
/// wants the whole body in memory.
#[cfg(test)]
fn read_request(stream: &mut impl Read) -> Result<HttpReq> {
    let mut req = read_head(stream)?;
    read_body_fully(stream, &mut req)?;
    Ok(req)
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

/// Writes a complete `Connection: close` response with a body.
fn write_response(out: &mut impl Write, status: u16, content_type: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n",
        reason = reason_phrase(status),
        len = body.len(),
    );
    if out.write_all(head.as_bytes()).is_ok() {
        let _ = out.write_all(body);
        let _ = out.flush();
    }
}

fn write_json<S: Serialize>(out: &mut impl Write, status: u16, body: &S) {
    let json = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    write_response(out, status, "application/json", &json);
}

fn write_status(out: &mut impl Write, status: u16) {
    write_response(out, status, "text/plain", b"");
}

/// Streams a file as the response body (Content-Length = file size).
fn write_file(out: &mut impl Write, path: &Path) {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return write_status(out, 404),
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let head = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/octet-stream\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n",
    );
    if out.write_all(head.as_bytes()).is_ok() {
        let _ = std::io::copy(&mut file, out);
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::sync::client::SyncClient;

    /// End-to-end: real TLS handshake + fingerprint pinning + token pairing.
    #[test]
    fn pairing_handshake_with_pinning() {
        let stop = Arc::new(AtomicBool::new(false));
        let server = SyncServer::start_for_test("TestServer".to_string(), stop.clone())
            .expect("server starts");
        let url = server.pair_url();
        let handle = std::thread::spawn(move || server.run(|_| {}));

        let info = protocol::parse_pair_url(&url, super::super::now_unix()).expect("URL");

        // Correct fingerprint + token → pairing succeeds.
        let client_caps = Capabilities {
            schema: protocol::SCHEMA_VERSION,
            youtube_enabled: false,
        };
        let mut client = SyncClient::new(&info, "dev-1".into(), "TestClient".into(), client_caps);
        client.pair(&info.token).expect("pairing succeeds");
        assert_eq!(client.peer_name, "TestServer");
        // The server advertised its capabilities back (test server has YT on).
        assert!(
            client.peer_caps.youtube_enabled,
            "peer caps must round-trip"
        );

        // Wrong fingerprint → TLS pinning rejects (fails before the token).
        let mut bad = info.clone();
        bad.fingerprint = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into();
        let mut bad_client = SyncClient::new(
            &bad,
            "dev-2".into(),
            "Boese".into(),
            Capabilities::default(),
        );
        assert!(bad_client.pair(&bad.token).is_err(), "MITM must fail");

        stop.store(true, Ordering::Relaxed);
        client.disconnect();
        let _ = handle.join();
    }

    /// Offer/decision handshake over real TLS (file bytes themselves need a
    /// configured music_dir and are covered by manual end-to-end testing).
    #[test]
    fn share_offer_decision_roundtrip() {
        let stop = Arc::new(AtomicBool::new(false));
        let server = SyncServer::start_for_test("Srv".to_string(), stop.clone()).expect("server");
        let url = server.pair_url();
        let share = server.share_channel();
        let handle = std::thread::spawn(move || server.run(|_| {}));

        let info = protocol::parse_pair_url(&url, super::super::now_unix()).expect("URL");
        let mut client = SyncClient::new(&info, "d".into(), "Cli".into(), Capabilities::default());
        client.pair(&info.token).expect("pair");

        // Nothing on offer yet.
        assert!(client.fetch_offer().expect("fetch").is_none());

        // Server-side user parks an outgoing offer → client picks it up.
        share.lock().unwrap().outgoing = Some(ShareManifest {
            device_name: "Srv".into(),
            total_size: 42,
            ..Default::default()
        });
        let got = client.fetch_offer().expect("fetch").expect("offer");
        assert_eq!(got.total_size, 42);

        // Client returns a decision (server emits OfferAccepted; 200 expected).
        client
            .send_decision(&ShareDecision {
                accept: true,
                files: vec!["a.mp3".into()],
                ..Default::default()
            })
            .expect("decision");

        // Client-as-sender can post an offer; keep-alive ping works.
        client.send_offer(&ShareManifest::default()).expect("offer");
        client.ping().expect("ping");

        stop.store(true, Ordering::Relaxed);
        client.disconnect();
        let _ = handle.join();
    }

    #[test]
    fn parses_request_line_headers_and_body() {
        let raw = b"POST /sync/import?x=1 HTTP/1.1\r\n\
                    Host: localhost\r\n\
                    Authorization: Bearer abc\r\n\
                    Content-Length: 5\r\n\r\nhello";
        let mut cursor = std::io::Cursor::new(raw.to_vec());
        let req = read_request(&mut cursor).expect("parses");
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/sync/import");
        assert_eq!(req.query, "x=1");
        assert_eq!(req.header("Authorization"), Some("Bearer abc"));
        assert_eq!(req.body, b"hello");
    }

    #[test]
    fn body_is_capped_to_content_length() {
        // Extra bytes beyond Content-Length must not be read into the body.
        let raw = b"GET /ping HTTP/1.1\r\nContent-Length: 3\r\n\r\nABCDEFG";
        let mut cursor = std::io::Cursor::new(raw.to_vec());
        let req = read_request(&mut cursor).expect("parses");
        assert_eq!(req.body, b"ABC");
    }
}
