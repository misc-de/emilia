//! In-app self-update for the Flatpak build.
//!
//! Two halves: a cheap **version check** (compare the version baked into this
//! build against the newest released version on `main`) and a sandbox-safe
//! **install** via the Flatpak portal (`org.freedesktop.portal.Flatpak`), the
//! only way an app inside the sandbox can update itself. Outside Flatpak the
//! whole feature is a no-op (there is nothing to self-update).

use anyhow::Result;

/// The bundled metainfo, so the build's marketing version is available offline.
const METAINFO: &str = include_str!("../../data/de.cais.Emilia.metainfo.xml");

/// Newest released marketing version of `main`, fetched from the metainfo on
/// GitHub (raw). This is the same file [`current_version`] reads at build time,
/// so the two are directly comparable.
const LATEST_METAINFO_URL: &str =
    "https://raw.githubusercontent.com/misc-de/emilia/main/data/de.cais.Emilia.metainfo.xml";

/// Extracts the newest `<release version="…">` from a metainfo document.
fn top_release_version(metainfo: &str) -> Option<&str> {
    // Anchor on `<release version="` (not `<release`) so the `<releases>` wrapper
    // tag is never matched.
    metainfo
        .split("<release version=\"")
        .nth(1)?
        .split('"')
        .next()
}

/// The marketing version this binary was built at — the newest `<release>` in
/// the bundled metainfo. Deliberately **not** `CARGO_PKG_VERSION` (that is the
/// per-commit dev counter, which races ahead of the real release number).
pub fn current_version() -> &'static str {
    top_release_version(METAINFO).unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// Running inside the Flatpak sandbox? Self-update only applies there.
pub fn in_flatpak() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

/// Fetches the newest released version from `main`. `None` on any network or
/// parse error (treated as "no update known").
fn fetch_latest_version() -> Option<String> {
    let body = ureq::get(LATEST_METAINFO_URL)
        .timeout(std::time::Duration::from_secs(10))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    top_release_version(&body).map(str::to_string)
}

/// `true` if dotted version `a` is strictly newer than `b` (numeric per field).
fn is_newer(a: &str, b: &str) -> bool {
    let parts = |s: &str| {
        s.split('.')
            .map(|x| x.parse::<u32>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    parts(a) > parts(b)
}

/// The newer version string if an update is available, else `None`. Only ever
/// reports inside Flatpak. Runs the network fetch, so call it off the UI thread.
pub fn check() -> Option<String> {
    if !in_flatpak() {
        return None;
    }
    let latest = fetch_latest_version()?;
    is_newer(&latest, current_version()).then_some(latest)
}

/// Asks the Flatpak portal to update this app from its remote. The download runs
/// in the background and the new version becomes active on the next start.
/// **Blocking** (a D-Bus round-trip) — run off the UI thread. Errors (no portal,
/// too-old host) propagate so the caller can fall back to a manual hint.
pub fn request_update() -> Result<()> {
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedObjectPath, Value};

    let conn = zbus::blocking::Connection::session()?;
    let portal = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.portal.Flatpak",
        "/org/freedesktop/portal/Flatpak",
        "org.freedesktop.portal.Flatpak",
    )?;
    // CreateUpdateMonitor(a{sv} options) -> o handle
    let opts: HashMap<String, Value> = HashMap::new();
    let monitor: OwnedObjectPath = portal.call("CreateUpdateMonitor", &(opts,))?;

    // UpdateMonitor.Update(s parent_window, a{sv} options); empty parent + opts.
    let mon = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.portal.Flatpak",
        monitor.as_str(),
        "org.freedesktop.portal.Flatpak.UpdateMonitor",
    )?;
    let upd_opts: HashMap<String, Value> = HashMap::new();
    mon.call::<_, _, ()>("Update", &("", upd_opts))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_top_release() {
        // The bundled metainfo must expose a sane current version.
        let v = current_version();
        assert!(v.split('.').all(|p| p.parse::<u32>().is_ok()), "got {v}");
    }

    #[test]
    fn version_ordering() {
        assert!(is_newer("0.6.5", "0.6.4"));
        assert!(is_newer("0.7.0", "0.6.9"));
        assert!(is_newer("0.6.10", "0.6.9"));
        assert!(!is_newer("0.6.4", "0.6.4"));
        assert!(!is_newer("0.6.3", "0.6.4"));
    }
}
