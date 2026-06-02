//! UI binding of device sync: multi-page dialog (mode →
//! server/QR or scan/camera → paired/progress), wiring the server
//! thread and the client worker into relm4.
//!
//! Logic/network lives in [`crate::core::sync`]; here only widgets + event flow.

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

/// Runtime and widget state of device sync.
///
/// All fields are `Option`, so `Default` is derived. The widgets
/// are created when the dialog is opened; the handles are kept stored
/// so that `update_cmd` can update them later.
#[derive(Default)]
pub(crate) struct SyncState {
    /// Stop flag of the server thread (set → accept loop ends).
    pub stop: Option<Arc<AtomicBool>>,
    /// Running camera scanner pipeline (drop stops the camera).
    pub scanner: Option<Scanner>,
    pub dialog: Option<adw::Dialog>,
    pub nav: Option<adw::NavigationView>,
    pub qr: Option<gtk::Picture>,
    pub cam: Option<gtk::Picture>,
    /// Status label of the paired page (pairing, import, transfer).
    pub status: Option<gtk::Label>,
    /// Status label of the server page (address/waiting).
    pub server_status: Option<gtk::Label>,
    pub progress: Option<gtk::ProgressBar>,
    /// Stable device ID (cached from the settings).
    pub device_id: Option<String>,
}

impl App {
    /// Persistent device ID (generated once and stored in the settings).
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

    /// Display name of this device (setting or hostname).
    fn sync_device_name(&self) -> String {
        self.library
            .get_setting("sync_device_name")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(sync::default_device_name)
    }

    /// Opens the multi-page sync dialog (start page: mode selection).
    pub(crate) fn open_sync_dialog(
        &mut self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        // Clean up any open dialog/server first.
        self.teardown_sync();

        let dialog = adw::Dialog::builder()
            .title(&gettext("Device sync"))
            .content_width(420)
            .content_height(520)
            .build();
        let nav = adw::NavigationView::new();

        // Shared widgets that are updated later.
        // `can_shrink(true)` + `Contain`: the QR code scales down squarely to the
        // available width (so it also fits on narrow phone displays),
        // instead of overflowing the edge at its full pixel size.
        let qr = gtk::Picture::builder()
            .width_request(220)
            .height_request(220)
            .can_shrink(true)
            .content_fit(gtk::ContentFit::Contain)
            .hexpand(false)
            .halign(gtk::Align::Center)
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

        // Add the mode page first → it is the (root) start page.
        // Server/scan are opened on top of it via `push_by_tag` when needed; if
        // "server" came first, it would already be the root and `push_by_tag("server")`
        // (from `start_sync_server`) would do nothing (you'd stay on the mode
        // page).
        nav.add(&self.sync_page_mode(sender));
        nav.add(&self.sync_page_server(&qr, &server_status));
        nav.add(&self.sync_page_scan(&cam));
        nav.add(&self.sync_page_paired(&status, &progress));

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

    /// Start server mode: set up the server thread and show the QR page.
    pub(crate) fn start_sync_server(&mut self, sender: &ComponentSender<Self>) {
        if let Some(nav) = &self.sync.nav {
            nav.push_by_tag("server");
        }
        if self.sync.stop.is_some() {
            return; // already running
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

    /// Start client mode: camera scanner with live preview.
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

    /// A QR code was decoded: validate the URL and start the client sync.
    pub(crate) fn handle_sync_qr(&mut self, url: &str, sender: &ComponentSender<Self>) {
        if self.sync.stop.is_some() || self.sync.scanner.is_none() {
            return; // already being processed / scanner stopped
        }
        let info = match protocol::parse_pair_url(url, sync::now_unix()) {
            Ok(info) => info,
            Err(_) => return, // other/invalid code – keep scanning
        };
        // Stop the camera once a valid code has been detected.
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

    /// Processes a [`SyncEvent`] from the server thread or client worker.
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
                // Paired → tint the sync icon at the top green.
                self.sync_connected = true;
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
                // Reload views so the imported content appears.
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
                self.sync_connected = false;
                if let Some(st) = &self.sync.status {
                    st.set_text(&gettext("Disconnected."));
                }
            }
            SyncEvent::ServerStopped => {
                self.sync_connected = false;
                self.sync.stop = None;
            }
            SyncEvent::Error(msg) => self.toast(&msg),
        }
    }

    /// Cleans up the sync state (stop server, camera off, close dialog).
    pub(crate) fn teardown_sync(&mut self) {
        self.sync_connected = false;
        if let Some(stop) = self.sync.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.sync.scanner = None; // drop stops the camera
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

/// Wraps content in a `NavigationPage` with its own header bar
/// (the `NavigationView` automatically adds a back button).
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

/// Full client sync in a worker thread: pair → metadata in
/// both directions → files (server → client) → disconnect. Reports each step
/// via `out`.
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

    // 1. Fetch the peer's metadata and apply it locally.
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

    // 2. Send own metadata to the peer.
    if let Ok(local) = data::export_library(&lib) {
        let _ = client.push_export(&local);
        let _ = out.send(Cmd::Sync(SyncEvent::ExportSent));
    }

    // 3. Download audio files that are missing locally.
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
        // Don't transfer an existing file of the same size again.
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
