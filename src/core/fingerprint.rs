//! Audio-Fingerprint via **Chromaprint** (`fpcalc`-Binary).
//!
//! Liest die Datei nur (dekodiert sie zur Analyse) – verändert sie nie. Gibt
//! Dauer (Sekunden) und Fingerprint zurück, die anschließend an AcoustID gehen.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Result};
use serde::Deserialize;

/// Ergebnis von `fpcalc -json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Fingerprint {
    /// Länge in Sekunden (von AcoustID benötigt).
    pub duration: f64,
    /// Komprimierter Chromaprint-Fingerprint (base64-ähnlich).
    pub fingerprint: String,
}

/// Prüft, ob `fpcalc` (Chromaprint) im Pfad verfügbar ist.
pub fn available() -> bool {
    Command::new("fpcalc")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Berechnet den Chromaprint-Fingerprint einer Audiodatei.
pub fn compute(path: &Path) -> Result<Fingerprint> {
    let output = Command::new("fpcalc").arg("-json").arg(path).output()?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("fpcalc failed: {}", err.trim()));
    }
    let fp: Fingerprint = serde_json::from_slice(&output.stdout)?;
    Ok(fp)
}
