//! Device sync as a standalone relm4 component: a multi-page dialog (mode →
//! server/QR or scan/camera → paired/progress) plus the server thread and the
//! client worker. Extracted from the `App` god-object.
//!
//! Logic/network lives in [`crate::core::sync`]; here only widgets + event flow.
//! The component owns its dialog and worker; it tells the parent (via `Output`)
//! only the two things the parent still needs: the connected state (for the
//! green header icon) and that an import happened (so the parent reloads the
//! affected library views).

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

/// The device-sync component (owns the dialog, server thread and client worker).
pub(crate) struct SyncPage {
    /// Own DB connection (device id/name from the settings).
    library: Library,
    /// Window the dialog is presented on (set on `Open`).
    window: Option<adw::ApplicationWindow>,
    /// Stop flag of the server thread (set → accept loop ends).
    stop: Option<Arc<AtomicBool>>,
    /// Running camera scanner pipeline (drop stops the camera).
    scanner: Option<Scanner>,
    dialog: Option<adw::Dialog>,
    nav: Option<adw::NavigationView>,
    qr: Option<gtk::Picture>,
    cam: Option<gtk::Picture>,
    /// Status label of the paired page (pairing, import, transfer).
    status: Option<gtk::Label>,
    /// Status label of the server page (address/waiting).
    server_status: Option<gtk::Label>,
    progress: Option<gtk::ProgressBar>,
    /// Stable device ID (cached from the settings).
    device_id: Option<String>,
}

#[derive(Debug)]
pub(crate) enum SyncInput {
    /// Open the sync dialog on the given window (start page: mode selection).
    Open(adw::ApplicationWindow),
    StartServer,
    StartScan,
    QrDecoded(String),
    DialogClosed,
}

#[derive(Debug)]
pub(crate) enum SyncOutput {
    /// Paired/disconnected → the parent tints the header sync icon.
    ConnectedChanged(bool),
    /// Metadata was imported → the parent reloads favorites/playlists/podcasts.
    Imported,
}

#[relm4::component(pub(crate))]
impl Component for SyncPage {
    type Init = ();
    type Input = SyncInput;
    type Output = SyncOutput;
    type CommandOutput = SyncEvent;

    view! {
        // Hidden placeholder: the component only manages a *presented* dialog.
        #[root]
        gtk::Box {}
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        _sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let library = Library::open().expect("library");
        let model = SyncPage {
            library,
            window: None,
            stop: None,
            scanner: None,
            dialog: None,
            nav: None,
            qr: None,
            cam: None,
            status: None,
            server_status: None,
            progress: None,
            device_id: None,
        };
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: SyncInput, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            SyncInput::Open(window) => {
                self.window = Some(window);
                self.open_dialog(&sender);
            }
            SyncInput::StartServer => self.start_server(&sender),
            SyncInput::StartScan => self.start_scan(&sender),
            SyncInput::QrDecoded(url) => self.handle_qr(&url, &sender),
            SyncInput::DialogClosed => self.teardown(&sender),
        }
    }

    fn update_cmd(&mut self, ev: SyncEvent, sender: ComponentSender<Self>, _root: &Self::Root) {
        self.on_event(ev, &sender);
    }
}

impl SyncPage {
    /// Persistent device ID (generated once and stored in the settings).
    fn device_id(&mut self) -> String {
        if let Some(id) = &self.device_id {
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
        self.device_id = Some(id.clone());
        id
    }

    /// Display name of this device (setting or hostname).
    fn device_name(&self) -> String {
        self.library
            .get_setting("sync_device_name")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(sync::default_device_name)
    }

    /// Opens the multi-page sync dialog (start page: mode selection).
    fn open_dialog(&mut self, sender: &ComponentSender<Self>) {
        // Clean up any open dialog/server first.
        self.teardown(sender);
        let Some(window) = self.window.clone() else {
            return;
        };

        let dialog = adw::Dialog::builder()
            .title(&gettext("Device sync"))
            .content_width(420)
            .content_height(520)
            .build();
        let nav = adw::NavigationView::new();

        // Shared widgets that are updated later. `can_shrink(true)` + `Contain`:
        // the QR code scales down squarely so it also fits narrow phone displays.
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

        // Mode page first → it is the (root) start page; server/scan are pushed
        // on top via `push_by_tag` when needed.
        nav.add(&self.page_mode(sender));
        nav.add(&page_server(&qr, &server_status));
        nav.add(&page_scan(&cam));
        nav.add(&page_paired(&status, &progress));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&nav));
        dialog.set_child(Some(&toolbar));

        {
            let sender = sender.clone();
            dialog.connect_closed(move |_| sender.input(SyncInput::DialogClosed));
        }

        self.dialog = Some(dialog.clone());
        self.nav = Some(nav);
        self.qr = Some(qr);
        self.cam = Some(cam);
        self.status = Some(status);
        self.server_status = Some(server_status);
        self.progress = Some(progress);

        dialog.present(Some(&window));
    }

    fn page_mode(&self, sender: &ComponentSender<Self>) -> adw::NavigationPage {
        let group = adw::PreferencesGroup::builder()
            .description(&gettext("Connect two devices on the same network."))
            .build();

        let rows: [(String, String, &str, fn() -> SyncInput); 2] = [
            (
                gettext("Offer connection"),
                gettext("Start a server and show a QR code"),
                "network-transmit-receive-symbolic",
                || SyncInput::StartServer,
            ),
            (
                gettext("Scan QR code"),
                gettext("Point the camera at the other device's code"),
                "camera-photo-symbolic",
                || SyncInput::StartScan,
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

    /// Start server mode: set up the server thread and show the QR page.
    fn start_server(&mut self, sender: &ComponentSender<Self>) {
        if let Some(nav) = &self.nav {
            nav.push_by_tag("server");
        }
        if self.stop.is_some() {
            return; // already running
        }
        let device_name = self.device_name();
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = Some(stop.clone());

        sender.spawn_command(move |out| {
            let server = match SyncServer::start(device_name, stop) {
                Ok(s) => s,
                Err(e) => {
                    let _ = out.send(SyncEvent::Error(e.to_string()));
                    return;
                }
            };
            let _ = out.send(SyncEvent::ServerReady {
                pair_url: server.pair_url(),
                host: server.host().to_string(),
                port: server.port(),
            });
            server.run(|ev| {
                let _ = out.send(ev);
            });
            let _ = out.send(SyncEvent::ServerStopped);
        });
    }

    /// Start client mode: camera scanner with live preview.
    fn start_scan(&mut self, sender: &ComponentSender<Self>) {
        if let Some(nav) = &self.nav {
            nav.push_by_tag("scan");
        }
        if self.scanner.is_some() {
            return;
        }
        let sender_dec = sender.clone();
        match Scanner::start(move |url| sender_dec.input(SyncInput::QrDecoded(url))) {
            Ok((scanner, paintable)) => {
                match (&self.cam, &paintable) {
                    (Some(cam), Some(p)) => cam.set_paintable(Some(p)),
                    _ => {} // preview unavailable – the code is still detected
                }
                self.scanner = Some(scanner);
            }
            Err(e) => tracing::warn!("Camera scanner failed: {e}"),
        }
    }

    /// A QR code was decoded: validate the URL and start the client sync.
    fn handle_qr(&mut self, url: &str, sender: &ComponentSender<Self>) {
        if self.stop.is_some() || self.scanner.is_none() {
            return; // already being processed / scanner stopped
        }
        let info = match protocol::parse_pair_url(url, sync::now_unix()) {
            Ok(info) => info,
            Err(_) => return, // other/invalid code – keep scanning
        };
        // Stop the camera once a valid code has been detected.
        self.scanner = None;
        if let Some(st) = &self.status {
            st.set_text(&gettext("Connecting …"));
        }

        let device_id = self.device_id();
        let device_name = self.device_name();
        sender.spawn_command(move |out| {
            run_client_sync(info, device_id, device_name, &out);
        });
    }

    /// Processes a [`SyncEvent`] from the server thread or client worker.
    fn on_event(&mut self, ev: SyncEvent, sender: &ComponentSender<Self>) {
        match ev {
            SyncEvent::ServerReady {
                pair_url,
                host,
                port,
            } => {
                if let Some(qr) = &self.qr {
                    if let Ok(tex) = sync::qr::render_qr(&pair_url) {
                        qr.set_paintable(Some(&tex));
                    }
                }
                if let Some(st) = &self.server_status {
                    st.set_text(&gettext_f(
                        "Waiting for another device … ({addr})",
                        &[("addr", &format!("{host}:{port}"))],
                    ));
                }
            }
            SyncEvent::PeerPaired { peer_name } => {
                // Paired → tint the sync icon at the top green.
                let _ = sender.output(SyncOutput::ConnectedChanged(true));
                if let Some(nav) = &self.nav {
                    nav.push_by_tag("paired");
                }
                if let Some(st) = &self.status {
                    st.set_text(&gettext_f("Connected with {name}", &[("name", &peer_name)]));
                }
            }
            SyncEvent::ImportReceived { stats } => {
                if let Some(st) = &self.status {
                    st.set_text(&gettext_f(
                        "Received {fav} favorites, {pl} playlists, {pod} podcasts.",
                        &[
                            ("fav", &stats.favorites.to_string()),
                            ("pl", &stats.playlists.to_string()),
                            ("pod", &stats.podcasts.to_string()),
                        ],
                    ));
                }
                // Ask the parent to reload the imported content.
                let _ = sender.output(SyncOutput::Imported);
            }
            SyncEvent::ExportSent => {
                if let Some(st) = &self.status {
                    st.set_text(&gettext("Metadata synced. Transferring files …"));
                }
            }
            SyncEvent::FileProgress { done, total, name } => {
                if let Some(p) = &self.progress {
                    p.set_visible(true);
                    p.set_fraction(done as f64 / total.max(1) as f64);
                    p.set_text(Some(&format!("{done}/{total}")));
                }
                if let Some(st) = &self.status {
                    st.set_text(&name);
                }
            }
            SyncEvent::TransferDone { files } => {
                if let Some(p) = &self.progress {
                    p.set_fraction(1.0);
                }
                if let Some(st) = &self.status {
                    st.set_text(&gettext_f("Done – {n} files transferred.", &[("n", &files.to_string())]));
                }
            }
            SyncEvent::PeerDisconnected => {
                let _ = sender.output(SyncOutput::ConnectedChanged(false));
                if let Some(st) = &self.status {
                    st.set_text(&gettext("Disconnected."));
                }
            }
            SyncEvent::ServerStopped => {
                let _ = sender.output(SyncOutput::ConnectedChanged(false));
                self.stop = None;
            }
            SyncEvent::Error(msg) => tracing::warn!("Sync error: {msg}"),
        }
    }

    /// Cleans up the sync state (stop server, camera off, close dialog).
    fn teardown(&mut self, sender: &ComponentSender<Self>) {
        let _ = sender.output(SyncOutput::ConnectedChanged(false));
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.scanner = None; // drop stops the camera
        if let Some(dialog) = self.dialog.take() {
            dialog.close();
        }
        self.nav = None;
        self.qr = None;
        self.cam = None;
        self.status = None;
        self.server_status = None;
        self.progress = None;
    }
}

fn page_server(qr: &gtk::Picture, server_status: &gtk::Label) -> adw::NavigationPage {
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

fn page_scan(cam: &gtk::Picture) -> adw::NavigationPage {
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

fn page_paired(status: &gtk::Label, progress: &gtk::ProgressBar) -> adw::NavigationPage {
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

/// Wraps content in a `NavigationPage` with its own header bar.
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

/// Full client sync in a worker thread: pair → metadata both ways → files
/// (server → client) → disconnect. Reports each step via `out`.
fn run_client_sync(
    info: PairingInfo,
    device_id: String,
    device_name: String,
    out: &relm4::Sender<SyncEvent>,
) {
    let mut client = SyncClient::new(&info, device_id, device_name);

    if let Err(e) = client.pair(&info.token) {
        let _ = out.send(SyncEvent::Error(e.to_string()));
        return;
    }
    let _ = out.send(SyncEvent::PeerPaired {
        peer_name: client.peer_name.clone(),
    });

    let lib = match Library::open() {
        Ok(lib) => lib,
        Err(e) => {
            let _ = out.send(SyncEvent::Error(e.to_string()));
            client.disconnect();
            return;
        }
    };

    // 1. Fetch the peer's metadata and apply it locally.
    let server_export = match client.fetch_export() {
        Ok(exp) => exp,
        Err(e) => {
            let _ = out.send(SyncEvent::Error(e.to_string()));
            client.disconnect();
            return;
        }
    };
    if let Ok(stats) = data::import_library(&lib, &server_export) {
        let _ = out.send(SyncEvent::ImportReceived { stats });
    }

    // 2. Send own metadata to the peer.
    if let Ok(local) = data::export_library(&lib) {
        let _ = client.push_export(&local);
        let _ = out.send(SyncEvent::ExportSent);
    }

    // 3. Download audio files that are missing locally.
    let music_dir = lib.get_setting("music_dir").ok().flatten().unwrap_or_default();
    let total = server_export.files.len() as u64;
    let mut transferred = 0usize;
    for (i, f) in server_export.files.iter().enumerate() {
        let done = i as u64 + 1;
        let _ = out.send(SyncEvent::FileProgress {
            done,
            total,
            name: f.path.clone(),
        });
        if music_dir.is_empty() || f.path.starts_with('/') || f.path.contains("..") {
            continue;
        }
        let dest = std::path::Path::new(&music_dir).join(&f.path);
        // Don't transfer an existing file of the same size again.
        if f.size != 0 && std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) == f.size {
            continue;
        }
        if client.download_file(&f.path, &dest).is_ok() {
            transferred += 1;
        }
    }
    let _ = out.send(SyncEvent::TransferDone { files: transferred });

    client.disconnect();
    let _ = out.send(SyncEvent::PeerDisconnected);
}
