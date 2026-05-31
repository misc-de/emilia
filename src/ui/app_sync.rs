//! UI-Anbindung der Geräte-Synchronisierung: mehrseitiger Dialog (Modus →
//! Server/QR bzw. Scan/Webcam → Gepaart/Fortschritt), Anbindung des Server-
//! Threads und des Client-Workers an relm4.
//!
//! Logik/Netzwerk liegt in [`crate::core::sync`]; hier nur Widgets + Eventfluss.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::sync::client::SyncClient;
use crate::core::sync::protocol::{self, PairingInfo};
use crate::core::sync::scanner::Scanner;
use crate::core::sync::server::SyncServer;
use crate::core::sync::{self, crypto, data, SyncEvent};
use crate::i18n::{gettext, gettext_f};
use crate::ui::app::{App, Cmd, Msg};

/// Laufzeit- und Widget-Zustand der Geräte-Synchronisierung.
///
/// Alle Felder sind `Option`, daher leitet sich `Default` ab. Die Widgets
/// werden beim Öffnen des Dialogs erzeugt; die Handles bleiben gespeichert,
/// damit `update_cmd` sie später aktualisieren kann.
#[derive(Default)]
pub(crate) struct SyncState {
    /// Stop-Flag des Server-Threads (gesetzt → Annahmeschleife endet).
    pub stop: Option<Arc<AtomicBool>>,
    /// Laufende Webcam-Scanner-Pipeline (Drop stoppt die Kamera).
    pub scanner: Option<Scanner>,
    pub dialog: Option<adw::Dialog>,
    pub nav: Option<adw::NavigationView>,
    pub qr: Option<gtk::Picture>,
    pub cam: Option<gtk::Picture>,
    /// Status-Label der Gepaart-Seite (Kopplung, Import, Übertragung).
    pub status: Option<gtk::Label>,
    /// Status-Label der Server-Seite (Adresse/Warten).
    pub server_status: Option<gtk::Label>,
    pub progress: Option<gtk::ProgressBar>,
    /// Stabile Geräte-ID (gecacht aus den Einstellungen).
    pub device_id: Option<String>,
}

impl App {
    /// Persistente Geräte-ID (einmal erzeugt und in den Einstellungen abgelegt).
    fn sync_device_id(&mut self) -> String {
        if let Some(id) = &self.sync.device_id {
            return id.clone();
        }
        let id = self
            .library
            .get_setting("sync_device_id")
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                let new = crypto::random_hex(16);
                let _ = self.library.set_setting("sync_device_id", &new);
                new
            });
        self.sync.device_id = Some(id.clone());
        id
    }

    /// Anzeigename dieses Geräts (Einstellung oder Hostname).
    fn sync_device_name(&self) -> String {
        self.library
            .get_setting("sync_device_name")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(sync::default_device_name)
    }

    /// Öffnet den mehrseitigen Synchronisierungs-Dialog (Startseite: Moduswahl).
    pub(crate) fn open_sync_dialog(
        &mut self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        // Eventuell offenen Dialog/Server zuerst aufräumen.
        self.teardown_sync();

        let dialog = adw::Dialog::builder()
            .title(&gettext("Device sync"))
            .content_width(420)
            .content_height(520)
            .build();
        let nav = adw::NavigationView::new();

        // Gemeinsame Widgets, die später aktualisiert werden.
        let qr = gtk::Picture::builder()
            .width_request(260)
            .height_request(260)
            .can_shrink(false)
            .build();
        let cam = gtk::Picture::builder()
            .width_request(320)
            .height_request(240)
            .content_fit(gtk::ContentFit::Contain)
            .build();
        let status = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        let server_status = gtk::Label::builder()
            .wrap(true)
            .css_classes(["dim-label"])
            .justify(gtk::Justification::Center)
            .build();
        let progress = gtk::ProgressBar::builder().show_text(true).visible(false).build();

        nav.add(&self.sync_page_server(&qr, &server_status));
        nav.add(&self.sync_page_scan(&cam));
        nav.add(&self.sync_page_paired(&status, &progress));
        nav.push(&self.sync_page_mode(sender));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&nav));
        dialog.set_child(Some(&toolbar));

        {
            let sender = sender.clone();
            dialog.connect_closed(move |_| sender.input(Msg::SyncDialogClosed));
        }

        self.sync.dialog = Some(dialog.clone());
        self.sync.nav = Some(nav);
        self.sync.qr = Some(qr);
        self.sync.cam = Some(cam);
        self.sync.status = Some(status);
        self.sync.server_status = Some(server_status);
        self.sync.progress = Some(progress);

        dialog.present(Some(root));
    }

    fn sync_page_mode(&self, sender: &ComponentSender<Self>) -> adw::NavigationPage {
        let group = adw::PreferencesGroup::builder()
            .description(&gettext("Connect two devices on the same network."))
            .build();

        let rows: [(String, String, &str, fn() -> Msg); 2] = [
            (
                gettext("Offer connection"),
                gettext("Start a server and show a QR code"),
                "network-transmit-receive-symbolic",
                || Msg::SyncStartServer,
            ),
            (
                gettext("Scan QR code"),
                gettext("Point the camera at the other device's code"),
                "camera-photo-symbolic",
                || Msg::SyncStartScan,
            ),
        ];
        for (title, subtitle, icon, make) in rows {
            let row = adw::ActionRow::builder()
                .title(&title)
                .subtitle(&subtitle)
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let sender = sender.clone();
            row.connect_activated(move |_| sender.input(make()));
            group.add(&row);
        }

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.append(&group);
        nav_page("mode", &gettext("Device sync"), &content)
    }

    fn sync_page_server(&self, qr: &gtk::Picture, server_status: &gtk::Label) -> adw::NavigationPage {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_valign(gtk::Align::Center);

        let hint = gtk::Label::builder()
            .label(&gettext("Scan this code on the other device."))
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        qr.set_halign(gtk::Align::Center);
        content.append(&hint);
        content.append(qr);
        content.append(server_status);
        nav_page("server", &gettext("Offer connection"), &content)
    }

    fn sync_page_scan(&self, cam: &gtk::Picture) -> adw::NavigationPage {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_valign(gtk::Align::Center);

        let hint = gtk::Label::builder()
            .label(&gettext("Point the camera at the QR code."))
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        cam.set_halign(gtk::Align::Center);
        content.append(&hint);
        content.append(cam);
        nav_page("scan", &gettext("Scan QR code"), &content)
    }

    fn sync_page_paired(&self, status: &gtk::Label, progress: &gtk::ProgressBar) -> adw::NavigationPage {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_valign(gtk::Align::Center);
        content.append(status);
        content.append(progress);
        nav_page("paired", &gettext("Connected"), &content)
    }

    /// Server-Modus starten: Server-Thread aufsetzen und QR-Seite zeigen.
    pub(crate) fn start_sync_server(&mut self, sender: &ComponentSender<Self>) {
        if let Some(nav) = &self.sync.nav {
            nav.push_by_tag("server");
        }
        if self.sync.stop.is_some() {
            return; // läuft bereits
        }
        let device_name = self.sync_device_name();
        let stop = Arc::new(AtomicBool::new(false));
        self.sync.stop = Some(stop.clone());

        sender.spawn_command(move |out| {
            let server = match SyncServer::start(device_name, stop) {
                Ok(s) => s,
                Err(e) => {
                    let _ = out.send(Cmd::Sync(SyncEvent::Error(e.to_string())));
                    return;
                }
            };
            let _ = out.send(Cmd::Sync(SyncEvent::ServerReady {
                pair_url: server.pair_url(),
                host: server.host().to_string(),
                port: server.port(),
            }));
            server.run(|ev| {
                let _ = out.send(Cmd::Sync(ev));
            });
            let _ = out.send(Cmd::Sync(SyncEvent::ServerStopped));
        });
    }

    /// Client-Modus starten: Webcam-Scanner mit Live-Vorschau.
    pub(crate) fn start_sync_scan(&mut self, sender: &ComponentSender<Self>) {
        if let Some(nav) = &self.sync.nav {
            nav.push_by_tag("scan");
        }
        if self.sync.scanner.is_some() {
            return;
        }
        let sender_dec = sender.clone();
        match Scanner::start(move |url| sender_dec.input(Msg::SyncQrDecoded(url))) {
            Ok((scanner, paintable)) => {
                match (&self.sync.cam, &paintable) {
                    (Some(cam), Some(p)) => cam.set_paintable(Some(p)),
                    _ => self
                        .toast(&gettext("Camera preview unavailable – the code is still detected")),
                }
                self.sync.scanner = Some(scanner);
            }
            Err(e) => self.toast(&e.to_string()),
        }
    }

    /// Ein QR-Code wurde dekodiert: URL prüfen und den Client-Sync starten.
    pub(crate) fn handle_sync_qr(&mut self, url: &str, sender: &ComponentSender<Self>) {
        if self.sync.stop.is_some() || self.sync.scanner.is_none() {
            return; // bereits in Bearbeitung / Scanner gestoppt
        }
        let info = match protocol::parse_pair_url(url, sync::now_unix()) {
            Ok(info) => info,
            Err(_) => return, // anderer/ungültiger Code – weiterscannen
        };
        // Kamera anhalten, sobald ein gültiger Code erkannt wurde.
        self.sync.scanner = None;
        if let Some(st) = &self.sync.status {
            st.set_text(&gettext("Connecting …"));
        }

        let device_id = self.sync_device_id();
        let device_name = self.sync_device_name();
        sender.spawn_command(move |out| {
            run_client_sync(info, device_id, device_name, &out);
        });
    }

    /// Verarbeitet ein [`SyncEvent`] aus dem Server-Thread bzw. Client-Worker.
    pub(crate) fn on_sync_event(&mut self, ev: SyncEvent, sender: &ComponentSender<Self>) {
        match ev {
            SyncEvent::ServerReady {
                pair_url,
                host,
                port,
            } => {
                if let Some(qr) = &self.sync.qr {
                    match sync::qr::render_qr(&pair_url) {
                        Ok(tex) => qr.set_paintable(Some(&tex)),
                        Err(e) => self.toast(&e.to_string()),
                    }
                }
                if let Some(st) = &self.sync.server_status {
                    st.set_text(&gettext_f(
                        "Waiting for another device … ({addr})",
                        &[("addr", &format!("{host}:{port}"))],
                    ));
                }
            }
            SyncEvent::PeerPaired { peer_name } => {
                if let Some(nav) = &self.sync.nav {
                    nav.push_by_tag("paired");
                }
                if let Some(st) = &self.sync.status {
                    st.set_text(&gettext_f(
                        "Connected with {name}",
                        &[("name", &peer_name)],
                    ));
                }
            }
            SyncEvent::ImportReceived { stats } => {
                if let Some(st) = &self.sync.status {
                    st.set_text(&gettext_f(
                        "Received {fav} favorites, {pl} playlists, {pod} podcasts.",
                        &[
                            ("fav", &stats.favorites.to_string()),
                            ("pl", &stats.playlists.to_string()),
                            ("pod", &stats.podcasts.to_string()),
                        ],
                    ));
                }
                // Ansichten neu laden, damit die übernommenen Inhalte erscheinen.
                self.load_favorites(sender);
                self.reload_playlists(sender);
                self.reload_podcasts(sender);
            }
            SyncEvent::ExportSent => {
                if let Some(st) = &self.sync.status {
                    st.set_text(&gettext("Metadata synced. Transferring files …"));
                }
            }
            SyncEvent::FileProgress { done, total, name } => {
                if let Some(p) = &self.sync.progress {
                    p.set_visible(true);
                    p.set_fraction(done as f64 / total.max(1) as f64);
                    p.set_text(Some(&format!("{done}/{total}")));
                }
                if let Some(st) = &self.sync.status {
                    st.set_text(&name);
                }
            }
            SyncEvent::TransferDone { files } => {
                if let Some(p) = &self.sync.progress {
                    p.set_fraction(1.0);
                }
                if let Some(st) = &self.sync.status {
                    st.set_text(&gettext_f(
                        "Done – {n} files transferred.",
                        &[("n", &files.to_string())],
                    ));
                }
                self.toast(&gettext("Sync complete"));
            }
            SyncEvent::PeerDisconnected => {
                if let Some(st) = &self.sync.status {
                    st.set_text(&gettext("Disconnected."));
                }
            }
            SyncEvent::ServerStopped => {
                self.sync.stop = None;
            }
            SyncEvent::Error(msg) => self.toast(&msg),
        }
    }

    /// Räumt den Sync-Zustand auf (Server stoppen, Kamera aus, Dialog schließen).
    pub(crate) fn teardown_sync(&mut self) {
        if let Some(stop) = self.sync.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.sync.scanner = None; // Drop stoppt die Kamera
        if let Some(dialog) = self.sync.dialog.take() {
            dialog.close();
        }
        self.sync.nav = None;
        self.sync.qr = None;
        self.sync.cam = None;
        self.sync.status = None;
        self.sync.server_status = None;
        self.sync.progress = None;
    }
}

/// Verpackt einen Inhalt in eine `NavigationPage` mit eigener Kopfleiste
/// (die `NavigationView` ergänzt automatisch eine Zurück-Schaltfläche).
fn nav_page(
    tag: &str,
    title: &str,
    content: &impl gtk::prelude::IsA<gtk::Widget>,
) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(content));
    adw::NavigationPage::builder()
        .tag(tag)
        .title(title)
        .child(&toolbar)
        .build()
}

/// Vollständiger Client-Sync in einem Worker-Thread: koppeln → Metadaten in
/// beide Richtungen → Dateien (Server → Client) → trennen. Meldet jeden Schritt
/// über `out`.
fn run_client_sync(
    info: PairingInfo,
    device_id: String,
    device_name: String,
    out: &relm4::Sender<Cmd>,
) {
    let mut client = SyncClient::new(&info, device_id, device_name);

    if let Err(e) = client.pair(&info.token) {
        let _ = out.send(Cmd::Sync(SyncEvent::Error(e.to_string())));
        return;
    }
    let _ = out.send(Cmd::Sync(SyncEvent::PeerPaired {
        peer_name: client.peer_name.clone(),
    }));

    let lib = match Library::open() {
        Ok(lib) => lib,
        Err(e) => {
            let _ = out.send(Cmd::Sync(SyncEvent::Error(e.to_string())));
            client.disconnect();
            return;
        }
    };

    // 1. Metadaten der Gegenstelle holen und lokal einspielen.
    let server_export = match client.fetch_export() {
        Ok(exp) => exp,
        Err(e) => {
            let _ = out.send(Cmd::Sync(SyncEvent::Error(e.to_string())));
            client.disconnect();
            return;
        }
    };
    if let Ok(stats) = data::import_library(&lib, &server_export) {
        let _ = out.send(Cmd::Sync(SyncEvent::ImportReceived { stats }));
    }

    // 2. Eigene Metadaten an die Gegenstelle senden.
    if let Ok(local) = data::export_library(&lib) {
        let _ = client.push_export(&local);
        let _ = out.send(Cmd::Sync(SyncEvent::ExportSent));
    }

    // 3. Audiodateien herunterladen, die lokal fehlen.
    let music_dir = lib.get_setting("music_dir").ok().flatten().unwrap_or_default();
    let total = server_export.files.len() as u64;
    let mut transferred = 0usize;
    for (i, f) in server_export.files.iter().enumerate() {
        let done = i as u64 + 1;
        let _ = out.send(Cmd::Sync(SyncEvent::FileProgress {
            done,
            total,
            name: f.path.clone(),
        }));
        if music_dir.is_empty() || f.path.starts_with('/') || f.path.contains("..") {
            continue;
        }
        let dest = std::path::Path::new(&music_dir).join(&f.path);
        // Vorhandene, größengleiche Datei nicht erneut übertragen.
        if f.size != 0
            && std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) == f.size
        {
            continue;
        }
        if client.download_file(&f.path, &dest).is_ok() {
            transferred += 1;
        }
    }
    let _ = out.send(Cmd::Sync(SyncEvent::TransferDone { files: transferred }));

    client.disconnect();
    let _ = out.send(Cmd::Sync(SyncEvent::PeerDisconnected));
}
