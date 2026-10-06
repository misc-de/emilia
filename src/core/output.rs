//! Audio outputs (PipeWire/PulseAudio via `pactl`).
//!
//! Detects available output devices – including known Bluetooth speakers
//! (`bluez_output.*`) – and the currently active default output. Read-only;
//! nothing in the system's audio configuration is modified.

use std::process::Command;

/// An audio output: `id` = stable sink name, `name` = display name.
#[derive(Debug, Clone)]
pub struct Output {
    pub id: String,
    pub name: String,
}

/// Runs `pactl` with a neutral locale (stable, English field names).
fn pactl(args: &[&str]) -> Option<String> {
    // A wedged sound server must not hang the caller.
    let out = crate::core::proc::output_timeout(
        Command::new("pactl").env("LC_ALL", "C").args(args),
        std::time::Duration::from_secs(5),
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Lists all output devices (sinks).
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
        } else if let Some(rest) = line.strip_prefix("Description:")
            && let Some(id) = pending_id.take()
        {
            let name = rest.trim().to_string();
            let name = if name.is_empty() { id.clone() } else { name };
            outputs.push(Output { id, name });
        }
    }
    outputs
}

/// Currently active default output (sink name), if determinable.
pub fn default_output() -> Option<String> {
    let s = pactl(&["get-default-sink"])?.trim().to_string();
    if s.is_empty() || s == "@DEFAULT_SINK@" {
        None
    } else {
        Some(s)
    }
}
