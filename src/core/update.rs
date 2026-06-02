//! Self-update of the **Flatpak** version.
//!
//! - **Check** (immediately, without waiting for the portal): compare the locally
//!   installed OSTree commit (from `/.flatpak-info`) with the latest commit in the
//!   published repo (via HTTP).
//! - **Install**: via the **Flatpak portal**
//!   (`org.freedesktop.portal.Flatpak`) – the only way to update oneself that is
//!   permitted from within the sandbox. The actual download runs in the
//!   background; the portal reports progress via a signal. After completion the
//!   app must be restarted.
//!
//! Outside of Flatpak (e.g. `cargo run`) none of this is available.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use gtk::gio;
use gtk::glib;

/// Base URL of the published OSTree repo (see `data/de.cais.Emilia.flatpakrepo`).
const REPO_BASE: &str = "https://misc-de.github.io/emilia/repo";

const PORTAL_NAME: &str = "org.freedesktop.portal.Flatpak";
const PORTAL_PATH: &str = "/org/freedesktop/portal/Flatpak";
const PORTAL_IFACE: &str = "org.freedesktop.portal.Flatpak";
const MONITOR_IFACE: &str = "org.freedesktop.portal.Flatpak.UpdateMonitor";

/// Is the app running as a Flatpak? Only then is self-update possible.
pub fn in_flatpak() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

/// Key data of the running instance read from `/.flatpak-info`.
pub struct AppInfo {
    pub id: String,
    pub arch: String,
    pub branch: String,
    /// Currently running OSTree commit (full 64-hex), if determinable.
    pub commit: Option<String>,
}

/// Reads `/.flatpak-info` (INI-like): app id, architecture, branch and the
/// running commit (preferring `app-commit`, otherwise from the deploy path).
pub fn app_info() -> Option<AppInfo> {
    let text = std::fs::read_to_string("/.flatpak-info").ok()?;
    let (mut id, mut arch, mut branch) = (None, None, None);
    let (mut app_commit, mut app_path) = (None, None);
    let mut section = String::new();
    for line in text.lines() {
        let l = line.trim();
        if let Some(name) = l.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.to_string();
            continue;
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        match (section.as_str(), k) {
            ("Application", "name") => id = Some(v.to_string()),
            ("Instance", "arch") => arch = Some(v.to_string()),
            ("Instance", "branch") => branch = Some(v.to_string()),
            ("Instance", "app-commit") => app_commit = Some(v.to_string()),
            ("Instance", "app-path") => app_path = Some(v.to_string()),
            _ => {}
        }
    }
    Some(AppInfo {
        id: id?,
        arch: arch.unwrap_or_else(|| std::env::consts::ARCH.to_string()),
        branch: branch.unwrap_or_else(|| "master".to_string()),
        commit: app_commit.or_else(|| app_path.as_deref().and_then(extract_commit)),
    })
}

/// Looks for the 64-character hex segment = OSTree commit in the deploy path.
fn extract_commit(path: &str) -> Option<String> {
    path.split('/')
        .find(|seg| seg.len() == 64 && seg.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_string)
}

/// Result of the update check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckResult {
    /// Local commit = remote commit – nothing to do.
    UpToDate,
    /// A newer commit is available in the repo.
    Available,
    /// Not determinable (no Flatpak, offline, repo unreachable …).
    Unknown,
}

/// Compares the local with the latest repo commit. **Network access** – call in
/// the worker thread, not in the UI thread.
pub fn check() -> CheckResult {
    let Some(info) = app_info() else {
        return CheckResult::Unknown;
    };
    let Some(local) = info.commit else {
        return CheckResult::Unknown;
    };
    // OSTree ref of the app build: app/<id>/<arch>/<branch> → file with the commit.
    let url = format!("{REPO_BASE}/refs/heads/app/{}/{}/{}", info.id, info.arch, info.branch);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(8))
        .build();
    let remote = match agent.get(&url).call() {
        Ok(resp) => match resp.into_string() {
            Ok(s) => s.trim().to_string(),
            Err(_) => return CheckResult::Unknown,
        },
        Err(_) => return CheckResult::Unknown,
    };
    if remote.is_empty() {
        CheckResult::Unknown
    } else if remote.eq_ignore_ascii_case(&local) {
        CheckResult::UpToDate
    } else {
        CheckResult::Available
    }
}

/// Starts the self-update via the Flatpak portal. `on_finish` is called exactly
/// once: `Ok(())` = done (restart needed), `Err(msg)` = error. The actual
/// download runs asynchronously; this function returns immediately. **Must run
/// in the main thread** (GLib main context for the signals).
pub fn install<F: Fn(Result<(), String>) + 'static>(on_finish: F) -> Result<()> {
    let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;

    // 1) Create an update monitor → yields its object path.
    let create_params = glib::Variant::parse(None, "(@a{sv} {},)")?;
    let reply = conn
        .call_sync(
            Some(PORTAL_NAME),
            PORTAL_PATH,
            PORTAL_IFACE,
            "CreateUpdateMonitor",
            Some(&create_params),
            Some(glib::VariantTy::new("(o)").map_err(|e| anyhow!("{e}"))?),
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
        )
        .map_err(|e| anyhow!("CreateUpdateMonitor: {e}"))?;
    let monitor_path = reply
        .child_value(0)
        .str()
        .ok_or_else(|| anyhow!("invalid monitor path"))?
        .to_string();

    // 2) Listen for progress signals; unsubscribe on completion/error.
    let on_finish = Rc::new(on_finish);
    let sub: Rc<Cell<Option<gio::SignalSubscriptionId>>> = Rc::new(Cell::new(None));
    let id = {
        let on_finish = on_finish.clone();
        let sub = sub.clone();
        conn.signal_subscribe(
            Some(PORTAL_NAME),
            Some(MONITOR_IFACE),
            Some("Progress"),
            Some(&monitor_path),
            None,
            gio::DBusSignalFlags::NONE,
            move |conn, _sender, _path, _iface, _signal, params| {
                // params: (a{sv}) with, among others, `status` (u: 0 running, 1 empty, 2 done)
                // and – in the error case – `error`/`error_message` (s).
                let info = params.child_value(0);
                let status = dict_get(&info, "status").and_then(|v| v.get::<u32>());
                let err = dict_get(&info, "error_message")
                    .and_then(|v| v.str().map(str::to_string))
                    .filter(|s| !s.is_empty());
                let done = match (&err, status) {
                    (Some(msg), _) => Some(Err(msg.clone())),
                    (None, Some(2)) => Some(Ok(())),
                    _ => None,
                };
                if let Some(result) = done {
                    on_finish(result);
                    if let Some(id) = sub.take() {
                        conn.signal_unsubscribe(id);
                    }
                }
            },
        )
    };
    sub.set(Some(id));

    // 3) Trigger the update (returns immediately; progress via signal).
    let update_params = glib::Variant::parse(None, "('', @a{sv} {})")?;
    conn.call_sync(
        Some(PORTAL_NAME),
        &monitor_path,
        MONITOR_IFACE,
        "Update",
        Some(&update_params),
        None,
        gio::DBusCallFlags::NONE,
        -1,
        gio::Cancellable::NONE,
    )
    .map_err(|e| anyhow!("Update: {e}"))?;

    Ok(())
}

/// Looks up a key in an `a{sv}` variant and unpacks the (in `v` wrapped) value.
/// `None` if the key is missing.
fn dict_get(dict: &glib::Variant, key: &str) -> Option<glib::Variant> {
    for i in 0..dict.n_children() {
        let entry = dict.child_value(i); // {sv}
        if entry.child_value(0).str() == Some(key) {
            return entry.child_value(1).as_variant(); // v → inner value
        }
    }
    None
}

/// For hints/fallback: the manual update command.
pub fn manual_command() -> String {
    let id = app_info().map(|i| i.id).unwrap_or_else(|| "de.cais.Emilia".to_string());
    format!("flatpak update {id}")
}
