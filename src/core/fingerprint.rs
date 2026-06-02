//! Audio fingerprint via **Chromaprint** (`fpcalc` binary).
//!
//! Only reads the file (decodes it for analysis) – never modifies it. Returns
//! duration (seconds) and fingerprint, which subsequently go to AcoustID.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Result};
use serde::Deserialize;

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
    Command::new("fpcalc")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Computes the Chromaprint fingerprint of an audio file.
pub fn compute(path: &Path) -> Result<Fingerprint> {
    let output = Command::new("fpcalc").arg("-json").arg(path).output()?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("fpcalc failed: {}", err.trim()));
    }
    let fp: Fingerprint = serde_json::from_slice(&output.stdout)?;
    Ok(fp)
}
