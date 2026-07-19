//! Device sync as a standalone relm4 component. **The live connection is
//! decoupled from any window:** the server thread / client worker keep running in
//! the background once paired, so the pairing survives closing the window. A
//! single navigation-free window swaps its content through the flow
//! (mode-select → QR/scan → "Connected with X" with a Disconnect action). The
//! header sync icon routes here for **pairing/status only**; it never starts a
//! share. Sharing is always initiated from an item's detail view, which hands us
//! a ready `Selection` via [`SyncInput::ShareSelection`] → size confirm →
//! progress → success. Extracted from the `App` god-object.
//!
//! Logic/network lives in [`crate::core::sync`]; here only widgets + event flow.
//! The component owns its window and worker; it tells the parent (via `Output`)
//! only the two things the parent still needs: the connected state (for the
//! green header icon) and that an import happened (so the parent reloads the
//! affected library views).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

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
use crate::ui::sync_share_ui::{build_confirm, build_review, ReviewHandles};

/// Commands from the UI to the persistent client-session worker.
enum ClientCmd {
    /// Resolve a selection to a manifest (worker builds it, emits `ManifestReady`).
    Prepare(Box<share::Selection>),
    /// Send the prepared offer, then upload accepted files.
    Send,
    /// Respond to an incoming offer, then download accepted files + apply.
    Decide(ShareDecision),
    /// Reject an incoming offer.
    Reject,
    /// Tear the session down (user tapped "Disconnect").
    Disconnect,
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
    /// Whether a live pairing exists (kept alive in the background, independent of
    /// the window). Drives the green header icon and the "Share" routing.
    connected: bool,
    /// The single navigation-free flow window — its content is swapped through the
    /// whole flow. `None` while no window is shown (the connection may still live).
    sub: Option<adw::Dialog>,
    qr: Option<gtk::Picture>,
    /// Offer side: read-only entry showing the pairing code as copyable text
    /// (the same `emilia://pair?…` URL the QR encodes), for cut&paste pairing.
    code_field: Option<gtk::Entry>,
    cam: Option<gtk::Picture>,
    /// Status label (pairing, import, transfer) of the current flow content.
    status: Option<gtk::Label>,
    /// Address/waiting label of the offer content.
    server_status: Option<gtk::Label>,
    progress: Option<gtk::ProgressBar>,
    /// Whether a successful exchange happened (→ show the success screen on finish).
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
    /// Header sync icon tapped: open the flow window. While connected this lands
    /// on the connected panel (Disconnect); otherwise on the mode
    /// selection (offer / scan).
    Open(adw::ApplicationWindow),
    StartServer,
    StartScan,
    QrDecoded(String),
    /// A pairing code pasted/typed as text (cut&paste alternative to scanning).
    PasteCode(String),
    /// Share a concrete selection built from a detail view with the connected
    /// peer: resolve it to a manifest off-thread, then show the size confirmation.
    ShareSelection {
        window: adw::ApplicationWindow,
        selection: Box<share::Selection>,
    },
    /// Size-confirmation "Send" → park/send the prepared offer.
    ConfirmSend,
    /// Cancel out of the confirmation back to the connected panel.
    CancelShare,
    /// Review "Accept" → apply the (selectively) accepted offer.
    AcceptOffer,
    /// Review "Reject all".
    RejectOffer,
    /// Transfer-success "Done" → back to the connected panel (stay paired).
    BackToConnected,
    /// User tapped "Disconnect" → end the live pairing.
    Disconnect,
    /// The flow window was closed. Keeps the connection alive if still paired;
    /// otherwise (e.g. closed mid-pairing) stops the server/scanner.
    SubClosed,
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
            connected: false,
            sub: None,
            qr: None,
            code_field: None,
            cam: None,
            status: None,
            server_status: None,
            progress: None,
            synced_ok: false,
            sync_summary: String::new(),
            peer_caps: Capabilities::default(),
            peer_name: String::new(),
            is_server: false,
            sub_toolbar: None,
            share_chan: None,
            client_cmd: None,
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
                self.open_entry(&sender);
            }
            SyncInput::StartServer => self.start_server(&sender),
            SyncInput::StartScan => self.start_scan(&sender),
            SyncInput::QrDecoded(url) => self.handle_qr(&url, false, &sender),
            SyncInput::PasteCode(text) => self.handle_qr(text.trim(), true, &sender),
            SyncInput::ShareSelection { window, selection } => {
                self.window = Some(window);
                self.share_selection(*selection, &sender);
            }
            SyncInput::ConfirmSend => self.confirm_send(),
            SyncInput::CancelShare => self.show_connected_panel(&sender),
            SyncInput::AcceptOffer => self.accept_offer(&sender),
            SyncInput::RejectOffer => self.reject_offer(&sender),
            SyncInput::BackToConnected => self.show_connected_panel(&sender),
            SyncInput::Disconnect => self.disconnect_peer(&sender),
            SyncInput::SubClosed => self.close_sub(&sender),
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
        self.library
            .get_setting("youtube_enabled")
            .ok()
            .flatten()
            .as_deref()
            == Some("1")
    }

    /// Header sync-icon entry: ensure the flow window exists, then land on the
    /// connected panel (if a pairing is live) or the mode selection.
    fn open_entry(&mut self, sender: &ComponentSender<Self>) {
        self.ensure_window(sender);
        if self.connected {
            self.show_connected_panel(sender);
        } else {
            self.show_mode_select(sender);
        }
    }

    /// Presents the single flow window (once); later phases only swap its content.
    fn ensure_window(&mut self, sender: &ComponentSender<Self>) {
        if self.sub.is_some() {
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        // No fixed content_height: the dialog follows its content's natural
        // height (mode select is short, QR/camera/review grow as needed).
        let dialog = adw::Dialog::builder()
            .title(gettext("Connect to share"))
            .content_width(440)
            .build();
        dialog.set_child(Some(&toolbar));
        {
            let sender = sender.clone();
            dialog.connect_closed(move |_| sender.input(SyncInput::SubClosed));
        }
        self.sub = Some(dialog.clone());
        self.sub_toolbar = Some(toolbar);
        crate::ui::app_helpers::close_on_click_outside(&dialog);
        dialog.present(Some(&window));
    }

    /// Updates the flow window's title (no-op if no window is shown).
    fn set_title(&self, title: &str) {
        if let Some(d) = &self.sub {
            d.set_title(title);
        }
    }

    /// Swaps the body of the open flow window (mode / QR / picker / review / …).
    fn set_sub_content(&self, content: &impl gtk::prelude::IsA<gtk::Widget>) {
        // Reset to the compact default size; phases that need more room (the
        // camera scan view) enlarge the window themselves afterwards.
        // `-1` = follow the content's natural height, so every phase gets exactly
        // the room it needs. The floor keeps the short phases (progress, mode
        // select) from rendering as a thin strip under the header bar.
        if let Some(d) = &self.sub {
            d.set_content_width(440);
            d.set_content_height(-1);
        }
        if let Some(tb) = &self.sub_toolbar {
            content.as_ref().set_size_request(-1, 240);
            tb.set_content(Some(content));
        }
    }

    /// Mode-selection content (offer a connection / scan a code).
    fn show_mode_select(&mut self, sender: &ComponentSender<Self>) {
        self.set_title(&gettext("Connect to share"));
        let group = adw::PreferencesGroup::builder()
            .description(gettext("Connect two devices on the same network."))
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
        self.set_sub_content(&content);
    }

    /// Builds a progress panel (status + progress bar) and stores the handles,
    /// returning the container to mount.
    fn progress_panel(&mut self) -> gtk::Box {
        let status = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        let progress = gtk::ProgressBar::builder()
            .show_text(true)
            .visible(false)
            .build();
        let content = padded_vbox();
        content.set_valign(gtk::Align::Center);
        content.append(&status);
        content.append(&progress);
        self.status = Some(status);
        self.progress = Some(progress);
        content
    }

    /// The connected panel — the single "Connected with X" success screen with a
    /// "Disconnect" action. The window's own header close button hides it while
    /// keeping the pairing alive, so no separate close/OK button is shown. Reached
    /// on pairing and whenever the header sync icon is tapped while a pairing is
    /// live. Sharing is **not** offered here; it is started per item from a detail
    /// view.
    fn show_connected_panel(&mut self, sender: &ComponentSender<Self>) {
        self.set_title(&gettext("Connect to share"));
        let content = padded_vbox();
        content.set_valign(gtk::Align::Center);

        let icon = gtk::Image::from_icon_name("emblem-ok-symbolic");
        icon.set_pixel_size(64);
        icon.add_css_class("success");
        let title = gtk::Label::builder()
            .label(gettext_f(
                "Connected with {name}",
                &[("name", &self.peer_name)],
            ))
            .css_classes(["title-2", "success"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        let hint = gtk::Label::builder()
            .label(gettext(
                "To share something, open a track or album and choose \u{201c}Share\u{201d}.",
            ))
            .css_classes(["dim-label"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();

        // Closing the window (its header close button) hides it but keeps the
        // pairing alive in the background; "Disconnect" actively ends it.
        let disconnect_btn = gtk::Button::builder()
            .label(gettext("Disconnect"))
            .css_classes(["destructive-action", "pill"])
            .halign(gtk::Align::Center)
            .build();
        {
            let sender = sender.clone();
            disconnect_btn.connect_clicked(move |_| sender.input(SyncInput::Disconnect));
        }

        content.append(&icon);
        content.append(&title);
        content.append(&hint);
        content.append(&disconnect_btn);
        // No live status widgets on this panel; a transfer rebuilds the progress
        // panel with its own status/progress labels.
        self.status = None;
        self.progress = None;
        self.set_sub_content(&content);
    }

    /// Start server mode: swap the window to the QR view and run the server.
    fn start_server(&mut self, sender: &ComponentSender<Self>) {
        self.ensure_window(sender);
        self.set_title(&gettext("Offer connection"));
        self.synced_ok = false;
        self.sync_summary.clear();

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
            .label(gettext(
                "Scan this code on the other device — or copy the code below and paste it there.",
            ))
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        // Copyable pairing code (the same URL the QR encodes) for cut&paste
        // pairing. Filled in once the server is ready (see `ServerReady`).
        let code_field = gtk::Entry::builder()
            .editable(false)
            .can_focus(true)
            .hexpand(true)
            .placeholder_text(gettext("Pairing code …"))
            .build();
        let copy_btn = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text(gettext("Copy code"))
            .valign(gtk::Align::Center)
            .build();
        {
            let code_field = code_field.clone();
            copy_btn.connect_clicked(move |b| {
                let text = code_field.text();
                if !text.is_empty() {
                    b.clipboard().set_text(&text);
                }
            });
        }
        let code_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        code_row.append(&code_field);
        code_row.append(&copy_btn);
        let status = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        let progress = gtk::ProgressBar::builder()
            .show_text(true)
            .visible(false)
            .build();

        let content = padded_vbox();
        content.set_valign(gtk::Align::Center);
        content.append(&hint);
        content.append(&qr);
        content.append(&server_status);
        content.append(&code_row);
        content.append(&status);
        content.append(&progress);
        self.set_sub_content(&content);

        self.qr = Some(qr);
        self.code_field = Some(code_field);
        self.server_status = Some(server_status);
        self.status = Some(status);
        self.progress = Some(progress);

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
                    st.set_text(&gettext_f(
                        "Cannot start: {err}",
                        &[("err", &e.to_string())],
                    ));
                }
                return;
            }
        };
        self.share_chan = Some(server.share_channel());
        self.stop = Some(stop);
        let (pair_url, host, port) = (server.pair_url(), server.host().to_string(), server.port());
        sender.spawn_command(move |out| {
            let _ = out.send(SyncEvent::ServerReady {
                pair_url,
                host,
                port,
            });
            server.run(|ev| {
                let _ = out.send(ev);
            });
            let _ = out.send(SyncEvent::ServerStopped);
        });
    }

    /// Start client mode: swap the window to the scan view with live preview.
    fn start_scan(&mut self, sender: &ComponentSender<Self>) {
        self.ensure_window(sender);
        self.set_title(&gettext("Scan QR code"));
        self.synced_ok = false;
        self.sync_summary.clear();
        self.is_server = false;

        let cam = gtk::Picture::builder()
            .width_request(360)
            .height_request(360)
            .hexpand(true)
            .vexpand(true)
            .halign(gtk::Align::Fill)
            .valign(gtk::Align::Fill)
            .content_fit(gtk::ContentFit::Contain)
            .build();
        let hint = gtk::Label::builder()
            .label(gettext(
                "Point the camera at the QR code — or paste a pairing code below.",
            ))
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        // Paste-a-code alternative to scanning: the offering device shows the
        // same code as copyable text.
        let paste_field = gtk::Entry::builder()
            .hexpand(true)
            .placeholder_text(gettext("Paste pairing code …"))
            .build();
        crate::ui::widgets::no_autofocus(&paste_field);
        let connect_btn = gtk::Button::builder()
            .label(gettext("Connect"))
            .css_classes(["suggested-action"])
            .valign(gtk::Align::Center)
            .build();
        {
            let sender = sender.clone();
            let paste_field = paste_field.clone();
            connect_btn.connect_clicked(move |_| {
                sender.input(SyncInput::PasteCode(paste_field.text().to_string()));
            });
        }
        {
            // Enter in the field connects too.
            let sender = sender.clone();
            paste_field.connect_activate(move |e| {
                sender.input(SyncInput::PasteCode(e.text().to_string()));
            });
        }
        let paste_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        paste_row.append(&paste_field);
        paste_row.append(&connect_btn);
        let status = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        let progress = gtk::ProgressBar::builder()
            .show_text(true)
            .visible(false)
            .build();

        let content = padded_vbox();
        content.append(&hint);
        content.append(&cam);
        content.append(&paste_row);
        content.append(&status);
        content.append(&progress);
        self.set_sub_content(&content);
        // Enlarge the flow window for the scan view so the camera preview is
        // large instead of a thumbnail (reset to compact by `set_sub_content`
        // for the other phases).
        if let Some(d) = &self.sub {
            d.set_content_width(560);
            d.set_content_height(640);
        }

        self.cam = Some(cam);
        self.status = Some(status);
        self.progress = Some(progress);

        if self.scanner.is_some() {
            return;
        }
        let sender_dec = sender.clone();
        match Scanner::start(move |url| sender_dec.input(SyncInput::QrDecoded(url))) {
            Ok((scanner, paintable)) => {
                // preview unavailable → the code is still detected
                if let (Some(cam), Some(p)) = (&self.cam, &paintable) {
                    cam.set_paintable(Some(p));
                }
                self.scanner = Some(scanner);
            }
            Err(e) => {
                // Surface the reason in the window instead of an empty preview.
                tracing::warn!("Camera scanner failed: {e}");
                if let Some(st) = &self.status {
                    st.set_text(&gettext_f(
                        "Camera unavailable: {err}",
                        &[("err", &e.to_string())],
                    ));
                }
            }
        }
    }

    /// A pairing code was decoded (QR) or pasted: validate the URL and start the
    /// client sync. `from_paste` surfaces a parse error in the status label (a
    /// scanned frame is just ignored so scanning can continue).
    fn handle_qr(&mut self, url: &str, from_paste: bool, sender: &ComponentSender<Self>) {
        // Guard against double processing: already connecting (client worker
        // spawned) or running as the server. Not gated on the scanner, so a
        // pasted code works even when the camera is unavailable.
        if self.client_cmd.is_some() || self.stop.is_some() {
            return;
        }
        let info = match protocol::parse_pair_url(url, sync::now_unix()) {
            Ok(info) => info,
            Err(_) => {
                if from_paste {
                    if let Some(st) = &self.status {
                        st.set_text(&gettext("Invalid or expired pairing code."));
                    }
                }
                return; // other/invalid code – keep scanning
            }
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
                if let Some(field) = &self.code_field {
                    field.set_text(&pair_url);
                }
                if let Some(st) = &self.server_status {
                    st.set_text(&gettext_f(
                        "Waiting for another device … ({addr})",
                        &[("addr", &format!("{host}:{port}"))],
                    ));
                }
            }
            SyncEvent::PeerPaired {
                peer_name,
                peer_caps,
            } => {
                self.peer_caps = peer_caps;
                self.peer_name = peer_name;
                self.connected = true;
                // Paired → tint the sync icon at the top green.
                let _ = sender.output(SyncOutput::ConnectedChanged(true));
                // The single connection-success screen ("Connected with X" with
                // Share / Disconnect). The connection now lives in the background,
                // independent of this window.
                self.show_connected_panel(sender);
            }
            SyncEvent::ImportReceived { stats } => {
                // A metadata exchange happened. The final success screen is shown
                // on TransferDone (server side is told via /share/complete), so
                // here we only record the summary + reload the imported views.
                self.synced_ok = true;
                self.sync_summary = gettext_f(
                    "Received {fav} favorites, {pl} playlists, {pod} podcasts, {st} stations, {meta} covers/photos.",
                    &[
                        ("fav", &stats.favorites.to_string()),
                        ("pl", &stats.playlists.to_string()),
                        ("pod", &stats.podcasts.to_string()),
                        ("st", &stats.stations.to_string()),
                        ("meta", &stats.meta.to_string()),
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
                // A share finished (this side, or the peer told us via
                // /share/complete) → the transfer-success screen with a "Done"
                // button back to the connected panel. The pairing stays alive.
                self.synced_ok = true;
                let files_line = gettext_f("{n} files transferred.", &[("n", &files.to_string())]);
                self.sync_summary = if self.sync_summary.is_empty() {
                    files_line
                } else {
                    format!("{}\n{}", self.sync_summary, files_line)
                };
                // Server-as-receiver: the client's uploaded files only landed now,
                // so register the file-dependent content (recordings/memos) here —
                // at accept time the audio wasn't there yet, so it was skipped.
                if self.is_server {
                    if let Some(manifest) = &self.incoming_manifest {
                        let _ = share::apply_files(&self.library, manifest);
                        let _ = sender.output(SyncOutput::Imported);
                    }
                }
                // Reset the parked offer/decision so the next share starts fresh.
                if let Some(chan) = &self.share_chan {
                    if let Ok(mut c) = chan.lock() {
                        c.outgoing = None;
                        c.decision = None;
                    }
                }
                self.prepared_manifest = None;
                self.incoming_manifest = None;
                // The receiver registers each file as it lands; on the server side
                // that finishes only now (after the client uploaded), so refresh the
                // parent's library views once more here, when the files are in.
                let _ = sender.output(SyncOutput::Imported);
                self.show_success(sender);
            }
            SyncEvent::PeerDisconnected => {
                self.on_connection_lost(sender);
            }
            SyncEvent::ServerStopped => {
                self.stop = None;
                self.share_chan = None;
                self.on_connection_lost(sender);
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

    /// Swaps the window to the green transfer-success screen (summary + a "Done"
    /// button back to the connected panel — the pairing stays alive).
    fn show_success(&mut self, sender: &ComponentSender<Self>) {
        let content = padded_vbox();
        content.set_valign(gtk::Align::Center);

        let icon = gtk::Image::from_icon_name("emblem-ok-symbolic");
        icon.set_pixel_size(64);
        icon.add_css_class("success");
        let title = gtk::Label::builder()
            .label(gettext("Sync successful"))
            .css_classes(["title-2", "success"])
            .build();
        let detail = gtk::Label::builder()
            .label(&self.sync_summary)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .build();
        let done = gtk::Button::builder()
            .label(gettext("Done"))
            .css_classes(["suggested-action", "pill"])
            .halign(gtk::Align::Center)
            .margin_top(6)
            .build();
        {
            let sender = sender.clone();
            done.connect_clicked(move |_| sender.input(SyncInput::BackToConnected));
        }

        content.append(&icon);
        content.append(&title);
        content.append(&detail);
        content.append(&done);
        self.status = None;
        self.progress = None;
        self.set_sub_content(&content);
    }

    /// The pairing was lost (peer disconnected, timeout or server stopped): grey
    /// the header icon, drop the connection resources, and — if a window is open —
    /// show a short notice with a way to reconnect.
    fn on_connection_lost(&mut self, sender: &ComponentSender<Self>) {
        let was_connected = self.connected;
        self.connected = false;
        self.client_cmd = None;
        let _ = sender.output(SyncOutput::ConnectedChanged(false));
        // Only show the notice if the user is actually looking at the window and
        // we had a live pairing (avoid flashing it during a normal teardown).
        if was_connected && self.sub.is_some() {
            self.show_disconnected_notice(sender);
        }
    }

    /// A small "Disconnected" page with a "Connect again" button (→ mode select).
    fn show_disconnected_notice(&mut self, sender: &ComponentSender<Self>) {
        self.set_title(&gettext("Connect to share"));
        let content = padded_vbox();
        content.set_valign(gtk::Align::Center);
        let label = gtk::Label::builder()
            .label(gettext("Disconnected."))
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        let again = gtk::Button::builder()
            .label(gettext("Connect again"))
            .css_classes(["suggested-action", "pill"])
            .halign(gtk::Align::Center)
            .margin_top(6)
            .build();
        if let Some(window) = self.window.clone() {
            let sender = sender.clone();
            again.connect_clicked(move |_| sender.input(SyncInput::Open(window.clone())));
        }
        content.append(&label);
        content.append(&again);
        self.status = None;
        self.progress = None;
        self.set_sub_content(&content);
    }

    // --- Selective share (sender + receiver) -------------------------------

    /// Share a concrete selection (built from a detail view) with the connected
    /// peer: resolve it to a manifest off the UI thread, then show the size
    /// confirmation. No-op when not paired.
    fn share_selection(&mut self, sel: share::Selection, sender: &ComponentSender<Self>) {
        if !self.connected {
            return;
        }
        self.ensure_window(sender);
        self.set_title(&gettext("Connect to share"));
        // Brief feedback while hashing runs.
        let panel = self.progress_panel();
        self.set_sub_content(&panel);
        if let Some(st) = &self.status {
            st.set_text(&gettext("Preparing …"));
        }
        if self.is_server {
            let caps = self.peer_caps.clone();
            sender.spawn_oneshot_command(move || {
                let m = Library::open()
                    .ok()
                    .and_then(|lib| share::build_manifest(&lib, &sel, &caps).ok())
                    .unwrap_or_default();
                SyncEvent::ManifestReady { manifest: m }
            });
        } else if let Some(cmd) = &self.client_cmd {
            let _ = cmd.send(ClientCmd::Prepare(Box::new(sel)));
        }
    }

    /// Manifest built → show the size confirmation.
    fn on_manifest_ready(&mut self, manifest: ShareManifest, sender: &ComponentSender<Self>) {
        let page = build_confirm(&manifest, sender);
        self.prepared_manifest = Some(manifest);
        self.set_sub_content(&page);
    }

    /// Confirmation "Send" → park (server) or send (client) the prepared offer.
    fn confirm_send(&mut self) {
        let panel = self.progress_panel();
        self.set_sub_content(&panel);
        if self.is_server {
            if let (Some(chan), Some(m)) = (&self.share_chan, self.prepared_manifest.clone()) {
                if let Ok(mut c) = chan.lock() {
                    // Fresh share: park the new offer, drop any stale decision from
                    // a previous one (timing-independent).
                    c.outgoing = Some(m);
                    c.decision = None;
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

    /// Peer offered a share → classify it and show the review screen. The peer
    /// may share while our window is closed (background connection), so make sure
    /// a window is present first.
    fn on_share_offered(&mut self, manifest: ShareManifest, sender: &ComponentSender<Self>) {
        self.ensure_window(sender);
        self.set_title(&gettext("Connect to share"));
        let reviews = share::review_files(&self.library, &manifest);
        let yt = self.youtube_enabled();
        let (page, handles) = build_review(&manifest, &reviews, yt, sender);
        self.review = Some(handles);
        self.incoming_manifest = Some(manifest);
        self.set_sub_content(&page);
    }

    /// Review "Accept" → apply the (selectively) accepted offer.
    fn accept_offer(&mut self, sender: &ComponentSender<Self>) {
        let Some(review) = self.review.take() else {
            return;
        };
        let decision = review.to_decision();
        let panel = self.progress_panel();
        self.set_sub_content(&panel);
        if let Some(st) = &self.status {
            st.set_text(&gettext("Receiving …"));
        }
        if self.is_server {
            // Park the decision (client uploads files) + apply blobs/YT and
            // register accepted files locally off the UI thread. Drop any stale
            // parked offer so the next cycle starts clean.
            if let Some(chan) = &self.share_chan {
                if let Ok(mut c) = chan.lock() {
                    c.decision = Some(decision.clone());
                    c.outgoing = None;
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
        self.show_connected_panel(sender);
    }

    /// "Disconnect" tapped → end the live pairing and close the window. The
    /// background server/worker tears itself down and reports back via
    /// `PeerDisconnected`/`ServerStopped`.
    fn disconnect_peer(&mut self, sender: &ComponentSender<Self>) {
        if self.is_server {
            if let Some(stop) = &self.stop {
                stop.store(true, Ordering::Relaxed);
            }
        } else if let Some(cmd) = &self.client_cmd {
            let _ = cmd.send(ClientCmd::Disconnect);
        }
        self.connected = false;
        let _ = sender.output(SyncOutput::ConnectedChanged(false));
        if let Some(sub) = self.sub.take() {
            sub.close();
        }
    }

    /// The flow window was closed. If a pairing is still live it is **kept** (the
    /// background server/worker keeps running, the icon stays green); only the
    /// per-window widgets are dropped. Otherwise (closed mid-attempt or after a
    /// disconnect) the server/scanner are stopped.
    fn close_sub(&mut self, sender: &ComponentSender<Self>) {
        self.sub = None;
        if self.connected {
            self.clear_window_widgets();
        } else {
            self.stop_connection();
            self.clear_window_widgets();
            let _ = sender.output(SyncOutput::ConnectedChanged(false));
        }
    }

    /// Stops the background connection resources (server thread + camera + client
    /// worker). Leaves the per-window widgets to [`Self::clear_window_widgets`].
    fn stop_connection(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.scanner = None; // drop stops the camera
        self.share_chan = None;
        self.client_cmd = None; // dropping the sender ends the client worker
        self.connected = false;
        self.is_server = false;
    }

    /// Drops the per-window widget handles (after the flow window is gone). Does
    /// **not** touch the live connection (stop flag / share channel / worker).
    fn clear_window_widgets(&mut self) {
        self.qr = None;
        self.cam = None;
        self.status = None;
        self.server_status = None;
        self.progress = None;
        self.synced_ok = false;
        self.sync_summary.clear();
        self.sub_toolbar = None;
        self.review = None;
        self.prepared_manifest = None;
        self.incoming_manifest = None;
        // `is_server` / `share_chan` / `client_cmd` / `connected` are connection
        // state and are kept here (cleared by `stop_connection` on disconnect).
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

/// Persistent client-session worker: pairs, then **stays connected**, looping —
/// polling for an incoming offer and serving commands from the UI (prepare/send
/// an offer, accept/reject an incoming one) — with a periodic ping that keeps the
/// server session warm. Each share runs to completion and the loop returns to
/// idle (the pairing survives), until the UI asks to `Disconnect` (or drops the
/// command channel) or the link to the server breaks.
fn run_client_session(
    info: PairingInfo,
    device_id: String,
    device_name: String,
    yt_enabled: bool,
    cmd_rx: mpsc::Receiver<ClientCmd>,
    out: &relm4::Sender<SyncEvent>,
) {
    let caps = Capabilities {
        schema: protocol::SCHEMA_VERSION,
        youtube_enabled: yt_enabled,
    };
    let mut client = SyncClient::new(&info, device_id, device_name, caps);
    if let Err(e) = client.pair(&info.token) {
        let _ = out.send(SyncEvent::Error(e.to_string()));
        let _ = out.send(SyncEvent::PeerDisconnected);
        return;
    }
    let _ = out.send(SyncEvent::PeerPaired {
        peer_name: client.peer_name.clone(),
        peer_caps: client.peer_caps.clone(),
    });

    let mut prepared: Option<ShareManifest> = None;
    let mut offered = false;
    let mut last_ping = Instant::now();
    'session: loop {
        if last_ping.elapsed() > Duration::from_secs(10) {
            // A failed ping means the server is gone → end the session.
            if client.ping().is_err() {
                break 'session;
            }
            last_ping = Instant::now();
        }
        // Poll for an offer from the server-side user (only while idle).
        if !offered && prepared.is_none() {
            match client.fetch_offer() {
                Ok(Some(m)) => {
                    offered = true;
                    let _ = out.send(SyncEvent::ShareOffered { manifest: m });
                }
                Ok(None) => {}
                Err(_) => break 'session, // connection lost
            }
        }
        match cmd_rx.recv_timeout(Duration::from_millis(300)) {
            Ok(ClientCmd::Prepare(sel)) => {
                let m = Library::open()
                    .ok()
                    .and_then(|lib| share::build_manifest(&lib, &sel, &client.peer_caps).ok())
                    .unwrap_or_default();
                prepared = Some(m.clone());
                let _ = out.send(SyncEvent::ManifestReady { manifest: m });
            }
            Ok(ClientCmd::Send) => {
                let Some(m) = prepared.take() else { continue };
                if let Err(e) = client.send_offer(&m) {
                    let _ = out.send(SyncEvent::Error(e.to_string()));
                    break 'session;
                }
                // Wait for the peer's decision (keep pinging). Bail to idle on a
                // Disconnect command; end the session if the link drops.
                let decision = loop {
                    if last_ping.elapsed() > Duration::from_secs(10) {
                        if client.ping().is_err() {
                            break 'session;
                        }
                        last_ping = Instant::now();
                    }
                    match client.fetch_decision() {
                        Ok(Some(d)) => break d,
                        Ok(None) => {}
                        Err(_) => break ShareDecision::default(),
                    }
                    match cmd_rx.try_recv() {
                        Ok(ClientCmd::Disconnect) | Err(mpsc::TryRecvError::Disconnected) => {
                            break 'session
                        }
                        _ => {}
                    }
                    std::thread::sleep(Duration::from_millis(400));
                };
                if decision.accept {
                    let files = client_upload(&client, &m, &decision, out);
                    let _ = client.notify_complete(files);
                    let _ = out.send(SyncEvent::TransferDone { files });
                }
                offered = false;
                // Stay connected: back to idle for the next share.
            }
            Ok(ClientCmd::Decide(decision)) => {
                // Re-fetch the offer to know which files to pull (the server keeps
                // it parked); then send the decision and transfer + apply.
                if let Ok(Some(m)) = client.fetch_offer() {
                    let _ = client.send_decision(&decision);
                    client_receive(&client, &m, &decision, out);
                }
                offered = false;
                prepared = None;
                // Stay connected.
            }
            Ok(ClientCmd::Reject) => {
                let _ = client.send_decision(&ShareDecision::default());
                offered = false;
                // Stay connected.
            }
            Ok(ClientCmd::Disconnect) | Err(mpsc::RecvTimeoutError::Disconnected) => break 'session,
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
    let base = Library::open()
        .ok()
        .and_then(|l| l.get_setting("music_dir").ok().flatten())
        .unwrap_or_default();
    let total = manifest
        .files
        .iter()
        .filter(|f| decision.files.contains(&f.rel_path))
        .count() as u64;
    let mut n = 0usize;
    for (i, f) in manifest
        .files
        .iter()
        .filter(|f| decision.files.contains(&f.rel_path))
        .enumerate()
    {
        let _ = out.send(SyncEvent::FileProgress {
            done: i as u64 + 1,
            total,
            name: transfer_label(f),
        });
        // A memo lives outside the music folder; locate it in the memo store.
        let abs = match f.rel_path.strip_prefix(crate::core::sync::MEMO_PREFIX) {
            Some(name) => crate::core::mic::memos_dir()
                .join(name)
                .to_string_lossy()
                .into_owned(),
            None => data::resolve(&f.rel_path, &base),
        };
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
    let base = lib
        .get_setting("music_dir")
        .ok()
        .flatten()
        .unwrap_or_default();
    let total = manifest
        .files
        .iter()
        .filter(|f| decision.files.contains(&f.rel_path))
        .count() as u64;
    for (i, f) in manifest
        .files
        .iter()
        .filter(|f| decision.files.contains(&f.rel_path))
        .enumerate()
    {
        let _ = out.send(SyncEvent::FileProgress {
            done: i as u64 + 1,
            total,
            name: transfer_label(f),
        });
        if let Some(dest) = crate::core::sync::resolve_new(&base, &f.rel_path) {
            if client.download_file(&f.rel_path, &dest).is_ok()
                && !f.rel_path.starts_with(crate::core::sync::MEMO_PREFIX)
            {
                // Re-read the file we just received so it is indexed and sorted in
                // from its own tags (not the sender's second-hand metadata). Memos
                // are not music — they are registered by `apply_received` instead.
                crate::core::scanner::ingest_file(&lib, &dest);
            }
        }
    }
    let stats = apply_received(&lib, manifest, decision);
    let _ = out.send(SyncEvent::ImportReceived { stats });
    // Tell the (passive) server-as-sender we finished, so its UI also shows the
    // transfer-success screen.
    let _ = client.notify_complete(total as usize);
    let _ = out.send(SyncEvent::TransferDone {
        files: total as usize,
    });
}

/// Applies library/YT blobs (favorites/playlists/podcasts/categories/EQ + YT).
/// The accepted audio files are **not** registered here: each is read in and
/// indexed from its own tags the moment it lands ([`crate::core::scanner::ingest_file`],
/// called by the download loop on the client and the `/files/put` handler on the
/// server), so a half-finished transfer never leaves rows for files that never
/// arrived. `stats.files` is the intended count, for the summary only.
fn apply_received(
    lib: &Library,
    manifest: &ShareManifest,
    decision: &ShareDecision,
) -> sync::ImportStats {
    let mut stats = share::apply_manifest(lib, manifest, decision).unwrap_or_default();
    stats.files = manifest
        .files
        .iter()
        .filter(|f| decision.files.contains(&f.rel_path))
        .count();
    stats
}

/// A centered, path-free label for the transfer progress: the artist, album and
/// title of the file currently moving, one per line, skipping whatever is empty.
/// Falls back to the bare file name (never the full path) when no tags are known.
fn transfer_label(f: &share::ManifestFile) -> String {
    let mut lines: Vec<&str> = Vec::new();
    if let Some(a) = f.artist.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        lines.push(a);
    }
    if let Some(al) = f.album.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        lines.push(al);
    }
    let title = f.title.trim();
    if !title.is_empty() {
        lines.push(title);
    }
    if lines.is_empty() {
        // Last resort: the file name only, not the path.
        return f
            .rel_path
            .rsplit('/')
            .next()
            .unwrap_or(&f.rel_path)
            .to_string();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use crate::core::sync::resolve_new;

    #[test]
    fn resolve_new_accepts_only_relative_normal_paths() {
        let base = std::env::temp_dir().join(format!("emilia-sync-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let base_s = base.to_string_lossy();

        let ok = resolve_new(&base_s, "Album/track.mp3").unwrap();
        assert!(ok.starts_with(&base));
        assert!(ok.ends_with("Album/track.mp3"));

        assert!(resolve_new(&base_s, "/etc/passwd").is_none());
        assert!(resolve_new(&base_s, "../escape.mp3").is_none());
        assert!(resolve_new(&base_s, "Album/../escape.mp3").is_none());

        let _ = std::fs::remove_dir_all(base);
    }
}
