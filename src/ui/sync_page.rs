//! Device sync as a standalone relm4 component: a mode-selection dialog plus a
//! dedicated, navigation-free window for each flow (offer/QR or scan/camera),
//! which morphs into the paired/progress view and finally a green success
//! message in place. Owns the server thread and the client worker. Extracted
//! from the `App` god-object.
//!
//! Logic/network lives in [`crate::core::sync`]; here only widgets + event flow.
//! The component owns its dialogs and worker; it tells the parent (via `Output`)
//! only the two things the parent still needs: the connected state (for the
//! green header icon) and that an import happened (so the parent reloads the
//! affected library views).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use std::path::{Component as PathComponent, Path, PathBuf};

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::sync::client::SyncClient;
use crate::core::sync::protocol::{self, Capabilities, PairingInfo};
use crate::core::sync::scanner::Scanner;
use crate::core::sync::server::{ShareChannel, SyncServer};
use crate::core::sync::share::{self, ShareDecision, ShareManifest};
use crate::core::sync::{self, crypto, data, SyncEvent};
use crate::i18n::{gettext, gettext_f};
use crate::ui::sync_share_ui::{build_confirm, build_picker, build_review, PickerHandles, ReviewHandles};

/// Commands from the UI to the persistent client-session worker.
enum ClientCmd {
    /// Resolve a selection to a manifest (worker builds it, emits `ManifestReady`).
    Prepare(share::Selection),
    /// Send the prepared offer, then upload accepted files.
    Send,
    /// Respond to an incoming offer, then download accepted files + apply.
    Decide(ShareDecision),
    /// Reject an incoming offer.
    Reject,
    /// Tear the session down.
    Cancel,
}

fn safe_sync_dest(music_dir: &str, rel_path: &str) -> Option<PathBuf> {
    if music_dir.is_empty() || rel_path.is_empty() {
        return None;
    }
    let rel = Path::new(rel_path);
    if rel.is_absolute() {
        return None;
    }
    if rel.components().any(|c| !matches!(c, PathComponent::Normal(_))) {
        return None;
    }
    let base = std::fs::canonicalize(music_dir).ok()?;
    let dest = base.join(rel);
    let parent = dest.parent()?;
    std::fs::create_dir_all(parent).ok()?;
    let parent = std::fs::canonicalize(parent).ok()?;
    parent.starts_with(&base).then_some(dest)
}

/// The device-sync component (owns the dialogs, server thread and client worker).
pub(crate) struct SyncPage {
    /// Own DB connection (device id/name from the settings).
    library: Library,
    /// Window the dialogs are presented on (set on `Open`).
    window: Option<adw::ApplicationWindow>,
    /// Stop flag of the server thread (set → accept loop ends).
    stop: Option<Arc<AtomicBool>>,
    /// Running camera scanner pipeline (drop stops the camera).
    scanner: Option<Scanner>,
    /// Mode-selection dialog ("Device sync").
    dialog: Option<adw::Dialog>,
    /// Standalone window of the active flow (offer/QR or scan) — no navigation.
    sub: Option<adw::Dialog>,
    qr: Option<gtk::Picture>,
    cam: Option<gtk::Picture>,
    /// The QR/camera section of the active flow window, hidden once paired so the
    /// same window shows only the connection status/progress.
    details: Option<gtk::Box>,
    /// Status label (pairing, import, transfer) of the active flow window.
    status: Option<gtk::Label>,
    /// Address/waiting label of the offer window.
    server_status: Option<gtk::Label>,
    progress: Option<gtk::ProgressBar>,
    /// Green "sync successful" box (icon + title + detail), shown when done.
    success: Option<gtk::Box>,
    success_detail: Option<gtk::Label>,
    /// Whether a successful exchange happened (→ show the success box on finish).
    synced_ok: bool,
    /// Accumulated summary (imported counts, transferred files) for the success box.
    sync_summary: String,
    /// Capabilities the peer advertised at pair time (gates e.g. YouTube sharing).
    peer_caps: Capabilities,
    /// Peer display name (for the share/review labels).
    peer_name: String,
    /// Whether this device is the HTTPS server (offerer) or the scanning client.
    is_server: bool,
    /// ToolbarView of the flow window, so its content can be swapped (picker /
    /// confirm / review / progress).
    sub_toolbar: Option<adw::ToolbarView>,
    /// Server role: UI→server share handshake channel.
    share_chan: Option<Arc<Mutex<ShareChannel>>>,
    /// Client role: command channel to the session worker.
    client_cmd: Option<mpsc::Sender<ClientCmd>>,
    /// Sender picker widget handles (read into a `Selection` on continue).
    picker: Option<PickerHandles>,
    /// Receiver review widget handles (read into a `ShareDecision` on accept).
    review: Option<ReviewHandles>,
    /// The offer we are about to send (server role parks it; client role sends).
    prepared_manifest: Option<ShareManifest>,
    /// The offer we received and are reviewing.
    incoming_manifest: Option<ShareManifest>,
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
    /// User tapped "Share" on the paired screen → open the selection picker.
    OpenSharePicker,
    /// Picker "Continue" → resolve the selection to a manifest (size confirm next).
    PreparePicked,
    /// Size-confirmation "Send" → park/send the prepared offer.
    ConfirmSend,
    /// Cancel out of the picker/confirm back to the paired screen.
    CancelShare,
    /// Review "Accept" → apply the (selectively) accepted offer.
    AcceptOffer,
    /// Review "Reject all".
    RejectOffer,
    /// The flow window (offer/scan) was closed → stop its activity.
    SubClosed,
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
        // Hidden placeholder: the component only manages *presented* dialogs.
        #[root]
        gtk::Box {}
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        _sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // A failed second connection must not crash the whole app; degrade to a
        // temporary in-memory DB (logged) instead of panicking the UI thread.
        let library = Library::open_or_memory();
        let model = SyncPage {
            library,
            window: None,
            stop: None,
            scanner: None,
            dialog: None,
            sub: None,
            qr: None,
            cam: None,
            details: None,
            status: None,
            server_status: None,
            progress: None,
            success: None,
            success_detail: None,
            synced_ok: false,
            sync_summary: String::new(),
            peer_caps: Capabilities::default(),
            peer_name: String::new(),
            is_server: false,
            sub_toolbar: None,
            share_chan: None,
            client_cmd: None,
            picker: None,
            review: None,
            prepared_manifest: None,
            incoming_manifest: None,
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
            SyncInput::OpenSharePicker => self.open_share_picker(&sender),
            SyncInput::PreparePicked => self.prepare_picked(&sender),
            SyncInput::ConfirmSend => self.confirm_send(),
            SyncInput::CancelShare => self.show_paired_panel(&sender),
            SyncInput::AcceptOffer => self.accept_offer(&sender),
            SyncInput::RejectOffer => self.reject_offer(&sender),
            SyncInput::SubClosed => self.close_sub(&sender),
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

    /// Whether YouTube is enabled locally (gates YT sharing in both directions).
    fn youtube_enabled(&self) -> bool {
        self.library.get_setting("youtube_enabled").ok().flatten().as_deref() == Some("1")
    }

    /// Opens the mode-selection dialog (offer a connection / scan a code).
    fn open_dialog(&mut self, sender: &ComponentSender<Self>) {
        // Clean up any open dialog/server first.
        self.teardown(sender);
        let Some(window) = self.window.clone() else {
            return;
        };

        let dialog = adw::Dialog::builder()
            .title(&gettext("Device sync"))
            .content_width(420)
            .content_height(420)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&self.mode_content(sender)));
        dialog.set_child(Some(&toolbar));

        {
            let sender = sender.clone();
            dialog.connect_closed(move |_| sender.input(SyncInput::DialogClosed));
        }

        self.dialog = Some(dialog.clone());
        dialog.present(Some(&window));
    }

    /// The mode-selection content (two rows: offer / scan).
    fn mode_content(&self, sender: &ComponentSender<Self>) -> gtk::Box {
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

        let content = padded_vbox();
        content.append(&group);
        content
    }

    /// Presents a standalone flow window (no navigation: only a close button).
    /// `content` becomes its body; closing it sends [`SyncInput::SubClosed`].
    fn present_sub(
        &mut self,
        window: &adw::ApplicationWindow,
        title: &str,
        content: &impl gtk::prelude::IsA<gtk::Widget>,
        sender: &ComponentSender<Self>,
    ) {
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(content));

        let dialog = adw::Dialog::builder()
            .title(title)
            .content_width(440)
            .content_height(600)
            .build();
        dialog.set_child(Some(&toolbar));
        {
            let sender = sender.clone();
            dialog.connect_closed(move |_| sender.input(SyncInput::SubClosed));
        }
        self.sub = Some(dialog.clone());
        self.sub_toolbar = Some(toolbar);
        dialog.present(Some(window));
    }

    /// Swaps the body of the open flow window (picker / confirm / review / …).
    fn set_sub_content(&self, content: &impl gtk::prelude::IsA<gtk::Widget>) {
        if let Some(tb) = &self.sub_toolbar {
            tb.set_content(Some(content));
        }
    }

    /// Builds the standard progress panel (status + progress bar + green success
    /// box) and stores the handles, returning the container to mount.
    fn progress_panel(&mut self) -> gtk::Box {
        let status = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        let progress = gtk::ProgressBar::builder().show_text(true).visible(false).build();
        let (success, success_detail) = build_success();
        let content = padded_vbox();
        content.set_valign(gtk::Align::Center);
        content.append(&status);
        content.append(&progress);
        content.append(&success);
        self.status = Some(status);
        self.progress = Some(progress);
        self.success = Some(success);
        self.success_detail = Some(success_detail);
        self.details = None;
        content
    }

    /// Shows the post-pairing panel: a "Share" button + a hint, plus the progress
    /// panel widgets (used later for transfer status / success).
    fn show_paired_panel(&mut self, sender: &ComponentSender<Self>) {
        let panel = self.progress_panel();
        let share_btn = gtk::Button::builder()
            .label(&gettext_f("Share with {peer}", &[("peer", &self.peer_name)]))
            .css_classes(["suggested-action", "pill"])
            .halign(gtk::Align::Center)
            .build();
        {
            let sender = sender.clone();
            share_btn.connect_clicked(move |_| sender.input(SyncInput::OpenSharePicker));
        }
        let hint = gtk::Label::builder()
            .label(&gettext("…or wait for the other device to share."))
            .css_classes(["dim-label"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        panel.prepend(&hint);
        panel.prepend(&share_btn);
        self.set_sub_content(&panel);
    }

    /// Start server mode: present the standalone QR window and run the server.
    fn start_server(&mut self, sender: &ComponentSender<Self>) {
        if self.sub.is_some() {
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        self.synced_ok = false;
        self.sync_summary.clear();

        // QR + waiting status live in a "details" box that is hidden on pairing.
        let qr = gtk::Picture::builder()
            .width_request(220)
            .height_request(220)
            .can_shrink(true)
            .content_fit(gtk::ContentFit::Contain)
            .hexpand(false)
            .halign(gtk::Align::Center)
            .build();
        let server_status = gtk::Label::builder()
            .wrap(true)
            .css_classes(["dim-label"])
            .justify(gtk::Justification::Center)
            .build();
        let hint = gtk::Label::builder()
            .label(&gettext("Scan this code on the other device."))
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        let details = gtk::Box::new(gtk::Orientation::Vertical, 12);
        details.set_valign(gtk::Align::Center);
        details.append(&hint);
        details.append(&qr);
        details.append(&server_status);

        let status = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        let progress = gtk::ProgressBar::builder().show_text(true).visible(false).build();
        let (success, success_detail) = build_success();

        let content = padded_vbox();
        content.set_valign(gtk::Align::Center);
        content.append(&details);
        content.append(&status);
        content.append(&progress);
        content.append(&success);

        self.present_sub(&window, &gettext("Offer connection"), &content, sender);
        self.qr = Some(qr);
        self.server_status = Some(server_status);
        self.status = Some(status);
        self.progress = Some(progress);
        self.details = Some(details);
        self.success = Some(success);
        self.success_detail = Some(success_detail);

        self.is_server = true;
        if self.stop.is_some() {
            return; // already running
        }
        let device_name = self.device_name();
        let stop = Arc::new(AtomicBool::new(false));
        // Create the server on the UI thread so we can grab its share channel,
        // then move it into the worker for the (blocking) accept loop.
        let server = match SyncServer::start(device_name, stop.clone()) {
            Ok(s) => s,
            Err(e) => {
                if let Some(st) = &self.status {
                    st.set_text(&gettext_f("Cannot start: {err}", &[("err", &e.to_string())]));
                }
                return;
            }
        };
        self.share_chan = Some(server.share_channel());
        self.stop = Some(stop);
        let (pair_url, host, port) = (server.pair_url(), server.host().to_string(), server.port());
        sender.spawn_command(move |out| {
            let _ = out.send(SyncEvent::ServerReady { pair_url, host, port });
            server.run(|ev| {
                let _ = out.send(ev);
            });
            let _ = out.send(SyncEvent::ServerStopped);
        });
    }

    /// Start client mode: present the standalone scan window with live preview.
    fn start_scan(&mut self, sender: &ComponentSender<Self>) {
        if self.sub.is_some() {
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        self.synced_ok = false;
        self.sync_summary.clear();
        self.is_server = false;

        let cam = gtk::Picture::builder()
            .width_request(320)
            .height_request(240)
            .content_fit(gtk::ContentFit::Contain)
            .halign(gtk::Align::Center)
            .build();
        let hint = gtk::Label::builder()
            .label(&gettext("Point the camera at the QR code."))
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        let details = gtk::Box::new(gtk::Orientation::Vertical, 12);
        details.set_valign(gtk::Align::Center);
        details.append(&hint);
        details.append(&cam);

        let status = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        let progress = gtk::ProgressBar::builder().show_text(true).visible(false).build();
        let (success, success_detail) = build_success();

        let content = padded_vbox();
        content.set_valign(gtk::Align::Center);
        content.append(&details);
        content.append(&status);
        content.append(&progress);
        content.append(&success);

        self.present_sub(&window, &gettext("Scan QR code"), &content, sender);
        self.cam = Some(cam);
        self.status = Some(status);
        self.progress = Some(progress);
        self.details = Some(details);
        self.success = Some(success);
        self.success_detail = Some(success_detail);

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
            Err(e) => {
                // Surface the reason in the window instead of an empty preview.
                tracing::warn!("Camera scanner failed: {e}");
                if let Some(d) = &self.details {
                    d.set_visible(false);
                }
                if let Some(st) = &self.status {
                    st.set_text(&gettext_f(
                        "Camera unavailable: {err}",
                        &[("err", &e.to_string())],
                    ));
                }
            }
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
        let yt_enabled = self.youtube_enabled();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        self.client_cmd = Some(cmd_tx);
        sender.spawn_command(move |out| {
            run_client_session(info, device_id, device_name, yt_enabled, cmd_rx, &out);
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
            SyncEvent::PeerPaired { peer_name, peer_caps } => {
                self.peer_caps = peer_caps;
                self.peer_name = peer_name;
                // Paired → tint the sync icon at the top green.
                let _ = sender.output(SyncOutput::ConnectedChanged(true));
                // Swap the QR/camera window for the paired panel (Share button +
                // status/progress/success).
                self.show_paired_panel(sender);
                if let Some(st) = &self.status {
                    st.set_text(&gettext_f("Connected with {name}", &[("name", &self.peer_name)]));
                }
            }
            SyncEvent::ImportReceived { stats } => {
                // A successful metadata exchange happened.
                self.synced_ok = true;
                self.sync_summary = gettext_f(
                    "Received {fav} favorites, {pl} playlists, {pod} podcasts.",
                    &[
                        ("fav", &stats.favorites.to_string()),
                        ("pl", &stats.playlists.to_string()),
                        ("pod", &stats.podcasts.to_string()),
                    ],
                );
                if let Some(st) = &self.status {
                    st.set_text(&self.sync_summary);
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
                // Client side: files done → success now (with a combined summary).
                self.synced_ok = true;
                let files_line =
                    gettext_f("{n} files transferred.", &[("n", &files.to_string())]);
                self.sync_summary = if self.sync_summary.is_empty() {
                    files_line
                } else {
                    format!("{}\n{}", self.sync_summary, files_line)
                };
                self.show_success();
            }
            SyncEvent::PeerDisconnected => {
                let _ = sender.output(SyncOutput::ConnectedChanged(false));
                // Server side has no TransferDone: show success here if the
                // exchange completed, otherwise a neutral disconnect note.
                if self.synced_ok {
                    self.show_success();
                } else if let Some(st) = &self.status {
                    st.set_text(&gettext("Disconnected."));
                }
            }
            SyncEvent::ServerStopped => {
                let _ = sender.output(SyncOutput::ConnectedChanged(false));
                self.stop = None;
            }
            SyncEvent::ShareOffered { manifest } => self.on_share_offered(manifest, sender),
            SyncEvent::ManifestReady { manifest } => self.on_manifest_ready(manifest, sender),
            SyncEvent::OfferAccepted { .. } => {
                if let Some(st) = &self.status {
                    st.set_text(&gettext("The other device accepted. Transferring …"));
                }
            }
            SyncEvent::Error(msg) => {
                tracing::warn!("Sync error: {msg}");
                if let Some(st) = &self.status {
                    st.set_text(&gettext_f("Error: {e}", &[("e", &msg)]));
                }
            }
        }
    }

    /// Replaces the QR/status/progress widgets with the green success message.
    fn show_success(&self) {
        if let Some(d) = &self.details {
            d.set_visible(false);
        }
        if let Some(st) = &self.status {
            st.set_visible(false);
        }
        if let Some(p) = &self.progress {
            p.set_visible(false);
        }
        if let Some(det) = &self.success_detail {
            det.set_text(&self.sync_summary);
        }
        if let Some(sb) = &self.success {
            sb.set_visible(true);
        }
    }

    // --- Selective share (sender + receiver) -------------------------------

    /// "Share" tapped → show the selection picker.
    fn open_share_picker(&mut self, sender: &ComponentSender<Self>) {
        let (page, handles) = build_picker(&self.library, &self.peer_name, &self.peer_caps, sender);
        self.picker = Some(handles);
        self.set_sub_content(&page);
    }

    /// Picker "Continue" → resolve the selection to a manifest off the UI thread.
    fn prepare_picked(&mut self, sender: &ComponentSender<Self>) {
        let Some(picker) = self.picker.take() else { return };
        let sel = picker.to_selection();
        // Brief feedback while hashing runs.
        let panel = self.progress_panel();
        self.set_sub_content(&panel);
        if let Some(st) = &self.status {
            st.set_text(&gettext("Preparing …"));
        }
        if self.is_server {
            let yt = self.peer_caps.youtube_enabled;
            sender.spawn_oneshot_command(move || {
                let m = Library::open()
                    .ok()
                    .and_then(|lib| share::build_manifest(&lib, &sel, yt).ok())
                    .unwrap_or_default();
                SyncEvent::ManifestReady { manifest: m }
            });
        } else if let Some(cmd) = &self.client_cmd {
            let _ = cmd.send(ClientCmd::Prepare(sel));
        }
    }

    /// Manifest built → show the size confirmation.
    fn on_manifest_ready(&mut self, manifest: ShareManifest, sender: &ComponentSender<Self>) {
        let names: Vec<String> = manifest
            .files
            .iter()
            .take(8)
            .map(|f| if f.rel_path.is_empty() { f.title.clone() } else { f.rel_path.clone() })
            .collect();
        let total = manifest.total_size;
        let count = manifest.files.len();
        self.prepared_manifest = Some(manifest);
        let page = build_confirm(total, count, &names, sender);
        self.set_sub_content(&page);
    }

    /// Confirmation "Send" → park (server) or send (client) the prepared offer.
    fn confirm_send(&mut self) {
        let panel = self.progress_panel();
        self.set_sub_content(&panel);
        if self.is_server {
            if let (Some(chan), Some(m)) = (&self.share_chan, self.prepared_manifest.clone()) {
                if let Ok(mut c) = chan.lock() {
                    c.outgoing = Some(m);
                }
            }
            if let Some(st) = &self.status {
                st.set_text(&gettext("Waiting for the other device to accept …"));
            }
        } else {
            if let Some(cmd) = &self.client_cmd {
                let _ = cmd.send(ClientCmd::Send);
            }
            if let Some(st) = &self.status {
                st.set_text(&gettext("Sending …"));
            }
        }
    }

    /// Peer offered a share → classify it and show the review screen.
    fn on_share_offered(&mut self, manifest: ShareManifest, sender: &ComponentSender<Self>) {
        let reviews = share::review_files(&self.library, &manifest);
        let yt = self.youtube_enabled();
        let (page, handles) = build_review(&manifest, &reviews, yt, sender);
        self.review = Some(handles);
        self.incoming_manifest = Some(manifest);
        self.set_sub_content(&page);
    }

    /// Review "Accept" → apply the (selectively) accepted offer.
    fn accept_offer(&mut self, sender: &ComponentSender<Self>) {
        let Some(review) = self.review.take() else { return };
        let decision = review.to_decision();
        let panel = self.progress_panel();
        self.set_sub_content(&panel);
        if let Some(st) = &self.status {
            st.set_text(&gettext("Receiving …"));
        }
        if self.is_server {
            // Park the decision (client uploads files) + apply blobs/YT and
            // register accepted files locally off the UI thread.
            if let Some(chan) = &self.share_chan {
                if let Ok(mut c) = chan.lock() {
                    c.decision = Some(decision.clone());
                }
            }
            self.synced_ok = true;
            if let Some(manifest) = self.incoming_manifest.clone() {
                let dec = decision;
                sender.spawn_oneshot_command(move || {
                    let stats = Library::open()
                        .map(|lib| apply_received(&lib, &manifest, &dec))
                        .unwrap_or_default();
                    SyncEvent::ImportReceived { stats }
                });
            }
        } else if let Some(cmd) = &self.client_cmd {
            let _ = cmd.send(ClientCmd::Decide(decision));
        }
    }

    /// Review "Reject all".
    fn reject_offer(&mut self, sender: &ComponentSender<Self>) {
        self.review = None;
        self.incoming_manifest = None;
        if self.is_server {
            if let Some(chan) = &self.share_chan {
                if let Ok(mut c) = chan.lock() {
                    c.decision = Some(ShareDecision::default()); // accept = false
                }
            }
        } else if let Some(cmd) = &self.client_cmd {
            let _ = cmd.send(ClientCmd::Reject);
        }
        self.show_paired_panel(sender);
    }

    /// The flow window was closed: stop its server/camera but keep the
    /// mode-selection dialog open so the user can pick the other option.
    fn close_sub(&mut self, sender: &ComponentSender<Self>) {
        let _ = sender.output(SyncOutput::ConnectedChanged(false));
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.scanner = None; // drop stops the camera
        self.sub = None;
        self.clear_flow_widgets();
    }

    /// Cleans up all sync state (stop server, camera off, close both dialogs).
    fn teardown(&mut self, sender: &ComponentSender<Self>) {
        let _ = sender.output(SyncOutput::ConnectedChanged(false));
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.scanner = None; // drop stops the camera
        if let Some(sub) = self.sub.take() {
            sub.close();
        }
        if let Some(dialog) = self.dialog.take() {
            dialog.close();
        }
        self.clear_flow_widgets();
    }

    /// Drops the per-flow widget handles (after a flow window is gone).
    fn clear_flow_widgets(&mut self) {
        self.qr = None;
        self.cam = None;
        self.details = None;
        self.status = None;
        self.server_status = None;
        self.progress = None;
        self.success = None;
        self.success_detail = None;
        self.synced_ok = false;
        self.sync_summary.clear();
        self.sub_toolbar = None;
        self.share_chan = None;
        self.client_cmd = None;
        self.picker = None;
        self.review = None;
        self.prepared_manifest = None;
        self.incoming_manifest = None;
        self.is_server = false;
    }
}

/// A vertical box with the standard dialog padding.
fn padded_vbox() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 12);
    b.set_margin_top(12);
    b.set_margin_bottom(12);
    b.set_margin_start(12);
    b.set_margin_end(12);
    b
}

/// The green "sync successful" box (hidden until shown). Returns the box and its
/// detail label (filled with the imported counts / transferred files).
fn build_success() -> (gtk::Box, gtk::Label) {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
    b.set_valign(gtk::Align::Center);
    b.set_visible(false);

    let icon = gtk::Image::from_icon_name("emblem-ok-symbolic");
    icon.set_pixel_size(64);
    icon.add_css_class("success");

    let title = gtk::Label::builder()
        .label(&gettext("Sync successful"))
        .css_classes(["title-2", "success"])
        .build();
    let detail = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label"])
        .build();

    b.append(&icon);
    b.append(&title);
    b.append(&detail);
    (b, detail)
}

/// Persistent client-session worker: pairs, then loops — polling for an incoming
/// offer and serving commands from the UI (prepare/send an offer, accept/reject
/// an incoming one) — with a periodic ping that keeps the server session warm
/// while the user reviews. Each terminal action runs to completion then ends.
fn run_client_session(
    info: PairingInfo,
    device_id: String,
    device_name: String,
    yt_enabled: bool,
    cmd_rx: mpsc::Receiver<ClientCmd>,
    out: &relm4::Sender<SyncEvent>,
) {
    let caps = Capabilities { schema: protocol::SCHEMA_VERSION, youtube_enabled: yt_enabled };
    let mut client = SyncClient::new(&info, device_id, device_name, caps);
    if let Err(e) = client.pair(&info.token) {
        let _ = out.send(SyncEvent::Error(e.to_string()));
        return;
    }
    let _ = out.send(SyncEvent::PeerPaired {
        peer_name: client.peer_name.clone(),
        peer_caps: client.peer_caps.clone(),
    });

    let mut prepared: Option<ShareManifest> = None;
    let mut offered = false;
    let mut last_ping = Instant::now();
    loop {
        if last_ping.elapsed() > Duration::from_secs(10) {
            let _ = client.ping();
            last_ping = Instant::now();
        }
        // Poll for an offer from the server-side user (only while idle).
        if !offered && prepared.is_none() {
            if let Ok(Some(m)) = client.fetch_offer() {
                offered = true;
                let _ = out.send(SyncEvent::ShareOffered { manifest: m });
            }
        }
        match cmd_rx.recv_timeout(Duration::from_millis(300)) {
            Ok(ClientCmd::Prepare(sel)) => {
                let m = Library::open()
                    .ok()
                    .and_then(|lib| share::build_manifest(&lib, &sel, client.peer_caps.youtube_enabled).ok())
                    .unwrap_or_default();
                prepared = Some(m.clone());
                let _ = out.send(SyncEvent::ManifestReady { manifest: m });
            }
            Ok(ClientCmd::Send) => {
                let Some(m) = prepared.clone() else { continue };
                if let Err(e) = client.send_offer(&m) {
                    let _ = out.send(SyncEvent::Error(e.to_string()));
                    break;
                }
                // Wait for the peer's decision (keep pinging).
                let decision = loop {
                    if last_ping.elapsed() > Duration::from_secs(10) {
                        let _ = client.ping();
                        last_ping = Instant::now();
                    }
                    match client.fetch_decision() {
                        Ok(Some(d)) => break d,
                        Ok(None) => {}
                        Err(_) => break ShareDecision::default(),
                    }
                    if matches!(cmd_rx.try_recv(), Ok(ClientCmd::Cancel)) {
                        break ShareDecision::default();
                    }
                    std::thread::sleep(Duration::from_millis(400));
                };
                if decision.accept {
                    let files = client_upload(&client, &m, &decision, out);
                    let _ = out.send(SyncEvent::TransferDone { files });
                }
                break;
            }
            Ok(ClientCmd::Decide(decision)) => {
                // Re-fetch the offer to know which files to pull (the server keeps
                // it parked); then send the decision and transfer + apply.
                if let Ok(Some(m)) = client.fetch_offer() {
                    let _ = client.send_decision(&decision);
                    client_receive(&client, &m, &decision, out);
                }
                break;
            }
            Ok(ClientCmd::Reject) => {
                let _ = client.send_decision(&ShareDecision::default());
                break;
            }
            Ok(ClientCmd::Cancel) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    client.disconnect();
    let _ = out.send(SyncEvent::PeerDisconnected);
}

/// Uploads the accepted files from our library to the peer (client-as-sender).
fn client_upload(
    client: &SyncClient,
    manifest: &ShareManifest,
    decision: &ShareDecision,
    out: &relm4::Sender<SyncEvent>,
) -> usize {
    let base = Library::open().ok().and_then(|l| l.get_setting("music_dir").ok().flatten()).unwrap_or_default();
    let total = manifest.files.iter().filter(|f| decision.files.contains(&f.rel_path)).count() as u64;
    let mut done = 0u64;
    let mut n = 0usize;
    for f in manifest.files.iter().filter(|f| decision.files.contains(&f.rel_path)) {
        done += 1;
        let _ = out.send(SyncEvent::FileProgress { done, total, name: f.rel_path.clone() });
        let abs = data::resolve(&f.rel_path, &base);
        if client.upload_file(&f.rel_path, Path::new(&abs)).is_ok() {
            n += 1;
        }
    }
    n
}

/// Downloads the accepted files into our library + applies library/YT blobs
/// (client-as-receiver).
fn client_receive(
    client: &SyncClient,
    manifest: &ShareManifest,
    decision: &ShareDecision,
    out: &relm4::Sender<SyncEvent>,
) {
    let Ok(lib) = Library::open() else { return };
    let base = lib.get_setting("music_dir").ok().flatten().unwrap_or_default();
    let total = manifest.files.iter().filter(|f| decision.files.contains(&f.rel_path)).count() as u64;
    let mut done = 0u64;
    for f in manifest.files.iter().filter(|f| decision.files.contains(&f.rel_path)) {
        done += 1;
        let _ = out.send(SyncEvent::FileProgress { done, total, name: f.rel_path.clone() });
        if let Some(dest) = safe_sync_dest(&base, &f.rel_path) {
            if client.download_file(&f.rel_path, &dest).is_ok() {
                register_track(&lib, &base, f);
            }
        }
    }
    let stats = apply_received(&lib, manifest, decision);
    let _ = out.send(SyncEvent::ImportReceived { stats });
    let _ = out.send(SyncEvent::TransferDone { files: total as usize });
}

/// Applies library/YT blobs and registers the accepted files in the `track`
/// table so they are immediately playable (no rescan needed).
fn apply_received(lib: &Library, manifest: &ShareManifest, decision: &ShareDecision) -> sync::ImportStats {
    let mut stats = share::apply_manifest(lib, manifest, decision).unwrap_or_default();
    let base = lib.get_setting("music_dir").ok().flatten().unwrap_or_default();
    let mut n = 0;
    for f in manifest.files.iter().filter(|f| decision.files.contains(&f.rel_path)) {
        register_track(lib, &base, f);
        n += 1;
    }
    stats.files = n;
    stats
}

/// Inserts a minimal `track` row for a received file from its manifest metadata.
fn register_track(lib: &Library, base: &str, f: &share::ManifestFile) {
    let _ = lib.upsert_track(&crate::model::Track {
        path: data::resolve(&f.rel_path, base),
        title: f.title.clone(),
        artist: f.artist.clone(),
        album: f.album.clone(),
        duration_ms: f.duration_ms,
        ..Default::default()
    });
}

#[cfg(test)]
mod tests {
    use super::safe_sync_dest;

    #[test]
    fn safe_sync_dest_accepts_only_relative_normal_paths() {
        let base = std::env::temp_dir().join(format!(
            "emilia-sync-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let base_s = base.to_string_lossy();

        let ok = safe_sync_dest(&base_s, "Album/track.mp3").unwrap();
        assert!(ok.starts_with(&base));
        assert!(ok.ends_with("Album/track.mp3"));

        assert!(safe_sync_dest(&base_s, "/etc/passwd").is_none());
        assert!(safe_sync_dest(&base_s, "../escape.mp3").is_none());
        assert!(safe_sync_dest(&base_s, "Album/../escape.mp3").is_none());

        let _ = std::fs::remove_dir_all(base);
    }
}
