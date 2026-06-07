//! Dialogs: action menu (long press), share dialog and settings.
//! Split out of app.rs – pure reordering, no functional change.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::i18n::{gettext, gettext_f};
use crate::model::Source;
use crate::ui::app::{cover_widget, App, CtxTarget, FsKind, Msg};

/// The idle/empty hint shown in the search dialog before anything is typed.
fn search_hint() -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name("system-search-symbolic")
        .title(gettext("Search the library"))
        .description(gettext("Find by artist, album, song or date."))
        .vexpand(true)
        .build()
}

impl App {
    /// Action menu on long press (folder or track).
    pub(crate) fn open_context_menu(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let Some(entry) = self.nav.context_target.as_ref() else {
            return;
        };

        // Fetch the cover/photo of an artist/album target on demand, so it also
        // loads when the detail was opened from Favorites/Audiobooks/Concerts –
        // not just from the Artists/Albums overviews (which already do this).
        // Like there, the image appears on the next open (background fetch).
        match entry {
            CtxTarget::Artist(m) => self.fetch_focus_artist(sender, &m.name),
            CtxTarget::Album(m) => self.fetch_focus_album(sender, &m.artist, &m.album),
            // A song offers its album's cover alternatives → fetch them too.
            CtxTarget::Fs(e) => {
                if let Some((artist, album)) = self.fs_album(e) {
                    self.fetch_focus_album(sender, &artist, &album);
                }
            }
        }

        let dialog = adw::Dialog::builder().title(entry.heading()).build();
        // Fixed content width like every other detail dialog (playlist, podcast,
        // streaming, YouTube). Without it the floating dialog adopts the natural
        // width of its content – which collapses to a narrow sliver depending on
        // what is loaded. On the phone the bottom sheet ignores this and uses the
        // full width anyway.
        dialog.set_content_width(600);
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

        // Lyrics – expandable pulldown above the info (like the properties).
        // Source priority: embedded tags → DB cache (filled while playing). When
        // nothing is available yet, the pulldown starts hidden and an LRCLIB
        // lookup reveals it once it returns (see `on_file_lyrics_fetched`).
        if let CtxTarget::Fs(e) = entry {
            if let Some(epath) = e.path().filter(|_| !e.is_dir()) {
                let path_str = epath.to_string_lossy().to_string();
                let text = crate::core::scanner::read_lyrics(epath).or_else(|| {
                    self.library
                        .get_cached_lyrics(&path_str)
                        .and_then(|l| l.display_text())
                });
                let group = adw::PreferencesGroup::new();
                let exp = adw::ExpanderRow::builder().title(gettext("Lyrics")).build();
                let label = gtk::Label::builder()
                    .label(text.as_deref().unwrap_or_default())
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
                if text.is_none() {
                    // Nothing local: hide the pulldown and try to fetch it online.
                    group.set_visible(false);
                    *self.lyrics.file_pending.borrow_mut() =
                        Some((path_str.clone(), label, group));
                    self.fetch_file_lyrics(&path_str);
                }
            }
        }

        // "Info" – expandable with detail rows
        let info_group = adw::PreferencesGroup::new();
        let expander = adw::ExpanderRow::builder().title(gettext("Info")).build();
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
        let is_current = current_path.is_some()
            && self.transport.playing_path.as_deref() == current_path.as_deref();

        // Artist with only **one** song: "Play artist" + order makes no sense
        // (and the order doesn't even capture single songs without an album).
        // So treat it like a single piece – a plain "Play"; a click starts
        // exactly this song (`CtxPlay`).
        let play_kind = if matches!(play_kind, PlayKind::Artist)
            && self
                .ctx_artist()
                .is_some_and(|n| self.artist_files(&n).len() == 1)
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
                let order = gtk::DropDown::from_strings(&[
                    &gettext("Oldest first"),
                    &gettext("Newest first"),
                ]);
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
        *self.nav.ctx_play.borrow_mut() = current_path.map(|p| (play_row.clone(), p));

        // Remote file: offer an offline download (if not already present).
        if let CtxTarget::Fs(crate::ui::fs_row::FsEntry::RemoteFile {
            rel_path,
            downloaded: None,
            ..
        }) = entry
        {
            let rel = rel_path.clone();
            let dl_row = adw::ActionRow::builder()
                .title(gettext("Download"))
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

        // Favorite star (mark/remove) – only when Favorites is enabled as a nav
        // section (otherwise the action would point at a hidden view).
        if !self.nav.hidden_sections.contains("favorites") {
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
        }

        // Remaining actions.
        let mut actions: Vec<(String, &str, fn() -> Msg)> =
            vec![(gettext("Add to queue"), "list-add-symbolic", || {
                Msg::CtxAddQueue
            })];
        // "Add to playlist" only when Playlists is enabled as a nav section.
        if !self.nav.hidden_sections.contains("playlists") {
            actions.push((gettext("Add to playlist"), "view-list-symbolic", || {
                Msg::CtxAddPlaylist
            }));
        }
        if show_eq {
            actions.push((
                gettext("Equalizer settings"),
                "preferences-other-symbolic",
                || Msg::CtxEqualizer,
            ));
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
            let ctx_play = self.nav.ctx_play.clone();
            dialog.connect_closed(move |_| *ctx_play.borrow_mut() = None);
        }
        dialog.present(Some(root));
    }

    /// Shows/hides the detail dialog's remembered play row accordingly:
    /// hidden as long as exactly this track is playing; visible once it ends
    /// or is switched.
    pub(crate) fn refresh_ctx_play(&self) {
        if let Some((row, path)) = self.nav.ctx_play.borrow().as_ref() {
            row.set_visible(self.transport.playing_path.as_deref() != Some(path.as_path()));
        }
    }

    /// Opens the settings dialog (among others, sets the music folder).
    /// Fills the "Other sources" list with **all** configured extra sources
    /// (second local folder + Nextcloud/WebDAV). Called on open **and** after
    /// every add/remove or a Nextcloud connect (via `Msg::SourcesChanged`), so
    /// the display is correct immediately – without restarting the dialog.
    pub(crate) fn fill_src_list(&self, list: &gtk::ListBox, sender: &ComponentSender<Self>) {
        while let Some(c) = list.first_child() {
            list.remove(&c);
        }
        let sources: Vec<Source> = Library::open()
            .ok()
            .and_then(|l| l.list_sources().ok())
            .unwrap_or_default();
        if sources.is_empty() {
            list.append(
                &adw::ActionRow::builder()
                    .title(gettext("No additional sources"))
                    .css_classes(["dim-label"])
                    .build(),
            );
            return;
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
                .tooltip_text(gettext("Remove"))
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

    /// Library search (title-bar search icon): a search field that, as you type,
    /// lists matching artists, albums and songs (incl. file-date matches).
    /// Activating a hit plays the song / opens the album / opens the artist.
    pub(crate) fn open_search_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::Dialog::builder().title(gettext("Search")).build();
        // Same fixed width as the other detail dialogs; full-width bottom sheet
        // on the phone.
        dialog.set_content_width(600);
        dialog.set_content_height(560);
        self.adapt_detail_dialog(&dialog);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());

        let outer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        let entry = gtk::SearchEntry::builder()
            .placeholder_text(gettext("Artist, album, song, date …"))
            .hexpand(true)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        outer.append(&entry);

        let results = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        results.append(&search_hint());

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&results)
            .build();
        outer.append(&scroller);
        toolbar.set_content(Some(&outer));
        dialog.set_child(Some(&toolbar));

        // Live search: SQLite is local and the result count is capped, so we can
        // re-query on each (already debounced) change of the search entry.
        let sender = sender.clone();
        let dlg = dialog.clone();
        entry.connect_search_changed(move |e| {
            while let Some(c) = results.first_child() {
                results.remove(&c);
            }
            let term = e.text().to_string();
            let q = term.trim();
            if q.is_empty() {
                results.append(&search_hint());
                return;
            }
            let Ok(lib) = Library::open() else { return };
            let res = lib.search_library(q, 30).unwrap_or_default();
            if res.is_empty() {
                results.append(
                    &adw::StatusPage::builder()
                        .icon_name("system-search-symbolic")
                        .title(gettext("No results"))
                        .vexpand(true)
                        .build(),
                );
                return;
            }

            // --- Artists ---
            if !res.artists.is_empty() {
                let group = adw::PreferencesGroup::builder()
                    .title(format!("{} ({})", gettext("Artists"), res.artists.len()))
                    .build();
                for name in &res.artists {
                    let row = adw::ActionRow::builder()
                        .title(gtk::glib::markup_escape_text(name))
                        .activatable(true)
                        .build();
                    row.add_prefix(&gtk::Image::from_icon_name("avatar-default-symbolic"));
                    let sender = sender.clone();
                    let dlg = dlg.clone();
                    let name = name.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::SearchOpenArtist(name.clone()));
                        dlg.close();
                    });
                    group.add(&row);
                }
                results.append(&group);
            }

            // --- Albums ---
            if !res.albums.is_empty() {
                let group = adw::PreferencesGroup::builder()
                    .title(format!("{} ({})", gettext("Albums"), res.albums.len()))
                    .build();
                for a in &res.albums {
                    let mut sub = a.artist.clone();
                    if let Some(y) = a.year {
                        sub = if sub.trim().is_empty() {
                            y.to_string()
                        } else {
                            format!("{sub} · {y}")
                        };
                    }
                    let row = adw::ActionRow::builder()
                        .title(gtk::glib::markup_escape_text(&a.album))
                        .subtitle(gtk::glib::markup_escape_text(&sub))
                        .activatable(true)
                        .build();
                    row.add_prefix(&gtk::Image::from_icon_name("media-optical-symbolic"));
                    let sender = sender.clone();
                    let dlg = dlg.clone();
                    let album = a.album.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::SearchOpenAlbum(album.clone()));
                        dlg.close();
                    });
                    group.add(&row);
                }
                results.append(&group);
            }

            // --- Songs ---
            if !res.songs.is_empty() {
                let group = adw::PreferencesGroup::builder()
                    .title(format!("{} ({})", gettext("Songs"), res.songs.len()))
                    .build();
                for s in &res.songs {
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(a) = s.artist.as_ref().filter(|a| !a.trim().is_empty()) {
                        parts.push(a.clone());
                    }
                    if let Some(al) = s.album.as_ref().filter(|a| !a.trim().is_empty()) {
                        parts.push(al.clone());
                    }
                    let row = adw::ActionRow::builder()
                        .title(gtk::glib::markup_escape_text(&s.title))
                        .subtitle(gtk::glib::markup_escape_text(&parts.join(" · ")))
                        .activatable(true)
                        .build();
                    row.add_prefix(&gtk::Image::from_icon_name("audio-x-generic-symbolic"));
                    row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
                    let sender = sender.clone();
                    let dlg = dlg.clone();
                    let path = s.path.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::SearchPlayTrack(path.clone()));
                        dlg.close();
                    });
                    group.add(&row);
                }
                results.append(&group);
            }
        });

        dialog.present(Some(root));
        entry.grab_focus();
    }

    pub(crate) fn open_settings(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::PreferencesDialog::new();
        let page = adw::PreferencesPage::builder()
            .title(gettext("Library"))
            .icon_name("folder-symbolic")
            .build();
        let group = adw::PreferencesGroup::builder()
            .title(gettext("Music folder"))
            .description(gettext("Folder for the file system view"))
            .build();

        let not_set = gettext("Not set");
        let current = self.files.music_dir.as_deref().unwrap_or(&not_set);
        // First entry shows only the path (no "Music folder" label).
        let row = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(current))
            .title_lines(2)
            .build();

        let button = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text(gettext("Choose folder"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();

        {
            let sender = sender.clone();
            let win = root.clone();
            let row = row.clone();
            button.connect_clicked(move |_| {
                let chooser = gtk::FileDialog::builder()
                    .title(gettext("Choose music folder"))
                    .build();
                let sender = sender.clone();
                let row = row.clone();
                chooser.select_folder(Some(&win), gtk::gio::Cancellable::NONE, move |res| {
                    if let Ok(folder) = res {
                        if let Some(path) = folder.path() {
                            row.set_title(&gtk::glib::markup_escape_text(&path.to_string_lossy()));
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
            .title(gettext("Other sources"))
            .description(gettext("Shown as tabs in the file view"))
            .build();
        let src_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        src_group.add(&src_list);

        // Fill from the DB and remember the list, so `Msg::SourcesChanged`
        // (fired after add/remove **and** after a Nextcloud connect) can refresh
        // it live while the dialog stays open.
        self.fill_src_list(&src_list, sender);
        *self.settings_src_list.borrow_mut() = Some(src_list.clone());

        // Button row: add a local folder. (A Nextcloud is added via the button in
        // the "Nextcloud" group below; both kinds land in this same list.)
        let add_local = gtk::Button::builder()
            .label(gettext("Add local folder"))
            .css_classes(["flat"])
            .build();
        {
            let win = root.clone();
            let sender = sender.clone();
            add_local.connect_clicked(move |_| {
                let chooser = gtk::FileDialog::builder()
                    .title(gettext("Choose folder"))
                    .build();
                let sender = sender.clone();
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
            .title(gettext("Nextcloud"))
            .description(gettext(
                "Connect a Nextcloud and index its music folder like a local library.",
            ))
            .build();
        let connect = adw::ActionRow::builder()
            .title(gettext("Connect to Nextcloud"))
            .subtitle(gettext(
                "Scan the login QR code or enter the details manually.",
            ))
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
        // Connected Nextcloud sources are listed (and removable) together with the
        // local ones in the "Other sources" group above – no separate list here.

        let lib_page = page;

        // --- Category: Sound ---
        let page = adw::PreferencesPage::builder()
            .title(gettext("Sound"))
            .icon_name("preferences-other-symbolic")
            .build();
        // Global equalizer (basis for everything without a custom artist/album/track EQ).
        let eq_group = adw::PreferencesGroup::builder()
            .title(gettext("Equalizer"))
            .description(gettext(
                "Global sound control. It applies everywhere unless a custom \
                 setting is set for an artist, an album or a track.",
            ))
            .build();
        let eq_row = adw::ActionRow::builder()
            .title(gettext("Global equalizer"))
            .subtitle(gettext("Ten bands, per output"))
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
            .title(gettext("Search"))
            .icon_name("system-search-symbolic")
            .build();

        // 1. Automatic fetch (first option).
        let auto_group = adw::PreferencesGroup::builder()
            .title(gettext("Read music data"))
            .description(gettext(
                "Complete missing cover art, photos and tracks from open online sources.",
            ))
            .build();
        let auto_row = adw::SwitchRow::builder()
            .title(gettext("Fetch automatically"))
            .subtitle(gettext(
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
            .title(gettext("AcoustID"))
            .description(gettext(
                "Optional key for fingerprint-based track detection (free at acoustid.org/new-application).",
            ))
            .build();
        let key_row = adw::EntryRow::builder()
            .title(gettext("AcoustID API key"))
            .build();
        key_row.set_text(self.enrich_state.acoustid_key.as_deref().unwrap_or(""));
        key_row.set_show_apply_button(true);
        crate::ui::widgets::no_autofocus(&key_row);
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
            .title(gettext("fanart.tv"))
            .description(gettext("Optional key for showing several artist photos."))
            .build();
        let fanart_row = adw::EntryRow::builder()
            .title(gettext("fanart.tv API key"))
            .build();
        fanart_row.set_text(self.enrich_state.fanart_key.as_deref().unwrap_or(""));
        fanart_row.set_show_apply_button(true);
        crate::ui::widgets::no_autofocus(&fanart_row);
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

        let search_page = page;

        // --- Category: View ---
        let page = adw::PreferencesPage::builder()
            .title(gettext("View"))
            .icon_name("view-list-symbolic")
            .build();

        // Display language at the very top (takes effect after restarting the app).
        let lang_group = adw::PreferencesGroup::builder()
            .title(gettext("Language"))
            .build();
        // The shared language list ([`crate::i18n::LANGUAGES`], codes + endonyms),
        // with the "System default" choice prepended so it stays on top. The
        // endonyms are shown untranslated; English is the source language.
        let mut lang_codes: Vec<&str> = vec!["system"];
        lang_codes.extend(crate::i18n::LANGUAGES.iter().map(|(c, _)| *c));
        let mut lang_labels: Vec<String> = vec![gettext("System default")];
        lang_labels.extend(crate::i18n::LANGUAGES.iter().map(|(_, l)| (*l).to_string()));
        let lang_label_refs: Vec<&str> = lang_labels.iter().map(String::as_str).collect();
        let lang_row = adw::ComboRow::builder()
            .title(gettext("Display language"))
            .subtitle(gettext("Takes effect after a restart"))
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
                let code = lang_codes
                    .get(r.selected() as usize)
                    .copied()
                    .unwrap_or("system");
                sender.input(Msg::SetLanguage(code.to_string()));
            });
        }
        lang_group.add(&lang_row);
        page.add(&lang_group);

        // Appearance: color scheme automatic/dark/light (takes effect immediately).
        let theme_group = adw::PreferencesGroup::builder()
            .title(gettext("Appearance"))
            .build();
        let theme_codes = ["system", "dark", "light"];
        let theme_labels = [gettext("Automatic"), gettext("Dark"), gettext("Light")];
        let theme_label_refs: Vec<&str> = theme_labels.iter().map(String::as_str).collect();
        let theme_row = adw::ComboRow::builder()
            .title(gettext("Theme"))
            .model(&gtk::StringList::new(&theme_label_refs))
            .build();
        let cur_scheme = self
            .library
            .get_setting("color_scheme")
            .ok()
            .flatten()
            .unwrap_or_else(|| "system".to_string());
        let cur_theme_idx = theme_codes
            .iter()
            .position(|c| *c == cur_scheme)
            .unwrap_or(0);
        theme_row.set_selected(cur_theme_idx as u32);
        {
            // Connect the handler only after `set_selected`, so the preselection
            // doesn't trigger a change.
            let sender = sender.clone();
            theme_row.connect_selected_notify(move |r| {
                let code = theme_codes
                    .get(r.selected() as usize)
                    .copied()
                    .unwrap_or("system");
                sender.input(Msg::SetColorScheme(code.to_string()));
            });
        }
        theme_group.add(&theme_row);
        page.add(&theme_group);

        // Gallery view (cover grid) instead of a list + tiles per row.
        let gallery_group = adw::PreferencesGroup::builder()
            .title(gettext("List display"))
            .build();
        let gallery_row = adw::SwitchRow::builder()
            .title(gettext("Gallery view"))
            .subtitle(gettext("Show lists as a grid of cover thumbnails"))
            .active(self.libview.gallery_view)
            .build();
        {
            let sender = sender.clone();
            gallery_row.connect_active_notify(move |r| {
                sender.input(Msg::SetGalleryView(r.is_active()));
            });
        }
        gallery_group.add(&gallery_row);
        let cols_row = adw::SpinRow::builder()
            .title(gettext("Tiles per row"))
            .adjustment(&gtk::Adjustment::new(
                self.libview.gallery_columns as f64,
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
            .title(gettext("Menu"))
            .icon_name("open-menu-symbolic")
            .build();
        let sections_group = adw::PreferencesGroup::builder()
            .title(gettext("Menu items"))
            .description(gettext(
                "Drag handle to reorder; the switch hides a menu item.",
            ))
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        // Shared, local state of the dialog (alongside the model).
        let order = std::rc::Rc::new(std::cell::RefCell::new(self.nav.section_order.clone()));
        let hidden = std::rc::Rc::new(std::cell::RefCell::new(self.nav.hidden_sections.clone()));
        rebuild_section_rows(&list, &order, &hidden, sender);
        sections_group.add(&list);
        page.add(&sections_group);
        let menu_page = page;

        // --- Category: Cache (incl. the recording timeshift buffer) ---
        let page = adw::PreferencesPage::builder()
            .title(gettext("Cache"))
            .icon_name("media-record-symbolic")
            .build();
        let streaming_group = adw::PreferencesGroup::builder()
            .title(gettext("Streaming"))
            .description(gettext(
                "Timeshift buffer for recording the currently playing station.",
            ))
            .build();
        let buffer_row = adw::SpinRow::builder()
            .title(gettext("Recording buffer (minutes)"))
            .subtitle(gettext(
                "Keep the last minutes of a station so you can record a song after it played. 0 turns it off.",
            ))
            .adjustment(&gtk::Adjustment::new(
                self.streaming.recording_buffer_minutes as f64,
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
            .title(gettext("Hidden"))
            .icon_name("view-conceal-symbolic")
            .build();
        let hidden_group = adw::PreferencesGroup::builder()
            .title(gettext("Hidden content"))
            .description(gettext(
                "Artists, albums and tracks whose properties are visible nowhere – each the object that carries the setting. Use the eye to show them again.",
            ))
            .build();
        let entries = self.library.hidden_entries();
        if entries.is_empty() {
            hidden_group.add(
                &adw::ActionRow::builder()
                    .title(gettext("Nothing hidden"))
                    .build(),
            );
        }
        for (scope, key, title, is_dir) in entries {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&title))
                .subtitle(hidden_kind(&scope))
                .build();
            row.add_prefix(&cover_widget(
                self.entry_cover(&scope, &key, is_dir).as_deref(),
                hidden_icon(&scope),
            ));
            let reveal = gtk::Button::builder()
                .icon_name("view-reveal-symbolic")
                .tooltip_text(gettext("Show again"))
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

        // YouTube (optional feature; the extractor yt-dlp is downloaded at
        // runtime, never bundled, and the feature is off by default). Lives on
        // the "Library" page (added to `lib_page` below).
        let yt_group = adw::PreferencesGroup::builder()
            .title(gettext("YouTube"))
            .description(gettext(
                "Search and play YouTube, follow channels, add videos/playlists to your music or keep them offline. May be restricted in some countries.",
            ))
            .build();
        let yt_enable = adw::SwitchRow::builder()
            .title(gettext("Enable YouTube"))
            .subtitle(gettext(
                "Adds a YouTube section. Requires downloading yt-dlp below.",
            ))
            .active(self.youtube.enabled)
            .build();
        {
            let sender = sender.clone();
            yt_enable.connect_active_notify(move |r| {
                sender.input(Msg::SetYoutubeEnabled(r.is_active()));
            });
        }
        yt_group.add(&yt_enable);

        let ytdlp_row = adw::ActionRow::builder()
            .title("yt-dlp")
            .subtitle(gettext(
                "Required tool – downloaded into the app data folder (not bundled).",
            ))
            .build();
        // Probing the installed version spawns `yt-dlp --version` (a Python zipapp
        // whose import takes a second or more on a phone). NEVER do that on the UI
        // thread while building the dialog – it would freeze the settings open for
        // seconds. Show the cached value (or the busy text) and run the probe in the
        // background; `Cmd::YtDlpChecked` updates the row when it finishes. (Reuses
        // the already-translated "Working …" string rather than a new one.)
        let cached = self.youtube.ytdlp_version.clone();
        let status = gtk::Label::builder().css_classes(["dim-label"]).build();
        status.set_text(&match &cached {
            Some(v) => gettext_f("Installed (version {v})", &[("v", v)]),
            None => gettext("Working …"),
        });
        ytdlp_row.add_suffix(&status);
        let dl_label = if cached.is_some() {
            gettext("Update")
        } else {
            gettext("Download")
        };
        let dl_btn = gtk::Button::builder()
            .label(&dl_label)
            .valign(gtk::Align::Center)
            .build();
        dl_btn.add_css_class("flat");
        {
            let sender = sender.clone();
            // Download vs. update is decided from the cached version at click time
            // (see `Msg::FetchYtDlp`), so the button is correct even mid-probe.
            dl_btn.connect_clicked(move |_| sender.input(Msg::FetchYtDlp));
        }
        ytdlp_row.add_suffix(&dl_btn);
        yt_group.add(&ytdlp_row);
        // The YouTube group lives at the bottom of the "Library" page.
        lib_page.add(&yt_group);
        // Remember the status label + button so a finished probe/download/update
        // refreshes them (see `refresh_ytdlp_status_label`).
        *self.youtube.settings_status.borrow_mut() = Some(status);
        *self.youtube.settings_dl_btn.borrow_mut() = Some(dl_btn);
        {
            let status_slot = self.youtube.settings_status.clone();
            let btn_slot = self.youtube.settings_dl_btn.clone();
            dialog.connect_closed(move |_| {
                *status_slot.borrow_mut() = None;
                *btn_slot.borrow_mut() = None;
            });
        }
        // Resolve the real version in the background unless it is already cached.
        if cached.is_none() {
            sender.spawn_command(|out| {
                let _ = out.send(crate::ui::app::Cmd::YtDlpChecked(
                    crate::core::youtube::version(),
                ));
            });
        }

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
        let dest = match self.nav.context_target.as_ref() {
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
            .title(gettext("Choose a custom image"))
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

    /// Set an album's cover (from the picker), refreshing the albums view on a
    /// real change.
    pub(crate) fn set_album_cover(&mut self, artist: String, album: String, path: String) {
        let mut meta = self
            .library
            .get_album_meta(&artist, &album)
            .ok()
            .flatten()
            .unwrap_or_else(|| crate::model::AlbumMeta::pending(&artist, &album));
        if meta.cover_path.as_deref() != Some(path.as_str()) {
            meta.cover_path = Some(path);
            let _ = self.library.upsert_album_meta(&meta);
            self.reload_albums();
        }
    }

    /// Set an artist's image (from the picker), refreshing the artists view.
    pub(crate) fn set_artist_image(&mut self, name: String, path: String) {
        let mut meta = self
            .library
            .get_artist_meta(&name)
            .ok()
            .flatten()
            .unwrap_or_else(|| crate::model::ArtistMeta::pending(&name));
        if meta.image_path.as_deref() != Some(path.as_str()) {
            meta.image_path = Some(path);
            let _ = self.library.upsert_artist_meta(&meta);
            self.reload_artists();
        }
    }

    /// Persist a scope+key's section/area assignment and reload the views
    /// (concerts/audiobooks are derived live from the properties).
    pub(crate) fn set_areas(
        &mut self,
        sender: &ComponentSender<Self>,
        scope: &'static str,
        key: String,
        value: String,
    ) {
        if let Err(e) = self.library.set_category(scope, &key, Some(&value)) {
            tracing::error!("Failed to save properties: {e}");
        }
        self.reload_library_overviews();
        self.load_concerts(sender);
        self.load_audiobooks(sender);
        self.load_dir(sender);
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
