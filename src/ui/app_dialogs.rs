//! Action menu (long-press context menu): play / share / queue / refresh.
//! Split out of app.rs – pure reordering, no functional change.

use crate::i18n::{gettext, gettext_f};
use crate::ui::app::{App, CtxTarget, FsKind, Msg};
use crate::ui::app_assistant::AssistantMsg;
use crate::ui::app_views_sources::SourceMsg;
use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

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

        // Assistant — opens an AI chat scoped to this object.
        let asst_group = adw::PreferencesGroup::new();
        let asst_row = adw::ActionRow::builder()
            .title(gettext("Assistant"))
            .subtitle(gettext("Ask the AI to do something with this"))
            .activatable(true)
            .build();
        asst_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        {
            let sender = sender.clone();
            asst_row.connect_activated(move |_| {
                sender.input(Msg::Assistant(AssistantMsg::OpenChat));
            });
        }
        asst_group.add(&asst_row);
        content.append(&asst_group);

        // Lyrics – expandable pulldown below the info (like the properties).
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
                    *self.lyrics.file_pending.borrow_mut() = Some((path_str.clone(), label, group));
                    self.fetch_file_lyrics(&path_str);
                }
            }
        }

        // "Properties" – category per level (track/album/artist), inherited.
        if let Some(merkmale) = self.ctx_merkmale(entry, sender) {
            content.append(&merkmale);
        }

        // Album type switch (Automatic / Album / Single / Compilation).
        if let Some(kind_group) = self.ctx_album_kind_group(entry, sender) {
            content.append(&kind_group);
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
                sender.input(Msg::Source(SourceMsg::DownloadRemote(rel.clone())));
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
                "multimedia-equalizer-symbolic",
                || Msg::CtxEqualizer,
            ));
        }
        // Same share icon as the title bar's device-sync button.
        actions.push((gettext("Share"), "emilia-share-symbolic", || Msg::CtxShare));
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
        // Header bar: the item name with the opened category shown discreetly
        // below it (subtitle), plus a refresh button on the left that re-fetches
        // the cover/metadata and rebuilds the detail view.
        let ab = self.is_audiobook(entry);
        let category = match entry {
            CtxTarget::Artist(_) => gettext("Artist"),
            CtxTarget::Album(_) if ab => gettext("Audiobook"),
            CtxTarget::Album(_) => gettext("Album"),
            CtxTarget::Fs(e) if e.is_dir() => match self.fs_music_kind(e) {
                Some(FsKind::Album { .. }) if ab => gettext("Audiobook"),
                Some(FsKind::Album { .. }) => gettext("Album"),
                Some(FsKind::Artist(_)) => gettext("Artist"),
                _ if ab => gettext("Audiobook"),
                _ => gettext("Folder"),
            },
            CtxTarget::Fs(_) => gettext("Track"),
        };
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&adw::WindowTitle::new(&entry.heading(), &category)));
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.set_tooltip_text(Some(&gettext("Refresh")));
        {
            let sender = sender.clone();
            let dialog = dialog.clone();
            refresh.connect_clicked(move |_| {
                sender.input(Msg::CtxRefresh);
                dialog.close();
            });
        }
        header.pack_start(&refresh);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        // Remember the open dialog so a cover/photo change can rebuild it; forget
        // it (and the play row) as soon as it closes.
        *self.nav.ctx_dialog.borrow_mut() = Some(dialog.clone());
        {
            let ctx_play = self.nav.ctx_play.clone();
            let ctx_dialog = self.nav.ctx_dialog.clone();
            let this = dialog.clone();
            dialog.connect_closed(move |_| {
                *ctx_play.borrow_mut() = None;
                // Only clear if it's still us (a rebuild may have replaced it).
                let is_current = ctx_dialog.borrow().as_ref() == Some(&this);
                if is_current {
                    *ctx_dialog.borrow_mut() = None;
                }
            });
        }
        crate::ui::app_helpers::close_on_click_outside(&dialog);
        dialog.present(Some(root));
    }

    /// Rebuilds the open context/detail dialog in place (close + re-open) so a
    /// just-changed cover/photo shows immediately. No-op when none is open.
    pub(crate) fn refresh_context_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let Some(old) = self.nav.ctx_dialog.borrow_mut().take() else {
            return; // no detail dialog open → nothing to rebuild
        };
        old.close();
        self.open_context_menu(root, sender);
    }

    /// Shows/hides the detail dialog's remembered play row accordingly:
    /// hidden as long as exactly this track is playing; visible once it ends
    /// or is switched.
    pub(crate) fn refresh_ctx_play(&self) {
        if let Some((row, path)) = self.nav.ctx_play.borrow().as_ref() {
            row.set_visible(self.transport.playing_path.as_deref() != Some(path.as_path()));
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

    /// Context menu: play the current target (file/folder/album/artist).
    pub(crate) fn on_ctx_play(&mut self) {
        if let Some(entry) = self.nav.context_target.clone() {
            let files = self.ctx_files(&entry);
            if !files.is_empty() {
                self.transport.queue = files;
                self.transport.queue_pos = 0;
                self.play_current();
                self.refresh_queue_icons();
            }
        }
    }

    /// Context menu: play the target album in track order (shuffle off).
    pub(crate) fn on_ctx_play_album(&mut self) {
        // Album always in track order from song 1, without shuffle; at the end
        // of the queue `play_next` stops by itself (no further song).
        if let Some((artist, album)) = self.ctx_album() {
            let files = self.album_files(&artist, &album);
            if !files.is_empty() {
                self.transport.shuffle = false;
                self.transport.queue = files;
                self.transport.queue_pos = 0;
                self.play_current();
                self.refresh_queue_icons();
            }
        }
    }

    /// Context menu: play all tracks of the target artist, albums by year
    /// (newest or oldest first), each album top-down (shuffle off).
    pub(crate) fn on_ctx_play_artist(&mut self, newest_first: bool) {
        // Albums by year (oldest/newest first), each album top-down,
        // without shuffle.
        if let Some(name) = self.ctx_artist() {
            let files = self.artist_files_ordered(&name, newest_first);
            if !files.is_empty() {
                self.transport.shuffle = false;
                self.transport.queue = files;
                self.transport.queue_pos = 0;
                self.play_current();
                self.refresh_queue_icons();
            }
        }
    }

    /// Context menu: share the target over device sync (or open pairing first).
    pub(crate) fn on_ctx_share(&mut self, root: &adw::ApplicationWindow) {
        if let Some(target) = self.nav.context_target.clone() {
            self.share_items(self.ctx_share_selection(&target), root);
        } else if !self.sync_connected {
            self.share_items(crate::core::sync::share::Selection::default(), root);
        }
    }

    /// Shared entry point for every "Share" action (music detail view, plus the
    /// station/podcast/playlist/YouTube detail views): when paired, hand the
    /// selection to the SyncPage (size confirmation → send); otherwise open the
    /// pairing dialog so the user can connect first.
    pub(crate) fn share_items(
        &self,
        selection: crate::core::sync::share::Selection,
        root: &adw::ApplicationWindow,
    ) {
        use crate::ui::sync_page::SyncInput;
        if !self.sync_connected {
            self.sync_page.emit(SyncInput::Open(root.clone()));
            return;
        }
        if selection.is_empty() {
            self.toast(&gettext("Nothing here to share"));
            return;
        }
        self.sync_page.emit(SyncInput::ShareSelection {
            window: root.clone(),
            selection: Box::new(selection),
        });
    }

    /// Detail view's refresh button: force a fresh online fetch of the open
    /// target's cover/metadata, then rebuild the detail view from the current
    /// data (the old dialog was closed by the button).
    pub(crate) fn on_ctx_refresh(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let Some(target) = self.nav.context_target.clone() else {
            return;
        };
        // Let the galleries re-attempt this session and reset the failure
        // counters, so a previously failed online match is retried.
        self.libview.gallery_tried.borrow_mut().clear();
        match &target {
            CtxTarget::Artist(m) => {
                self.library.reset_artist_attempts(&m.name);
                self.fetch_focus_artist(sender, &m.name);
            }
            CtxTarget::Album(m) => {
                self.library.reset_album_attempts(&m.artist, &m.album);
                self.fetch_focus_album(sender, &m.artist, &m.album);
            }
            CtxTarget::Fs(e) => {
                if let Some((artist, album)) = self.fs_album(e) {
                    self.library.reset_album_attempts(&artist, &album);
                    self.fetch_focus_album(sender, &artist, &album);
                }
            }
        }
        self.run_local_covers(sender);
        self.toast(&gettext("Refreshing …"));
        self.open_context_menu(root, sender);
    }

    /// Context menu: append the target's tracks to the user queue.
    pub(crate) fn on_ctx_add_queue(&mut self) {
        if let Some(entry) = self.nav.context_target.clone() {
            let mut files = self.ctx_files(&entry);
            let n = files.len();
            // Explicit enqueue: append to the user queue, never the active
            // context. Playback is untouched; the tracks play next, ahead
            // of the rest of the running album.
            self.transport.user_queue.append(&mut files);
            self.reload_queue_list();
            self.refresh_queue_icons();
            self.save_queue();
            self.toast(&gettext_f(
                "Added {n} tracks to the queue",
                &[("n", &n.to_string())],
            ));
        }
    }
}
