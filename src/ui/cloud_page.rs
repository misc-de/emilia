//! Setup dialog for a Nextcloud/WebDAV source as a standalone relm4 component:
//! credentials via login QR code (the Nextcloud app's default) **or** manually,
//! plus a connection test. Extracted from the `App` god-object.
//!
//! WebDAV logic lives in [`crate::core::webdav`]. The component owns the dialog
//! + camera; it emits `SourcesChanged` (a new source was saved → parent reloads
//! tabs/views) and `Indexed` (the cloud library finished indexing → parent
//! rebuilds albums/artists).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::source;
use crate::core::sync::scanner::Scanner;
use crate::core::webdav::{self, Creds};
use crate::i18n::{gettext, gettext_f};
use crate::model::Source;

/// The Nextcloud-connect component (owns the dialog + camera scanner).
#[derive(Default)]
pub(crate) struct CloudPage {
    dialog: Option<adw::Dialog>,
    /// Running webcam scanner pipeline (dropping it stops the camera).
    scanner: Option<Scanner>,
    cam: Option<gtk::Picture>,
    url_row: Option<adw::EntryRow>,
    user_row: Option<adw::EntryRow>,
    pass_row: Option<adw::PasswordEntryRow>,
    path_row: Option<adw::EntryRow>,
    status: Option<gtk::Label>,
    /// Already-connected Nextcloud servers offered for reuse (a second music
    /// folder on the same server, without scanning/typing the login again).
    existing: Vec<Source>,
    /// The server picked for reuse (`None` = set up a brand-new connection).
    chosen_source: Option<Source>,
    /// New-connection area (mode chooser + QR + manual). Hidden while a saved
    /// server is reused, and (with saved servers) until "New connection" is
    /// picked from the list.
    new_section: Option<gtk::Box>,
    /// Camera + hint – the QR sub-mode of a new connection.
    qr_box: Option<gtk::Box>,
    /// Manual-entry group – the manual sub-mode of a new connection. QR and
    /// manual are mutually exclusive (never both visible at once).
    manual_group: Option<adw::PreferencesGroup>,
    /// QR/manual chooser. `manual_btn` drives the sub-mode (its `toggled`
    /// emits `SetManual`); `qr_btn` is kept to reselect QR programmatically.
    qr_btn: Option<gtk::ToggleButton>,
    manual_btn: Option<gtk::ToggleButton>,
    /// Music folder + status + buttons. Revealed once a choice has been made.
    details: Option<gtk::Box>,
}

#[derive(Debug)]
pub(crate) enum CloudInput {
    /// Open the dialog on `window`; `mobile` → present as a bottom sheet.
    /// `existing` are already-connected Nextcloud servers offered for reuse.
    Open {
        window: adw::ApplicationWindow,
        mobile: bool,
        existing: Vec<Source>,
    },
    /// Reuse the saved server `existing[idx]` – only the music folder is asked.
    ReuseServer(usize),
    /// Set up a brand-new connection (then choose QR scan or manual entry).
    NewConnection,
    /// New-connection sub-mode: `true` = manual entry, `false` = scan QR.
    SetManual(bool),
    QrDecoded(String),
    Test,
    Save,
    Closed,
}

#[derive(Debug)]
pub(crate) enum CloudOutput {
    /// A source (id) was saved → parent reloads sources/tabs, switches to the
    /// new tab and reloads views.
    SourcesChanged(i64),
    /// The cloud library finished indexing → parent rebuilds albums/artists.
    Indexed,
}

#[derive(Debug)]
pub(crate) enum CloudCmd {
    Tested(Result<(), String>),
    Indexed,
}

#[relm4::component(pub(crate))]
impl Component for CloudPage {
    type Init = ();
    type Input = CloudInput;
    type Output = CloudOutput;
    type CommandOutput = CloudCmd;

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
        let model = CloudPage::default();
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: CloudInput, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            CloudInput::Open {
                window,
                mobile,
                existing,
            } => self.open_dialog(&window, mobile, existing, &sender),
            CloudInput::ReuseServer(idx) => self.reuse_server(idx),
            CloudInput::NewConnection => self.new_connection(&sender),
            CloudInput::SetManual(manual) => self.set_manual(manual, &sender),
            CloudInput::QrDecoded(code) => self.handle_qr(&code),
            CloudInput::Test => self.test(&sender),
            CloudInput::Save => self.save(&sender),
            CloudInput::Closed => {
                self.scanner = None;
                self.dialog = None;
            }
        }
    }

    fn update_cmd(&mut self, cmd: CloudCmd, sender: ComponentSender<Self>, _root: &Self::Root) {
        match cmd {
            CloudCmd::Tested(Ok(())) => self.status(&gettext("Connection works")),
            CloudCmd::Tested(Err(_)) => {
                self.status(&gettext("Connection failed – check the details"))
            }
            CloudCmd::Indexed => {
                let _ = sender.output(CloudOutput::Indexed);
            }
        }
    }
}

impl CloudPage {
    /// Opens the "Add Nextcloud" dialog. With saved servers it first shows the
    /// list (reuse one, or "New connection" at the bottom); only after picking
    /// "New connection" does it offer the QR scan or manual entry – never both
    /// at once. Without any saved server it starts straight in new-connection
    /// (QR) mode.
    fn open_dialog(
        &mut self,
        window: &adw::ApplicationWindow,
        mobile: bool,
        existing: Vec<Source>,
        sender: &ComponentSender<Self>,
    ) {
        self.existing = existing;
        self.chosen_source = None;
        let dialog = adw::Dialog::builder()
            .title(gettext("Connect to Nextcloud"))
            .content_width(420)
            .build();
        if mobile {
            dialog.set_presentation_mode(adw::DialogPresentationMode::BottomSheet);
        }

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // 1) Already-connected servers: list each (reuse → just a new music
        //    folder), with "New connection" as the last entry.
        if !self.existing.is_empty() {
            let conn_group = adw::PreferencesGroup::builder()
                .title(gettext("Connection"))
                .build();
            let list = gtk::ListBox::builder()
                .selection_mode(gtk::SelectionMode::None)
                .css_classes(["boxed-list"])
                .build();
            for (i, s) in self.existing.iter().enumerate() {
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(&s.name))
                    .subtitle(gtk::glib::markup_escape_text(
                        s.base_url.as_deref().unwrap_or(""),
                    ))
                    .activatable(true)
                    .build();
                row.add_prefix(&gtk::Image::from_icon_name("network-server-symbolic"));
                row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                let sender = sender.clone();
                row.connect_activated(move |_| sender.input(CloudInput::ReuseServer(i)));
                list.append(&row);
            }
            let new_row = adw::ActionRow::builder()
                .title(gettext("New connection"))
                .activatable(true)
                .build();
            new_row.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
            new_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            {
                let sender = sender.clone();
                new_row.connect_activated(move |_| sender.input(CloudInput::NewConnection));
            }
            list.append(&new_row);
            conn_group.add(&list);
            content.append(&conn_group);
        }

        // 2) New connection: a QR/manual chooser (mutually exclusive) with the
        //    camera or the form below it. Hidden until "New connection" is
        //    picked (or shown right away when there is no saved server).
        let new_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .visible(false)
            .build();

        let mode = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .halign(gtk::Align::Center)
            .css_classes(["linked"])
            .build();
        let qr_btn = gtk::ToggleButton::builder()
            .label(gettext("Scan QR code"))
            .active(true)
            .build();
        let manual_btn = gtk::ToggleButton::builder()
            .label(gettext("Enter manually"))
            .build();
        manual_btn.set_group(Some(&qr_btn));
        {
            let sender = sender.clone();
            manual_btn.connect_toggled(move |b| {
                sender.input(CloudInput::SetManual(b.is_active()));
            });
        }
        mode.append(&qr_btn);
        mode.append(&manual_btn);
        new_section.append(&mode);

        // QR sub-mode: camera preview + hint.
        let qr_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        let cam = gtk::Picture::builder()
            .height_request(180)
            .visible(false)
            .build();
        cam.add_css_class("card");
        qr_box.append(&cam);
        let hint = gtk::Label::builder()
            .label(gettext("Point the camera at the Nextcloud login QR code"))
            .wrap(true)
            .xalign(0.5)
            .css_classes(["dim-label"])
            .build();
        qr_box.append(&hint);
        new_section.append(&qr_box);

        // Manual sub-mode: the credential fields (hidden while in QR mode).
        let manual_group = adw::PreferencesGroup::builder().visible(false).build();
        let url_row = adw::EntryRow::builder()
            .title(gettext("Server URL"))
            .build();
        let user_row = adw::EntryRow::builder().title(gettext("User name")).build();
        let pass_row = adw::PasswordEntryRow::builder()
            .title(gettext("App password"))
            .build();
        crate::ui::widgets::no_autofocus(&url_row);
        crate::ui::widgets::no_autofocus(&user_row);
        crate::ui::widgets::no_autofocus(&pass_row);
        manual_group.add(&url_row);
        manual_group.add(&user_row);
        manual_group.add(&pass_row);
        new_section.append(&manual_group);
        content.append(&new_section);

        // 3) Details: the music folder + status + buttons, revealed once a
        //    connection (reuse or new) has been chosen.
        let details = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .visible(false)
            .build();
        let path_group = adw::PreferencesGroup::builder()
            .title(gettext("Music folder to index"))
            .build();
        let path_row = adw::EntryRow::builder()
            .title(gettext("Folder (e.g. /Music)"))
            .build();
        crate::ui::widgets::no_autofocus(&path_row);
        path_group.add(&path_row);
        details.append(&path_group);

        let status = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();
        details.append(&status);

        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk::Align::End)
            .build();
        let test_btn = gtk::Button::builder()
            .label(gettext("Test connection"))
            .build();
        {
            let sender = sender.clone();
            test_btn.connect_clicked(move |_| sender.input(CloudInput::Test));
        }
        let save_btn = gtk::Button::builder()
            .label(gettext("Save"))
            .css_classes(["suggested-action"])
            .build();
        {
            let sender = sender.clone();
            save_btn.connect_clicked(move |_| sender.input(CloudInput::Save));
        }
        buttons.append(&test_btn);
        buttons.append(&save_btn);
        details.append(&buttons);
        content.append(&details);

        toolbar.set_content(Some(&content));
        dialog.set_child(Some(&toolbar));
        {
            let sender = sender.clone();
            dialog.connect_closed(move |_| sender.input(CloudInput::Closed));
        }
        crate::ui::app_helpers::close_on_click_outside(&dialog);
        dialog.present(Some(window));

        self.cam = Some(cam);
        self.url_row = Some(url_row);
        self.user_row = Some(user_row);
        self.pass_row = Some(pass_row);
        self.path_row = Some(path_row);
        self.status = Some(status);
        self.new_section = Some(new_section);
        self.qr_box = Some(qr_box);
        self.manual_group = Some(manual_group);
        self.qr_btn = Some(qr_btn);
        self.manual_btn = Some(manual_btn);
        self.details = Some(details);
        self.dialog = Some(dialog);

        // Without a saved server there is nothing to list → go straight into a
        // new connection (QR mode, camera starts).
        if self.existing.is_empty() {
            self.new_connection(sender);
        }
    }

    /// Reuse a saved server: hide the new-connection area, reveal the details
    /// and ask only for the music folder (login comes from the Secret Service).
    fn reuse_server(&mut self, idx: usize) {
        self.chosen_source = self.existing.get(idx).cloned();
        self.scanner = None;
        if let Some(s) = &self.new_section {
            s.set_visible(false);
        }
        if let Some(d) = &self.details {
            d.set_visible(true);
        }
        let host = self
            .chosen_source
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_default();
        self.status(&gettext_f(
            "Using the saved login of {host} – just set the music folder",
            &[("host", &host)],
        ));
    }

    /// Set up a new connection: reveal the QR/manual chooser + details and
    /// default to QR mode (camera).
    fn new_connection(&mut self, sender: &ComponentSender<Self>) {
        self.chosen_source = None;
        if let Some(s) = &self.new_section {
            s.set_visible(true);
        }
        if let Some(d) = &self.details {
            d.set_visible(true);
        }
        // Reset the chooser to QR (radio: activating QR deactivates manual);
        // then (re)start the camera regardless of whether that flipped state.
        if let Some(b) = &self.qr_btn {
            b.set_active(true);
        }
        self.set_manual(false, sender);
    }

    /// Switches the new-connection sub-mode. QR and manual are mutually
    /// exclusive – only one is ever visible.
    fn set_manual(&mut self, manual: bool, sender: &ComponentSender<Self>) {
        if manual {
            self.scanner = None;
            if let Some(c) = &self.qr_box {
                c.set_visible(false);
            }
            if let Some(m) = &self.manual_group {
                m.set_visible(true);
            }
            self.status("");
        } else {
            if let Some(m) = &self.manual_group {
                m.set_visible(false);
            }
            if let Some(c) = &self.qr_box {
                c.set_visible(true);
            }
            self.start_scan(sender);
        }
    }

    /// Starts the webcam scanner for the login QR code.
    fn start_scan(&mut self, sender: &ComponentSender<Self>) {
        if self.scanner.is_some() {
            return;
        }
        let sender_dec = sender.clone();
        match Scanner::start(move |code| sender_dec.input(CloudInput::QrDecoded(code))) {
            Ok((scanner, paintable)) => {
                if let (Some(cam), Some(p)) = (&self.cam, &paintable) {
                    cam.set_paintable(Some(p));
                    cam.set_visible(true);
                }
                self.scanner = Some(scanner);
                self.status(&gettext("Point the camera at the login QR code"));
            }
            Err(e) => {
                // No camera available → switch straight to manual entry. The
                // toggle is the single source of truth, so flipping it drives
                // the visibility switch via `SetManual`.
                tracing::info!("Nextcloud camera unavailable: {e}");
                if let Some(b) = &self.manual_btn {
                    b.set_active(true);
                }
            }
        }
    }

    /// A QR code was decoded: interpret it as a Nextcloud login code and show
    /// the filled fields (manual mode) so the user can set the path and save.
    fn handle_qr(&mut self, code: &str) {
        let Some((server, user, pass)) = webdav::parse_nc_login(code) else {
            return; // other/invalid code – keep scanning
        };
        self.scanner = None; // stop the camera
        if let Some(b) = &self.manual_btn {
            b.set_active(true);
        }
        if let Some(c) = &self.qr_box {
            c.set_visible(false);
        }
        if let Some(m) = &self.manual_group {
            m.set_visible(true);
        }
        if let Some(r) = &self.url_row {
            r.set_text(&server);
        }
        if let Some(r) = &self.user_row {
            r.set_text(&user);
        }
        if let Some(r) = &self.pass_row {
            r.set_text(&pass);
        }
        self.status(&gettext("Login data scanned – set the music path and save"));
    }

    /// Reads the form fields into credentials (all required except the path).
    /// When a saved server is reused, URL/login come from the Secret Service and
    /// only the music folder is taken from the form.
    fn creds(&self) -> Option<Creds> {
        let path = self.path_row.as_ref()?.text().trim().to_string();
        if let Some(src) = &self.chosen_source {
            let mut c = Creds::from_source(src)?;
            c.music_path = normalize_music_path(&path);
            return Some(c);
        }
        let url = self.url_row.as_ref()?.text().trim().to_string();
        let user = self.user_row.as_ref()?.text().trim().to_string();
        let pass = self.pass_row.as_ref()?.text().to_string();
        if url.is_empty() || user.is_empty() || pass.is_empty() {
            return None;
        }
        Some(Creds {
            base_url: url.trim_end_matches('/').to_string(),
            user,
            pass,
            music_path: normalize_music_path(&path),
        })
    }

    /// Connection test in the background (PROPFIND on the music root).
    fn test(&mut self, sender: &ComponentSender<Self>) {
        let Some(creds) = self.creds() else {
            self.status(&gettext("Please fill in URL, user and app password"));
            return;
        };
        self.status(&gettext("Testing …"));
        sender.spawn_oneshot_command(move || {
            CloudCmd::Tested(webdav::test_connection(&creds).map_err(|e| e.to_string()))
        });
    }

    /// Saves the source and closes the dialog, then indexes in the background.
    fn save(&mut self, sender: &ComponentSender<Self>) {
        let Some(creds) = self.creds() else {
            self.status(&gettext("Please fill in URL, user and app password"));
            return;
        };
        // Reusing a server: name the new tab after its music folder so it is
        // distinguishable from the first tab (which carries the host name).
        let name = self.chosen_source.as_ref().map(|_| {
            creds
                .music_path
                .rsplit('/')
                .find(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| gettext("Nextcloud"))
        });
        match Library::open().and_then(|lib| source::add_webdav_source_named(&lib, creds, name)) {
            Ok(src) => {
                let _ = sender.output(CloudOutput::SourcesChanged(src.id));
                self.scanner = None;
                if let Some(d) = self.dialog.take() {
                    d.close();
                }
                // Index the cloud library in the background so the tracks feel
                // like local ones (artists/albums + covers/photos).
                sender.spawn_command(move |out| {
                    if let Ok(lib) = Library::open() {
                        match crate::core::webdav::index_into(&lib, &src) {
                            Ok(n) => tracing::info!("Indexed {n} Nextcloud tracks"),
                            Err(e) => tracing::warn!("Nextcloud indexing failed: {e}"),
                        }
                    }
                    let _ = out.send(CloudCmd::Indexed);
                });
            }
            Err(e) => {
                tracing::error!("add webdav source failed: {e}");
                self.status(&gettext("Could not save this source"));
            }
        }
    }

    fn status(&self, msg: &str) {
        if let Some(s) = &self.status {
            s.set_text(msg);
        }
    }
}

/// Normalizes the music subpath (leading slash, no trailing slash;
/// empty = cloud root).
fn normalize_music_path(p: &str) -> String {
    let p = p.trim().trim_end_matches('/');
    if p.is_empty() {
        String::new()
    } else if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
}
