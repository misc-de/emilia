//! Setup dialog for an SMB share source as a standalone relm4 component:
//! server, share, login and the music folder inside the share, plus a
//! connection test. Mirrors [`crate::ui::cloud_page`] for Nextcloud.
//!
//! SMB logic lives in [`crate::core::smb`]. The component owns the dialog; it
//! emits `SourcesChanged` (a new source was saved → parent reloads tabs/views)
//! and `Indexed` (the share finished indexing → parent rebuilds albums/artists).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::smb::{self, SmbCreds};
use crate::core::source;
use crate::i18n::gettext;

#[derive(Default)]
pub(crate) struct SmbPage {
    dialog: Option<adw::Dialog>,
    server_row: Option<adw::EntryRow>,
    share_row: Option<adw::EntryRow>,
    user_row: Option<adw::EntryRow>,
    pass_row: Option<adw::PasswordEntryRow>,
    path_row: Option<adw::EntryRow>,
    status: Option<gtk::Label>,
}

#[derive(Debug)]
pub(crate) enum SmbInput {
    /// Open the dialog on `window`; `mobile` → present as a bottom sheet.
    Open {
        window: adw::ApplicationWindow,
        mobile: bool,
    },
    Test,
    Save,
    Closed,
}

#[derive(Debug)]
pub(crate) enum SmbOutput {
    /// A source (id) was saved → parent reloads sources/tabs, switches to the
    /// new tab and reloads views.
    SourcesChanged(i64),
    /// The share finished indexing → parent rebuilds albums/artists.
    Indexed,
}

#[derive(Debug)]
pub(crate) enum SmbCmd {
    Tested(Result<(), String>),
    Indexed,
}

#[relm4::component(pub(crate))]
impl Component for SmbPage {
    type Init = ();
    type Input = SmbInput;
    type Output = SmbOutput;
    type CommandOutput = SmbCmd;

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
        let model = SmbPage::default();
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: SmbInput, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            SmbInput::Open { window, mobile } => self.open_dialog(&window, mobile, &sender),
            SmbInput::Test => self.test(&sender),
            SmbInput::Save => self.save(&sender),
            SmbInput::Closed => self.dialog = None,
        }
    }

    fn update_cmd(&mut self, cmd: SmbCmd, sender: ComponentSender<Self>, _root: &Self::Root) {
        match cmd {
            SmbCmd::Tested(Ok(())) => self.status(&gettext("Connection works")),
            SmbCmd::Tested(Err(e)) => {
                tracing::info!("SMB connection test failed: {e}");
                self.status(&gettext("Connection failed – check the details"));
            }
            SmbCmd::Indexed => {
                let _ = sender.output(SmbOutput::Indexed);
            }
        }
    }
}

impl SmbPage {
    /// Opens the "Connect to SMB share" dialog: the connection form plus the
    /// music folder, with test and save buttons.
    fn open_dialog(
        &mut self,
        window: &adw::ApplicationWindow,
        mobile: bool,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::Dialog::builder()
            .title(gettext("Connect to SMB share"))
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

        let conn_group = adw::PreferencesGroup::builder()
            .title(gettext("Share"))
            .description(gettext(
                "A NAS, a Windows share or a Samba server on the network (SMB 2 or newer).",
            ))
            .build();
        let server_row = adw::EntryRow::builder()
            .title(gettext("Server (e.g. nas.local or 192.168.0.5)"))
            .build();
        let share_row = adw::EntryRow::builder()
            .title(gettext("Share name (e.g. music)"))
            .build();
        let user_row = adw::EntryRow::builder().title(gettext("User name")).build();
        let pass_row = adw::PasswordEntryRow::builder()
            .title(gettext("Password"))
            .build();
        for r in [&server_row, &share_row, &user_row] {
            crate::ui::widgets::no_autofocus(r);
        }
        crate::ui::widgets::no_autofocus(&pass_row);
        conn_group.add(&server_row);
        conn_group.add(&share_row);
        conn_group.add(&user_row);
        conn_group.add(&pass_row);
        content.append(&conn_group);

        let path_group = adw::PreferencesGroup::builder()
            .title(gettext("Music folder to index"))
            .build();
        let path_row = adw::EntryRow::builder()
            .title(gettext("Folder in the share (e.g. /Music)"))
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
            test_btn.connect_clicked(move |_| sender.input(SmbInput::Test));
        }
        let save_btn = gtk::Button::builder()
            .label(gettext("Save"))
            .css_classes(["suggested-action"])
            .build();
        {
            let sender = sender.clone();
            save_btn.connect_clicked(move |_| sender.input(SmbInput::Save));
        }
        buttons.append(&test_btn);
        buttons.append(&save_btn);
        content.append(&buttons);

        toolbar.set_content(Some(&content));
        dialog.set_child(Some(&toolbar));
        {
            let sender = sender.clone();
            dialog.connect_closed(move |_| sender.input(SmbInput::Closed));
        }
        crate::ui::app_helpers::close_on_click_outside(&dialog);
        dialog.present(Some(window));

        self.server_row = Some(server_row);
        self.share_row = Some(share_row);
        self.user_row = Some(user_row);
        self.pass_row = Some(pass_row);
        self.path_row = Some(path_row);
        self.status = Some(status);
        self.dialog = Some(dialog);
    }

    /// Reads the form into credentials. The server field also accepts a full
    /// location (`smb://nas/music/Alben`), in which case the share (and a
    /// folder behind it) are taken from there.
    fn creds(&self) -> Option<SmbCreds> {
        let server = self.server_row.as_ref()?.text().trim().to_string();
        let share = self.share_row.as_ref()?.text().trim().to_string();
        let user = self.user_row.as_ref()?.text().trim().to_string();
        let pass = self.pass_row.as_ref()?.text().to_string();
        let mut path = self.path_row.as_ref()?.text().trim().to_string();
        if server.is_empty() || user.is_empty() {
            return None;
        }
        let loc = if share.is_empty() {
            let loc = smb::parse_location(&server)?;
            if path.is_empty() {
                path = loc.subpath.clone();
            }
            loc
        } else {
            let mut loc =
                smb::parse_location(&format!("{}/{share}", server.trim_end_matches('/')))?;
            loc.share = share;
            loc
        };
        Some(SmbCreds::new(&loc, user, pass, &path))
    }

    /// Connection test in the background (open the music folder).
    fn test(&mut self, sender: &ComponentSender<Self>) {
        let Some(creds) = self.creds() else {
            self.status(&gettext("Please fill in server, share and user name"));
            return;
        };
        self.status(&gettext("Testing …"));
        sender.spawn_oneshot_command(move || {
            SmbCmd::Tested(smb::test_connection(&creds).map_err(|e| e.to_string()))
        });
    }

    /// Saves the source and closes the dialog, then indexes in the background.
    fn save(&mut self, sender: &ComponentSender<Self>) {
        let Some(creds) = self.creds() else {
            self.status(&gettext("Please fill in server, share and user name"));
            return;
        };
        match Library::open().and_then(|lib| source::add_smb_source(&lib, &creds, None)) {
            Ok(src) => {
                let _ = sender.output(SmbOutput::SourcesChanged(src.id));
                if let Some(d) = self.dialog.take() {
                    d.close();
                }
                // Index the share in the background so the tracks feel like
                // local ones (artists/albums + covers/photos).
                sender.spawn_command(move |out| {
                    if let Ok(lib) = Library::open() {
                        match crate::core::remote::index_into(&lib, &src) {
                            Ok(n) => tracing::info!("Indexed {n} SMB tracks"),
                            Err(e) => tracing::warn!("SMB indexing failed: {e}"),
                        }
                    }
                    let _ = out.send(SmbCmd::Indexed);
                });
            }
            Err(e) => {
                tracing::error!("add SMB source failed: {e}");
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
