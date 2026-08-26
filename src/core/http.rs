//! Minimal blocking HTTP/1.1 read/write helpers.
//!
//! Shared by the two blocking servers Emilia embeds — device sync
//! ([`crate::core::sync::server`]) and the lean MCP JSON-RPC backend
//! ([`crate::core::mcp::server_jsonrpc`]): read the request head with
//! `httparse`, read a length-bounded body, write a `Connection: close`
//! response. One request per connection — no keep-alive, which keeps both
//! servers free of pipelining edge cases. Body caps stay with the callers
//! (`max_body`), since a sync file upload and a JSON-RPC call want very
//! different limits.

use std::io::{Read, Write};

use anyhow::{anyhow, Result};
use serde::Serialize;

/// Cap for the request head (request line + headers).
const MAX_HEADER: usize = 64 * 1024;

/// A parsed HTTP/1.1 request: just what the dispatch needs.
pub struct HttpReq {
    pub method: String,
    /// Request target without the query string.
    pub path: String,
    /// Raw query string (after `?`), empty if none. Unused by the MCP endpoint,
    /// which takes no query parameters.
    pub query: String,
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

/// Full read of a request (head + body) — for tests and any caller that wants
/// the whole body in memory in one call.
#[cfg(test)]
pub fn read_request(stream: &mut impl Read, max_body: usize) -> Result<HttpReq> {
    let mut req = read_head(stream)?;
    read_body_fully(stream, &mut req, max_body)?;
    Ok(req)
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
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that hands out the payload in fixed-size slices, so the parser is
    /// exercised across read boundaries the way a real socket delivers data
    /// (a header block split mid-token is the classic parser bug).
    struct Chunked {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }

    impl Chunked {
        fn new(data: impl Into<Vec<u8>>, chunk: usize) -> Self {
            Self {
                data: data.into(),
                pos: 0,
                chunk,
            }
        }
    }

    impl Read for Chunked {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = self
                .chunk
                .min(out.len())
                .min(self.data.len().saturating_sub(self.pos));
            out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn post(body: &str) -> String {
        format!(
            "POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn parses_a_normal_request() {
        let raw = post(r#"{"jsonrpc":"2.0"}"#);
        let mut s = Chunked::new(raw.clone(), 4096);
        let mut req = read_head(&mut s).expect("head");
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/mcp");
        assert_eq!(req.content_length, 17);
        assert_eq!(req.header("host"), Some("x"), "lookup is case-insensitive");
        read_body_fully(&mut s, &mut req, 8192).unwrap();
        assert_eq!(req.body, br#"{"jsonrpc":"2.0"}"#);
    }

    /// One byte at a time: the head must still parse identically.
    #[test]
    fn head_split_across_reads_still_parses() {
        let raw = post("hello");
        for chunk in [1, 2, 3, 7, 64] {
            let mut s = Chunked::new(raw.clone(), chunk);
            let mut req = read_head(&mut s).unwrap_or_else(|e| panic!("chunk {chunk}: {e}"));
            read_body_fully(&mut s, &mut req, 8192).unwrap();
            assert_eq!(req.method, "POST", "chunk {chunk}");
            assert_eq!(req.body, b"hello", "chunk {chunk}");
        }
    }

    /// The path must never carry the query string (the dispatches match on it
    /// exactly); the query itself is kept for the callers that read parameters.
    #[test]
    fn query_string_is_split_off_the_path() {
        let raw = "GET /health?verbose=1&x=2 HTTP/1.1\r\nHost: x\r\n\r\n";
        let mut s = Chunked::new(raw, 4096);
        let req = read_head(&mut s).unwrap();
        assert_eq!(req.path, "/health");
        assert_eq!(req.query, "verbose=1&x=2");

        // No "?" at all → empty query, path untouched.
        let mut s = Chunked::new("GET /health HTTP/1.1\r\nHost: x\r\n\r\n", 4096);
        let req = read_head(&mut s).unwrap();
        assert_eq!(req.path, "/health");
        assert_eq!(req.query, "");
    }

    /// An endless header block must be refused by the cap, not buffered forever.
    #[test]
    fn oversized_header_is_refused_and_bounded() {
        // Never sends the terminating blank line.
        let flood = format!(
            "GET / HTTP/1.1\r\n{}",
            "X-Pad: aaaaaaaaaaaaaaaa\r\n".repeat(8000)
        );
        assert!(flood.len() > MAX_HEADER);
        let mut s = Chunked::new(flood, 4096);
        assert!(
            read_head(&mut s).is_err(),
            "must not accept an unterminated head"
        );
    }

    #[test]
    fn truncated_and_empty_inputs_error_instead_of_hanging() {
        // Closed before any header arrived.
        assert!(read_head(&mut Chunked::new("", 16)).is_err());
        // Head begun but never terminated, then EOF.
        assert!(read_head(&mut Chunked::new("POST /mcp HTTP/1.1\r\nHost: x", 16)).is_err());
        // Garbage that is not HTTP at all.
        assert!(read_head(&mut Chunked::new("\x16\x03\x01\x02\x00\r\n\r\n", 16)).is_err());
    }

    /// More headers than the fixed parse buffer holds: an error, never a panic.
    #[test]
    fn too_many_headers_is_an_error_not_a_panic() {
        let many = format!(
            "GET / HTTP/1.1\r\n{}\r\n",
            (0..64).map(|i| format!("X-{i}: v\r\n")).collect::<String>()
        );
        assert!(read_head(&mut Chunked::new(many, 4096)).is_err());
    }

    /// Header values are not required to be UTF-8; they must not panic the parser.
    #[test]
    fn non_utf8_header_value_is_lossy_not_fatal() {
        let mut raw = b"GET / HTTP/1.1\r\nX-Bin: ".to_vec();
        raw.extend_from_slice(&[0xff, 0xfe, 0x80]);
        raw.extend_from_slice(b"\r\n\r\n");
        let req = read_head(&mut Chunked::new(raw, 4096)).expect("head");
        assert!(req.header("x-bin").is_some());
    }

    /// A body shorter than announced ends the read at EOF instead of blocking.
    #[test]
    fn short_body_does_not_hang() {
        let raw = "POST /mcp HTTP/1.1\r\nContent-Length: 1000\r\n\r\nonly-this";
        let mut s = Chunked::new(raw, 4096);
        let mut req = read_head(&mut s).unwrap();
        assert_eq!(req.content_length, 1000);
        read_body_fully(&mut s, &mut req, 8192).unwrap();
        assert_eq!(req.body, b"only-this", "stops at EOF, does not spin");
    }

    /// `max_body` wins over a larger advertised `Content-Length`.
    #[test]
    fn body_is_clamped_to_max_body() {
        let raw = post(&"a".repeat(500));
        let mut s = Chunked::new(raw, 4096);
        let mut req = read_head(&mut s).unwrap();
        read_body_fully(&mut s, &mut req, 100).unwrap();
        assert_eq!(req.body.len(), 100);
    }

    /// A missing or unparsable Content-Length reads no body at all.
    #[test]
    fn absent_or_bogus_content_length_reads_nothing() {
        for head in [
            "POST /mcp HTTP/1.1\r\nHost: x\r\n\r\nignored",
            "POST /mcp HTTP/1.1\r\nContent-Length: banana\r\n\r\nignored",
            "POST /mcp HTTP/1.1\r\nContent-Length: -5\r\n\r\nignored",
        ] {
            let mut s = Chunked::new(head, 4096);
            let mut req = read_head(&mut s).unwrap();
            assert_eq!(req.content_length, 0, "{head:?}");
            read_body_fully(&mut s, &mut req, 8192).unwrap();
            assert!(req.body.is_empty(), "{head:?}");
        }
    }

    #[test]
    fn responses_are_well_formed() {
        let mut out = Vec::new();
        write_json(&mut out, 200, &serde_json::json!({ "ok": true }));
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(s.contains("Content-Type: application/json\r\n"));
        assert!(s.contains("Content-Length: 11\r\n"));
        assert!(s.contains("Connection: close\r\n"));
        assert!(s.ends_with("\r\n\r\n{\"ok\":true}"));

        let mut out = Vec::new();
        write_status(&mut out, 401);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        assert!(s.contains("Content-Length: 0\r\n"));
    }

    /// Writing to a sink that fails must not panic — a peer can vanish mid-write.
    #[test]
    fn write_to_a_broken_sink_is_survivable() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        write_status(&mut Broken, 200);
        write_json(&mut Broken, 200, &serde_json::json!({ "a": 1 }));
    }
}
