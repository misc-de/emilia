//! Multi-source / remote (Files tabs): source tabs, the active remote source,
//! remote directory load + background tag fetch, offline key sets.
//! Split out of app_views.rs – pure reordering, no functional change.

use std::path::PathBuf;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::i18n::gettext;
use crate::model::Source;
use crate::ui::app::{ActiveSource, App, Cmd, Msg};
use crate::ui::fs_row::FsEntry;

impl App {
    /// Rebuilds the source tab bar: a "Music" tab for the primary directory plus
    /// one per additional source (a linked radio group, only when there is more
    /// than one folder), followed by a trailing "+" that is always present so a
    /// folder/Nextcloud can be added straight from the Files view. A tab click
    /// sends [`Msg::SelectSource`], the "+" sends [`Msg::AddSourceMenu`].
    pub(crate) fn rebuild_source_tabs(&mut self) {
        while let Some(c) = self.files.source_tabs.first_child() {
            self.files.source_tabs.remove(&c);
        }
        self.files.source_tab_buttons.clear();
        // The primary "Music" tab only exists when a music folder is configured;
        // without it a single extra source is the only folder there is.
        let primary_ok = self
            .files
            .music_dir
            .as_deref()
            .is_some_and(|d| !d.trim().is_empty());
        let mut tabs: Vec<(ActiveSource, String)> = Vec::new();
        if primary_ok {
            tabs.push((ActiveSource::Primary, gettext("Music")));
        }
        for s in &self.files.sources {
            tabs.push((ActiveSource::Source(s.id), s.name.clone()));
        }

        if !tabs.is_empty() {
            // Show the tab(s) even for a single folder (so the bar reads as a
            // proper tab menu, not just a lone "+"). The linked toggle group
            // lives in its own box so the "linked" styling does not join the
            // standalone "+" button to it.
            let group_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(6)
                .hexpand(true)
                .css_classes(["linked", "emilia-tabbar"])
                .build();
            let mut group: Option<gtk::ToggleButton> = None;
            for (sel, label) in tabs {
                // An ellipsizing label (long source names like "cloud.cais.de"
                // would otherwise force the whole window wider than a phone).
                let lbl = gtk::Label::new(Some(&label));
                lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
                lbl.set_max_width_chars(10);
                let btn = gtk::ToggleButton::new();
                btn.set_child(Some(&lbl));
                // Full width like the Podcast/Streaming tab switchers.
                btn.set_hexpand(true);
                match &group {
                    Some(g) => btn.set_group(Some(g)),
                    None => group = Some(btn.clone()),
                }
                // Set the active state BEFORE the handler is connected –
                // otherwise the preselection already triggers a (superfluous)
                // switch.
                btn.set_active(sel == self.files.active_source);
                let input = self.input.clone();
                let sel_cb = sel.clone();
                btn.connect_toggled(move |b| {
                    if b.is_active() {
                        let _ = input.send(Msg::SelectSource(sel_cb.clone()));
                    }
                });
                // Long-press (touch) and right-click (mouse) open the tab's
                // context menu (rename / edit / remove). Claiming the sequence
                // keeps the press from also toggling the tab.
                {
                    let input = self.input.clone();
                    let sel_lp = sel.clone();
                    let lp = gtk::GestureLongPress::new();
                    lp.connect_pressed(move |g, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        let _ = input.send(Msg::SourceContextMenu(sel_lp.clone()));
                    });
                    btn.add_controller(lp);
                }
                {
                    let input = self.input.clone();
                    let sel_rc = sel.clone();
                    let rc = gtk::GestureClick::new();
                    rc.set_button(gtk::gdk::BUTTON_SECONDARY);
                    rc.connect_pressed(move |g, _, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        let _ = input.send(Msg::SourceContextMenu(sel_rc.clone()));
                    });
                    btn.add_controller(rc);
                }
                group_box.append(&btn);
                self.files.source_tab_buttons.push((sel, btn));
            }
            self.files.source_tabs.append(&group_box);
        } else {
            // No folder at all → no toggles; a hexpanding filler keeps the "+"
            // on the right-hand edge of the tab bar.
            let filler = gtk::Box::builder().hexpand(true).build();
            self.files.source_tabs.append(&filler);
        }

        // Trailing "+": add a local folder or a Nextcloud. Always present.
        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text(gettext("Add folder or Nextcloud"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let input = self.input.clone();
        add.connect_clicked(move |_| {
            let _ = input.send(Msg::AddSourceMenu);
        });
        self.files.source_tabs.append(&add);
    }

    /// Whether the source toggle group is shown: whenever there is at least one
    /// folder (primary or source). Used for the small gap below the tab bar.
    /// (The tab bar itself is always visible for the "+".)
    pub(crate) fn source_tabs_visible(&self) -> bool {
        !self.files.source_tab_buttons.is_empty()
    }

    /// The "+" in the tab bar: ask whether to add a local folder or a Nextcloud.
    pub(crate) fn open_add_source_menu(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::AlertDialog::new(
            Some(&gettext("Add a source")),
            Some(&gettext(
                "Add a local folder or connect a Nextcloud as another tab.",
            )),
        );
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.add_response("folder", &gettext("Local folder"));
        dialog.add_response("nextcloud", &gettext("Nextcloud"));
        dialog.set_default_response(Some("folder"));
        dialog.set_close_response("cancel");
        {
            let sender = sender.clone();
            dialog.connect_response(None, move |_, resp| match resp {
                "folder" => sender.input(Msg::AddLocalFolder),
                "nextcloud" => sender.input(Msg::AddCloudSource),
                _ => {}
            });
        }
        dialog.present(Some(root));
    }

    /// Opens the native folder chooser and adds the picked directory as an extra
    /// local source. Shared by the Files "+" and the settings "Add local folder"
    /// button.
    pub(crate) fn add_local_folder_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let chooser = gtk::FileDialog::builder()
            .title(gettext("Choose folder"))
            .build();
        let sender = sender.clone();
        chooser.select_folder(Some(root), gtk::gio::Cancellable::NONE, move |res| {
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
                        match lib.add_source(&src) {
                            // Switch to the freshly added tab and show its folder.
                            Ok(id) => {
                                sender.input(Msg::SourceAdded(id));
                                return;
                            }
                            Err(e) => tracing::warn!("add local source failed: {e}"),
                        }
                    }
                    sender.input(Msg::SourcesChanged);
                }
            }
        });
    }

    /// Opens the context menu for a source tab (a popover anchored to that tab
    /// button): rename / edit (folder or music path) / remove. The primary
    /// "Music" tab is the main library folder, so it only offers "Change
    /// folder".
    pub(crate) fn open_source_context_menu(
        &self,
        sel: ActiveSource,
        sender: &ComponentSender<Self>,
    ) {
        let Some((_, anchor)) = self
            .files
            .source_tab_buttons
            .iter()
            .find(|(s, _)| *s == sel)
        else {
            return;
        };
        let pop = gtk::Popover::builder().autohide(true).build();
        pop.set_parent(anchor);
        let menu = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .build();
        // A flat, left-aligned menu entry.
        let item = |label: &str, destructive: bool| {
            let b = gtk::Button::builder().label(label).build();
            b.add_css_class("flat");
            if destructive {
                b.add_css_class("destructive-action");
            }
            if let Some(w) = b.child() {
                if let Ok(lbl) = w.downcast::<gtk::Label>() {
                    lbl.set_xalign(0.0);
                }
            }
            b
        };
        match sel {
            ActiveSource::Primary => {
                let edit = item(&gettext("Change folder"), false);
                let sender = sender.clone();
                let pop2 = pop.downgrade();
                edit.connect_clicked(move |_| {
                    if let Some(p) = pop2.upgrade() {
                        p.popdown();
                    }
                    sender.input(Msg::SourceEdit(ActiveSource::Primary));
                });
                menu.append(&edit);
            }
            ActiveSource::Source(id) => {
                let rename = item(&gettext("Rename"), false);
                {
                    let sender = sender.clone();
                    let p = pop.downgrade();
                    rename.connect_clicked(move |_| {
                        if let Some(p) = p.upgrade() {
                            p.popdown();
                        }
                        sender.input(Msg::SourceRename(id));
                    });
                }
                menu.append(&rename);

                let is_webdav = self
                    .files
                    .sources
                    .iter()
                    .any(|s| s.id == id && s.kind == "webdav");
                let edit_label = if is_webdav {
                    gettext("Change music folder")
                } else {
                    gettext("Change folder")
                };
                let edit = item(&edit_label, false);
                {
                    let sender = sender.clone();
                    let p = pop.downgrade();
                    edit.connect_clicked(move |_| {
                        if let Some(p) = p.upgrade() {
                            p.popdown();
                        }
                        sender.input(Msg::SourceEdit(ActiveSource::Source(id)));
                    });
                }
                menu.append(&edit);

                let del = item(&gettext("Remove"), true);
                {
                    let sender = sender.clone();
                    let p = pop.downgrade();
                    del.connect_clicked(move |_| {
                        if let Some(p) = p.upgrade() {
                            p.popdown();
                        }
                        sender.input(Msg::SourceDelete(id));
                    });
                }
                menu.append(&del);
            }
        }
        pop.set_child(Some(&menu));
        pop.connect_closed(|p| p.unparent());
        pop.popup();
    }

    /// Rename dialog for a source (an entry pre-filled with the current name).
    pub(crate) fn open_source_rename_dialog(
        &self,
        id: i64,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let current = self
            .files
            .sources
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        let dialog = adw::AlertDialog::new(Some(&gettext("Rename source")), None);
        let entry = gtk::Entry::builder()
            .text(&current)
            .activates_default(true)
            .build();
        crate::ui::widgets::no_autofocus(&entry);
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[
            ("cancel", &gettext("Cancel")),
            ("rename", &gettext("Rename")),
        ]);
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");
        {
            let sender = sender.clone();
            dialog.connect_response(None, move |_, resp| {
                if resp == "rename" {
                    sender.input(Msg::SourceRenameDo {
                        id,
                        name: entry.text().to_string(),
                    });
                }
            });
        }
        dialog.present(Some(root));
    }

    /// "Edit" a tab: change the folder (local source / primary music folder) via
    /// the folder chooser, or the indexed music subpath of a WebDAV source via a
    /// small text dialog.
    pub(crate) fn open_source_edit(
        &self,
        sel: ActiveSource,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        match sel {
            ActiveSource::Primary => {
                let chooser = gtk::FileDialog::builder()
                    .title(gettext("Choose music folder"))
                    .build();
                let sender = sender.clone();
                chooser.select_folder(Some(root), gtk::gio::Cancellable::NONE, move |res| {
                    if let Ok(f) = res {
                        if let Some(p) = f.path() {
                            sender.input(Msg::SetMusicDir(p));
                        }
                    }
                });
            }
            ActiveSource::Source(id) => {
                let Some(src) = self.files.sources.iter().find(|s| s.id == id).cloned() else {
                    return;
                };
                if src.kind == "webdav" {
                    let dialog = adw::AlertDialog::new(
                        Some(&gettext("Change music folder")),
                        Some(&gettext("Subfolder on the server to index, e.g. /Music.")),
                    );
                    let entry = gtk::Entry::builder()
                        .text(src.music_path.as_deref().unwrap_or(""))
                        .activates_default(true)
                        .build();
                    crate::ui::widgets::no_autofocus(&entry);
                    dialog.set_extra_child(Some(&entry));
                    dialog.add_responses(&[
                        ("cancel", &gettext("Cancel")),
                        ("save", &gettext("Save")),
                    ]);
                    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
                    dialog.set_default_response(Some("save"));
                    dialog.set_close_response("cancel");
                    let sender = sender.clone();
                    dialog.connect_response(None, move |_, resp| {
                        if resp == "save" {
                            sender.input(Msg::SourceSetMusicPath {
                                id,
                                path: entry.text().to_string(),
                            });
                        }
                    });
                    dialog.present(Some(root));
                } else {
                    let chooser = gtk::FileDialog::builder()
                        .title(gettext("Choose folder"))
                        .build();
                    let sender = sender.clone();
                    chooser.select_folder(Some(root), gtk::gio::Cancellable::NONE, move |res| {
                        if let Ok(f) = res {
                            if let Some(p) = f.path() {
                                sender.input(Msg::SourceSetPath { id, path: p });
                            }
                        }
                    });
                }
            }
        }
    }

    /// Applies a new root path to a local source and reloads it if it is active.
    pub(crate) fn on_source_set_path(
        &mut self,
        id: i64,
        path: PathBuf,
        sender: &ComponentSender<Self>,
    ) {
        let _ = self.library.set_source_path(id, &path.to_string_lossy());
        self.on_sources_changed(sender);
        if self.files.active_source == ActiveSource::Source(id) {
            self.apply_source(ActiveSource::Source(id), sender);
        }
    }

    /// Applies a new music subpath to a WebDAV source: persist it, drop the old
    /// indexed tracks and re-index in the background, then reload it if active.
    pub(crate) fn on_source_set_music_path(
        &mut self,
        id: i64,
        path: String,
        sender: &ComponentSender<Self>,
    ) {
        let p = path.trim().trim_end_matches('/');
        let normalized = if p.is_empty() {
            String::new()
        } else if p.starts_with('/') {
            p.to_string()
        } else {
            format!("/{p}")
        };
        let _ = self.library.set_source_music_path(id, &normalized);
        self.on_sources_changed(sender);
        sender.spawn_oneshot_command(move || {
            if let Ok(lib) = Library::open() {
                let _ = lib.clear_source_tracks(id);
                if let Some(src) = lib
                    .list_sources()
                    .ok()
                    .and_then(|v| v.into_iter().find(|s| s.id == id))
                {
                    match crate::core::webdav::index_into(&lib, &src) {
                        Ok(n) => tracing::info!("Re-indexed {n} tracks after music path change"),
                        Err(e) => tracing::warn!("Re-index after music path change failed: {e}"),
                    }
                }
            }
            Cmd::CloudReindexed { manual: false }
        });
        if self.files.active_source == ActiveSource::Source(id) {
            self.apply_source(ActiveSource::Source(id), sender);
        }
    }

    /// The source to switch to when the current `active_source` is no longer
    /// valid (its tab was dropped or the source was removed): the primary
    /// "Music" folder if configured, otherwise the first extra source. Returns
    /// `None` when the current selection is still fine.
    pub(crate) fn active_source_fallback(&self) -> Option<ActiveSource> {
        let primary_ok = self
            .files
            .music_dir
            .as_deref()
            .is_some_and(|d| !d.trim().is_empty());
        let valid = match &self.files.active_source {
            ActiveSource::Primary => primary_ok,
            ActiveSource::Source(id) => self.files.sources.iter().any(|s| s.id == *id),
        };
        if valid {
            return None;
        }
        if primary_ok {
            Some(ActiveSource::Primary)
        } else {
            self.files
                .sources
                .first()
                .map(|s| ActiveSource::Source(s.id))
                .or(Some(ActiveSource::Primary))
        }
    }

    /// Switches the active source, re-roots the file view accordingly and
    /// reloads the folder. Also mirrors the active state of the tabs.
    pub(crate) fn apply_source(&mut self, sel: ActiveSource, sender: &ComponentSender<Self>) {
        self.files.active_source = sel.clone();
        match &sel {
            ActiveSource::Primary => {
                self.files.root_dir = self.files.music_dir.as_ref().map(PathBuf::from);
                self.files.browse_dir = self.files.root_dir.clone();
            }
            ActiveSource::Source(id) => {
                if let Some(s) = self.files.sources.iter().find(|s| s.id == *id) {
                    match s.kind.as_str() {
                        "local" => {
                            let p = s.path.clone().map(PathBuf::from);
                            self.files.root_dir = p.clone();
                            self.files.browse_dir = p;
                            self.files.remote_browse = None;
                        }
                        // WebDAV: local paths empty, remote browser at the root.
                        _ => {
                            self.files.root_dir = None;
                            self.files.browse_dir = None;
                            self.files.remote_browse = Some(String::new());
                        }
                    }
                }
            }
        }
        if !matches!(self.files.active_source, ActiveSource::Source(_)) {
            self.files.remote_browse = None;
        }
        self.sync_source_tabs();
        self.load_dir(sender);
    }

    /// The active source as a WebDAV source (if it is one).
    pub(crate) fn active_remote_source(&self) -> Option<crate::model::Source> {
        let ActiveSource::Source(id) = self.files.active_source else {
            return None;
        };
        self.files
            .sources
            .iter()
            .find(|s| s.id == id && s.kind == "webdav")
            .cloned()
    }

    /// Loads a folder of the active remote source (PROPFIND in the background).
    pub(crate) fn load_remote_dir(
        &mut self,
        sender: &ComponentSender<Self>,
        source: crate::model::Source,
    ) {
        let rel = self.files.remote_browse.clone().unwrap_or_default();
        let Some(creds) = crate::core::webdav::Creds::from_source(&source) else {
            self.libview.entries.guard().clear();
            self.libview.loading = false;
            self.files.remote_error = Some(gettext(
                "This source is not configured correctly (URL/login missing).",
            ));
            return;
        };
        // Clear any previous error for this fresh attempt.
        self.files.remote_error = None;
        self.libview.loading = true;
        let active = self.files.active_source.clone();
        sender.spawn_oneshot_command(move || {
            let res = crate::core::webdav::list(&creds, &rel).map_err(|e| e.to_string());
            Cmd::RemoteEntries(res, active, rel)
        });
    }

    /// Fetches title/artist/duration of the still untagged remote files of the
    /// current folder in the background (range GET, capped at 40) and
    /// reports each read file individually as [`Cmd::RemoteTags`] – so the
    /// rows fill up gradually instead of only at the end.
    pub(crate) fn start_remote_tag_fetch(
        &mut self,
        sender: &ComponentSender<Self>,
        source: &crate::model::Source,
    ) {
        let Some(creds) = crate::core::webdav::Creds::from_source(source) else {
            return;
        };
        let rels: Vec<String> = {
            let guard = self.libview.entries.guard();
            (0..guard.len())
                .filter_map(|i| {
                    guard.get(i).and_then(|r| match &r.entry {
                        // Only files whose tags are not already known from the DB
                        // (title still empty) need a network read.
                        FsEntry::RemoteFile {
                            rel_path,
                            title: None,
                            downloaded: None,
                            ..
                        } => Some(rel_path.clone()),
                        _ => None,
                    })
                })
                .take(40)
                .collect()
        };
        if rels.is_empty() {
            return;
        }
        sender.spawn_command(move |out| {
            for r in rels {
                let (t, a, d) = crate::core::webdav::read_tags(&creds, &r);
                if t.is_some() || a.is_some() || d.is_some() {
                    let _ = out.send(Cmd::RemoteTags(vec![(r, t, a, d)]));
                }
            }
        });
    }

    /// Mirrors the active state of the tab buttons onto `active_source`.
    pub(crate) fn sync_source_tabs(&self) {
        for (sel, btn) in &self.files.source_tab_buttons {
            let want = *sel == self.files.active_source;
            if btn.is_active() != want {
                btn.set_active(want);
            }
        }
    }

    /// Loads the albums overview from the DB into the factory (incl. online cover).
    /// (artist, album) keys whose source is currently offline.
    pub(crate) fn offline_album_keys(&self) -> std::collections::HashSet<(String, String)> {
        let mut out = std::collections::HashSet::new();
        for &id in &self.offline_sources {
            if let Ok(pairs) = self.library.remote_album_keys(id) {
                out.extend(pairs);
            }
        }
        out
    }

    /// Artist names (lowercased) whose source is currently offline.
    pub(crate) fn offline_artist_names_lc(&self) -> Vec<String> {
        let mut out = Vec::new();
        for &id in &self.offline_sources {
            if let Ok(names) = self.library.remote_artists(id) {
                out.extend(names.into_iter().map(|s| s.to_lowercase()));
            }
        }
        out
    }

    /// Is this track path from a currently disconnected source?
    pub(crate) fn is_offline_path(&self, path: &str) -> bool {
        crate::core::webdav::parse_nc_path(path)
            .map(|(id, _)| self.offline_sources.contains(&id))
            .unwrap_or(false)
    }
}
