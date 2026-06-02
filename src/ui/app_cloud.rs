//! Einrichtungsdialog für eine Nextcloud-/WebDAV-Quelle: Zugangsdaten per
//! Login-QR-Code (Standard der Nextcloud-App) **oder** manuell, plus
//! Verbindungstest. Reine UI/Eventfluss-Anbindung – die WebDAV-Logik liegt in
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

/// Widget-Zustand des Nextcloud-Dialogs (Handles, damit ein per QR gescanntes
/// Ergebnis die Formularfelder füllen kann).
#[derive(Default)]
pub(crate) struct CloudState {
    pub dialog: Option<adw::Dialog>,
    /// Laufende Webcam-Scanner-Pipeline (Drop stoppt die Kamera).
    pub scanner: Option<Scanner>,
    pub cam: Option<gtk::Picture>,
    /// Aufklappbarer Bereich für die manuelle Eingabe (klappt die Kamera weg).
    pub manual: Option<adw::ExpanderRow>,
    pub url_row: Option<adw::EntryRow>,
    pub user_row: Option<adw::EntryRow>,
    pub pass_row: Option<adw::PasswordEntryRow>,
    pub path_row: Option<adw::EntryRow>,
    pub status: Option<gtk::Label>,
}

impl App {
    /// Öffnet den Dialog „Nextcloud hinzufügen".
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

        // Kamera-Vorschau: liest sofort den Login-QR-Code (Standard).
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

        // Manuelle Eingabe als aufklappbarer Bereich. Aufklappen blendet die
        // Kamera aus (nur manuelle Eingabe), Zuklappen bringt sie zurück.
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

        // Musikordner zum Indizieren – immer sichtbar (bei Verbindung anzugeben).
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

        // Aktionsknöpfe.
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

        // Kamera direkt starten – sie liest den Login-QR sofort.
        self.start_cloud_scan(sender);
    }

    /// Startet den Webcam-Scanner für den Login-QR-Code.
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
                // Keine Kamera verfügbar → direkt auf manuelle Eingabe umschalten.
                tracing::info!("Nextcloud camera unavailable: {e}");
                if let Some(m) = &self.cloud.manual {
                    m.set_expanded(true);
                }
            }
        }
    }

    /// Ein QR-Code wurde dekodiert: als Nextcloud-Login-Code deuten und die
    /// Formularfelder füllen.
    pub(crate) fn handle_cloud_qr(&mut self, code: &str) {
        let Some((server, user, pass)) = webdav::parse_nc_login(code) else {
            return; // anderer/ungültiger Code – weiterscannen
        };
        self.cloud.scanner = None; // Kamera anhalten
        if let Some(cam) = &self.cloud.cam {
            cam.set_visible(false);
        }
        // Manuelle Eingabe aufklappen, damit die gescannten Daten sichtbar sind.
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

    /// Liest die Formularfelder zu Zugangsdaten (alle Pflichtfelder außer Pfad).
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

    /// Verbindungstest im Hintergrund (PROPFIND auf die Musikwurzel).
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

    /// Speichert die Quelle (nach gefülltem Formular) und schließt den Dialog.
    pub(crate) fn save_cloud(&mut self, sender: &ComponentSender<Self>) {
        let Some(creds) = self.cloud_creds() else {
            self.cloud_status(&gettext("Please fill in URL, user and app password"));
            return;
        };
        // Anzeigename: Host der URL.
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
                // Die Cloud-Bibliothek im Hintergrund einlesen, damit sich die
                // Titel wie lokale anfühlen (Interpreten/Alben + Cover/Fotos).
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

    /// Ergebnis des Verbindungstests anzeigen.
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

/// Normalisiert den Musik-Unterpfad (führender Slash, ohne Schluss-Slash;
/// leer = Cloud-Wurzel).
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
