//! Shared download helpers: streaming a remote response to disk with a **hard
//! size cap**, so a hostile or broken server can never fill the user's disk.
//!
//! Two layers of protection, used together at each download site:
//! 1. [`check_content_length`] rejects up front when the server *advertises* a
//!    body beyond the limit – cheap, avoids even starting the transfer.
//! 2. [`copy_capped`] streams with a running cap that aborts the moment the
//!    source exceeds the limit – this is what defends against a server that
//!    omits or *lies about* `Content-Length` (e.g. chunked transfer).

use std::io::{Read, Write};
use std::time::Duration;

use anyhow::{bail, Result};
use serde::de::DeserializeOwned;

/// Max attempts before a transient-failure retry gives up (see [`get_with_retry`]).
pub const RETRY_MAX: usize = 4;
/// First backoff delay; doubles each retry, capped at [`RETRY_MAX_BACKOFF`].
pub const RETRY_BASE_BACKOFF: Duration = Duration::from_millis(1500);
pub const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Whether a `ureq` error is a **transient** failure worth retrying: a transport
/// error (connection refused, timeout, DNS, reset) or a server/rate-limit status
/// (`429`/`5xx`). Client `4xx` (other than 429) won't change on a retry. Shared
/// by [`get_with_retry`] and the WebDAV verb-specific retry loops.
pub fn is_transient(err: &ureq::Error) -> bool {
    match err {
        ureq::Error::Transport(_) => true,
        ureq::Error::Status(code, _) => matches!(code, 429 | 500 | 502 | 503 | 504),
    }
}

/// App-wide default `User-Agent`: a plain Chrome string. Several endpoints (e.g.
/// YouTube's feed) reject the library's default UA with a `404`/`403`, so every
/// request sends this unless a caller passes its own (e.g. MusicBrainz, which
/// wants an app-identifying UA).
pub const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Runs a `GET url` on `agent` with defensive retry + exponential backoff
/// against **transient** failures: network/transport errors, server `5xx`, and
/// rate limits (`429`/`503`, honouring `Retry-After`). A `404` maps to
/// `Ok(None)` (no content). Non-transient errors, and a persistent failure after
/// [`RETRY_MAX`] attempts, are returned. `user_agent` is sent on every attempt;
/// when `None`, the app-wide [`BROWSER_UA`] is sent instead of ureq's default.
/// `label` is for logging only — pass a value **without** a query string (it may
/// carry API keys). Performs the request itself (rather than via a closure) so
/// the large `ureq::Error` never crosses a closure boundary.
pub fn get_with_retry(
    agent: &ureq::Agent,
    url: &str,
    user_agent: Option<&str>,
    label: &str,
) -> Result<Option<ureq::Response>> {
    let mut backoff = RETRY_BASE_BACKOFF;
    let mut attempt = 0usize;
    loop {
        let req = agent
            .get(url)
            .set("User-Agent", user_agent.unwrap_or(BROWSER_UA));
        match req.call() {
            Ok(resp) => return Ok(Some(resp)),
            // No content (404) – not an error, and not worth retrying.
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(e) => {
                attempt += 1;
                if !is_transient(&e) || attempt > RETRY_MAX {
                    return Err(e.into());
                }
                // Honour Retry-After on rate limits; otherwise exponential backoff.
                let wait = match &e {
                    ureq::Error::Status(_, resp) => resp
                        .header("Retry-After")
                        .and_then(|s| s.trim().parse::<u64>().ok())
                        .map(Duration::from_secs)
                        .unwrap_or(backoff),
                    _ => backoff,
                }
                .min(RETRY_MAX_BACKOFF);
                tracing::debug!(
                    "transient error on {label} (attempt {attempt}/{RETRY_MAX}); \
                     retrying in {wait:?}: {e}"
                );
                std::thread::sleep(wait);
                backoff = (backoff * 2).min(RETRY_MAX_BACKOFF);
            }
        }
    }
}

/// Ceiling for a single downloaded media file (2 GiB). Generous enough for long
/// / lossless podcast episodes and large remote tracks, but bounds a runaway or
/// malicious download well short of filling a typical disk.
pub const MAX_DOWNLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Rejects a response whose advertised `Content-Length` exceeds `limit`, before
/// any bytes are written. Returns the parsed length when present (`None` if the
/// server sends no/unparsable length – then [`copy_capped`] is the safety net).
pub fn check_content_length(resp: &ureq::Response, limit: u64) -> Result<Option<u64>> {
    match resp
        .header("Content-Length")
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        Some(len) if len > limit => {
            bail!("remote file is {len} bytes, exceeds the {limit}-byte download limit")
        }
        Some(len) => Ok(Some(len)),
        None => Ok(None),
    }
}

/// Ceiling for a JSON API response body (16 MiB). Metadata bodies (MusicBrainz,
/// Deezer, AcoustID, …) are far smaller in practice; this only bounds a hostile
/// or broken server that would otherwise stream an unbounded body into memory.
pub const MAX_JSON_BYTES: u64 = 16 * 1024 * 1024;

/// Deserializes a JSON response body, reading **at most** `limit` bytes. Unlike
/// `ureq::Response::into_json()`, which reads the whole body unbounded, this caps
/// the read so a malicious or broken server cannot exhaust memory. Errors if the
/// body exceeds `limit` or is not valid JSON.
pub fn json_capped<T: DeserializeOwned>(resp: ureq::Response, limit: u64) -> Result<T> {
    let mut buf = Vec::new();
    // `+ 1` so a body of exactly `limit` bytes still parses, while anything
    // larger is detected (we read one byte past the limit, then bail).
    resp.into_reader()
        .take(limit.saturating_add(1))
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > limit {
        bail!("JSON response exceeds the {limit}-byte limit");
    }
    Ok(serde_json::from_slice(&buf)?)
}

/// Streams `reader` into `writer`, reading **at most** `limit` bytes. Returns
/// the number of bytes written, or an error the moment the source exceeds
/// `limit`. Never reads (or writes) more than `limit + 1` bytes, so the partial
/// file a caller must clean up on error is bounded – it does not keep streaming
/// an endless body to disk.
pub fn copy_capped(reader: impl Read, writer: &mut impl Write, limit: u64) -> Result<u64> {
    copy_capped_progress(reader, writer, limit, |_| {})
}

/// Chunk size of the streaming copy – also the granularity at which
/// [`copy_capped_progress`] reports progress.
const COPY_CHUNK: usize = 64 * 1024;

/// Like [`copy_capped`], but calls `on_progress(bytes_written_so_far)` after
/// every chunk, so a caller can drive a progress display. The callback runs on
/// the copying (worker) thread and is invoked often – throttle inside it if the
/// consumer is expensive (see `podcast::download_episode_progress`).
pub fn copy_capped_progress(
    reader: impl Read,
    writer: &mut impl Write,
    limit: u64,
    mut on_progress: impl FnMut(u64),
) -> Result<u64> {
    // `+ 1` so a body of exactly `limit` bytes still succeeds, while anything
    // larger is detected (we read one byte past the limit, then bail).
    let mut limited = reader.take(limit.saturating_add(1));
    let mut buf = vec![0u8; COPY_CHUNK];
    let mut written: u64 = 0;
    loop {
        let n = match limited.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            // A signal interrupted the read – not an error, just retry.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        writer.write_all(&buf[..n])?;
        written += n as u64;
        if written > limit {
            bail!("download exceeds the {limit}-byte size limit");
        }
        on_progress(written);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_within_limit_succeeds() {
        let data = vec![7u8; 1000];
        let mut out = Vec::new();
        let n = copy_capped(&data[..], &mut out, 2000).unwrap();
        assert_eq!(n, 1000);
        assert_eq!(out.len(), 1000);
    }

    #[test]
    fn copy_at_exact_limit_succeeds() {
        let data = vec![1u8; 4096];
        let mut out = Vec::new();
        let n = copy_capped(&data[..], &mut out, 4096).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(out.len(), 4096);
    }

    #[test]
    fn copy_over_limit_errors_and_is_bounded() {
        let data = vec![7u8; 5000];
        let mut out = Vec::new();
        assert!(copy_capped(&data[..], &mut out, 4096).is_err());
        // Never buffered more than limit + 1 bytes to the sink.
        assert!(out.len() <= 4097, "wrote {} bytes", out.len());
    }

    #[test]
    fn progress_reports_monotonic_totals_ending_at_the_size() {
        // Two full chunks plus a partial one, so more than one report lands.
        let data = vec![3u8; COPY_CHUNK * 2 + 17];
        let mut out = Vec::new();
        let mut seen = Vec::new();
        let n =
            copy_capped_progress(&data[..], &mut out, u64::MAX, |done| seen.push(done)).unwrap();
        assert_eq!(n, data.len() as u64);
        assert!(seen.len() >= 3, "only {} reports", seen.len());
        assert!(
            seen.windows(2).all(|w| w[0] < w[1]),
            "not monotonic: {seen:?}"
        );
        assert_eq!(seen.last().copied(), Some(data.len() as u64));
    }
}
