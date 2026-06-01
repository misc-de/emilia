//! Selbst-Aktualisierung der **Flatpak**-Version.
//!
//! - **Prüfen** (sofort, ohne auf das Portal zu warten): den lokal installierten
//!   OSTree-Commit (aus `/.flatpak-info`) mit dem neuesten Commit im veröffentlichten
//!   Repo (per HTTP) vergleichen.
//! - **Installieren**: über das **Flatpak-Portal**
//!   (`org.freedesktop.portal.Flatpak`) – die einzige aus der Sandbox heraus
//!   zulässige Möglichkeit, sich selbst zu aktualisieren. Das eigentliche
//!   Herunterladen läuft im Hintergrund; das Portal meldet den Fortschritt per
//!   Signal. Nach Abschluss muss die App neu gestartet werden.
//!
//! Außerhalb von Flatpak (z. B. `cargo run`) ist nichts davon verfügbar.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use gtk::gio;
use gtk::glib;

/// Basis-URL des veröffentlichten OSTree-Repos (siehe `data/de.cais.Emilia.flatpakrepo`).
const REPO_BASE: &str = "https://misc-de.github.io/emilia/repo";

const PORTAL_NAME: &str = "org.freedesktop.portal.Flatpak";
const PORTAL_PATH: &str = "/org/freedesktop/portal/Flatpak";
const PORTAL_IFACE: &str = "org.freedesktop.portal.Flatpak";
const MONITOR_IFACE: &str = "org.freedesktop.portal.Flatpak.UpdateMonitor";

/// Läuft die App als Flatpak? Nur dann ist Selbst-Aktualisierung möglich.
pub fn in_flatpak() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

/// Aus `/.flatpak-info` gelesene Eckdaten der laufenden Instanz.
pub struct AppInfo {
    pub id: String,
    pub arch: String,
    pub branch: String,
    /// Aktuell laufender OSTree-Commit (volle 64-Hex), falls ermittelbar.
    pub commit: Option<String>,
}

/// Liest `/.flatpak-info` (INI-artig): App-Id, Architektur, Branch und den
/// laufenden Commit (bevorzugt `app-commit`, sonst aus dem Deploy-Pfad).
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

/// Sucht im Deploy-Pfad das 64-stellige Hex-Segment = OSTree-Commit.
fn extract_commit(path: &str) -> Option<String> {
    path.split('/')
        .find(|seg| seg.len() == 64 && seg.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_string)
}

/// Ergebnis der Update-Prüfung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckResult {
    /// Lokaler Commit = Remote-Commit – nichts zu tun.
    UpToDate,
    /// Im Repo liegt ein neuerer Commit vor.
    Available,
    /// Nicht ermittelbar (kein Flatpak, offline, Repo nicht erreichbar …).
    Unknown,
}

/// Vergleicht den lokalen mit dem neuesten Repo-Commit. **Netzzugriff** – im
/// Worker-Thread aufrufen, nicht im UI-Thread.
pub fn check() -> CheckResult {
    let Some(info) = app_info() else {
        return CheckResult::Unknown;
    };
    let Some(local) = info.commit else {
        return CheckResult::Unknown;
    };
    // OSTree-Ref des App-Builds: app/<id>/<arch>/<branch> → Datei mit dem Commit.
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

/// Startet die Selbst-Aktualisierung über das Flatpak-Portal. `on_finish` wird
/// genau einmal aufgerufen: `Ok(())` = fertig (Neustart nötig), `Err(msg)` =
/// Fehler. Der eigentliche Download läuft asynchron; diese Funktion kehrt sofort
/// zurück. **Muss im Hauptthread laufen** (GLib-Main-Context für die Signale).
pub fn install<F: Fn(Result<(), String>) + 'static>(on_finish: F) -> Result<()> {
    let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;

    // 1) Update-Monitor anlegen → liefert dessen Objektpfad.
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

    // 2) Auf Fortschritts-Signale lauschen; bei Abschluss/Fehler abmelden.
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
                // params: (a{sv}) mit u. a. `status` (u: 0 läuft, 1 leer, 2 fertig)
                // und – im Fehlerfall – `error`/`error_message` (s).
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

    // 3) Aktualisierung anstoßen (kehrt sofort zurück; Fortschritt via Signal).
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

/// Schlägt einen Schlüssel in einem `a{sv}`-Variant nach und entpackt den
/// (in `v` verpackten) Wert. `None`, wenn der Schlüssel fehlt.
fn dict_get(dict: &glib::Variant, key: &str) -> Option<glib::Variant> {
    for i in 0..dict.n_children() {
        let entry = dict.child_value(i); // {sv}
        if entry.child_value(0).str() == Some(key) {
            return entry.child_value(1).as_variant(); // v → innerer Wert
        }
    }
    None
}

/// Für Hinweise/Fallback: der manuelle Aktualisierungsbefehl.
pub fn manual_command() -> String {
    let id = app_info().map(|i| i.id).unwrap_or_else(|| "de.cais.Emilia".to_string());
    format!("flatpak update {id}")
}
