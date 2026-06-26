//! First-run setup assistant, a standalone relm4 component.
//!
//! Shown once on the very first launch (no `setup_complete` flag and no music
//! folder yet). It walks the user through four steps — language, "do you
//! already have a collection?", the music folder, and which menu items to use —
//! and reports the result to [`crate::ui::app::App`] via [`SetupOutput`], which
//! persists it and kicks off the initial scan.
//!
//! Like [`crate::ui::sync_page::SyncPage`] the component owns no visible root;
//! it builds and presents a single modal [`adw::Dialog`] on demand and swaps an
//! inner [`adw::ViewStack`] between the steps. The look is plain libadwaita
//! (boxed lists, cards, accent-tinted stepper) so light/dark and the narrow
//! Phosh layout (the dialog becomes a bottom sheet) come for free.

use std::collections::HashSet;
use std::path::PathBuf;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, switch_language, system_language_code, LANGUAGES};
use crate::ui::app::SECTIONS;

/// Number of wizard steps (0..STEPS-1).
const STEPS: usize = 4;

/// The setup-wizard component. Holds the chosen values plus the few widgets
/// that have to be updated as the user moves between steps.
pub(crate) struct SetupPage {
    /// Main window the dialog is presented on (set on `Open`).
    window: Option<adw::ApplicationWindow>,
    /// The presented dialog (built once on the first `Open`).
    dialog: Option<adw::Dialog>,
    /// Current step (0..STEPS-1).
    step: usize,
    /// Chosen display-language code (e.g. "de"); pre-set to the system language.
    lang_code: String,
    /// Whether the user already has a collection (vs. starting fresh). Only
    /// changes the explanatory text of the folder step.
    has_collection: bool,
    /// Chosen music folder (pre-filled with the XDG music dir).
    music_dir: PathBuf,
    /// Menu items (section stack names) the user wants enabled.
    enabled: HashSet<&'static str>,

    // Widgets updated across steps:
    view_stack: adw::ViewStack,
    step_circles: Vec<gtk::Box>,
    back_btn: gtk::Button,
    next_btn: gtk::Button,
    folder_title: gtk::Label,
    folder_subtitle: gtk::Label,
    folder_row: adw::ActionRow,
}

#[derive(Debug)]
pub(crate) enum SetupInput {
    /// Open (and, the first time, build) the wizard on the given window.
    Open(adw::ApplicationWindow),
    SelectLanguage(String),
    SetHasCollection(bool),
    /// "Browse…" tapped → open the folder chooser.
    PickFolder,
    /// A folder was chosen in the file dialog.
    FolderChosen(PathBuf),
    /// A feature/menu-item switch was toggled.
    ToggleSection(&'static str, bool),
    Next,
    Back,
    /// "Cancel" on the first step → abort first-run setup and quit.
    Cancel,
    /// Final "Continue" → emit the result and close.
    Finish,
}

#[derive(Debug)]
pub(crate) enum SetupOutput {
    /// The user completed the wizard. The parent persists everything and starts
    /// the initial scan / applies the language.
    Finished {
        lang_code: String,
        music_dir: PathBuf,
        enabled_sections: Vec<String>,
    },
}

#[relm4::component(pub(crate))]
impl Component for SetupPage {
    type Init = ();
    type Input = SetupInput;
    type Output = SetupOutput;
    type CommandOutput = ();

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
        // Default music folder: the XDG music dir, else ~/Music.
        let music_dir = dirs::audio_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Music")))
            .unwrap_or_else(|| PathBuf::from("."));
        // Default: every menu item on, except the opt-in YouTube section.
        let enabled = SECTIONS
            .iter()
            .map(|(name, _, _)| *name)
            .filter(|n| *n != "youtube")
            .collect();

        let model = SetupPage {
            window: None,
            dialog: None,
            step: 0,
            lang_code: system_language_code().to_string(),
            has_collection: true,
            music_dir,
            enabled,
            view_stack: adw::ViewStack::new(),
            step_circles: Vec::new(),
            back_btn: gtk::Button::new(),
            next_btn: gtk::Button::new(),
            folder_title: gtk::Label::new(None),
            folder_subtitle: gtk::Label::new(None),
            folder_row: adw::ActionRow::new(),
        };
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: SetupInput, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            SetupInput::Open(window) => {
                self.window = Some(window);
                self.ensure_dialog(&sender);
            }
            SetupInput::SelectLanguage(code) => {
                if code != self.lang_code {
                    self.lang_code = code.clone();
                    // Load the chosen language right away so the rest of the
                    // wizard — and this very page — appears in it, rather than
                    // only after the final restart. gettext can't retranslate
                    // the existing widgets, so rebuild the dialog from scratch.
                    switch_language(&code);
                    self.rebuild_dialog(&sender);
                }
            }
            SetupInput::SetHasCollection(has) => {
                self.has_collection = has;
                self.apply_folder_text();
            }
            SetupInput::PickFolder => self.pick_folder(&sender),
            SetupInput::FolderChosen(path) => {
                self.music_dir = path;
                self.folder_row.set_title(&gtk::glib::markup_escape_text(
                    &self.music_dir.to_string_lossy(),
                ));
            }
            SetupInput::ToggleSection(name, on) => {
                if on {
                    self.enabled.insert(name);
                } else {
                    self.enabled.remove(name);
                }
            }
            SetupInput::Next => {
                if self.step + 1 < STEPS {
                    self.step += 1;
                    self.apply_step();
                } else {
                    // On the last step the primary button finishes the wizard.
                    sender.input(SetupInput::Finish);
                }
            }
            SetupInput::Back => {
                if self.step > 0 {
                    self.step -= 1;
                    self.apply_step();
                } else {
                    // On the first step the button reads "Cancel" (there is
                    // nothing to go back to), so route it to the abort path.
                    sender.input(SetupInput::Cancel);
                }
            }
            SetupInput::Cancel => {
                // Abort first-run setup: nothing is persisted (so the wizard
                // reappears on the next launch) and no library exists yet, so
                // just quit. Mirror the main window's hard exit — `app.quit()`
                // alone leaves the MPRIS/zbus task keeping the process alive.
                if let Some(d) = self.dialog.take() {
                    d.set_can_close(true);
                    d.close();
                }
                if let Some(app) = self.window.as_ref().and_then(|w| w.application()) {
                    app.quit();
                }
                std::process::exit(0);
            }
            SetupInput::Finish => {
                let enabled_sections = self
                    .enabled
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>();
                let _ = sender.output(SetupOutput::Finished {
                    lang_code: self.lang_code.clone(),
                    music_dir: self.music_dir.clone(),
                    enabled_sections,
                });
                if let Some(d) = &self.dialog {
                    d.set_can_close(true);
                    d.close();
                }
            }
        }
    }
}

impl SetupPage {
    /// Builds and presents the dialog (once). Subsequent `Open`s are no-ops.
    fn ensure_dialog(&mut self, sender: &ComponentSender<Self>) {
        if self.dialog.is_some() {
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };

        // --- Stepper (numbered circles + captions, accent-tinted active step) ---
        let stepper = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        stepper.set_halign(gtk::Align::Center);
        stepper.set_margin_top(10);
        stepper.set_margin_bottom(4);
        let captions = [
            gettext("Language"),
            gettext("Collection"),
            gettext("Folder"),
            gettext("Features"),
        ];
        for (i, caption) in captions.iter().enumerate() {
            if i > 0 {
                let line = gtk::Separator::new(gtk::Orientation::Horizontal);
                line.set_valign(gtk::Align::Center);
                line.set_width_request(28);
                line.set_margin_bottom(18);
                stepper.append(&line);
            }
            let item = gtk::Box::new(gtk::Orientation::Vertical, 4);
            item.set_halign(gtk::Align::Center);
            let circle = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            circle.set_size_request(30, 30);
            circle.set_halign(gtk::Align::Center);
            circle.add_css_class("emilia-step");
            let num = gtk::Label::new(Some(&(i + 1).to_string()));
            num.set_halign(gtk::Align::Center);
            num.set_valign(gtk::Align::Center);
            num.set_hexpand(true);
            circle.append(&num);
            let cap = gtk::Label::new(Some(caption));
            cap.add_css_class("caption");
            cap.add_css_class("dim-label");
            item.append(&circle);
            item.append(&cap);
            stepper.append(&item);
            self.step_circles.push(circle);
        }

        // --- The four step pages inside a ViewStack ---
        self.view_stack.set_vexpand(true);
        self.view_stack
            .add_named(&self.build_language_page(sender), Some("s0"));
        self.view_stack
            .add_named(&self.build_collection_page(sender), Some("s1"));
        self.view_stack
            .add_named(&self.build_folder_page(sender), Some("s2"));
        self.view_stack
            .add_named(&self.build_features_page(sender), Some("s3"));

        // --- Bottom navigation (Cancel/Back / Next-or-Continue) ---
        // The label is set per-step in `apply_step` ("Cancel" on step 0).
        self.back_btn.add_css_class("flat");
        {
            let sender = sender.clone();
            self.back_btn
                .connect_clicked(move |_| sender.input(SetupInput::Back));
        }
        self.next_btn.add_css_class("suggested-action");
        self.next_btn.add_css_class("pill");
        {
            let sender = sender.clone();
            self.next_btn
                .connect_clicked(move |_| sender.input(SetupInput::Next));
        }
        let nav = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        nav.set_margin_top(6);
        nav.set_margin_bottom(12);
        nav.set_margin_start(12);
        nav.set_margin_end(12);
        nav.append(&self.back_btn);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        nav.append(&spacer);
        nav.append(&self.next_btn);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&stepper);
        content.append(&self.view_stack);
        content.append(&nav);

        let header = adw::HeaderBar::new();
        header.set_show_start_title_buttons(false);
        header.set_show_end_title_buttons(false);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));

        let dialog = adw::Dialog::builder()
            .title(gettext("Set up Emilia"))
            .content_width(460)
            .content_height(680)
            .build();
        // The wizard must finish so a music folder gets set; only "Continue"
        // re-enables closing (see `Finish`).
        dialog.set_can_close(false);
        dialog.set_child(Some(&toolbar));
        self.dialog = Some(dialog.clone());

        self.apply_step();
        dialog.present(Some(&window));
    }

    /// Tears down and rebuilds the dialog in the currently active language,
    /// keeping the wizard's state (current step and chosen values). Used when
    /// the language changes mid-setup: gettext only affects newly built widgets,
    /// so the visible dialog has to be recreated.
    fn rebuild_dialog(&mut self, sender: &ComponentSender<Self>) {
        // Drop the old dialog (it was built with `can_close = false`).
        if let Some(d) = self.dialog.take() {
            d.set_can_close(true);
            d.close();
        }
        // Reset the widgets `ensure_dialog` appends to or rebuilds, so the fresh
        // build starts clean; `self.step` and the chosen values are preserved.
        self.view_stack = adw::ViewStack::new();
        self.step_circles.clear();
        self.back_btn = gtk::Button::new();
        self.next_btn = gtk::Button::new();
        self.folder_title = gtk::Label::new(None);
        self.folder_subtitle = gtk::Label::new(None);
        self.folder_row = adw::ActionRow::new();
        // With `self.dialog` now `None`, this rebuilds and re-presents.
        self.ensure_dialog(sender);
    }

    /// Wraps a step's inner box in a Clamp + scroller for narrow/short screens.
    fn page_scroller(inner: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
        let clamp = adw::Clamp::builder().maximum_size(400).child(inner).build();
        gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build()
    }

    /// Icon + heading shown at the top of each step page.
    fn step_header(icon: &str, title: &str) -> gtk::Box {
        let head = gtk::Box::new(gtk::Orientation::Vertical, 12);
        head.set_halign(gtk::Align::Center);
        let img = gtk::Image::from_icon_name(icon);
        img.set_pixel_size(64);
        img.add_css_class("accent");
        head.append(&img);
        let label = gtk::Label::new(Some(title));
        label.add_css_class("title-2");
        label.set_justify(gtk::Justification::Center);
        label.set_wrap(true);
        head.append(&label);
        head
    }

    /// Empty vertical box with the standard page margins.
    fn page_box() -> gtk::Box {
        let b = gtk::Box::new(gtk::Orientation::Vertical, 18);
        b.set_margin_top(18);
        b.set_margin_bottom(18);
        b.set_margin_start(12);
        b.set_margin_end(12);
        b
    }

    fn build_language_page(&self, sender: &ComponentSender<Self>) -> gtk::ScrolledWindow {
        let page = Self::page_box();
        page.append(&Self::step_header(
            "preferences-desktop-locale-symbolic",
            &gettext("Select your language"),
        ));
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let mut leader: Option<gtk::CheckButton> = None;
        for (code, endonym) in LANGUAGES {
            let check = gtk::CheckButton::new();
            match &leader {
                Some(l) => check.set_group(Some(l)),
                None => leader = Some(check.clone()),
            }
            check.set_active(*code == self.lang_code.as_str());
            let row = adw::ActionRow::builder().title(*endonym).build();
            row.add_prefix(&check);
            row.set_activatable_widget(Some(&check));
            {
                let sender = sender.clone();
                let code = (*code).to_string();
                check.connect_toggled(move |c| {
                    if c.is_active() {
                        sender.input(SetupInput::SelectLanguage(code.clone()));
                    }
                });
            }
            list.append(&row);
        }
        page.append(&list);
        Self::page_scroller(&page)
    }

    fn build_collection_page(&self, sender: &ComponentSender<Self>) -> gtk::ScrolledWindow {
        let page = Self::page_box();
        page.append(&Self::step_header(
            "folder-music-symbolic",
            &gettext("Do you already have a music collection?"),
        ));

        let cards = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        cards.set_homogeneous(true);
        let make_card = |icon: &str, title: &str, sub: &str| {
            let btn = gtk::ToggleButton::new();
            btn.add_css_class("card");
            let inner = gtk::Box::new(gtk::Orientation::Vertical, 8);
            inner.set_margin_top(16);
            inner.set_margin_bottom(16);
            inner.set_margin_start(10);
            inner.set_margin_end(10);
            let img = gtk::Image::from_icon_name(icon);
            img.set_pixel_size(40);
            img.add_css_class("accent");
            inner.append(&img);
            let t = gtk::Label::new(Some(title));
            t.add_css_class("heading");
            t.set_wrap(true);
            t.set_justify(gtk::Justification::Center);
            inner.append(&t);
            let s = gtk::Label::new(Some(sub));
            s.add_css_class("caption");
            s.add_css_class("dim-label");
            s.set_wrap(true);
            s.set_justify(gtk::Justification::Center);
            inner.append(&s);
            btn.set_child(Some(&inner));
            btn
        };
        let yes = make_card(
            "audio-x-generic-symbolic",
            &gettext("Yes, I have an existing collection"),
            &gettext("Add your existing music to the app."),
        );
        let no = make_card(
            "folder-new-symbolic",
            &gettext("No, start fresh"),
            &gettext("Build your collection from scratch."),
        );
        no.set_group(Some(&yes));
        yes.set_active(self.has_collection);
        no.set_active(!self.has_collection);
        {
            let sender = sender.clone();
            yes.connect_toggled(move |b| {
                if b.is_active() {
                    sender.input(SetupInput::SetHasCollection(true));
                }
            });
        }
        {
            let sender = sender.clone();
            no.connect_toggled(move |b| {
                if b.is_active() {
                    sender.input(SetupInput::SetHasCollection(false));
                }
            });
        }
        cards.append(&yes);
        cards.append(&no);
        page.append(&cards);

        let hint = gtk::Label::new(Some(&gettext(
            "You can add more music sources later in Settings.",
        )));
        hint.add_css_class("dim-label");
        hint.set_wrap(true);
        hint.set_justify(gtk::Justification::Center);
        page.append(&hint);

        Self::page_scroller(&page)
    }

    fn build_folder_page(&self, sender: &ComponentSender<Self>) -> gtk::ScrolledWindow {
        let page = Self::page_box();
        page.append(&Self::step_header(
            "folder-open-symbolic",
            &gettext("Music folder"),
        ));

        // Title + subtitle are filled in by `apply_folder_text` (they depend on
        // the collection choice made in the previous step).
        self.folder_title.add_css_class("title-4");
        self.folder_title.set_wrap(true);
        self.folder_title.set_justify(gtk::Justification::Center);
        self.folder_subtitle.add_css_class("dim-label");
        self.folder_subtitle.set_wrap(true);
        self.folder_subtitle.set_justify(gtk::Justification::Center);
        page.append(&self.folder_title);
        page.append(&self.folder_subtitle);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        self.folder_row.set_title(&gtk::glib::markup_escape_text(
            &self.music_dir.to_string_lossy(),
        ));
        self.folder_row.set_title_lines(2);
        let browse = gtk::Button::builder()
            .label(gettext("Browse…"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let sender = sender.clone();
            browse.connect_clicked(move |_| sender.input(SetupInput::PickFolder));
        }
        self.folder_row.add_suffix(&browse);
        self.folder_row.set_activatable_widget(Some(&browse));
        list.append(&self.folder_row);
        page.append(&list);

        Self::page_scroller(&page)
    }

    fn build_features_page(&self, sender: &ComponentSender<Self>) -> gtk::ScrolledWindow {
        let page = Self::page_box();
        page.append(&Self::step_header(
            "view-grid-symbolic",
            &gettext("Choose features to use"),
        ));
        let hint = gtk::Label::new(Some(&gettext(
            "Pick the menu items you want to see. You can show, hide and reorder them anytime in Settings.",
        )));
        hint.add_css_class("dim-label");
        hint.set_wrap(true);
        hint.set_justify(gtk::Justification::Center);
        page.append(&hint);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        for (name, label, icon) in SECTIONS {
            let row = adw::SwitchRow::builder()
                .title(gettext(label))
                .subtitle(gettext(crate::ui::app::section_description(name)))
                .active(self.enabled.contains(name))
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            {
                let sender = sender.clone();
                row.connect_active_notify(move |r| {
                    sender.input(SetupInput::ToggleSection(name, r.is_active()));
                });
            }
            list.append(&row);
        }
        page.append(&list);
        Self::page_scroller(&page)
    }

    /// Sets the folder step's heading/subtitle from the collection choice.
    fn apply_folder_text(&self) {
        if self.has_collection {
            self.folder_title
                .set_text(&gettext("Choose your music folder"));
            self.folder_subtitle.set_text(&gettext(
                "Pick the folder that already holds your collection. You can add more sources later in Settings.",
            ));
        } else {
            self.folder_title
                .set_text(&gettext("Choose a folder for your music"));
            self.folder_subtitle.set_text(&gettext(
                "This is where downloads and recordings will be stored.",
            ));
        }
    }

    /// Opens the folder chooser, pre-pointed at the current selection.
    fn pick_folder(&self, sender: &ComponentSender<Self>) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let chooser = gtk::FileDialog::builder()
            .title(gettext("Choose music folder"))
            .build();
        if self.music_dir.is_dir() {
            chooser.set_initial_folder(Some(&gtk::gio::File::for_path(&self.music_dir)));
        }
        let sender = sender.clone();
        chooser.select_folder(Some(&window), gtk::gio::Cancellable::NONE, move |res| {
            if let Ok(folder) = res {
                if let Some(path) = folder.path() {
                    sender.input(SetupInput::FolderChosen(path));
                }
            }
        });
    }

    /// Reflects `self.step` in the stepper, the visible page, the folder text
    /// and the navigation buttons.
    fn apply_step(&self) {
        self.view_stack
            .set_visible_child_name(&format!("s{}", self.step));
        for (i, circle) in self.step_circles.iter().enumerate() {
            if i == self.step {
                circle.add_css_class("emilia-step-active");
            } else {
                circle.remove_css_class("emilia-step-active");
            }
        }
        // Step 0 has nothing to go back to, so the button cancels setup instead.
        self.back_btn.set_label(&if self.step == 0 {
            gettext("Cancel")
        } else {
            gettext("Back")
        });
        let last = self.step + 1 == STEPS;
        self.next_btn.set_label(&if last {
            gettext("Continue")
        } else {
            gettext("Next")
        });
        // On the last step the "Continue" click is routed to `Finish` by the
        // `Next` handler. Keep the folder text in sync when arriving at it.
        if self.step == 2 {
            self.apply_folder_text();
        }
    }
}
