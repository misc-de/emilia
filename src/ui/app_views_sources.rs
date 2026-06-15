//! Multi-source / remote (Files tabs): source tabs, the active remote source,
//! remote directory load + background tag fetch, offline key sets.
//! Split out of app_views.rs – pure reordering, no functional change.

use std::path::PathBuf;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::gettext;
use crate::ui::app::{ActiveSource, App, Cmd, Msg};
use crate::ui::fs_row::FsEntry;

impl App {
    /// Rebuilds the source tab bar: a "Music" tab for the primary
    /// directory plus one per additional source. The buttons are connected as a
    /// radio group; a click sends [`Msg::SelectSource`].
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
        // Only one folder → no tab bar (nothing to switch between).
        if tabs.len() <= 1 {
            return;
        }
        let mut group: Option<gtk::ToggleButton> = None;
        for (sel, label) in tabs {
            // An ellipsizing label (long source names like "cloud.cais.de" would
            // otherwise force the whole window wider than a phone screen).
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
            // Set the active state BEFORE the handler is connected – otherwise
            // the preselection already triggers a (superfluous) switch.
            btn.set_active(sel == self.files.active_source);
            let input = self.input.clone();
            let sel_cb = sel.clone();
            btn.connect_toggled(move |b| {
                if b.is_active() {
                    let _ = input.send(Msg::SelectSource(sel_cb.clone()));
                }
            });
            self.files.source_tabs.append(&btn);
            self.files.source_tab_buttons.push((sel, btn));
        }
    }

    /// Whether the source tab bar should be shown: only when there is more than
    /// one folder (primary + sources) to switch between.
    pub(crate) fn source_tabs_visible(&self) -> bool {
        self.files.source_tab_buttons.len() > 1
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
