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
use crate::i18n::gettext;

/// The Nextcloud-connect component (owns the dialog + camera scanner).
#[derive(Default)]
pub(crate) struct CloudPage {
    dialog: Option<adw::Dialog>,
    /// Running webcam scanner pipeline (dropping it stops the camera).
    scanner: Option<Scanner>,
    cam: Option<gtk::Picture>,
    /// Expandable area for manual entry (folds the camera away).
    manual: Option<adw::ExpanderRow>,
    url_row: Option<adw::EntryRow>,
    user_row: Option<adw::EntryRow>,
    pass_row: Option<adw::PasswordEntryRow>,
    path_row: Option<adw::EntryRow>,
    status: Option<gtk::Label>,
}

#[derive(Debug)]
pub(crate) enum CloudInput {
    /// Open the dialog on `window`; `mobile` → present as a bottom sheet.
    Open {
        window: adw::ApplicationWindow,
        mobile: bool,
    },
    ManualToggle(bool),
    QrDecoded(String),
    Test,
    Save,
    Closed,
}

#[derive(Debug)]
pub(crate) enum CloudOutput {
    /// A source was saved → parent reloads sources/tabs and views.
    SourcesChanged,
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
            CloudInput::Open { window, mobile } => self.open_dialog(&window, mobile, &sender),
            CloudInput::ManualToggle(expanded) => {
                if expanded {
                    // Manual expanded → pause the camera and hide it.
                    self.scanner = None;
                    if let Some(cam) = &self.cam {
                        cam.set_visible(false);
                    }
                } else {
                    self.start_scan(&sender);
                }
            }
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
    /// Opens the "Add Nextcloud" dialog.
    fn open_dialog(
        &mut self,
        window: &adw::ApplicationWindow,
        mobile: bool,
        sender: &ComponentSender<Self>,
    ) {
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

        // Camera preview: reads the login QR code right away (default).
        let cam = gtk::Picture::builder()
            .height_request(180)
            .visible(false)
            .build();
        cam.add_css_class("card");
        content.append(&cam);
        let hint = gtk::Label::builder()
            .label(gettext("Point the camera at the Nextcloud login QR code"))
            .wrap(true)
            .xalign(0.5)
            .css_classes(["dim-label"])
            .build();
        content.append(&hint);

        // Manual entry as an expandable area. Expanding hides the camera.
        let manual_group = adw::PreferencesGroup::new();
        let manual = adw::ExpanderRow::builder()
            .title(gettext("Enter the details manually"))
            .build();
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
        manual.add_row(&url_row);
        manual.add_row(&user_row);
        manual.add_row(&pass_row);
        manual_group.add(&manual);
        content.append(&manual_group);
        {
            let sender = sender.clone();
            manual.connect_expanded_notify(move |e| {
                sender.input(CloudInput::ManualToggle(e.is_expanded()));
            });
        }

        // Music folder to index – always visible (given when connecting).
        let path_group = adw::PreferencesGroup::builder()
            .title(gettext("Music folder to index"))
            .build();
        let path_row = adw::EntryRow::builder()
            .title(gettext("Folder (e.g. /Music)"))
            .build();
        crate::ui::widgets::no_autofocus(&path_row);
        path_group.add(&path_row);
        content.append(&path_group);

        let status = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();
        content.append(&status);

        // Action buttons.
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
        content.append(&buttons);

        toolbar.set_content(Some(&content));
        dialog.set_child(Some(&toolbar));
        {
            let sender = sender.clone();
            dialog.connect_closed(move |_| sender.input(CloudInput::Closed));
        }
        dialog.present(Some(window));

        self.cam = Some(cam);
        self.manual = Some(manual);
        self.url_row = Some(url_row);
        self.user_row = Some(user_row);
        self.pass_row = Some(pass_row);
        self.path_row = Some(path_row);
        self.status = Some(status);
        self.dialog = Some(dialog);

        // Start the camera right away – it reads the login QR immediately.
        self.start_scan(sender);
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
                // No camera available → switch straight to manual entry.
                tracing::info!("Nextcloud camera unavailable: {e}");
                if let Some(m) = &self.manual {
                    m.set_expanded(true);
                }
            }
        }
    }

    /// A QR code was decoded: interpret it as a Nextcloud login code.
    fn handle_qr(&mut self, code: &str) {
        let Some((server, user, pass)) = webdav::parse_nc_login(code) else {
            return; // other/invalid code – keep scanning
        };
        self.scanner = None; // stop the camera
        if let Some(cam) = &self.cam {
            cam.set_visible(false);
        }
        if let Some(m) = &self.manual {
            m.set_expanded(true);
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
    fn creds(&self) -> Option<Creds> {
        let url = self.url_row.as_ref()?.text().trim().to_string();
        let user = self.user_row.as_ref()?.text().trim().to_string();
        let pass = self.pass_row.as_ref()?.text().to_string();
        let path = self.path_row.as_ref()?.text().trim().to_string();
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
        match Library::open().and_then(|lib| source::add_webdav_source(&lib, creds)) {
            Ok(src) => {
                let _ = sender.output(CloudOutput::SourcesChanged);
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
