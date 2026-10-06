//! Views and data helpers: load and group folder/album/artist, build the
//! subpages (artist → albums → tracks), plus the context/detail
//! helpers (ctx_*) and cover resolution. Extracted from app.rs – pure
//! reorganization, no functional change.

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::scanner;
use crate::i18n::gettext;
use crate::ui::app::{App, Cmd, Msg, artist_count_subtitle, find_scroller, read_entries};
use crate::ui::card_list::CardItem;
use crate::ui::enrich::enrich_worker;

pub(crate) use crate::ui::app_views_album::{
    AlbumPageRef, most_common_album_base, natural_key, track_disc,
};

/// Maps an album overview to the rows of a virtualised [`CardList`]: the same
/// title/subtitle/cover the old `AlbumCard` factory rendered, plus the offline
/// badge for albums whose source is currently unreachable.
fn album_cards(
    albums: &[crate::model::AlbumMeta],
    offline_keys: &std::collections::HashSet<(String, String)>,
) -> Vec<CardItem> {
    albums
        .iter()
        .map(|a| CardItem {
            title: a.album.clone(),
            subtitle: album_card_subtitle(a),
            image: a.cover_path.clone(),
            offline: offline_keys.contains(&(a.artist.clone(), a.album.clone())),
            // Runtime + play button on the right, as in the file list. The
            // running album is marked by its card key (name + display artist).
            duration_ms: a.total_duration_ms.unwrap_or(0),
            play_key: Some(crate::core::album_group::card_key(&a.album, &a.artist)),
        })
        .collect()
}

/// Album subtitle: "Artist · Year · N songs" (whichever parts are known).
/// Kept byte-for-byte as the old `album_row` factory rendered it, so the
/// virtualised list reads exactly like the one it replaced.
fn album_card_subtitle(m: &crate::model::AlbumMeta) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !m.artist.is_empty() {
        parts.push(m.artist.clone());
    }
    if let Some(year) = m.year {
        parts.push(year.to_string());
    }
    if m.track_count > 0 {
        parts.push(crate::i18n::ngettext_n(
            "{n} song",
            "{n} songs",
            m.track_count as u32,
        ));
    }
    parts.join(" · ")
}

impl App {
    /// Scroller of the file list (ancestor of the entries `ListBox`).
    pub(crate) fn fs_scroller(&self) -> Option<gtk::ScrolledWindow> {
        self.libview
            .entries
            .widget()
            .ancestor(gtk::ScrolledWindow::static_type())
            .and_downcast::<gtk::ScrolledWindow>()
    }

    /// Starts reading the current folder in the background (with spinner).
    pub(crate) fn load_dir(&mut self, sender: &ComponentSender<Self>) {
        // Remote source? → dedicated WebDAV browser (PROPFIND), not the local FS.
        if let Some(source) = self.active_remote_source() {
            self.load_remote_dir(sender, source);
            return;
        }
        // Local folder → no remote error applies (clear a stale one from before).
        self.files.remote_error = None;
        // Remember the scroll position of the currently shown folder before it is replaced.
        if let (Some(dir), Some(sc)) = (self.files.shown_dir.clone(), self.fs_scroller()) {
            self.files
                .fs_scroll
                .borrow_mut()
                .insert(dir, sc.vadjustment().value());
        }
        match self.files.browse_dir.clone() {
            Some(dir) => {
                // Remember the current folder (for "continue where you left off").
                let _ = self
                    .library
                    .set_setting("browse_dir", &dir.to_string_lossy());
                self.libview.loading = true;
                sender.spawn_oneshot_command(move || Cmd::Entries(read_entries(dir)));
            }
            None => {
                self.libview.entries.guard().clear();
                self.libview.loading = false;
            }
        }
    }

    pub(crate) fn reload_albums(&mut self) {
        let snap = self.library.category_snapshot().ok();
        self.reload_albums_with(snap.as_ref());
    }

    /// Reloads both the album and artist overviews, building the shared category
    /// snapshot only once instead of once per overview (they're almost always
    /// reloaded together).
    pub(crate) fn reload_library_overviews(&mut self) {
        let snap = self.library.category_snapshot().ok();
        self.reload_albums_with(snap.as_ref());
        self.reload_artists_with(snap.as_ref());
        self.reload_singles_with(snap.as_ref());
        self.reload_compilations_with(snap.as_ref());
    }

    pub(crate) fn reload_singles(&mut self) {
        let snap = self.library.category_snapshot().ok();
        self.reload_singles_with(snap.as_ref());
    }

    pub(crate) fn reload_singles_with(&mut self, snap: Option<&crate::core::db::CategorySnapshot>) {
        self.reload_kind_with(crate::core::category::Area::Singles, "singles", snap);
    }

    pub(crate) fn reload_compilations(&mut self) {
        let snap = self.library.category_snapshot().ok();
        self.reload_compilations_with(snap.as_ref());
    }

    pub(crate) fn reload_compilations_with(
        &mut self,
        snap: Option<&crate::core::db::CategorySnapshot>,
    ) {
        self.reload_kind_with(
            crate::core::category::Area::Compilations,
            "compilations",
            snap,
        );
    }

    /// Fills in the album covers the overview query left empty (no artist on
    /// the card has a stored cover): a stored cover of the same title by the
    /// card's primary artist, otherwise the first embedded/cached track cover
    /// among the card's own tracks. Never a same-titled album of a foreign
    /// artist — keyed by name alone, that put e.g. Gorillaz' "Greatest Hits"
    /// cover on Queen's. Only the coverless cards pay these lookups.
    fn resolve_album_covers(&self, albums: &mut [crate::model::AlbumMeta]) {
        for album in albums.iter_mut() {
            if album
                .cover_path
                .as_deref()
                .is_some_and(|p| !p.trim().is_empty())
            {
                continue;
            }
            album.cover_path = self
                .library
                .album_cover_related(&album.artist, &album.album)
                .ok()
                .flatten()
                .or_else(|| {
                    self.library
                        .album_card_tracks(&album.artist, &album.album)
                        .unwrap_or_default()
                        .iter()
                        .find_map(|t| crate::core::online::local_track_cover(&t.path))
                });
        }
    }

    /// Shared reload for the Singles / Compilations pages — mirrors
    /// [`Self::reload_albums_with`] but pulls the albums filed in the section's
    /// own [`Area`] (kind-aware default + any "Available in" override) and writes
    /// to the section's own factory/overview/headers (chosen by `section`).
    fn reload_kind_with(
        &mut self,
        area: crate::core::category::Area,
        section: &'static str,
        snap: Option<&crate::core::db::CategorySnapshot>,
    ) {
        let singles = section == "singles";
        let mut albums = self
            .library
            .albums_overview_in_area(area, snap)
            .unwrap_or_default();
        self.resolve_album_covers(&mut albums);
        self.sort_album_metas(section, &mut albums);
        let headers = self.album_meta_headers(section, &albums);
        let icon = if singles {
            "audio-x-generic-symbolic"
        } else {
            "view-grid-symbolic"
        };
        let (show_tracks, show_detail): (fn(usize) -> Msg, fn(usize) -> Msg) = if singles {
            (Msg::ShowSingleTracks, Msg::ShowSingleDetail)
        } else {
            (Msg::ShowCompilationTracks, Msg::ShowCompilationDetail)
        };
        if singles {
            self.libview.single_count = albums.len();
            self.libview.singles_overview = albums.clone();
            *self.libview.single_headers.borrow_mut() = headers.clone();
        } else {
            self.libview.compilation_count = albums.len();
            self.libview.compilations_overview = albums.clone();
            *self.libview.compilation_headers.borrow_mut() = headers.clone();
        }
        if self.libview.gallery_on(section) {
            let items: Vec<(Option<String>, &'static str, String)> = albums
                .iter()
                .map(|a| (a.cover_path.clone(), icon, a.album.clone()))
                .collect();
            let (gbox, gal) = if singles {
                (
                    &self.libview.singles_gallery_box,
                    &self.libview.singles_gallery,
                )
            } else {
                (
                    &self.libview.compilations_gallery_box,
                    &self.libview.compilations_gallery,
                )
            };
            self.fill_sectioned_gallery(
                gbox,
                gal,
                &items,
                headers.as_deref(),
                show_tracks,
                show_detail,
                self.libview.gallery_desc_on(section),
            );
        } else {
            let offline_keys = self.offline_album_keys();
            let items = album_cards(&albums, &offline_keys);
            let list = if singles {
                &self.libview.singles
            } else {
                &self.libview.compilations
            };
            list.set_items(items, headers);
        }
    }

    pub(crate) fn reload_albums_with(&mut self, snap: Option<&crate::core::db::CategorySnapshot>) {
        let mut albums = self.library.albums_overview_with(snap).unwrap_or_default();
        self.resolve_album_covers(&mut albums);
        // Apply the chosen sort order (criterion + direction). The DB already
        // returns the albums by name; here we re-order to match the user's pick.
        self.sort_albums(&mut albums);
        self.libview.album_count = albums.len();
        // Mirror the overview so that gallery clicks (the factory is empty then) can
        // resolve the entry by index.
        self.libview.albums_overview = albums.clone();
        // Per-row section headings for the chosen sort (alphabetical by name,
        // year by date, none otherwise) – shared by the list and the gallery.
        let headers = self.album_section_headers(&albums);
        *self.libview.album_headers.borrow_mut() = headers.clone();
        let offline_keys = self.offline_album_keys();
        if self.libview.gallery_on("albums") {
            let items: Vec<(Option<String>, &'static str, String)> = albums
                .iter()
                .map(|a| {
                    (
                        a.cover_path.clone(),
                        "media-optical-symbolic",
                        a.album.clone(),
                    )
                })
                .collect();
            self.fill_sectioned_gallery(
                &self.libview.albums_gallery_box,
                &self.libview.albums_gallery,
                &items,
                headers.as_deref(),
                Msg::ShowAlbumTracks,
                Msg::ShowAlbumDetail,
                self.libview.gallery_desc_on("albums"),
            );
        } else {
            self.libview
                .albums
                .set_items(album_cards(&albums, &offline_keys), headers);
        }
    }

    /// Reads the library (tags → DB) **in the background** – purely local, without
    /// network. `then_enrich`: afterwards optionally auto-fetch online (the
    /// `ScanDone` handler decides based on the switch + connection). `manual`:
    /// the scan is part of a user-triggered refresh (drives the refresh spinner).
    /// Returns `true` if a worker was actually spawned (i.e. a music folder is set).
    pub(crate) fn start_scan(
        &mut self,
        sender: &ComponentSender<Self>,
        then_enrich: bool,
        manual: bool,
    ) -> bool {
        // Deliberately the **primary** music directory (not `root_dir`, which
        // switches when changing to an additional source) – library/scan stay on
        // the main folder.
        let Some(root) = self.files.music_dir.as_ref().map(PathBuf::from) else {
            return false;
        };
        // Show the import progress overlay (spinner + progress bar + "Cancel")
        // while the potentially slow tag scan runs — for the automatic first
        // import *and* a manual rescan, so the user always sees how far along it
        // is instead of a frozen-looking window. Reset the counters and the
        // cancel flag for this run.
        self.scanning = true;
        self.scan_done = 0;
        self.scan_total = 0;
        self.scan_bytes = 0;
        self.scan_total_bytes = 0;
        self.scan_cancel.store(false, Ordering::Relaxed);
        let cancel = self.scan_cancel.clone();
        sender.spawn_command(move |out| {
            match Library::open() {
                Ok(lib) => {
                    let r = scanner::scan_into_progress(
                        &lib,
                        &root,
                        &cancel,
                        |done, total, bytes, total_bytes| {
                            let _ = out.send(Cmd::ScanProgress {
                                done,
                                total,
                                bytes,
                                total_bytes,
                            });
                        },
                    );
                    if let Err(e) = r {
                        tracing::warn!("Library scan failed: {e}");
                    }
                }
                Err(e) => tracing::error!("Database unavailable for scan: {e}"),
            }
            let _ = out.send(Cmd::ScanDone {
                then_enrich,
                manual,
            });
        });
        true
    }

    /// Starts online enrichment in the background. `scan_first`: read the tags
    /// beforehand (on manual fetch) – on the automatic run this is skipped,
    /// because the local scan already ran. The audio files are only
    /// read, never modified. Permanently unsuccessful entries (≥ 3 attempts)
    /// are skipped in both cases.
    /// `light`: quiet background top-up (periodic) – only artist photos &
    /// online cover. The fetch generally runs without a visible progress indicator.
    /// See [`enrich_worker`].
    pub(crate) fn run_enrich(
        &mut self,
        sender: &ComponentSender<Self>,
        scan_first: bool,
        light: bool,
    ) {
        // Enrichment refers to the primary library (`music_dir`),
        // regardless of which source is currently active in the file view.
        let Some(root) = self.files.music_dir.as_ref().map(PathBuf::from) else {
            if !light {
                self.toast(&gettext(
                    "No music folder set – please choose one in the settings",
                ));
            }
            return;
        };
        if self.enrich_state.enriching {
            return;
        }
        self.enrich_state
            .enrich_cancel
            .store(false, Ordering::Relaxed);
        let cancel = self.enrich_state.enrich_cancel.clone();
        self.enrich_state.enriching = true;
        sender.spawn_command(move |out| enrich_worker(root, cancel, scan_first, light, &out));
    }

    /// Re-indexes all Nextcloud/WebDAV sources in the background. Existing
    /// sources are only indexed when first added, so this is the way to pull
    /// newly added remote tracks (and their embedded covers) into the library
    /// afterwards. On completion [`Cmd::CloudReindexed`] rebuilds the views and
    /// fetches covers/photos. `manual` = triggered by the refresh button (force
    /// online enrichment); `false` = silent background top-up (e.g. at startup),
    /// which respects the passive auto-enrich setting. Returns `true` if a worker
    /// was actually spawned (i.e. at least one WebDAV source exists).
    pub(crate) fn reindex_cloud_sources(
        &mut self,
        sender: &ComponentSender<Self>,
        manual: bool,
    ) -> bool {
        let sources: Vec<crate::model::Source> = self
            .files
            .sources
            .iter()
            .filter(|s| s.is_remote())
            .cloned()
            .collect();
        if sources.is_empty() {
            return false;
        }
        sender.spawn_oneshot_command(move || {
            if let Ok(lib) = crate::core::db::Library::open() {
                for s in &sources {
                    match crate::core::remote::index_into(&lib, s) {
                        Ok(n) => tracing::info!("Re-indexed {n} tracks from '{}'", s.name),
                        Err(e) => tracing::warn!("Re-index of '{}' failed: {e}", s.name),
                    }
                }
            }
            Cmd::CloudReindexed { manual }
        });
        true
    }

    /// Loads the artists overview from the DB into the factory (incl. photo).
    /// If the artist photo is missing, an album cover is used as a substitute.
    pub(crate) fn reload_artists(&mut self) {
        let snap = self.library.category_snapshot().ok();
        self.reload_artists_with(snap.as_ref());
    }

    pub(crate) fn reload_artists_with(&mut self, snap: Option<&crate::core::db::CategorySnapshot>) {
        let mut artists = self.library.artists_overview_with(snap).unwrap_or_default();
        self.libview.artist_count = artists.len();
        // Fallback cover (an album cover) for artists **without** their own photo.
        // Build the album assignment in ONE pass over `all_tracks` –
        // previously this called `artist_album_cover` → `all_tracks` per artist
        // (O(artists×tracks); dominated startup noticeably).
        if artists
            .iter()
            .any(|a| a.image_path.as_deref().is_none_or(|p| p.trim().is_empty()))
        {
            use crate::core::artist::{norm_key, split_artists};
            let mut first_album: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for t in self.library.all_tracks().unwrap_or_default() {
                let (Some(artist), Some(album)) = (t.artist.as_deref(), t.album.as_deref()) else {
                    continue;
                };
                if album.trim().is_empty() {
                    continue;
                }
                for s in split_artists(artist) {
                    first_album
                        .entry(norm_key(&s))
                        .or_insert_with(|| album.to_string());
                }
            }
            for a in &mut artists {
                if a.image_path.as_deref().is_none_or(|p| p.trim().is_empty())
                    && let Some(album) = first_album.get(&norm_key(&a.name))
                {
                    a.image_path = self.album_cover_for(&a.name, album);
                }
            }
        }
        // Apply the section's chosen sort (criterion + direction).
        self.sort_artists(&mut artists);
        // Mirror the overview (for gallery index resolution, see reload_albums).
        self.libview.artists_overview = artists.clone();
        // Alphabetical section headings when sorting by name (shared list/gallery).
        let headers = self.artist_section_headers(&artists);
        *self.libview.artist_headers.borrow_mut() = headers.clone();
        if self.libview.gallery_on("artists") {
            let items: Vec<(Option<String>, &'static str, String)> = artists
                .iter()
                .map(|a| {
                    (
                        a.image_path.clone(),
                        "avatar-default-symbolic",
                        a.name.clone(),
                    )
                })
                .collect();
            self.fill_sectioned_gallery(
                &self.libview.artists_gallery_box,
                &self.libview.artists_gallery,
                &items,
                headers.as_deref(),
                Msg::OpenArtistTracks,
                Msg::ShowArtistDetail,
                self.libview.gallery_desc_on("artists"),
            );
        } else {
            let offline_names = self.offline_artist_names_lc();
            // Album/song counts for the secondary line, fetched in one pass.
            let counts = self.library.artist_counts().unwrap_or_default();
            let items: Vec<CardItem> = artists
                .iter()
                .map(|a| {
                    let name_lc = a.name.to_lowercase();
                    let (albums, songs) = counts
                        .get(&crate::core::artist::norm_key(&a.name))
                        .copied()
                        .unwrap_or((0, 0));
                    CardItem {
                        title: a.name.clone(),
                        subtitle: artist_count_subtitle(albums, songs),
                        image: a.image_path.clone(),
                        offline: offline_names.iter().any(|n| n.contains(&name_lc)),
                        // An artist is opened, not played from the overview →
                        // neither a runtime nor a play button on the row.
                        duration_ms: 0,
                        play_key: None,
                    }
                })
                .collect();
            self.libview.artists.set_items(items, headers);
        }
    }

    /// Wraps a content into a scrollable subpage (with header bar +
    /// back arrow) and pushes it onto the navigation stack.
    pub(crate) fn push_subpage(&self, title: &str, content: &gtk::Box) {
        self.push_subpage_inner(title, content, true);
    }

    /// Like [`Self::push_subpage`] but without the swipe-back gesture, for
    /// subpages that need their own horizontal drags (e.g. the waveform editor).
    pub(crate) fn push_subpage_fixed(&self, title: &str, content: &gtk::Box) {
        self.push_subpage_inner(title, content, false);
    }

    /// Wraps content that brings its own scrolling (e.g. a stack of
    /// `AdwPreferencesPage`s) into a subpage: no outer scroller — a second one
    /// would let the inner page grow without bound — and no swipe-back, since
    /// such pages carry horizontal drags of their own (the settings sliders).
    /// `tag` marks the page so it can be recognised later; the pushed page is
    /// returned so the caller can hook into it.
    pub(crate) fn push_subpage_self_scrolling(
        &self,
        title: &str,
        tag: &str,
        content: &gtk::Box,
    ) -> adw::NavigationPage {
        self.remember_overview_scroll();
        let page = adw::NavigationPage::builder()
            .title(title)
            .tag(tag)
            .child(content)
            .build();
        self.nav.nav_view.push(&page);
        page
    }

    /// When leaving the root overview, remembers the current scroll position of
    /// the visible section (restored when returning).
    fn remember_overview_scroll(&self) {
        let leaving_root = self
            .nav
            .nav_view
            .visible_page()
            .and_then(|p| p.tag())
            .is_some_and(|t| t == "main");
        if leaving_root
            && let Some(sc) = self
                .nav
                .view_stack
                .visible_child()
                .and_then(|c| find_scroller(&c))
        {
            let value = sc.vadjustment().value();
            *self.nav.overview_scroll.borrow_mut() = Some((sc, value));
        }
    }

    fn push_subpage_inner(&self, title: &str, content: &gtk::Box, swipe_back: bool) {
        self.remember_overview_scroll();

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(content)
            .build();
        // Swipe right anywhere on the subpage to go back — capture phase, so it
        // also works when the swipe starts on a list row or a cover (the click
        // handlers no longer swallow it). Skipped for subpages that need their
        // own horizontal drags.
        if swipe_back {
            let nav = self.nav.nav_view.clone();
            crate::ui::app::attach_swipe_back(
                &scroller,
                || true,
                move || {
                    nav.pop();
                },
            );
        }
        // No own header: the shared header above the NavigationView provides the
        // back arrow + title (so the top/bottom navigation stays visible).
        let page = adw::NavigationPage::builder()
            .title(title)
            .child(&scroller)
            .build();
        self.nav.nav_view.push(&page);
    }

    pub(crate) fn toast(&self, _msg: &str) {
        // On-screen messages at the bottom edge are disabled by request –
        // deliberately a no-op (the calls remain, easily reactivatable).
    }

    /// Shows a short bottom toast for a delete/remove with an "Undo" button. The
    /// real action (`action`, the actual deletion message) is **deferred**: it
    /// runs only when the toast is dismissed *without* the user pressing Undo
    /// (i.e. after the 2 s timeout). Pressing Undo cancels it. This is the one
    /// place toasts are (re)enabled – informational `toast()` stays a no-op.
    pub(crate) fn undo_toast(&self, sender: &ComponentSender<Self>, msg: &str, action: Msg) {
        let toast = adw::Toast::new(msg);
        toast.set_button_label(Some(&gettext("Undo")));
        toast.set_timeout(2);
        let undone = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let undone = undone.clone();
            toast.connect_button_clicked(move |_| undone.set(true));
        }
        {
            let undone = undone.clone();
            let sender = sender.clone();
            let action = std::cell::RefCell::new(Some(action));
            // Fires on timeout, on Undo, or when superseded by a newer toast.
            toast.connect_dismissed(move |_| {
                if !undone.get()
                    && let Some(m) = action.borrow_mut().take()
                {
                    sender.input(m);
                }
            });
        }
        self.toast_overlay.add_toast(toast);
    }
}
