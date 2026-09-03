//! Setup dialog for a Google Drive source as a standalone relm4 component:
//! sign in with Google in the browser (OAuth loopback flow), or reuse an
//! already signed-in account for a second music folder; then pick the music
//! folder and test/save. Mirrors [`crate::ui::cloud_page`] for Nextcloud.
//!
//! Drive/OAuth logic lives in [`crate::core::gdrive`]. The component owns the
//! dialog; it emits `SourcesChanged` (a new source was saved → parent reloads
//! tabs/views) and `Indexed` (the Drive folder finished indexing → parent
//! rebuilds albums/artists).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::gdrive::{self, GdCreds, OAuthClient, TokenSet};
use crate::core::source;
use crate::i18n::{gettext, gettext_f};
use crate::model::Source;

#[derive(Default)]
pub(crate) struct GDrivePage {
    dialog: Option<adw::Dialog>,
    window: Option<adw::ApplicationWindow>,
    /// Already signed-in accounts offered for reuse (a second music folder of
    /// the same Drive, without signing in again).
    existing: Vec<Source>,
    /// The account picked for reuse (`None` = a brand-new sign-in).
    chosen_source: Option<Source>,
    /// New sign-in area (OAuth client + sign-in button). Hidden while a saved
    /// account is reused, and (with saved accounts) until "New sign-in" is
    /// picked from the list.
    new_section: Option<gtk::Box>,
    /// OAuth client fields — only shown when no client is built in or saved.
    client_group: Option<adw::PreferencesGroup>,
    client_id_row: Option<adw::EntryRow>,
    client_secret_row: Option<adw::PasswordEntryRow>,
    signin_btn: Option<gtk::Button>,
    /// The consent URL, shown (selectable) in case the browser did not open.
    link: Option<gtk::Label>,
    /// Music folder + status + buttons. Revealed once an account is available.
    details: Option<gtk::Box>,
    path_row: Option<adw::EntryRow>,
    status: Option<gtk::Label>,
    /// Result of a completed new sign-in.
    tokens: Option<TokenSet>,
    account: Option<String>,
    /// The client used for the running/finished sign-in.
    client: Option<OAuthClient>,
}

#[derive(Debug)]
pub(crate) enum GDriveInput {
    /// Open the dialog on `window`; `mobile` → present as a bottom sheet.
    /// `existing` are already signed-in Drive accounts offered for reuse.
    Open {
        window: adw::ApplicationWindow,
        mobile: bool,
        existing: Vec<Source>,
    },
    /// Reuse the saved account `existing[idx]` – only the music folder is asked.
    ReuseAccount(usize),
    /// Set up a brand-new sign-in.
    NewSignIn,
    /// Start the browser sign-in.
    SignIn,
    Test,
    Save,
    Closed,
}

#[derive(Debug)]
pub(crate) enum GDriveOutput {
    /// A source (id) was saved → parent reloads sources/tabs, switches to the
    /// new tab and reloads views.
    SourcesChanged(i64),
    /// The Drive folder finished indexing → parent rebuilds albums/artists.
    Indexed,
}

#[derive(Debug)]
pub(crate) enum GDriveCmd {
    /// The browser sign-in finished: tokens + account e-mail, or the reason.
    SignedIn(Result<(TokenSet, String), String>),
    Tested(Result<(), String>),
    Indexed,
}

#[relm4::component(pub(crate))]
impl Component for GDrivePage {
    type Init = ();
    type Input = GDriveInput;
    type Output = GDriveOutput;
    type CommandOutput = GDriveCmd;

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
        let model = GDrivePage::default();
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: GDriveInput, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            GDriveInput::Open {
                window,
                mobile,
                existing,
            } => self.open_dialog(&window, mobile, existing, &sender),
            GDriveInput::ReuseAccount(idx) => self.reuse_account(idx),
            GDriveInput::NewSignIn => self.new_sign_in(),
            GDriveInput::SignIn => self.sign_in(&sender),
            GDriveInput::Test => self.test(&sender),
            GDriveInput::Save => self.save(&sender),
            GDriveInput::Closed => {
                self.dialog = None;
                self.window = None;
            }
        }
    }

    fn update_cmd(&mut self, cmd: GDriveCmd, sender: ComponentSender<Self>, _root: &Self::Root) {
        match cmd {
            GDriveCmd::SignedIn(Ok((tokens, email))) => {
                self.tokens = Some(tokens);
                self.account = Some(email.clone());
                if let Some(b) = &self.signin_btn {
                    b.set_sensitive(true);
                }
                if let Some(l) = &self.link {
                    l.set_visible(false);
                }
                if let Some(d) = &self.details {
                    d.set_visible(true);
                }
                self.status(&gettext_f(
                    "Signed in as {account} – set the music folder and save",
                    &[("account", &email)],
                ));
            }
            GDriveCmd::SignedIn(Err(e)) => {
                tracing::info!("Google sign-in failed: {e}");
                if let Some(b) = &self.signin_btn {
                    b.set_sensitive(true);
                }
                if let Some(l) = &self.link {
                    l.set_visible(false);
                }
                self.status(&gettext("Sign-in failed – please try again"));
            }
            GDriveCmd::Tested(Ok(())) => self.status(&gettext("Connection works")),
            GDriveCmd::Tested(Err(e)) => {
                tracing::info!("Google Drive connection test failed: {e}");
                self.status(&gettext("Connection failed – check the folder"));
            }
            GDriveCmd::Indexed => {
                let _ = sender.output(GDriveOutput::Indexed);
            }
        }
    }
}

impl GDrivePage {
    /// Opens the "Connect to Google Drive" dialog. With saved accounts it first
    /// lists them (reuse one, or "New sign-in" at the bottom); without any it
    /// starts straight in new-sign-in mode.
    fn open_dialog(
        &mut self,
        window: &adw::ApplicationWindow,
        mobile: bool,
        existing: Vec<Source>,
        sender: &ComponentSender<Self>,
    ) {
        self.existing = existing;
        self.chosen_source = None;
        self.tokens = None;
        self.account = None;
        self.client = None;
        self.window = Some(window.clone());
        let dialog = adw::Dialog::builder()
            .title(gettext("Connect to Google Drive"))
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

        // 1) Already signed-in accounts: list each (reuse → just a new music
        //    folder), with "New sign-in" as the last entry.
        if !self.existing.is_empty() {
            let acc_group = adw::PreferencesGroup::builder()
                .title(gettext("Account"))
                .build();
            let list = gtk::ListBox::builder()
                .selection_mode(gtk::SelectionMode::None)
                .css_classes(["boxed-list"])
                .build();
            for (i, s) in self.existing.iter().enumerate() {
                let account = crate::core::secrets::resolve_source_username(
                    s.id,
                    s.username.as_deref().unwrap_or(""),
                )
                .filter(|a| !a.is_empty())
                .unwrap_or_else(|| s.name.clone());
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(&account))
                    .subtitle(gtk::glib::markup_escape_text(&s.name))
                    .activatable(true)
                    .build();
                row.add_prefix(&gtk::Image::from_icon_name("avatar-default-symbolic"));
                row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                let sender = sender.clone();
                row.connect_activated(move |_| sender.input(GDriveInput::ReuseAccount(i)));
                list.append(&row);
            }
            let new_row = adw::ActionRow::builder()
                .title(gettext("New sign-in"))
                .activatable(true)
                .build();
            new_row.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
            new_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            {
                let sender = sender.clone();
                new_row.connect_activated(move |_| sender.input(GDriveInput::NewSignIn));
            }
            list.append(&new_row);
            acc_group.add(&list);
            content.append(&acc_group);
        }

        // 2) New sign-in: the OAuth client (only when none is built in/saved)
        //    and the sign-in button.
        let new_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .visible(false)
            .build();

        let client_group = adw::PreferencesGroup::builder()
            .title(gettext("OAuth client"))
            .description(gettext(
                "Google requires an OAuth client per app: create one of type “Desktop app” in the Google Cloud console (with the Drive API enabled) and paste its ID and secret here.",
            ))
            .visible(gdrive::oauth_client().is_none())
            .build();
        let client_id_row = adw::EntryRow::builder().title(gettext("Client ID")).build();
        let client_secret_row = adw::PasswordEntryRow::builder()
            .title(gettext("Client secret"))
            .build();
        crate::ui::widgets::no_autofocus(&client_id_row);
        crate::ui::widgets::no_autofocus(&client_secret_row);
        client_group.add(&client_id_row);
        client_group.add(&client_secret_row);
        new_section.append(&client_group);

        let signin_btn = gtk::Button::builder()
            .label(gettext("Sign in with Google"))
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        {
            let sender = sender.clone();
            signin_btn.connect_clicked(move |_| sender.input(GDriveInput::SignIn));
        }
        new_section.append(&signin_btn);
        let hint = gtk::Label::builder()
            .label(gettext(
                "The browser opens Google's sign-in page; Emilia only asks for read access to your Drive. Return here afterwards.",
            ))
            .wrap(true)
            .xalign(0.5)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .build();
        new_section.append(&hint);
        let link = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .selectable(true)
            .xalign(0.0)
            .visible(false)
            .css_classes(["dim-label", "caption"])
            .build();
        new_section.append(&link);
        content.append(&new_section);

        // 3) Details: the music folder + status + buttons, revealed once an
        //    account (reused or freshly signed in) is available.
        let details = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .visible(false)
            .build();
        let path_group = adw::PreferencesGroup::builder()
            .title(gettext("Music folder to index"))
            .build();
        let path_row = adw::EntryRow::builder()
            .title(gettext("Folder in My Drive (e.g. /Music)"))
            .build();
        crate::ui::widgets::no_autofocus(&path_row);
        path_group.add(&path_row);
        details.append(&path_group);

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
            test_btn.connect_clicked(move |_| sender.input(GDriveInput::Test));
        }
        let save_btn = gtk::Button::builder()
            .label(gettext("Save"))
            .css_classes(["suggested-action"])
            .build();
        {
            let sender = sender.clone();
            save_btn.connect_clicked(move |_| sender.input(GDriveInput::Save));
        }
        buttons.append(&test_btn);
        buttons.append(&save_btn);
        details.append(&buttons);
        content.append(&details);

        // The status sits below everything so sign-in progress is visible
        // before the details are revealed.
        let status = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();
        content.append(&status);

        toolbar.set_content(Some(&content));
        dialog.set_child(Some(&toolbar));
        {
            let sender = sender.clone();
            dialog.connect_closed(move |_| sender.input(GDriveInput::Closed));
        }
        crate::ui::app_helpers::close_on_click_outside(&dialog);
        dialog.present(Some(window));

        self.new_section = Some(new_section);
        self.client_group = Some(client_group);
        self.client_id_row = Some(client_id_row);
        self.client_secret_row = Some(client_secret_row);
        self.signin_btn = Some(signin_btn);
        self.link = Some(link);
        self.details = Some(details);
        self.path_row = Some(path_row);
        self.status = Some(status);
        self.dialog = Some(dialog);

        // Without a saved account there is nothing to list → straight to the
        // new sign-in.
        if self.existing.is_empty() {
            self.new_sign_in();
        }
    }

    /// Reuse a saved account: hide the sign-in area, reveal the details and
    /// ask only for the music folder (the token comes from the Secret Service).
    fn reuse_account(&mut self, idx: usize) {
        self.chosen_source = self.existing.get(idx).cloned();
        self.tokens = None;
        if let Some(s) = &self.new_section {
            s.set_visible(false);
        }
        if let Some(d) = &self.details {
            d.set_visible(true);
        }
        let name = self
            .chosen_source
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_default();
        self.status(&gettext_f(
            "Using the saved sign-in of {account} – just set the music folder",
            &[("account", &name)],
        ));
    }

    /// Set up a new sign-in: reveal the sign-in area (details follow once the
    /// browser sign-in completed).
    fn new_sign_in(&mut self) {
        self.chosen_source = None;
        if let Some(s) = &self.new_section {
            s.set_visible(true);
        }
        if let Some(d) = &self.details {
            d.set_visible(self.tokens.is_some());
        }
        self.status("");
    }

    /// The OAuth client to sign in with: the form fields when shown (saved as
    /// secret settings for next time), else the saved/built-in one.
    fn oauth_client(&self) -> Option<OAuthClient> {
        let shown = self.client_group.as_ref().is_some_and(|g| g.is_visible());
        if shown {
            let id = self.client_id_row.as_ref()?.text().trim().to_string();
            let secret = self.client_secret_row.as_ref()?.text().trim().to_string();
            if id.is_empty() || secret.is_empty() {
                return None;
            }
            let client = OAuthClient { id, secret };
            match Library::open().and_then(|lib| gdrive::set_oauth_client(&lib, &client)) {
                Ok(()) => {}
                Err(e) => tracing::warn!("could not save the Drive OAuth client: {e}"),
            }
            return Some(client);
        }
        gdrive::oauth_client()
    }

    /// Starts the loopback listener, opens the consent page in the browser and
    /// waits for the redirect on a worker.
    fn sign_in(&mut self, sender: &ComponentSender<Self>) {
        let Some(client) = self.oauth_client() else {
            self.status(&gettext(
                "Please enter the OAuth client ID and secret first",
            ));
            return;
        };
        let flow = match gdrive::oauth_begin(&client) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("Google sign-in could not start: {e}");
                self.status(&gettext("Sign-in could not start"));
                return;
            }
        };
        self.client = Some(client.clone());
        if let Some(b) = &self.signin_btn {
            b.set_sensitive(false);
        }
        // Open the browser; keep the link visible in case that fails silently
        // (e.g. no default browser on a phone shell).
        let url = flow.url.clone();
        gtk::UriLauncher::new(&url).launch(
            self.window.as_ref(),
            gtk::gio::Cancellable::NONE,
            |res| {
                if let Err(e) = res {
                    tracing::warn!("could not open the browser for the Google sign-in: {e}");
                }
            },
        );
        if let Some(l) = &self.link {
            l.set_text(&url);
            l.set_visible(true);
        }
        self.status(&gettext("Waiting for the sign-in in the browser …"));
        sender.spawn_oneshot_command(move || {
            let result = gdrive::oauth_finish(flow, &client, gdrive::SIGN_IN_TIMEOUT)
                .and_then(|tokens| {
                    let email = gdrive::account_email(&tokens.access_token)?;
                    Ok((tokens, email))
                })
                .map_err(|e| e.to_string());
            GDriveCmd::SignedIn(result)
        });
    }

    /// Credentials for the chosen account + the music folder from the form.
    /// A fresh sign-in has no source id yet: its access token is seeded under
    /// id 0 so a test needs no extra refresh.
    fn creds(&self) -> Option<GdCreds> {
        let path = self.path_row.as_ref()?.text().trim().to_string();
        let music_path = crate::core::remote::normalize_music_path(&path);
        if let Some(src) = &self.chosen_source {
            let mut c = GdCreds::from_source(src)?;
            c.music_path = music_path;
            return Some(c);
        }
        let tokens = self.tokens.as_ref()?;
        let client = self.client.clone().or_else(gdrive::oauth_client)?;
        gdrive::seed_token(0, &tokens.access_token, tokens.expires_at);
        Some(GdCreds {
            source_id: 0,
            client,
            refresh_token: tokens.refresh_token.clone(),
            account: self.account.clone().unwrap_or_default(),
            music_path,
        })
    }

    /// Connection test in the background (resolve the music folder).
    fn test(&mut self, sender: &ComponentSender<Self>) {
        let Some(creds) = self.creds() else {
            self.status(&gettext("Please sign in first"));
            return;
        };
        self.status(&gettext("Testing …"));
        sender.spawn_oneshot_command(move || {
            GDriveCmd::Tested(gdrive::test_connection(&creds).map_err(|e| e.to_string()))
        });
    }

    /// Saves the source and closes the dialog, then indexes in the background.
    fn save(&mut self, sender: &ComponentSender<Self>) {
        let Some(creds) = self.creds() else {
            self.status(&gettext("Please sign in first"));
            return;
        };
        // Reusing an account: name the new tab after its music folder so it is
        // distinguishable from the first tab (which carries the account).
        let name = self
            .chosen_source
            .as_ref()
            .map(|_| source::folder_tab_name(&creds.music_path, "Google Drive"));
        match Library::open().and_then(|lib| source::add_gdrive_source(&lib, &creds, name)) {
            Ok(src) => {
                // Carry the fresh access token over to the real source id.
                if let Some(t) = &self.tokens {
                    gdrive::seed_token(src.id, &t.access_token, t.expires_at);
                }
                let _ = sender.output(GDriveOutput::SourcesChanged(src.id));
                if let Some(d) = self.dialog.take() {
                    d.close();
                }
                // Index the Drive folder in the background so the tracks feel
                // like local ones (artists/albums + covers/photos).
                sender.spawn_command(move |out| {
                    if let Ok(lib) = Library::open() {
                        match crate::core::remote::index_into(&lib, &src) {
                            Ok(n) => tracing::info!("Indexed {n} Google Drive tracks"),
                            Err(e) => tracing::warn!("Google Drive indexing failed: {e}"),
                        }
                    }
                    let _ = out.send(GDriveCmd::Indexed);
                });
            }
            Err(e) => {
                tracing::error!("add Google Drive source failed: {e}");
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
