//! Dialogs: action menu (long press), share dialog and settings.
//! Split out of app.rs – pure reordering, no functional change.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use std::cell::RefCell;
use std::rc::Rc;

use crate::core::db::Library;
use crate::i18n::{gettext, gettext_f};
use crate::model::Source;
use crate::ui::app::{cover_widget, App, CtxTarget, FsKind, Msg};

impl App {
    /// Action menu on long press (folder or track).
    pub(crate) fn open_context_menu(&self, root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) {
        let Some(entry) = self.context_target.as_ref() else {
            return;
        };

        let dialog = adw::Dialog::builder().title(entry.heading()).build();
        // On the phone use the full width (bottom sheet) instead of floating.
        self.adapt_detail_dialog(&dialog);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Cover/photo, or – when there are multiple images – a carousel with dots.
        self.append_cover_or_gallery(&content, entry, sender, &dialog);

        // Lyrics (if present in the file tags) – expandable, above the info
        // (a pulldown like the properties).
        if let CtxTarget::Fs(e) = entry {
            if let Some(epath) = e.path().filter(|_| !e.is_dir()) {
                if let Some(lyrics) = crate::core::scanner::read_lyrics(epath) {
                    let group = adw::PreferencesGroup::new();
                    let exp = adw::ExpanderRow::builder().title(&gettext("Lyrics")).build();
                    let label = gtk::Label::builder()
                        .label(&lyrics)
                        .wrap(true)
                        .xalign(0.0)
                        .selectable(true)
                        .margin_top(8)
                        .margin_bottom(8)
                        .margin_start(12)
                        .margin_end(12)
                        .build();
                    exp.add_row(&label);
                    group.add(&exp);
                    content.append(&group);
                }
            }
        }

        // "Info" – expandable with detail rows
        let info_group = adw::PreferencesGroup::new();
        let expander = adw::ExpanderRow::builder().title(&gettext("Info")).build();
        for (label, value) in self.ctx_info_lines(entry) {
            let row = adw::ActionRow::builder()
                .title(&label)
                .subtitle(gtk::glib::markup_escape_text(&value))
                .build();
            row.set_subtitle_lines(2);
            expander.add_row(&row);
        }
        info_group.add(&expander);
        content.append(&info_group);

        // "Properties" – category per level (track/album/artist), inherited.
        if let Some(merkmale) = self.ctx_merkmale(entry, sender) {
            content.append(&merkmale);
        }

        // Actions
        let action_group = adw::PreferencesGroup::new();
        // Determine the target's playback kind (label + order of the play action).
        #[derive(Clone, Copy)]
        enum PlayKind {
            Album,
            Artist,
            Other,
        }
        let play_kind = match entry {
            CtxTarget::Album(_) => PlayKind::Album,
            CtxTarget::Artist(_) => PlayKind::Artist,
            CtxTarget::Fs(e) if e.is_dir() => match self.fs_music_kind(e) {
                Some(FsKind::Album { .. }) => PlayKind::Album,
                Some(FsKind::Artist(_)) => PlayKind::Artist,
                None => PlayKind::Other,
            },
            CtxTarget::Fs(_) => PlayKind::Other,
        };
        // Offer the equalizer where there is an unambiguous level: for tracks
        // and cards, and for folders recognized as an artist or album.
        let show_eq = !matches!(
            (entry, play_kind),
            (CtxTarget::Fs(e), PlayKind::Other) if e.is_dir()
        );

        // Play action: for album/artist its own text and its own order.
        // Path of the target track (files only) – basis for the dynamic
        // visibility of the "Play" action.
        let current_path: Option<std::path::PathBuf> = match entry {
            CtxTarget::Fs(e) if !e.is_dir() => e.path().cloned(),
            _ => None,
        };
        // As long as exactly this track is **playing**, don't show a "Play" action;
        // once it ends, it is shown again (see `refresh_ctx_play`).
        let is_current =
            current_path.is_some() && self.playing_path.as_deref() == current_path.as_deref();

        // Artist with only **one** song: "Play artist" + order makes no sense
        // (and the order doesn't even capture single songs without an album).
        // So treat it like a single piece – a plain "Play"; a click starts
        // exactly this song (`CtxPlay`).
        let play_kind = if matches!(play_kind, PlayKind::Artist)
            && self.ctx_artist().is_some_and(|n| self.artist_files(&n).len() == 1)
        {
            PlayKind::Other
        } else {
            play_kind
        };

        let play_row = adw::ActionRow::builder()
            .title(&match play_kind {
                PlayKind::Album => gettext("Play album"),
                PlayKind::Artist => gettext("Play artist"),
                PlayKind::Other => gettext("Play"),
            })
            .activatable(true)
            .build();
        play_row.add_prefix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
        match play_kind {
            PlayKind::Artist => {
                // Album order selectable, on the same line as the action.
                let order = gtk::DropDown::from_strings(&[&gettext("Oldest first"), &gettext("Newest first")]);
                order.set_valign(gtk::Align::Center);
                order.set_tooltip_text(Some(&gettext("Album order")));
                play_row.add_suffix(&order);
                let sender = sender.clone();
                let dialog = dialog.clone();
                play_row.connect_activated(move |_| {
                    sender.input(Msg::CtxPlayArtist {
                        newest_first: order.selected() == 1,
                    });
                    dialog.close();
                });
            }
            PlayKind::Album => {
                let sender = sender.clone();
                let dialog = dialog.clone();
                play_row.connect_activated(move |_| {
                    sender.input(Msg::CtxPlayAlbum);
                    dialog.close();
                });
            }
            PlayKind::Other => {
                let sender = sender.clone();
                let dialog = dialog.clone();
                play_row.connect_activated(move |_| {
                    sender.input(Msg::CtxPlay);
                    dialog.close();
                });
            }
        }
        action_group.add(&play_row);
        play_row.set_visible(!is_current);
        // Remember this play row so it reappears after the track ends.
        *self.ctx_play.borrow_mut() = current_path.map(|p| (play_row.clone(), p));

        // Remote file: offer an offline download (if not already present).
        if let CtxTarget::Fs(crate::ui::fs_row::FsEntry::RemoteFile {
            rel_path,
            downloaded: None,
            ..
        }) = entry
        {
            let rel = rel_path.clone();
            let dl_row = adw::ActionRow::builder()
                .title(&gettext("Download"))
                .activatable(true)
                .build();
            dl_row.add_prefix(&gtk::Image::from_icon_name("folder-download-symbolic"));
            let sender = sender.clone();
            let dialog = dialog.clone();
            dl_row.connect_activated(move |_| {
                sender.input(Msg::CtxDownloadRemote(rel.clone()));
                dialog.close();
            });
            action_group.add(&dl_row);
        }

        // Favorite star (mark/remove).
        let is_fav = self.target_is_favorite(entry);
        let fav_row = adw::ActionRow::builder()
            .title(&if is_fav {
                gettext("Remove from favorites")
            } else {
                gettext("Add to favorites")
            })
            .activatable(true)
            .build();
        fav_row.add_prefix(&gtk::Image::from_icon_name("emilia-favorite-symbolic"));
        {
            let sender = sender.clone();
            let dialog = dialog.clone();
            fav_row.connect_activated(move |_| {
                sender.input(Msg::ToggleFavorite);
                dialog.close();
            });
        }
        action_group.add(&fav_row);

        // Remaining actions.
        let mut actions: Vec<(String, &str, fn() -> Msg)> = vec![
            (gettext("Add to queue"), "list-add-symbolic", || Msg::CtxAddQueue),
            (gettext("Add to playlist"), "view-list-symbolic", || {
                Msg::CtxAddPlaylist
            }),
        ];
        if show_eq {
            actions.push((gettext("Equalizer settings"), "preferences-other-symbolic", || {
                Msg::CtxEqualizer
            }));
        }
        actions.push((gettext("Share"), "emblem-shared-symbolic", || Msg::CtxShare));
        for (label, icon, make_msg) in actions {
            let row = adw::ActionRow::builder()
                .title(&label)
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            let sender = sender.clone();
            let dialog = dialog.clone();
            row.connect_activated(move |_| {
                sender.input(make_msg());
                dialog.close();
            });
            action_group.add(&row);
        }
        content.append(&action_group);

        // For overly large content (e.g. on the phone) scroll vertically, otherwise
        // let the dialog grow to the natural content height. `Automatic` shows a
        // scrollbar on overflow – with `External` the lower actions (equalizer,
        // share) became unreachable on narrow windows.
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .propagate_natural_width(true)
            .vexpand(true)
            .child(&content)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        // Forget the remembered play row as soon as the dialog closes.
        {
            let ctx_play = self.ctx_play.clone();
            dialog.connect_closed(move |_| *ctx_play.borrow_mut() = None);
        }
        dialog.present(Some(root));
    }

    /// Shows/hides the detail dialog's remembered play row accordingly:
    /// hidden as long as exactly this track is playing; visible once it ends
    /// or is switched.
    pub(crate) fn refresh_ctx_play(&self) {
        if let Some((row, path)) = self.ctx_play.borrow().as_ref() {
            row.set_visible(self.playing_path.as_deref() != Some(path.as_path()));
        }
    }

    /// "Share" dialog: offer a connection (start the service) or scan a QR code.
    /// The actual device sync logic follows later.
    pub(crate) fn open_share_dialog(&self, root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) {
        let dialog = adw::Dialog::builder().title(&gettext("Share")).build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        let group = adw::PreferencesGroup::builder()
            .description(&gettext("Connect to another device to sync content."))
            .build();

        let actions: [(String, String, &str, fn() -> Msg); 2] = [
            (
                gettext("Offer connection"),
                gettext("Start the service and wait for another device"),
                "network-wireless-symbolic",
                || Msg::ShareHost,
            ),
            (
                gettext("Scan QR code"),
                gettext("Scan another device's code"),
                "camera-photo-symbolic",
                || Msg::ShareScan,
            ),
        ];

        for (title, subtitle, icon, make_msg) in actions {
            let row = adw::ActionRow::builder()
                .title(&title)
                .subtitle(&subtitle)
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            let sender = sender.clone();
            let dialog = dialog.clone();
            row.connect_activated(move |_| {
                sender.input(make_msg());
                dialog.close();
            });
            group.add(&row);
        }

        content.append(&group);

        // For overly large content (e.g. on the phone) scroll vertically, otherwise
        // let the dialog grow to the natural content height. `Automatic` shows a
        // scrollbar on overflow.
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .propagate_natural_width(true)
            .vexpand(true)
            .child(&content)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(root));
    }
    /// Opens the settings dialog (among others, sets the music folder).
    /// (New) Fills the "Connected" list of the Nextcloud settings page with the
    /// stored WebDAV sources. Called on open **and** after a connect
    /// (via `Msg::SourcesChanged`), so the display is correct immediately.
    pub(crate) fn fill_nc_list(&self, list: &gtk::ListBox, sender: &ComponentSender<Self>) {
        while let Some(c) = list.first_child() {
            list.remove(&c);
        }
        let webdav_sources: Vec<Source> = Library::open()
            .ok()
            .and_then(|l| l.list_sources().ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.kind == "webdav")
            .collect();
        if webdav_sources.is_empty() {
            list.append(
                &adw::ActionRow::builder()
                    .title(&gettext("No Nextcloud connected"))
                    .css_classes(["dim-label"])
                    .build(),
            );
            return;
        }
        for s in webdav_sources {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&s.name))
                .subtitle(gtk::glib::markup_escape_text(s.base_url.as_deref().unwrap_or("")))
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("network-server-symbolic"));
            let del = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text(&gettext("Remove"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let id = s.id;
                let sender = sender.clone();
                del.connect_clicked(move |_| {
                    if let Ok(lib) = Library::open() {
                        let _ = lib.delete_source(id);
                    }
                    sender.input(Msg::SourcesChanged);
                });
            }
            row.add_suffix(&del);
            list.append(&row);
        }
    }

    pub(crate) fn open_settings(&self, root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) {
        let dialog = adw::PreferencesDialog::new();
        let page = adw::PreferencesPage::builder()
            .title(&gettext("Library"))
            .icon_name("folder-symbolic")
            .build();
        let group = adw::PreferencesGroup::builder()
            .title(&gettext("Music folder"))
            .description(&gettext("Folder for the file system view"))
            .build();

        let not_set = gettext("Not set");
        let current = self.music_dir.as_deref().unwrap_or(&not_set);
        // First entry shows only the path (no "Music folder" label).
        let row = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(current))
            .title_lines(2)
            .build();

        let button = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text(&gettext("Choose folder"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();

        {
            let sender = sender.clone();
            let win = root.clone();
            let row = row.clone();
            button.connect_clicked(move |_| {
                let chooser = gtk::FileDialog::builder()
                    .title(&gettext("Choose music folder"))
                    .build();
                let sender = sender.clone();
                let row = row.clone();
                chooser.select_folder(Some(&win), gtk::gio::Cancellable::NONE, move |res| {
                    if let Ok(folder) = res {
                        if let Some(path) = folder.path() {
                            row.set_title(&gtk::glib::markup_escape_text(
                                &path.to_string_lossy(),
                            ));
                            sender.input(Msg::SetMusicDir(path));
                        }
                    }
                });
            });
        }

        row.add_suffix(&button);
        row.set_activatable_widget(Some(&button));
        group.add(&row);
        page.add(&group);

        // --- Other sources (second local folder / Nextcloud) ---
        // Its own connection to the DB (like everywhere in the code via `Library::open`),
        // so this dialog can maintain the list itself; the main window is
        // informed about changes via `Msg::SourcesChanged`.
        let src_group = adw::PreferencesGroup::builder()
            .title(&gettext("Other sources"))
            .description(&gettext("Shown as tabs in the file view"))
            .build();
        let src_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        src_group.add(&src_list);

        // (New) Fills the source list from the DB. Self-referential, so the
        // list refreshes after adding/removing without restarting the dialog.
        let populate: Rc<RefCell<Option<Box<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        {
            let populate_weak = Rc::downgrade(&populate);
            let src_list = src_list.clone();
            let sender_pop = sender.clone();
            *populate.borrow_mut() = Some(Box::new(move || {
                while let Some(c) = src_list.first_child() {
                    src_list.remove(&c);
                }
                let sources = Library::open()
                    .ok()
                    .and_then(|l| l.list_sources().ok())
                    .unwrap_or_default();
                if sources.is_empty() {
                    let empty = adw::ActionRow::builder()
                        .title(&gettext("No additional sources"))
                        .css_classes(["dim-label"])
                        .build();
                    src_list.append(&empty);
                }
                for s in sources {
                    let subtitle = match s.kind.as_str() {
                        "webdav" => s.base_url.clone().unwrap_or_default(),
                        _ => s.path.clone().unwrap_or_default(),
                    };
                    let icon = if s.kind == "webdav" {
                        "network-server-symbolic"
                    } else {
                        "drive-removable-media-symbolic"
                    };
                    let row = adw::ActionRow::builder()
                        .title(gtk::glib::markup_escape_text(&s.name))
                        .subtitle(gtk::glib::markup_escape_text(&subtitle))
                        .build();
                    row.add_prefix(&gtk::Image::from_icon_name(icon));
                    let del = gtk::Button::builder()
                        .icon_name("user-trash-symbolic")
                        .tooltip_text(&gettext("Remove"))
                        .valign(gtk::Align::Center)
                        .css_classes(["flat"])
                        .build();
                    {
                        let id = s.id;
                        let sender = sender_pop.clone();
                        let populate_weak = populate_weak.clone();
                        del.connect_clicked(move |_| {
                            if let Ok(lib) = Library::open() {
                                let _ = lib.delete_source(id);
                            }
                            sender.input(Msg::SourcesChanged);
                            if let Some(p) = populate_weak.upgrade() {
                                if let Some(f) = p.borrow().as_ref() {
                                    f();
                                }
                            }
                        });
                    }
                    row.add_suffix(&del);
                    src_list.append(&row);
                }
            }));
        }
        if let Some(f) = populate.borrow().as_ref() {
            f();
        }

        // Button row: add a local folder / Nextcloud.
        let add_local = gtk::Button::builder()
            .label(&gettext("Add local folder"))
            .css_classes(["flat"])
            .build();
        {
            let win = root.clone();
            let sender = sender.clone();
            let populate = populate.clone();
            add_local.connect_clicked(move |_| {
                let chooser = gtk::FileDialog::builder()
                    .title(&gettext("Choose folder"))
                    .build();
                let sender = sender.clone();
                let populate = populate.clone();
                chooser.select_folder(Some(&win), gtk::gio::Cancellable::NONE, move |res| {
                    if let Ok(folder) = res {
                        if let Some(path) = folder.path() {
                            let name = path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("Folder")
                                .to_string();
                            let src = Source {
                                id: 0,
                                kind: "local".into(),
                                name,
                                position: 0,
                                path: Some(path.to_string_lossy().into_owned()),
                                base_url: None,
                                username: None,
                                password: None,
                                music_path: None,
                            };
                            if let Ok(lib) = Library::open() {
                                let _ = lib.add_source(&src);
                            }
                            sender.input(Msg::SourcesChanged);
                            if let Some(f) = populate.borrow().as_ref() {
                                f();
                            }
                        }
                    }
                });
            });
        }
        let btn_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk::Align::Center)
            .margin_top(6)
            .build();
        btn_row.append(&add_local);
        src_group.add(&btn_row);
        page.add(&src_group);

        // Nextcloud directly in the library (no separate menu item).
        let nc_group = adw::PreferencesGroup::builder()
            .title(&gettext("Nextcloud"))
            .description(&gettext(
                "Connect a Nextcloud and index its music folder like a local library.",
            ))
            .build();
        let connect = adw::ActionRow::builder()
            .title(&gettext("Connect to Nextcloud"))
            .subtitle(&gettext("Scan the login QR code or enter the details manually."))
            .activatable(true)
            .build();
        connect.add_prefix(&gtk::Image::from_icon_name("network-server-symbolic"));
        connect.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        {
            let sender = sender.clone();
            connect.connect_activated(move |_| sender.input(Msg::AddCloudSource));
        }
        nc_group.add(&connect);
        page.add(&nc_group);

        // Already connected Nextcloud sources (for removal). The list is
        // remembered so it is fresh immediately after a successful connect.
        let nc_list_group = adw::PreferencesGroup::builder()
            .title(&gettext("Connected"))
            .build();
        let nc_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        self.fill_nc_list(&nc_list, sender);
        *self.settings_nc_list.borrow_mut() = Some(nc_list.clone());
        nc_list_group.add(&nc_list);
        page.add(&nc_list_group);

        let lib_page = page;

        // --- Category: Sound ---
        let page = adw::PreferencesPage::builder()
            .title(&gettext("Sound"))
            .icon_name("preferences-other-symbolic")
            .build();
        // Global equalizer (basis for everything without a custom artist/album/track EQ).
        let eq_group = adw::PreferencesGroup::builder()
            .title(&gettext("Equalizer"))
            .description(&gettext(
                "Global sound control. It applies everywhere unless a custom \
                 setting is set for an artist, an album or a track.",
            ))
            .build();
        let eq_row = adw::ActionRow::builder()
            .title(&gettext("Global equalizer"))
            .subtitle(&gettext("Ten bands, per output"))
            .activatable(true)
            .build();
        eq_row.add_prefix(&gtk::Image::from_icon_name("preferences-other-symbolic"));
        eq_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        {
            let sender = sender.clone();
            eq_row.connect_activated(move |_| sender.input(Msg::OpenGlobalEq));
        }
        eq_group.add(&eq_row);
        page.add(&eq_group);
        let sound_page = page;

        // --- Category: Search (read online metadata) ---
        let page = adw::PreferencesPage::builder()
            .title(&gettext("Search"))
            .icon_name("system-search-symbolic")
            .build();

        // 1. Automatic fetch (first option).
        let auto_group = adw::PreferencesGroup::builder()
            .title(&gettext("Read music data"))
            .description(&gettext(
                "Complete missing cover art, photos and tracks from open online sources.",
            ))
            .build();
        let auto_row = adw::SwitchRow::builder()
            .title(&gettext("Fetch automatically"))
            .subtitle(&gettext(
                "Loads missing data in the background at startup – on any connection.",
            ))
            .active(self.enrich_state.auto_enrich)
            .build();
        {
            let sender = sender.clone();
            auto_row.connect_active_notify(move |r| {
                sender.input(Msg::SetAutoEnrich(r.is_active()));
            });
        }
        auto_group.add(&auto_row);
        page.add(&auto_group);

        // 2. AcoustID.
        let acoustid_group = adw::PreferencesGroup::builder()
            .title(&gettext("AcoustID"))
            .description(&gettext(
                "Optional key for fingerprint-based track detection (free at acoustid.org/new-application).",
            ))
            .build();
        let key_row = adw::EntryRow::builder().title(&gettext("AcoustID API key")).build();
        key_row.set_text(self.enrich_state.acoustid_key.as_deref().unwrap_or(""));
        key_row.set_show_apply_button(true);
        {
            let sender = sender.clone();
            key_row.connect_apply(move |r| {
                sender.input(Msg::SetAcoustidKey(r.text().to_string()));
            });
        }
        acoustid_group.add(&key_row);
        page.add(&acoustid_group);

        // 3. fanart.tv.
        let fanart_group = adw::PreferencesGroup::builder()
            .title(&gettext("fanart.tv"))
            .description(&gettext("Optional key for showing several artist photos."))
            .build();
        let fanart_row = adw::EntryRow::builder()
            .title(&gettext("fanart.tv API key"))
            .build();
        fanart_row.set_text(self.enrich_state.fanart_key.as_deref().unwrap_or(""));
        fanart_row.set_show_apply_button(true);
        {
            let sender = sender.clone();
            fanart_row.connect_apply(move |r| {
                sender.input(Msg::SetFanartKey(r.text().to_string()));
            });
        }
        fanart_group.add(&fanart_row);
        page.add(&fanart_group);

        // --- Device synchronization: hidden in the settings
        //     (the feature stays reachable via the share button). ---

        // --- Software update (only in the Flatpak version) ---
        if crate::core::update::in_flatpak() {
            let update_group = adw::PreferencesGroup::builder()
                .title(&gettext("App update"))
                .description(&gettext(
                    "Emilia is installed as a Flatpak – check for a newer version and update directly.",
                ))
                .build();
            let update_row = adw::ActionRow::builder()
                .title(&gettext("Check for updates"))
                .subtitle(&gettext_f(
                    "Installed version: {v}",
                    &[("v", env!("CARGO_PKG_VERSION"))],
                ))
                .activatable(true)
                .build();
            update_row.add_prefix(&gtk::Image::from_icon_name("software-update-available-symbolic"));
            update_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            {
                let sender = sender.clone();
                update_row.connect_activated(move |_| sender.input(Msg::CheckForUpdates));
            }
            update_group.add(&update_row);
            page.add(&update_group);
        }
        let search_page = page;

        // --- Category: View ---
        let page = adw::PreferencesPage::builder()
            .title(&gettext("View"))
            .icon_name("view-list-symbolic")
            .build();

        // Display language at the very top (takes effect after restarting the app).
        let lang_group = adw::PreferencesGroup::builder()
            .title(&gettext("Language"))
            .build();
        // Stable codes alongside the display labels. "Deutsch"/"English" are
        // proper names and stay untranslated.
        let lang_codes = ["system", "de", "en"];
        let lang_labels = [gettext("System default"), "Deutsch".into(), "English".into()];
        let lang_label_refs: Vec<&str> = lang_labels.iter().map(String::as_str).collect();
        let lang_row = adw::ComboRow::builder()
            .title(&gettext("Display language"))
            .subtitle(&gettext("Takes effect after a restart"))
            .model(&gtk::StringList::new(&lang_label_refs))
            .build();
        let current_idx = lang_codes
            .iter()
            .position(|c| *c == self.settings.ui_language)
            .unwrap_or(0);
        lang_row.set_selected(current_idx as u32);
        {
            // Connect the handler only after `set_selected`, so the preselection
            // doesn't trigger a language change.
            let sender = sender.clone();
            lang_row.connect_selected_notify(move |r| {
                let code = lang_codes.get(r.selected() as usize).copied().unwrap_or("system");
                sender.input(Msg::SetLanguage(code.to_string()));
            });
        }
        lang_group.add(&lang_row);
        page.add(&lang_group);

        // Appearance: color scheme automatic/dark/light (takes effect immediately).
        let theme_group = adw::PreferencesGroup::builder()
            .title(&gettext("Appearance"))
            .build();
        let theme_codes = ["system", "dark", "light"];
        let theme_labels = [gettext("Automatic"), gettext("Dark"), gettext("Light")];
        let theme_label_refs: Vec<&str> = theme_labels.iter().map(String::as_str).collect();
        let theme_row = adw::ComboRow::builder()
            .title(&gettext("Theme"))
            .model(&gtk::StringList::new(&theme_label_refs))
            .build();
        let cur_scheme = self
            .library
            .get_setting("color_scheme")
            .ok()
            .flatten()
            .unwrap_or_else(|| "system".to_string());
        let cur_theme_idx = theme_codes.iter().position(|c| *c == cur_scheme).unwrap_or(0);
        theme_row.set_selected(cur_theme_idx as u32);
        {
            // Connect the handler only after `set_selected`, so the preselection
            // doesn't trigger a change.
            let sender = sender.clone();
            theme_row.connect_selected_notify(move |r| {
                let code = theme_codes.get(r.selected() as usize).copied().unwrap_or("system");
                sender.input(Msg::SetColorScheme(code.to_string()));
            });
        }
        theme_group.add(&theme_row);
        page.add(&theme_group);

        // Gallery view (cover grid) instead of a list + tiles per row.
        let gallery_group = adw::PreferencesGroup::builder()
            .title(&gettext("List display"))
            .build();
        let gallery_row = adw::SwitchRow::builder()
            .title(&gettext("Gallery view"))
            .subtitle(&gettext("Show lists as a grid of cover thumbnails"))
            .active(self.gallery_view)
            .build();
        {
            let sender = sender.clone();
            gallery_row.connect_active_notify(move |r| {
                sender.input(Msg::SetGalleryView(r.is_active()));
            });
        }
        gallery_group.add(&gallery_row);
        let cols_row = adw::SpinRow::builder()
            .title(&gettext("Tiles per row"))
            .adjustment(&gtk::Adjustment::new(
                self.gallery_columns as f64,
                2.0,
                8.0,
                1.0,
                1.0,
                0.0,
            ))
            .build();
        {
            let sender = sender.clone();
            cols_row.connect_value_notify(move |r| {
                sender.input(Msg::SetGalleryColumns(r.value() as u32));
            });
        }
        gallery_group.add(&cols_row);
        page.add(&gallery_group);

        let view_page = page;

        // --- Category: Menu (manage menu items) ---
        let page = adw::PreferencesPage::builder()
            .title(&gettext("Menu"))
            .icon_name("open-menu-symbolic")
            .build();
        let sections_group = adw::PreferencesGroup::builder()
            .title(&gettext("Menu items"))
            .description(&gettext(
                "Drag handle to reorder; the switch hides a menu item.",
            ))
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        // Shared, local state of the dialog (alongside the model).
        let order = std::rc::Rc::new(std::cell::RefCell::new(self.section_order.clone()));
        let hidden = std::rc::Rc::new(std::cell::RefCell::new(self.hidden_sections.clone()));
        rebuild_section_rows(&list, &order, &hidden, sender);
        sections_group.add(&list);
        page.add(&sections_group);
        let menu_page = page;

        // --- Category: Cache and recordings ---
        let page = adw::PreferencesPage::builder()
            .title(&gettext("Cache & recordings"))
            .icon_name("media-record-symbolic")
            .build();
        let streaming_group = adw::PreferencesGroup::builder()
            .title(&gettext("Streaming"))
            .description(&gettext(
                "Timeshift buffer for recording the currently playing station.",
            ))
            .build();
        let buffer_row = adw::SpinRow::builder()
            .title(&gettext("Recording buffer (minutes)"))
            .subtitle(&gettext(
                "Keep the last minutes of a station so you can record a song after it played. 0 turns it off.",
            ))
            .adjustment(&gtk::Adjustment::new(
                self.recording_buffer_minutes as f64,
                0.0,
                60.0,
                1.0,
                5.0,
                0.0,
            ))
            .build();
        {
            let sender = sender.clone();
            buffer_row.connect_value_notify(move |r| {
                sender.input(Msg::SetRecordingBufferMinutes(r.value() as u32));
            });
        }
        streaming_group.add(&buffer_row);
        page.add(&streaming_group);
        let cache_page = page;

        // --- Category: Hidden (far right) ---
        let page = adw::PreferencesPage::builder()
            .title(&gettext("Hidden"))
            .icon_name("view-conceal-symbolic")
            .build();
        let hidden_group = adw::PreferencesGroup::builder()
            .title(&gettext("Hidden content"))
            .description(&gettext(
                "Artists, albums and tracks whose properties are visible nowhere – each the object that carries the setting. Use the eye to show them again.",
            ))
            .build();
        let entries = self.library.hidden_entries();
        if entries.is_empty() {
            hidden_group.add(
                &adw::ActionRow::builder()
                    .title(&gettext("Nothing hidden"))
                    .build(),
            );
        }
        for (scope, key, title, is_dir) in entries {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&title))
                .subtitle(&hidden_kind(&scope))
                .build();
            row.add_prefix(&cover_widget(
                self.entry_cover(&scope, &key, is_dir).as_deref(),
                hidden_icon(&scope),
            ));
            let reveal = gtk::Button::builder()
                .icon_name("view-reveal-symbolic")
                .tooltip_text(&gettext("Show again"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let sender = sender.clone();
                let group = hidden_group.clone();
                let row = row.clone();
                reveal.connect_clicked(move |_| {
                    sender.input(Msg::UnhideEntry {
                        scope: scope.clone(),
                        key: key.clone(),
                    });
                    group.remove(&row);
                });
            }
            row.add_suffix(&reveal);
            hidden_group.add(&row);
        }
        page.add(&hidden_group);
        let hidden_page = page;

        // Order of the settings pages: "View" first.
        dialog.add(&view_page);
        dialog.add(&lib_page);
        dialog.add(&sound_page);
        dialog.add(&search_page);
        dialog.add(&menu_page);
        dialog.add(&cache_page);
        dialog.add(&hidden_page);

        dialog.present(Some(root));
    }

    /// File dialog for uploading a custom cover/photo for the current detail
    /// target (album → cover, artist → photo). The chosen image is copied into
    /// the cache and set as the primary image.
    pub(crate) fn open_cover_upload_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        enum Dest {
            Album(String, String),
            Artist(String),
        }
        let dest = match self.context_target.as_ref() {
            Some(CtxTarget::Album(m)) => Some(Dest::Album(m.artist.clone(), m.album.clone())),
            Some(CtxTarget::Artist(m)) => Some(Dest::Artist(m.name.clone())),
            // Folder in the file browser: resolve as an album or artist.
            _ => match self.ctx_album() {
                Some((a, al)) => Some(Dest::Album(a, al)),
                None => self.ctx_artist().map(Dest::Artist),
            },
        };
        let Some(dest) = dest else {
            self.toast(&gettext("No custom image can be set here"));
            return;
        };

        let filter = gtk::FileFilter::new();
        filter.add_pixbuf_formats();
        filter.set_name(Some(&gettext("Images")));
        let chooser = gtk::FileDialog::builder()
            .title(&gettext("Choose a custom image"))
            .default_filter(&filter)
            .build();

        let sender = sender.clone();
        chooser.open(Some(root), gtk::gio::Cancellable::NONE, move |res| {
            let Ok(file) = res else {
                return;
            };
            let Some(src) = file.path() else {
                return;
            };
            let is_artist = matches!(dest, Dest::Artist(_));
            let Some(cached) = store_custom_image(&src, is_artist) else {
                return;
            };
            match dest {
                Dest::Album(artist, album) => sender.input(Msg::SetAlbumCover {
                    artist,
                    album,
                    path: cached,
                }),
                Dest::Artist(name) => sender.input(Msg::SetArtistImage { name, path: cached }),
            }
        });
    }
}

/// Copies a chosen image into the cover or artist cache and returns the new
/// path. The file name is unique (timestamp), so the image is loaded fresh
/// immediately and no old cache entry applies.
fn store_custom_image(src: &std::path::Path, is_artist: bool) -> Option<String> {
    let dir = if is_artist {
        crate::core::online::artist_cache_dir()
    } else {
        crate::core::online::cover_cache_dir()
    };
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| e.len() <= 5)
        .unwrap_or("img");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let out = dir.join(format!("custom_{stamp}.{ext}"));
    std::fs::copy(src, &out).ok()?;
    Some(out.to_string_lossy().into_owned())
}

/// Rebuilds the menu item rows (drag handle, label, visibility switch) in the
/// current order. Reorderable by dragging; every change updates the local dialog
/// state (`order`/`hidden`) and reports it to the model, which applies navigation
/// and order immediately.
fn rebuild_section_rows(
    list: &gtk::ListBox,
    order: &std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
    hidden: &std::rc::Rc<std::cell::RefCell<std::collections::HashSet<String>>>,
    sender: &ComponentSender<App>,
) {
    while let Some(c) = list.first_child() {
        list.remove(&c);
    }
    let names: Vec<&'static str> = order.borrow().clone();
    for (idx, &name) in names.iter().enumerate() {
        let Some((label, _icon)) = crate::ui::app::section_meta(name) else {
            continue;
        };
        let row = adw::ActionRow::builder().title(label).build();

        // Drag handle on the left (a hint); the whole row is dragged.
        let handle = gtk::Image::from_icon_name("list-drag-handle-symbolic");
        handle.set_tooltip_text(Some(&gettext("Drag to reorder")));
        row.add_prefix(&handle);

        let drag = gtk::DragSource::new();
        drag.set_actions(gtk::gdk::DragAction::MOVE);
        {
            let name = name.to_string();
            drag.connect_prepare(move |_, _, _| {
                Some(gtk::gdk::ContentProvider::for_value(&name.to_value()))
            });
        }
        row.add_controller(drag);

        // DropTarget on the whole row: move the source to this position.
        let drop = gtk::DropTarget::new(String::static_type(), gtk::gdk::DragAction::MOVE);
        {
            let (list, order, hidden, sender) =
                (list.clone(), order.clone(), hidden.clone(), sender.clone());
            drop.connect_drop(move |_, value, _, _| {
                let Ok(src) = value.get::<String>() else {
                    return false;
                };
                let to = idx;
                let from = order.borrow().iter().position(|n| *n == src.as_str());
                let (Some(from), Some(name_static)) = (
                    from,
                    crate::ui::app::SECTIONS
                        .iter()
                        .map(|(n, _, _)| *n)
                        .find(|n| *n == src.as_str()),
                ) else {
                    return false;
                };
                if from == to {
                    return false;
                }
                {
                    let mut o = order.borrow_mut();
                    o.remove(from);
                    o.insert(to, name_static);
                }
                sender.input(Msg::MoveSection { from, to });
                rebuild_section_rows(&list, &order, &hidden, &sender);
                true
            });
        }
        row.add_controller(drop);

        // Visibility switch on the right.
        let sw = gtk::Switch::builder()
            .active(!hidden.borrow().contains(name))
            .valign(gtk::Align::Center)
            .build();
        {
            let (hidden, sender) = (hidden.clone(), sender.clone());
            sw.connect_active_notify(move |s| {
                // At least one menu item must stay visible.
                if !s.is_active() {
                    let visible = crate::ui::app::SECTIONS
                        .iter()
                        .filter(|(n, _, _)| !hidden.borrow().contains(*n))
                        .count();
                    if visible <= 1 {
                        s.set_active(true);
                        return;
                    }
                }
                if s.is_active() {
                    hidden.borrow_mut().remove(name);
                } else {
                    hidden.borrow_mut().insert(name.to_string());
                }
                sender.input(Msg::SetSectionVisible {
                    section: name,
                    visible: s.is_active(),
                });
            });
        }
        row.add_suffix(&sw);

        list.append(&row);
    }
}

/// Placeholder icon per level in the "Hidden" overview.
fn hidden_icon(scope: &str) -> &'static str {
    match scope {
        "album" => "media-optical-symbolic",
        "artist" => "avatar-default-symbolic",
        "folder" => "folder-symbolic",
        _ => "audio-x-generic-symbolic",
    }
}

/// Subtitle label per level in the "Hidden" overview.
fn hidden_kind(scope: &str) -> String {
    match scope {
        "album" => gettext("Album"),
        "artist" => gettext("Artist"),
        "folder" => gettext("Folder"),
        _ => gettext("Track"),
    }
}
