//! Audio-Ausgänge (PipeWire/PulseAudio über `pactl`).
//!
//! Erkennt vorhandene Ausgabegeräte – inklusive bekannter Bluetooth-Lautsprecher
//! (`bluez_output.*`) – und den aktuell aktiven Standard-Ausgang. Wird nur
//! gelesen; an der Audio-Konfiguration des Systems wird nichts verändert.

use std::process::Command;

/// Ein Audio-Ausgang: `id` = stabiler Sink-Name, `name` = Anzeigename.
#[derive(Debug, Clone)]
pub struct Output {
    pub id: String,
    pub name: String,
}

/// Führt `pactl` mit neutraler Locale aus (stabile, englische Feldnamen).
fn pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl")
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Listet alle Ausgabegeräte (Sinks) auf.
pub fn list_outputs() -> Vec<Output> {
    let Some(text) = pactl(&["list", "sinks"]) else {
        return Vec::new();
    };

    let mut outputs = Vec::new();
    let mut pending_id: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Name:") {
            pending_id = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("Description:") {
            if let Some(id) = pending_id.take() {
                let name = rest.trim().to_string();
                let name = if name.is_empty() { id.clone() } else { name };
                outputs.push(Output { id, name });
            }
        }
    }
    outputs
}

/// Aktuell aktiver Standard-Ausgang (Sink-Name), falls ermittelbar.
pub fn default_output() -> Option<String> {
    let s = pactl(&["get-default-sink"])?.trim().to_string();
    if s.is_empty() || s == "@DEFAULT_SINK@" {
        None
    } else {
        Some(s)
    }
}
