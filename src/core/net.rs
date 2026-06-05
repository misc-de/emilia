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

use anyhow::{bail, Result};

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

/// Streams `reader` into `writer`, reading **at most** `limit` bytes. Returns
/// the number of bytes written, or an error the moment the source exceeds
/// `limit`. Never reads (or writes) more than `limit + 1` bytes, so the partial
/// file a caller must clean up on error is bounded – it does not keep streaming
/// an endless body to disk.
pub fn copy_capped(reader: impl Read, writer: &mut impl Write, limit: u64) -> Result<u64> {
    // `+ 1` so a body of exactly `limit` bytes still succeeds, while anything
    // larger is detected (we read one byte past the limit, then bail).
    let mut limited = reader.take(limit.saturating_add(1));
    let n = std::io::copy(&mut limited, writer)?;
    if n > limit {
        bail!("download exceeds the {limit}-byte size limit");
    }
    Ok(n)
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
}
