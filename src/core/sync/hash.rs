//! Fast content fingerprint for sync dedup & collision detection:
//! **file size + SHA-256 of the first 1 MiB**. This avoids reading whole audio
//! files (which can be hundreds of MB) on large libraries / phones while still
//! distinguishing different files at the same path reliably in practice.
//!
//! Two files count as "the same content" iff *both* the size and the prefix
//! hash match. A change beyond the first 1 MiB is therefore not detected — an
//! accepted, documented trade-off for speed.

use std::io::Read;
use std::path::Path;

use anyhow::Result;
use sha2::{Digest, Sha256};

/// Bytes hashed from the head of the file (1 MiB).
pub const QUICK_HASH_PREFIX: u64 = 1024 * 1024;

/// Returns `(size_in_bytes, lowercase_hex_sha256_of_first_1MiB)`.
pub fn quick_hash(path: &Path) -> Result<(u64, String)> {
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut hasher = Sha256::new();
    let mut limited = file.take(QUICK_HASH_PREFIX);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = limited.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok((size, to_hex(&hasher.finalize())))
}

/// Lowercase hex of a byte slice (no `hex` crate dependency).
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("emilia-hash-{}-{name}", std::process::id()));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    #[test]
    fn hash_is_stable_and_size_correct() {
        let p = tmp("a", &vec![7u8; 3000]);
        let (s1, h1) = quick_hash(&p).unwrap();
        let (s2, h2) = quick_hash(&p).unwrap();
        assert_eq!(s1, 3000);
        assert_eq!(s2, 3000);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // sha256 hex
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn change_within_prefix_changes_hash_beyond_does_not() {
        // > 1 MiB so we can mutate both inside and outside the hashed prefix.
        let mut base = vec![1u8; (QUICK_HASH_PREFIX as usize) + 4096];
        let inside = tmp("inside", &base);
        let (_, h_base) = quick_hash(&inside).unwrap();

        base[(QUICK_HASH_PREFIX as usize) + 10] = 9; // beyond the prefix
        let beyond = tmp("beyond", &base);
        let (_, h_beyond) = quick_hash(&beyond).unwrap();
        assert_eq!(
            h_base, h_beyond,
            "change beyond 1 MiB is intentionally invisible"
        );

        base[100] = 9; // inside the prefix
        let within = tmp("within", &base);
        let (_, h_within) = quick_hash(&within).unwrap();
        assert_ne!(h_base, h_within, "change inside 1 MiB must change the hash");

        for p in [inside, beyond, within] {
            let _ = std::fs::remove_file(p);
        }
    }
}
