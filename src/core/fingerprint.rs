//! Audio fingerprint via **Chromaprint** (`fpcalc` binary).
//!
//! Only reads the file (decodes it for analysis) – never modifies it. Returns
//! duration (seconds) and fingerprint, which subsequently go to AcoustID.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::core::proc;

/// `fpcalc -version` is a trivial probe; a few seconds is plenty.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// `fpcalc` only fingerprints the first ~2 min of audio, so even long
/// audiobook files finish quickly; this is a generous backstop against a
/// binary that hangs on a corrupt/undecodable input.
const COMPUTE_TIMEOUT: Duration = Duration::from_secs(120);

/// Result of `fpcalc -json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Fingerprint {
    /// Length in seconds (required by AcoustID).
    pub duration: f64,
    /// Compressed Chromaprint fingerprint (base64-like).
    pub fingerprint: String,
}

/// Checks whether `fpcalc` (Chromaprint) is available in the path.
pub fn available() -> bool {
    let mut cmd = Command::new("fpcalc");
    cmd.arg("-version");
    proc::output_timeout(&mut cmd, PROBE_TIMEOUT)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Computes the Chromaprint fingerprint of an audio file.
pub fn compute(path: &Path) -> Result<Fingerprint> {
    let mut cmd = Command::new("fpcalc");
    cmd.arg("-json").arg(path);
    let output = proc::output_timeout(&mut cmd, COMPUTE_TIMEOUT)?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("fpcalc failed: {}", err.trim()));
    }
    let fp: Fingerprint = serde_json::from_slice(&output.stdout)?;
    Ok(fp)
}
