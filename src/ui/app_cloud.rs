//! Setup dialog for a Nextcloud/WebDAV source: credentials via
//! login QR code (the Nextcloud app's default) **or** manually, plus a
//! connection test. Pure UI/event-flow wiring - the WebDAV logic lives in
//! [`crate::core::webdav`].

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::sync::scanner::Scanner;
use crate::core::webdav::{self, Creds};
use crate::i18n::gettext;
use crate::model::Source;
use crate::ui::app::{App, Cmd, Msg};

/// Widget state of the Nextcloud dialog (handles, so that a result scanned via
/// QR can fill the form fields).
#[derive(Default)]
pub(crate) struct CloudState {
    pub dialog: Option<adw::Dialog>,
    /// Running webcam scanner pipeline (dropping it stops the camera).
    pub scanner: Option<Scanner>,
    pub cam: Option<gtk::Picture>,
    /// Expandable area for manual entry (folds the camera away).
    pub manual: Option<adw::ExpanderRow>,
    pub url_row: Option<adw::EntryRow>,
    pub user_row: Option<adw::EntryRow>,
    pub pass_row: Option<adw::PasswordEntryRow>,
    pub path_row: Option<adw::EntryRow>,
    pub status: Option<gtk::Label>,
}

impl App {
    /// Opens the "Add Nextcloud" dialog.
    pub(crate) fn open_cloud_dialog(
        &mut self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::Dialog::builder()
            .title(gettext("Connect to Nextcloud"))
            .content_width(420)
            .build();
        self.adapt_detail_dialog(&dialog);

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
            .label(&gettext("Point the camera at the Nextcloud login QR code"))
            .wrap(true)
            .xalign(0.5)
            .css_classes(["dim-label"])
            .build();
        content.append(&hint);

        // Manual entry as an expandable area. Expanding hides the
        // camera (manual entry only), collapsing brings it back.
        let manual_group = adw::PreferencesGroup::new();
        let manual = adw::ExpanderRow::builder()
            .title(&gettext("Enter the details manually"))
            .build();
        let url_row = adw::EntryRow::builder()
            .title(gettext("Server URL"))
            .build();
        let user_row = adw::EntryRow::builder().title(gettext("User name")).build();
        let pass_row = adw::PasswordEntryRow::builder()
            .title(gettext("App password"))
            .build();
        manual.add_row(&url_row);
        manual.add_row(&user_row);
        manual.add_row(&pass_row);
        manual_group.add(&manual);
        content.append(&manual_group);
        {
            let sender = sender.clone();
            manual.connect_expanded_notify(move |e| {
                sender.input(Msg::CloudManualToggle(e.is_expanded()));
            });
        }

        // Music folder to index - always visible (to be given when connecting).
        let path_group = adw::PreferencesGroup::builder()
            .title(&gettext("Music folder to index"))
            .build();
        let path_row = adw::EntryRow::builder()
            .title(gettext("Folder (e.g. /Music)"))
            .build();
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
            .label(&gettext("Test connection"))
            .build();
        {
            let sender = sender.clone();
            test_btn.connect_clicked(move |_| sender.input(Msg::CloudTest));
        }
        let save_btn = gtk::Button::builder()
            .label(&gettext("Save"))
            .css_classes(["suggested-action"])
            .build();
        {
            let sender = sender.clone();
            save_btn.connect_clicked(move |_| sender.input(Msg::CloudSave));
        }
        buttons.append(&test_btn);
        buttons.append(&save_btn);
        content.append(&buttons);

        toolbar.set_content(Some(&content));
        dialog.set_child(Some(&toolbar));
        {
            let sender = sender.clone();
            dialog.connect_closed(move |_| sender.input(Msg::CloudClosed));
        }
        dialog.present(Some(root));

        self.cloud.cam = Some(cam);
        self.cloud.manual = Some(manual);
        self.cloud.url_row = Some(url_row);
        self.cloud.user_row = Some(user_row);
        self.cloud.pass_row = Some(pass_row);
        self.cloud.path_row = Some(path_row);
        self.cloud.status = Some(status);
        self.cloud.dialog = Some(dialog);

        // Start the camera right away - it reads the login QR immediately.
        self.start_cloud_scan(sender);
    }

    /// Starts the webcam scanner for the login QR code.
    pub(crate) fn start_cloud_scan(&mut self, sender: &ComponentSender<Self>) {
        if self.cloud.scanner.is_some() {
            return;
        }
        let sender_dec = sender.clone();
        match Scanner::start(move |code| sender_dec.input(Msg::CloudQrDecoded(code))) {
            Ok((scanner, paintable)) => {
                if let (Some(cam), Some(p)) = (&self.cloud.cam, &paintable) {
                    cam.set_paintable(Some(p));
                    cam.set_visible(true);
                }
                self.cloud.scanner = Some(scanner);
                self.cloud_status(&gettext("Point the camera at the login QR code"));
            }
            Err(e) => {
                // No camera available → switch straight to manual entry.
                tracing::info!("Nextcloud camera unavailable: {e}");
                if let Some(m) = &self.cloud.manual {
                    m.set_expanded(true);
                }
            }
        }
    }

    /// A QR code was decoded: interpret it as a Nextcloud login code and fill
    /// the form fields.
    pub(crate) fn handle_cloud_qr(&mut self, code: &str) {
        let Some((server, user, pass)) = webdav::parse_nc_login(code) else {
            return; // other/invalid code - keep scanning
        };
        self.cloud.scanner = None; // stop the camera
        if let Some(cam) = &self.cloud.cam {
            cam.set_visible(false);
        }
        // Expand the manual entry so the scanned data is visible.
        if let Some(m) = &self.cloud.manual {
            m.set_expanded(true);
        }
        if let Some(r) = &self.cloud.url_row {
            r.set_text(&server);
        }
        if let Some(r) = &self.cloud.user_row {
            r.set_text(&user);
        }
        if let Some(r) = &self.cloud.pass_row {
            r.set_text(&pass);
        }
        self.cloud_status(&gettext("Login data scanned – set the music path and save"));
    }

    /// Reads the form fields into credentials (all required fields except the path).
    fn cloud_creds(&self) -> Option<Creds> {
        let url = self.cloud.url_row.as_ref()?.text().trim().to_string();
        let user = self.cloud.user_row.as_ref()?.text().trim().to_string();
        let pass = self.cloud.pass_row.as_ref()?.text().to_string();
        let path = self.cloud.path_row.as_ref()?.text().trim().to_string();
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
    pub(crate) fn test_cloud(&mut self, sender: &ComponentSender<Self>) {
        let Some(creds) = self.cloud_creds() else {
            self.cloud_status(&gettext("Please fill in URL, user and app password"));
            return;
        };
        self.cloud_status(&gettext("Testing …"));
        sender.spawn_oneshot_command(move || {
            Cmd::WebdavTested(webdav::test_connection(&creds).map_err(|e| e.to_string()))
        });
    }

    /// Saves the source (after the form is filled in) and closes the dialog.
    pub(crate) fn save_cloud(&mut self, sender: &ComponentSender<Self>) {
        let Some(creds) = self.cloud_creds() else {
            self.cloud_status(&gettext("Please fill in URL, user and app password"));
            return;
        };
        // Display name: host of the URL.
        let name = creds
            .base_url
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&creds.base_url)
            .split('/')
            .next()
            .unwrap_or("Nextcloud")
            .to_string();
        let src = Source {
            id: 0,
            kind: "webdav".into(),
            name,
            position: 0,
            path: None,
            base_url: Some(creds.base_url),
            username: Some(creds.user),
            password: Some(creds.pass),
            music_path: Some(creds.music_path),
        };
        match Library::open().and_then(|lib| lib.add_source(&src)) {
            Ok(id) => {
                sender.input(Msg::SourcesChanged);
                self.cloud.scanner = None;
                if let Some(d) = self.cloud.dialog.take() {
                    d.close();
                }
                // Index the cloud library in the background so the tracks
                // feel like local ones (artists/albums + covers/photos).
                let mut indexed = src.clone();
                indexed.id = id;
                sender.spawn_command(move |out| {
                    if let Ok(lib) = Library::open() {
                        match crate::core::webdav::index_into(&lib, &indexed) {
                            Ok(n) => tracing::info!("Indexed {n} Nextcloud tracks"),
                            Err(e) => tracing::warn!("Nextcloud indexing failed: {e}"),
                        }
                    }
                    let _ = out.send(Cmd::RemoteIndexed);
                });
            }
            Err(e) => {
                tracing::error!("add webdav source failed: {e}");
                self.cloud_status(&gettext("Could not save this source"));
            }
        }
    }

    /// Show the result of the connection test.
    pub(crate) fn on_webdav_tested(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => self.cloud_status(&gettext("Connection works")),
            Err(_) => self.cloud_status(&gettext("Connection failed – check the details")),
        }
    }

    fn cloud_status(&self, msg: &str) {
        if let Some(s) = &self.cloud.status {
            s.set_text(msg);
        }
    }
}

/// Normalizes the music subpath (leading slash, without a trailing slash;
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
