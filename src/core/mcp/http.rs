//! Minimal blocking HTTP/1.1 read/write helpers for the JSON-RPC backend.
//!
//! A trimmed copy of the device-sync server's request handling
//! ([`crate::core::sync::server`]): read the request head with `httparse`, read a
//! length-bounded body, write a `Connection: close` response. One request per
//! connection — no keep-alive, which keeps the server free of pipelining edge
//! cases. Kept separate so the MCP server does not depend on the sync module's
//! private internals.

use std::io::{Read, Write};

use anyhow::{anyhow, Result};
use serde::Serialize;

/// Cap for the request head (request line + headers).
const MAX_HEADER: usize = 64 * 1024;

/// A parsed HTTP/1.1 request: just what the dispatch needs.
pub struct HttpReq {
    pub method: String,
    /// Request target without the query string (the MCP endpoint takes none).
    pub path: String,
    pub headers: Vec<(String, String)>,
    /// Body bytes (filled by [`read_body_fully`]).
    pub body: Vec<u8>,
    /// `Content-Length` as advertised (not yet clamped).
    pub content_length: usize,
}

impl HttpReq {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Reads the request head (request line + headers); drives the TLS handshake on
/// the first read. `body` holds only the bytes already buffered past the header
/// block; the caller fills the rest with [`read_body_fully`].
pub fn read_head(stream: &mut impl Read) -> Result<HttpReq> {
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
    // Strip any query string — the single MCP endpoint takes no query params.
    let path = target
        .split_once('?')
        .map_or(target, |(p, _)| p)
        .to_string();
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
        headers,
        body: buf[head_end..].to_vec(),
        content_length,
    })
}

/// Fills `req.body` up to `Content-Length`, clamped to `max_body`.
pub fn read_body_fully(stream: &mut impl Read, req: &mut HttpReq, max_body: usize) -> Result<()> {
    let target = req.content_length.min(max_body);
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

/// Writes a complete `Connection: close` response with a body.
pub fn write_response(out: &mut impl Write, status: u16, content_type: &str, body: &[u8]) {
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

/// Serializes `body` to JSON and writes it as the response.
pub fn write_json<S: Serialize>(out: &mut impl Write, status: u16, body: &S) {
    let json = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    write_response(out, status, "application/json", &json);
}

/// Writes a bodyless status response.
pub fn write_status(out: &mut impl Write, status: u16) {
    write_response(out, status, "text/plain", b"");
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    }
}
