//! Views and data helpers: load and group folder/album/artist, build the
//! subpages (artist → albums → tracks), plus the context/detail
//! helpers (ctx_*) and cover resolution. Extracted from app.rs – pure
//! reorganization, no functional change.

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::category;
use crate::core::db::Library;
use crate::core::scanner;
use crate::i18n::{gettext, ngettext_n, npgettext_n};
use crate::model::{ArtistMeta, Track};
use crate::ui::app::{
    album_subtitle, artist_count_subtitle, cover_widget, duration_label, find_scroller,
    fmt_duration, most_common_artist, read_entries, ActiveSource, App, Cmd, CtxTarget, FsKind, Msg,
};
use crate::ui::enrich::enrich_worker;
use crate::ui::fs_row::FsEntry;

/// How a track tapped in an album track list is played back. Album contexts
/// (`Artist`/`Name`) play only the tapped track; a `Folder` (audiobook/concert)
/// keeps playing the whole folder so chapters continue.
#[derive(Clone)]
pub(crate) enum AlbumPlay {
    /// Artist context (artist → album).
    Artist,
    /// Albums overview (album name across artists).
    Name,
    /// Folder content (audiobook/concert): exactly the files in this folder.
    Folder(String),
}

/// Handle to the album track-list subpage currently rendered, so a late
/// MusicBrainz tracklist fetch — or a freshly downloaded missing track — can
/// refill the **same** content box in place (no navigation flicker).
#[derive(Clone)]
pub(crate) struct AlbumPageRef {
    /// Artist the page was opened for (the opener's argument; may be empty for
    /// the album-overview path).
    pub name: String,
    /// Display artist (most common across the album's tracks); the key for the
    /// canonical-tracklist lookup together with `album`.
    pub artist: String,
    pub album: String,
    pub play: AlbumPlay,
    /// The content box that lives inside the pushed navigation page.
    pub content: gtk::Box,
}

/// Worker-thread helper: resolve the album's MusicBrainz release (using the
/// stored mbid hint, else a fresh search) and cache its canonical tracklist.
/// Always records the fetch attempt (even on no match), so it runs at most once
/// per album until the cache is cleared.
fn fetch_and_store_tracklist(artist: &str, album: &str, mbid_hint: Option<&str>) {
    let Ok(lib) = Library::open() else { return };
    let client = crate::core::online::OnlineClient::new();
    let mbid = match mbid_hint {
        Some(m) if !m.trim().is_empty() => Some(m.to_string()),
        _ => client
            .match_release(artist, album)
            .ok()
            .flatten()
            .map(|m| m.mbid),
    };
    let tracks = match mbid {
        Some(m) => client.fetch_release_tracks(&m).unwrap_or_default(),
        None => Vec::new(),
    };
    let _ = lib.set_album_tracklist(artist, album, &tracks);
}

/// Album name without CD/disc suffix, so multi-CD albums collapse together:
/// "… Disc 2", "… CD 1", "… Cd 2 v 7", "… CD3" → the common base title.
fn album_base(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let clean = |w: &str| {
        w.to_lowercase()
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_string()
    };
    const MARKERS: [&str; 8] = [
        "disc", "disk", "cd", "teil", "part", "folge", "vol", "volume",
    ];
    let is_marker = |w: &str| {
        let c = clean(w);
        MARKERS.iter().any(|m| {
            c == *m
                || (c.starts_with(m)
                    && c.len() > m.len()
                    && c[m.len()..].chars().all(|d| d.is_ascii_digit()))
        })
    };
    const CONNECTORS: [&str; 6] = ["v", "von", "of", "x", "u", "und"];
    let is_suffix_tok = |w: &str| {
        let c = clean(w);
        c.is_empty()
            || c.chars().all(|d| d.is_ascii_digit())
            || CONNECTORS.contains(&c.as_str())
            || is_marker(w)
    };
    // First marker position from which on, until the end, only suffix tokens remain.
    let cut = (0..words.len())
        .find(|&i| is_marker(words[i]) && words[i..].iter().all(|w| is_suffix_tok(w)));
    let base = match cut {
        Some(i) => words[..i].join(" "),
        None => name.trim().to_string(),
    };
    let base = base.trim_matches(|c: char| c == '-' || c == ':' || c.is_whitespace());
    if base.is_empty() {
        name.trim().to_string()
    } else {
        base.to_string()
    }
}

/// Disc number from a folder segment like "CD2", "CD 2", "Disc 03", "Part 2".
/// Only if the segment **begins** with a disc keyword followed by
/// digits (possibly after a separator) – so "Greatest Hits" and the like
/// trigger nothing. Otherwise `None`.
fn disc_from_segment(seg: &str) -> Option<u32> {
    let s = seg.trim().to_ascii_lowercase();
    let bytes = s.as_bytes();
    const MARKERS: [&str; 6] = ["cd", "disc", "disk", "teil", "part", "folge"];
    for kw in MARKERS {
        // Look for the marker **anywhere** in the segment (e.g. "Wie Google tickt CD1"),
        // but only at a word boundary (no match in the middle of a word like
        // "abcd"), followed by digits.
        let mut from = 0;
        while let Some(rel) = s[from..].find(kw) {
            let pos = from + rel;
            let boundary = pos == 0 || !bytes[pos - 1].is_ascii_alphabetic();
            if boundary {
                let digits: String = s[pos + kw.len()..]
                    .trim_start_matches([' ', '_', '.', '#', '-'])
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                if let Ok(n) = digits.parse::<u32>() {
                    return Some(n);
                }
            }
            from = pos + kw.len();
        }
    }
    None
}

/// Effective disc number of a track. **File structure takes precedence:** a
/// CD/disc/part **subfolder** of the path is more reliable than a disc tag
/// (some audiobook rippers wrongly set `disc` = track number). Only real
/// "CD/Disc/Part…" folders count (see `disc_from_segment`), not the filename;
/// otherwise the `disc_no` tag, otherwise 1.
pub(crate) fn track_disc(t: &Track) -> u32 {
    if let Some(d) = std::path::Path::new(&t.path)
        .parent()
        .into_iter()
        .flat_map(|d| d.components())
        .filter_map(|c| c.as_os_str().to_str())
        .filter_map(disc_from_segment)
        .next_back()
    {
        return d;
    }
    t.disc_no.unwrap_or(1)
}

/// "Natural" sort key of a string: digit sequences are compared as numbers
/// (each digit block left-padded with zeros to a fixed width).
/// So "CD2" comes before "CD10", "3.2" before "3.10", and "01 01" before "02 01".
/// For the **file-structure sorting** of audiobooks/folder contents – robust
/// against missing zero-padding **and** unusable track tags.
pub(crate) fn natural_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut num = String::new();
            num.push(c);
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    num.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            let trimmed = num.trim_start_matches('0');
            let trimmed = if trimmed.is_empty() { "0" } else { trimmed };
            for _ in 0..16usize.saturating_sub(trimmed.len()) {
                out.push('0');
            }
            out.push_str(trimmed);
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Sorts tracks by **file structure**: subfolder (CD folder) first, then
/// disc, then track number, then path – consistent with playback
/// (`play_path`) and robust against wrong/missing disc tags (audiobooks).
fn sort_by_structure(tracks: &mut [Track]) {
    let parent = |t: &Track| {
        std::path::Path::new(&t.path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    tracks.sort_by(|a, b| {
        // Compare folder and file paths **naturally** (digit runs as numbers) so
        // that unpadded names ("Track 2" vs "Track 10", "CD2" vs "CD10") fall in
        // the right order. Without this the raw byte comparison was the only
        // tiebreak when track tags are missing/zero — the usual cause of an album
        // playing out of order.
        natural_key(&parent(a))
            .cmp(&natural_key(&parent(b)))
            .then(track_disc(a).cmp(&track_disc(b)))
            .then(a.track_no.unwrap_or(0).cmp(&b.track_no.unwrap_or(0)))
            .then_with(|| natural_key(&a.path).cmp(&natural_key(&b.path)))
    });
}

/// Most common album base title of a set of tracks (for the display title of a
/// subfolder grouped as an album).
pub(crate) fn most_common_album_base(tracks: &[&Track]) -> Option<String> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for t in tracks {
        if let Some(al) = t.album.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
            *counts.entry(album_base(al)).or_default() += 1;
        }
    }
    counts.into_iter().max_by_key(|(_, c)| *c).map(|(b, _)| b)
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
        let meta_covers = self.library.album_meta_covers().unwrap_or_default();
        for album in &mut albums {
            if album
                .cover_path
                .as_deref()
                .is_none_or(|p| p.trim().is_empty())
            {
                album.cover_path = meta_covers
                    .get(&album.album.to_lowercase())
                    .cloned()
                    .or_else(|| self.album_local_cover(&album.artist, &album.album));
            }
        }
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
            );
        } else {
            let offline_keys = self.offline_album_keys();
            let mut guard = if singles {
                self.libview.singles.guard()
            } else {
                self.libview.compilations.guard()
            };
            guard.clear();
            for a in albums {
                let offline = offline_keys.contains(&(a.artist.clone(), a.album.clone()));
                guard.push_back((a, offline));
            }
            drop(guard);
            if singles {
                self.libview.singles.widget().invalidate_headers();
            } else {
                self.libview.compilations.widget().invalidate_headers();
            }
        }
    }

    pub(crate) fn reload_albums_with(&mut self, snap: Option<&crate::core::db::CategorySnapshot>) {
        let mut albums = self.library.albums_overview_with(snap).unwrap_or_default();
        // Resolve missing covers: pull every stored album_meta cover in one
        // query (instead of an `album_cover` lookup per album), and only fall
        // back to the per-track local-cover scan for albums still without one.
        let meta_covers = self.library.album_meta_covers().unwrap_or_default();
        for album in &mut albums {
            if album
                .cover_path
                .as_deref()
                .is_none_or(|p| p.trim().is_empty())
            {
                album.cover_path = meta_covers
                    .get(&album.album.to_lowercase())
                    .cloned()
                    .or_else(|| self.album_local_cover(&album.artist, &album.album));
            }
        }
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
            );
        } else {
            let mut guard = self.libview.albums.guard();
            guard.clear();
            for a in albums {
                let offline = offline_keys.contains(&(a.artist.clone(), a.album.clone()));
                guard.push_back((a, offline));
            }
            drop(guard);
            // Refresh the section headings for the rebuilt rows (or clear them).
            self.libview.albums.widget().invalidate_headers();
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
            .filter(|s| s.kind == "webdav")
            .cloned()
            .collect();
        if sources.is_empty() {
            return false;
        }
        sender.spawn_oneshot_command(move || {
            if let Ok(lib) = crate::core::db::Library::open() {
                for s in &sources {
                    match crate::core::webdav::index_into(&lib, s) {
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
                if a.image_path.as_deref().is_none_or(|p| p.trim().is_empty()) {
                    if let Some(album) = first_album.get(&norm_key(&a.name)) {
                        a.image_path = self.album_cover_for(&a.name, album);
                    }
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
            );
        } else {
            let offline_names = self.offline_artist_names_lc();
            // Album/song counts for the secondary line, fetched in one pass.
            let counts = self.library.artist_counts().unwrap_or_default();
            let mut guard = self.libview.artists.guard();
            guard.clear();
            for a in artists {
                let name_lc = a.name.to_lowercase();
                let offline = offline_names.iter().any(|n| n.contains(&name_lc));
                let (albums, songs) = counts
                    .get(&crate::core::artist::norm_key(&a.name))
                    .copied()
                    .unwrap_or((0, 0));
                let subtitle = artist_count_subtitle(albums, songs);
                guard.push_back((a, offline, subtitle));
            }
            drop(guard);
            // Refresh the alphabetical headings for the rebuilt rows (or clear).
            self.libview.artists.widget().invalidate_headers();
        }
    }

    /// Returns the playable files of an entry: recursive for folders,
    /// only the single one for files.
    pub(crate) fn entry_files(&self, entry: &FsEntry) -> Vec<PathBuf> {
        // Remote (Nextcloud) entries: build the synthetic nc: paths so they can
        // be queued and played like local tracks (`start_track_playback` streams
        // them). Needs the source to be indexed (the DB holds the nc: tracks).
        if entry.is_remote() {
            let (Some(rel), ActiveSource::Source(id)) =
                (entry.rel_path(), &self.files.active_source)
            else {
                return Vec::new();
            };
            if entry.is_dir() {
                // All indexed tracks below this folder, in path (≈ track) order.
                let dir = crate::core::webdav::nc_path(*id, rel);
                let mut paths: Vec<PathBuf> = self
                    .library
                    .tracks_under_path(&dir)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| PathBuf::from(t.path))
                    .collect();
                paths.sort();
                return paths;
            }
            return vec![PathBuf::from(crate::core::webdav::nc_path(*id, rel))];
        }
        let Some(path) = entry.path() else {
            return Vec::new();
        };
        if entry.is_dir() {
            scanner::collect_audio_files(path)
        } else {
            vec![path.clone()]
        }
    }

    /// All files of an artist (from the library), in playback order.
    pub(crate) fn artist_files(&self, name: &str) -> Vec<PathBuf> {
        // Like the artist list (artist_sections/artist_albums): a track
        // counts toward the artist if their name appears in the artist
        // credit – possibly split from "feat." – (case-insensitive). Otherwise
        // the detail page would not count guest/composite tracks and would show "0
        // songs", even though the song list includes them.
        let target = crate::core::artist::norm_key(name);
        self.library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                t.artist.as_deref().is_some_and(|a| {
                    crate::core::artist::split_artists(a)
                        .iter()
                        .any(|s| crate::core::artist::norm_key(s) == target)
                })
            })
            .map(|t| PathBuf::from(t.path))
            .collect()
    }

    /// All files of an album (main artist + album), in playback order.
    /// Also counts feat. variants of the same main artist – matching the
    /// grouped albums overview.
    pub(crate) fn album_files(&self, artist: &str, album: &str) -> Vec<PathBuf> {
        let target = crate::core::artist::norm_key(artist);
        // Indexed album query instead of scanning the whole track table; the
        // main-artist refinement stays in Rust (split "feat." credits).
        self.library
            .tracks_by_album_name(album)
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                t.artist.as_deref().is_some_and(|a| {
                    crate::core::artist::split_artists(a)
                        .first()
                        .is_some_and(|p| crate::core::artist::norm_key(p) == target)
                })
            })
            .map(|t| PathBuf::from(t.path))
            .collect()
    }

    /// All tracks of an artist (possibly split from "feat."), grouped by
    /// album. A track counts toward the artist if their name appears in the
    /// track's split artist credit (case-insensitive) –
    /// matching the artist list, which also splits "feat." credits.
    /// Albums in the order from `all_tracks` (alphabetical), tracks per album
    /// by track number.
    pub(crate) fn artist_albums(&self, name: &str) -> Vec<(String, Vec<Track>)> {
        let target = crate::core::artist::norm_key(name);
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, Vec<Track>> =
            std::collections::HashMap::new();
        for t in self.library.all_tracks().unwrap_or_default() {
            let belongs = t.artist.as_deref().is_some_and(|a| {
                crate::core::artist::split_artists(a)
                    .iter()
                    .any(|s| crate::core::artist::norm_key(s) == target)
            });
            if !belongs {
                continue;
            }
            let album = t.album.clone().unwrap_or_default();
            if !groups.contains_key(&album) {
                order.push(album.clone());
            }
            groups.entry(album).or_default().push(t);
        }
        order
            .into_iter()
            .map(|album| {
                let tracks = groups.remove(&album).unwrap_or_default();
                (album, tracks)
            })
            .collect()
    }

    /// Splits an artist's tracks into **own albums** and **singles**:
    ///
    /// * If **all** tracks of an album belong to the artist (per the library), it
    ///   is their album → its own album entry `(album, display artist, tracks)`.
    /// * If they appear only on **part** of the album (e.g. as a guest on
    ///   2–3 pieces), those tracks count as singles.
    /// * Tracks with no album at all are also singles.
    ///
    /// Albums in the order from `all_tracks`; tracks per album by track number.
    pub(crate) fn artist_sections(
        &self,
        name: &str,
    ) -> (Vec<(String, String, Vec<Track>)>, Vec<Track>) {
        let target = crate::core::artist::norm_key(name);
        let all = self.library.all_tracks().unwrap_or_default();

        // Group the artist's tracks by album name (preserving order).
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, Vec<Track>> =
            std::collections::HashMap::new();
        for t in all {
            let belongs = t.artist.as_deref().is_some_and(|a| {
                crate::core::artist::split_artists(a)
                    .iter()
                    .any(|s| crate::core::artist::norm_key(s) == target)
            });
            if !belongs {
                continue;
            }
            let album = t.album.clone().unwrap_or_default();
            if !groups.contains_key(&album) {
                order.push(album.clone());
            }
            groups.entry(album).or_default().push(t);
        }

        let mut albums: Vec<(String, String, Vec<Track>)> = Vec::new();
        let mut singles: Vec<Track> = Vec::new();
        for album in order {
            let mine = groups.remove(&album).unwrap_or_default();
            if album.is_empty() {
                singles.extend(mine);
                continue;
            }
            // Only tracks where this artist is the **main artist**
            // form an album. Pure guest/feature tracks (name mentions) do
            // NOT feed into the album construction – they count as singles.
            let (own_tracks, guest_tracks): (Vec<Track>, Vec<Track>) =
                mine.into_iter().partition(|t| {
                    t.artist.as_deref().is_some_and(|a| {
                        crate::core::artist::split_artists(a)
                            .first()
                            .is_some_and(|p| crate::core::artist::norm_key(p) == target)
                    })
                });
            // Album only from two own tracks up; otherwise they count as singles.
            if own_tracks.len() >= 2 {
                let display_artist = most_common_artist(&own_tracks);
                albums.push((album, display_artist, own_tracks));
            } else {
                singles.extend(own_tracks);
            }
            singles.extend(guest_tracks);
        }
        (albums, singles)
    }

    /// Groups a playlist's tracks (given as paths, **order preserved**) into
    /// **albums** – an album name shared by 2+ entries – and standalone
    /// **songs**. Like [`Self::artist_sections`], but driven by an explicit
    /// path list, so entries the DB does not know (e.g. remote files) still
    /// show up as singles (with their display name).
    pub(crate) fn playlist_sections(
        &self,
        paths: &[String],
    ) -> (Vec<(String, String, Vec<Track>)>, Vec<Track>) {
        // Resolve each path to its track (order preserved); unknown paths
        // become a minimal single carrying just the path + a display name.
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, Vec<Track>> =
            std::collections::HashMap::new();
        for p in paths {
            let track = self
                .library
                .track_by_path(p)
                .ok()
                .flatten()
                .unwrap_or(Track {
                    id: 0,
                    path: p.clone(),
                    title: self.display_name(std::path::Path::new(p)),
                    artist: None,
                    album: None,
                    genre: None,
                    track_no: None,
                    disc_no: None,
                    duration_ms: None,
                    resume_ms: 0,
                    year: None,
                });
            let album = track.album.clone().unwrap_or_default();
            if !groups.contains_key(&album) {
                order.push(album.clone());
            }
            groups.entry(album).or_default().push(track);
        }

        let mut albums: Vec<(String, String, Vec<Track>)> = Vec::new();
        let mut singles: Vec<Track> = Vec::new();
        for album in order {
            let mine = groups.remove(&album).unwrap_or_default();
            // Untitled album, or just one track of it in the playlist → song.
            if album.is_empty() || mine.len() < 2 {
                singles.extend(mine);
                continue;
            }
            let display_artist = most_common_artist(&mine);
            albums.push((album, display_artist, mine));
        }
        (albums, singles)
    }

    /// Tracks that belong to "this album by this artist": all library
    /// tracks with the album name in whose (split) artist credit `name`
    /// appears. Sorted by file structure (CD folder → disc → track number).
    pub(crate) fn album_tracks_for_artist(&self, name: &str, album: &str) -> Vec<Track> {
        let target = crate::core::artist::norm_key(name);
        let mut tracks: Vec<Track> = self
            .library
            .tracks_by_album_name(album)
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                // Album membership via the main artist (like the
                // albums overview): "A feat. B" belongs to "A"'s album.
                t.artist.as_deref().is_some_and(|a| {
                    crate::core::artist::split_artists(a)
                        .first()
                        .is_some_and(|p| crate::core::artist::norm_key(p) == target)
                })
            })
            .collect();
        sort_by_structure(&mut tracks);
        tracks
    }

    /// All tracks with this album name – **across artists** (matching
    /// the albums overview, which groups purely by album name). Sorted by
    /// disc/track number, then path.
    pub(crate) fn album_tracks_by_name(&self, album: &str) -> Vec<Track> {
        let mut tracks: Vec<Track> = self.library.tracks_by_album_name(album).unwrap_or_default();
        sort_by_structure(&mut tracks);
        tracks
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

    fn push_subpage_inner(&self, title: &str, content: &gtk::Box, swipe_back: bool) {
        // If we are leaving the root overview, remember the current scroll position
        // of the visible section (restored when returning).
        let leaving_root = self
            .nav
            .nav_view
            .visible_page()
            .and_then(|p| p.tag())
            .is_some_and(|t| t == "main");
        if leaving_root {
            if let Some(sc) = self
                .nav
                .view_stack
                .visible_child()
                .and_then(|c| find_scroller(&c))
            {
                let value = sc.vadjustment().value();
                *self.nav.overview_scroll.borrow_mut() = Some((sc, value));
            }
        }

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

    /// Short tap on an artist: opens a subpage that first lists
    /// their **albums** (with cover) and then the **singles** (tracks without
    /// album, with cover). Tapping an album opens its tracks as
    /// a further subpage; tapping a single plays it.
    pub(crate) fn open_artist_tracks(&self, sender: &ComponentSender<Self>, meta: &ArtistMeta) {
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Separate own albums from the rest (guest tracks + tracks without album).
        let (album_groups, singles) = self.artist_sections(&meta.name);

        if album_groups.is_empty() && singles.is_empty() {
            content.append(
                &adw::StatusPage::builder()
                    .icon_name("avatar-default-symbolic")
                    .title(gettext("No tracks"))
                    .description(gettext(
                        "There are no songs for this artist in the library.",
                    ))
                    .build(),
            );
        }

        // --- Albums first ---
        if !album_groups.is_empty() {
            let n = album_groups.len();
            let group = adw::PreferencesGroup::builder()
                .title(format!("{} ({n})", gettext("Albums")))
                .build();
            for (album, display_artist, tracks) in &album_groups {
                let album_meta = self
                    .library
                    .get_album_meta(display_artist, album)
                    .ok()
                    .flatten();
                // Tag year (earliest track = original release) first, online fallback.
                let year = tracks
                    .iter()
                    .filter_map(|t| t.year)
                    .min()
                    .or_else(|| album_meta.as_ref().and_then(|m| m.year));
                let cover_path = album_meta.as_ref().and_then(|m| m.cover_path.clone());

                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(album))
                    .subtitle(album_subtitle(year, tracks.len()))
                    .activatable(true)
                    .build();
                row.add_css_class("emilia-flush");
                row.add_prefix(&cover_widget(
                    cover_path.as_deref(),
                    "media-optical-symbolic",
                ));

                // Total runtime of all album tracks + play button (layout as for
                // the singles). The button plays the whole album; a
                // tap on the row still opens the album subpage.
                let total_ms: i64 = tracks.iter().filter_map(|t| t.duration_ms).sum();
                if total_ms > 0 {
                    row.add_suffix(&duration_label(total_ms));
                }
                let play = gtk::Button::from_icon_name("media-playback-start-symbolic");
                play.add_css_class("flat");
                play.set_valign(gtk::Align::Center);
                play.set_tooltip_text(Some(&gettext("Play album")));
                {
                    let sender = sender.clone();
                    let name = meta.name.clone();
                    let album = album.clone();
                    play.connect_clicked(move |_| {
                        sender.input(Msg::PlayAlbum {
                            artist: name.clone(),
                            album: album.clone(),
                        });
                    });
                }
                row.add_suffix(&play);

                let album = album.clone();
                let display_artist = display_artist.clone();
                // Short tap: album subpage (songs of the album).
                {
                    let sender = sender.clone();
                    let name = meta.name.clone();
                    let album = album.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::OpenAlbumTracks {
                            artist: name.clone(),
                            album: album.clone(),
                        });
                    });
                }
                // Long press (touch) / right click (mouse): album detail view.
                crate::ui::app::on_secondary_click(&row, {
                    let sender = sender.clone();
                    let display_artist = display_artist.clone();
                    let album = album.clone();
                    move || {
                        sender.input(Msg::ShowAlbumDetailFor {
                            artist: display_artist.clone(),
                            album: album.clone(),
                        });
                    }
                });
                crate::ui::app::on_long_press(&row, {
                    let sender = sender.clone();
                    move || {
                        sender.input(Msg::ShowAlbumDetailFor {
                            artist: display_artist.clone(),
                            album: album.clone(),
                        })
                    }
                });
                group.add(&row);
            }
            content.append(&group);
        }

        // --- then the singles (guest tracks + tracks without album) ---
        if !singles.is_empty() {
            let n = singles.len();
            let group = adw::PreferencesGroup::builder()
                .title(format!("{} ({n})", gettext("Singles")))
                .build();
            for t in &singles {
                // Cover order (never a foreign folder image):
                // 1) embedded image of the track itself,
                // 2) cover of the actual album (also for guest tracks),
                // 3) photo of the main artist.
                let cover_path = crate::core::online::local_track_cover(&t.path)
                    .or_else(|| {
                        let album = t.album.as_deref().filter(|a| !a.trim().is_empty())?;
                        let artist = t.artist.as_deref().unwrap_or("");
                        // First exact (artist, album), otherwise any cover of the album.
                        self.library
                            .get_album_meta(artist, album)
                            .ok()
                            .flatten()
                            .and_then(|m| m.cover_path)
                            .or_else(|| self.library.album_cover(album).ok().flatten())
                    })
                    .or_else(|| {
                        let artist = t.artist.as_deref().filter(|a| !a.trim().is_empty())?;
                        let primary = crate::core::artist::split_artists(artist)
                            .into_iter()
                            .next()?;
                        self.library
                            .get_artist_meta(&primary)
                            .ok()
                            .flatten()
                            .and_then(|m| m.image_path)
                    });
                // Not activatable: the track plays via its play button; the
                // detail view opens on long press / right click.
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(&t.title))
                    .build();
                // Album as secondary info under the song name (if present).
                if let Some(al) = t.album.as_deref().filter(|a| !a.trim().is_empty()) {
                    row.set_subtitle(&gtk::glib::markup_escape_text(al));
                }
                row.add_css_class("emilia-flush");
                row.add_prefix(&cover_widget(
                    cover_path.as_deref(),
                    "audio-x-generic-symbolic",
                ));
                if let Some(ms) = t.duration_ms {
                    if ms > 0 {
                        row.add_suffix(&duration_label(ms));
                    }
                }
                let path = t.path.clone();
                // Play button: plays this track but keeps the list open.
                let play_btn = gtk::Button::builder()
                    .icon_name("media-playback-start-symbolic")
                    .tooltip_text(gettext("Play"))
                    .valign(gtk::Align::Center)
                    .css_classes(["flat"])
                    .build();
                {
                    let sender = sender.clone();
                    let name = meta.name.clone();
                    let path = path.clone();
                    play_btn.connect_clicked(move |_| {
                        sender.input(Msg::PlayArtistTrack {
                            name: name.clone(),
                            path: path.clone(),
                            close: false,
                        });
                    });
                }
                row.add_suffix(&play_btn);
                // Long press (touch) / right click (mouse): song detail view.
                crate::ui::app::on_secondary_click(&row, {
                    let sender = sender.clone();
                    let path = path.clone();
                    move || sender.input(Msg::ShowTrackDetail(path.clone()))
                });
                crate::ui::app::on_long_press(&row, {
                    let sender = sender.clone();
                    move || sender.input(Msg::ShowTrackDetail(path.clone()))
                });
                group.add(&row);
            }
            content.append(&group);
        }

        self.push_subpage(&meta.name, &content);
    }

    /// Tapping an album in the artist subpage: lists its tracks
    /// (with album cover) as a further subpage. Tapping a track
    /// plays the entire album from that track on.
    pub(crate) fn open_album_tracks(
        &self,
        sender: &ComponentSender<Self>,
        name: &str,
        album: &str,
    ) {
        // Tracks of the album – `all_tracks` already returns them sorted by track number.
        let tracks = self.album_tracks_for_artist(name, album);
        self.render_album_tracks(sender, tracks, name, album, AlbumPlay::Artist);
    }

    /// Album from the albums overview: **all** tracks of this album name
    /// (artist irrelevant). Tapping a track plays the whole album from here.
    pub(crate) fn open_album_by_name(&self, sender: &ComponentSender<Self>, album: &str) {
        let tracks = self.album_tracks_by_name(album);
        self.render_album_tracks(sender, tracks, "", album, AlbumPlay::Name);
    }

    /// Tracks of a folder in playback order (CD/disc, track number, path).
    /// Basis for the track list of a folder presented as an album.
    pub(crate) fn folder_tracks_ordered(&self, folder: &str) -> Vec<Track> {
        let mut tracks: Vec<Track> = self.library.tracks_under_path(folder).unwrap_or_default();
        // **File structure as truth:** sort folder contents (audiobooks/concerts) by
        // **natural path** – the filenames/CD folders dictate the
        // correct order, even when disc/track tags are missing or wrong.
        // (Album entries still use the track number.)
        tracks.sort_by_cached_key(|t| natural_key(&t.path));
        tracks
    }

    /// Tapping a **folder** audiobook/concert presented as an album: lists
    /// its tracks. Tapping a track plays the folder from there.
    pub(crate) fn open_folder_tracks(&self, sender: &ComponentSender<Self>, folder: &str) {
        let tracks = self.folder_tracks_ordered(folder);
        let refs: Vec<&Track> = tracks.iter().collect();
        let album = most_common_album_base(&refs).unwrap_or_else(|| {
            std::path::Path::new(folder)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        let name = most_common_artist(&tracks);
        self.render_album_tracks(
            sender,
            tracks,
            &name,
            &album,
            AlbumPlay::Folder(folder.to_string()),
        );
    }

    /// Shared rendering of an album track list. `play` determines how a
    /// tapped track is played (artist-related or by album name).
    fn render_album_tracks(
        &self,
        sender: &ComponentSender<Self>,
        tracks: Vec<Track>,
        name: &str,
        album: &str,
        play: AlbumPlay,
    ) {
        let display_artist = most_common_artist(&tracks);

        // The content box is kept (in `libview.album_page`) so a late tracklist
        // fetch or a freshly downloaded track can refill it in place.
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        self.fill_album_content(sender, &content, &tracks, album, &play);

        *self.libview.album_page.borrow_mut() = Some(AlbumPageRef {
            name: name.to_string(),
            artist: display_artist.clone(),
            album: album.to_string(),
            play: play.clone(),
            content: content.clone(),
        });

        // For real albums (not folder audiobooks) fetch the canonical tracklist
        // once in the background, so locally-missing tracks can be flagged — only
        // when YouTube is enabled (otherwise the feature is hidden, so there is
        // nothing to fetch for). The result refills the page above via
        // `Cmd::AlbumTracklistFetched`.
        if self.youtube.enabled
            && !matches!(play, AlbumPlay::Folder(_))
            && !display_artist.is_empty()
            && !self.library.tracklist_fetched(&display_artist, album)
        {
            let artist = display_artist.clone();
            let alb = album.to_string();
            let mbid_hint = self
                .library
                .get_album_meta(&display_artist, album)
                .ok()
                .flatten()
                .and_then(|m| m.mbid);
            sender.spawn_command(move |out| {
                fetch_and_store_tracklist(&artist, &alb, mbid_hint.as_deref());
                let _ = out.send(Cmd::AlbumTracklistFetched { artist, album: alb });
            });
        }

        // Header line: preferably the album artist, otherwise the page artist.
        let header_artist = if display_artist.is_empty() {
            name
        } else {
            display_artist.as_str()
        };
        let title = if header_artist.is_empty() {
            album.to_string()
        } else {
            format!("{header_artist} – {album}")
        };
        self.push_subpage(&title, &content);
    }

    /// Re-fill the currently shown album page (same content box, no navigation)
    /// when it matches `(artist, album)` — used after the canonical tracklist
    /// arrives or a missing track was added.
    pub(crate) fn refill_album_page(
        &self,
        sender: &ComponentSender<Self>,
        artist: &str,
        album: &str,
    ) {
        let page = self.libview.album_page.borrow().clone();
        let Some(page) = page else { return };
        if page.artist != artist || page.album != album {
            return;
        }
        let tracks = match &page.play {
            AlbumPlay::Name => self.album_tracks_by_name(album),
            AlbumPlay::Artist => self.album_tracks_for_artist(&page.name, album),
            AlbumPlay::Folder(f) => self.folder_tracks_ordered(f),
        };
        self.fill_album_content(sender, &page.content, &tracks, album, &page.play);
    }

    /// (Re)builds the rows of an album track-list `content` box: present tracks,
    /// plus greyed "missing" placeholders for tracks the canonical (MusicBrainz)
    /// tracklist has but the library lacks. Clears the box first so it can be
    /// called again to refresh in place.
    fn fill_album_content(
        &self,
        sender: &ComponentSender<Self>,
        content: &gtk::Box,
        tracks: &[Track],
        album: &str,
        play: &AlbumPlay,
    ) {
        use std::collections::HashSet;
        while let Some(child) = content.first_child() {
            content.remove(&child);
        }

        let display_artist = most_common_artist(tracks);
        let album_meta = self
            .library
            .get_album_meta(&display_artist, album)
            .ok()
            .flatten();
        let cover_path = album_meta
            .as_ref()
            .and_then(|m| m.cover_path.clone())
            .or_else(|| self.album_cover_for(&display_artist, album));
        let cover = cover_path
            .as_deref()
            .and_then(crate::ui::widgets::thumb_cached);

        let is_audiobook = {
            use crate::core::category::Area;
            let areas = match play {
                AlbumPlay::Folder(f) => self.library.folder_areas(f),
                _ => self.library.album_areas(&display_artist, album),
            };
            areas.contains(&Area::Audiobooks)
        };

        // Missing-track detection: only for real albums whose present tracks are
        // all numbered (so canonical positions can be matched reliably). Gated on
        // YouTube being enabled — adding a missing track needs it, so without it
        // the greyed entries are hidden entirely (not just non-functional).
        let is_album = !matches!(play, AlbumPlay::Folder(_)) && self.youtube.enabled;
        let can_detect = is_album && tracks.iter().all(|t| t.track_no.is_some());
        let cached = if is_album {
            self.library
                .album_tracklist(&display_artist, album)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let present_pos: HashSet<(u32, u32)> = tracks
            .iter()
            .filter_map(|t| t.track_no.map(|n| (track_disc(t), n)))
            .collect();
        // (disc, position, title) for each canonical track with no local file.
        let missing: Vec<(u32, u32, String)> = if can_detect {
            cached
                .iter()
                .filter(|(d, p, _, _)| !present_pos.contains(&(*d, *p)))
                .map(|(d, p, title, _)| (*d, *p, title.clone()))
                .collect()
        } else {
            Vec::new()
        };
        // Still waiting for the first fetch → show a discreet hint at the bottom.
        let pending = is_album
            && cached.is_empty()
            && !display_artist.is_empty()
            && !self.library.tracklist_fetched(&display_artist, album);

        // Title only for audiobooks; normal albums show the count in the heading.
        if is_audiobook {
            let lbl = gtk::Label::builder()
                .label(gtk::glib::markup_escape_text(album).as_str())
                .xalign(0.0)
                .wrap(true)
                .margin_start(4)
                .build();
            lbl.add_css_class("title-4");
            content.append(&lbl);
        }

        // Discs: union of present tracks and any (whole-disc) missing entries.
        let mut discs: Vec<u32> = tracks.iter().map(track_disc).collect();
        discs.extend(missing.iter().map(|(d, _, _)| *d));
        discs.sort_unstable();
        discs.dedup();
        let multi_disc = discs.len() > 1;

        // Builds a present-track row (cover, track number, duration, play).
        let make_row = |t: &Track| -> adw::ActionRow {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&t.title))
                .build();
            row.add_css_class("emilia-flush");
            row.add_prefix(&crate::ui::widgets::rounded_image(
                cover.as_ref(),
                "media-optical-symbolic",
                48,
            ));
            if let Some(no) = t.track_no {
                row.add_prefix(
                    &gtk::Label::builder()
                        .label(no.to_string())
                        .width_chars(2)
                        .xalign(1.0)
                        .css_classes(["dim-label", "numeric"])
                        .build(),
                );
            }
            if let Some(ms) = t.duration_ms {
                if ms > 0 {
                    row.add_suffix(&duration_label(ms));
                }
            }
            if self.is_offline_path(&t.path) {
                let badge = gtk::Image::from_icon_name("network-offline-symbolic");
                badge.add_css_class("emilia-offline");
                badge.set_pixel_size(14);
                badge.set_valign(gtk::Align::Center);
                row.add_suffix(&badge);
            }
            let path = t.path.clone();
            let build_msg = {
                let play = play.clone();
                move |path: String, close: bool| match &play {
                    AlbumPlay::Artist | AlbumPlay::Name => Msg::PlayOneTrack { path, close },
                    AlbumPlay::Folder(f) => Msg::PlayFolderTrack {
                        folder: f.clone(),
                        path,
                        close,
                    },
                }
            };
            let play_btn = gtk::Button::builder()
                .icon_name("media-playback-start-symbolic")
                .tooltip_text(gettext("Play"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let sender = sender.clone();
                let build_msg = build_msg.clone();
                let path = path.clone();
                play_btn.connect_clicked(move |_| sender.input(build_msg(path.clone(), false)));
            }
            row.add_suffix(&play_btn);
            crate::ui::app::on_secondary_click(&row, {
                let sender = sender.clone();
                let path = path.clone();
                move || sender.input(Msg::ShowTrackDetail(path.clone()))
            });
            crate::ui::app::on_long_press(&row, {
                let sender = sender.clone();
                move || sender.input(Msg::ShowTrackDetail(path.clone()))
            });
            row
        };

        // Builds a greyed "missing" row: tapping it offers to fetch & add it.
        let make_missing_row = |disc: u32, pos: u32, title: &str| -> adw::ActionRow {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(title))
                .subtitle(gettext("Missing — tap to add"))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            // Greyed out so it reads as "not here yet" but still a real entry.
            row.set_opacity(0.55);
            row.add_prefix(
                &gtk::Label::builder()
                    .label(pos.to_string())
                    .width_chars(2)
                    .xalign(1.0)
                    .css_classes(["dim-label", "numeric"])
                    .build(),
            );
            let add_icon = gtk::Image::from_icon_name("list-add-symbolic");
            add_icon.set_valign(gtk::Align::Center);
            row.add_suffix(&add_icon);
            let (artist, album, title) =
                (display_artist.clone(), album.to_string(), title.to_string());
            let sender = sender.clone();
            row.connect_activated(move |_| {
                sender.input(Msg::ShowMissingTrack {
                    artist: artist.clone(),
                    album: album.clone(),
                    disc,
                    position: pos,
                    title: title.clone(),
                });
            });
            row
        };

        // One entry of a (merged) disc, ordered by position.
        enum Item<'a> {
            Present(&'a Track),
            Missing { pos: u32, title: String },
        }

        let render_disc = |group: &adw::PreferencesGroup, disc: u32| {
            let mut items: Vec<(u32, Item)> = Vec::new();
            for t in tracks.iter().filter(|t| track_disc(t) == disc) {
                items.push((t.track_no.unwrap_or(0), Item::Present(t)));
            }
            for (_, pos, title) in missing.iter().filter(|(d, _, _)| *d == disc) {
                items.push((
                    *pos,
                    Item::Missing {
                        pos: *pos,
                        title: title.clone(),
                    },
                ));
            }
            items.sort_by_key(|(p, _)| *p);
            for (_, item) in items {
                match item {
                    Item::Present(t) => group.add(&make_row(t)),
                    Item::Missing { pos, title } => group.add(&make_missing_row(disc, pos, &title)),
                }
            }
        };

        if multi_disc {
            for disc in &discs {
                let present = tracks.iter().filter(|t| track_disc(t) == *disc).count();
                let group = adw::PreferencesGroup::builder()
                    .title(format!("CD {disc} ({present})"))
                    .build();
                render_disc(&group, *disc);
                content.append(&group);
            }
        } else {
            let group = if is_audiobook {
                adw::PreferencesGroup::new()
            } else {
                adw::PreferencesGroup::builder()
                    .title(
                        format!(
                            "{} ({})",
                            gtk::glib::markup_escape_text(album),
                            tracks.len()
                        )
                        .as_str(),
                    )
                    .build()
            };
            let disc = discs.first().copied().unwrap_or(1);
            render_disc(&group, disc);
            content.append(&group);
        }

        if pending {
            let lbl = gtk::Label::builder()
                .label(gettext("Checking for missing tracks …"))
                .xalign(0.0)
                .margin_start(4)
                .build();
            lbl.add_css_class("dim-label");
            content.append(&lbl);
        }
    }

    // ---- Target-dependent helpers for the detail view (file/folder, artist, album) ----

    /// Playable files of the detail target.
    pub(crate) fn ctx_files(&self, target: &CtxTarget) -> Vec<PathBuf> {
        match target {
            CtxTarget::Fs(e) => self.entry_files(e),
            CtxTarget::Artist(m) => self.artist_files(&m.name),
            CtxTarget::Album(m) => self.album_files(&m.artist, &m.album),
        }
    }

    /// Builds a share [`Selection`](crate::core::sync::share::Selection) for a
    /// detail-view target: all of its local files, as absolute paths. Empty when
    /// the target has no shareable local files (e.g. a YouTube-only item).
    pub(crate) fn ctx_share_selection(
        &self,
        target: &CtxTarget,
    ) -> crate::core::sync::share::Selection {
        let song_paths = self
            .ctx_files(target)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        crate::core::sync::share::Selection {
            song_paths,
            // Carry the collected metadata (artist photos, album covers + year,
            // categories) of the shared music along with the audio files.
            include_metadata: true,
            ..Default::default()
        }
    }

    /// Converts raw area entries (concerts/audiobooks) into a list of
    /// **albums and individual pieces**: "album"/"track" stay; a marked
    /// "folder" is resolved into its albums and loose tracks; "artist" is dropped.
    /// Deduplicated by (scope, key), alphabetically by title.
    pub(crate) fn expand_area_items(
        &self,
        raw: Vec<(String, String, String, bool)>,
    ) -> Vec<(String, String, String, bool)> {
        use std::collections::HashSet;
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut out: Vec<(String, String, String, bool)> = Vec::new();
        for (scope, key, title, is_dir) in raw {
            let expanded = match scope.as_str() {
                "album" | "track" => vec![(scope, key, title, is_dir)],
                "folder" => self.folder_albums_and_tracks(&key),
                _ => vec![], // do not list "artist" and the like as such
            };
            for e in expanded {
                if seen.insert((e.0.clone(), e.1.clone())) {
                    out.push(e);
                }
            }
        }
        out.sort_by_key(|a| a.2.to_lowercase());
        out
    }

    /// Resolves a folder into **albums** and **individual pieces**:
    /// * Each immediate **subfolder** is an album (multi-CD contents within
    ///   collapse into one entry; title = most common album tag without
    ///   CD/disc suffix, otherwise folder name).
    /// * Files **directly** in the folder are grouped into album entries by
    ///   **album tag** (deduplicated with concerts already marked as albums);
    ///   **no** individual files from an album.
    /// * Only files **without** an album tag are loose **individual pieces**.
    pub(crate) fn folder_albums_and_tracks(
        &self,
        dir: &str,
    ) -> Vec<(String, String, String, bool)> {
        use crate::core::category::album_key;
        use std::collections::BTreeMap;

        let base = dir.trim_end_matches('/');
        let prefix = format!("{base}/");
        let tracks: Vec<Track> = self.library.tracks_under_path(base).unwrap_or_default();

        // Group by immediate subfolder; without a subfolder = loose file.
        let mut subfolders: BTreeMap<String, Vec<&Track>> = BTreeMap::new();
        let mut loose: Vec<&Track> = Vec::new();
        for t in &tracks {
            let rel = &t.path[prefix.len()..];
            match rel.find('/') {
                Some(i) => subfolders.entry(rel[..i].to_string()).or_default().push(t),
                None => loose.push(t),
            }
        }

        let mut out = Vec::new();
        // Each immediate subfolder is normally one album (its CDs/parts collapse
        // into it). BUT if the subfolder is itself a container — e.g. an author
        // folder holding several audiobook albums — recurse into it so each album
        // becomes its own entry; otherwise the entry points at an artist folder
        // and its detail opens the *artist* instead of the audiobook. CD/Disc/Part
        // subfolders don't count as containers (they belong to one album).
        for (sub, grp) in &subfolders {
            let inner = format!("{base}/{sub}/");
            let has_album_subfolders = grp.iter().any(|t| {
                t.path
                    .strip_prefix(&inner)
                    .and_then(|rel| rel.split('/').next().filter(|_| rel.contains('/')))
                    .is_some_and(|seg| disc_from_segment(seg).is_none())
            });
            if has_album_subfolders {
                out.extend(self.folder_albums_and_tracks(&format!("{base}/{sub}")));
            } else {
                let key = format!("{base}/{sub}");
                let title = most_common_album_base(grp).unwrap_or_else(|| sub.clone());
                out.push(("folder".to_string(), key, title, true));
            }
        }
        // Loose files: group into an album entry by **album tag** – no
        // individual track from an album. The key uses the most common
        // main artist (feat. split off) like `albums_overview`, so that a
        // concert already marked as an album is deduplicated.
        use crate::core::artist::primary_artist;
        let mut by_album: BTreeMap<String, Vec<&Track>> = BTreeMap::new();
        for t in &loose {
            match t.album.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
                Some(al) => by_album.entry(al.to_string()).or_default().push(t),
                None => {
                    let title = if t.title.trim().is_empty() {
                        std::path::Path::new(&t.path)
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    } else {
                        t.title.clone()
                    };
                    out.push(("track".to_string(), t.path.clone(), title, false));
                }
            }
        }
        for (al, grp) in &by_album {
            let mut counts: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for t in grp {
                *counts
                    .entry(primary_artist(t.artist.as_deref().unwrap_or("")))
                    .or_default() += 1;
            }
            let artist = counts
                .into_iter()
                .max_by_key(|(_, n)| *n)
                .map(|(a, _)| a)
                .unwrap_or_default();
            out.push((
                "album".to_string(),
                album_key(&artist, al),
                al.clone(),
                false,
            ));
        }
        out
    }

    /// Cover/photo texture plus matching placeholder icon.
    /// Detects whether a filesystem folder corresponds to an artist or an album,
    /// and returns the matching EQ level as
    /// `(heading, hint, scope, key)` – matching [`Self::open_eq_editor`].
    /// This way the equalizer can be set directly from the file view at the artist or
    /// album level, with the same keys as in the artist/
    /// album overview (so that the settings do not duplicate).
    /// Detects whether a filesystem folder corresponds to an artist or an album.
    /// Basis for playback ("play album/artist") and
    /// the EQ level from the file view.
    pub(crate) fn fs_music_kind(&self, entry: &FsEntry) -> Option<FsKind> {
        if !entry.is_dir() {
            return None;
        }
        // Folder name = known artist? → artist (same key as
        // in the artist overview).
        if let Ok(Some(meta)) = self.library.get_artist_meta(entry.name()) {
            return Some(FsKind::Artist(meta.name));
        }
        // Otherwise: does the folder contain tracks of exactly one album? → album.
        let dir = entry.path()?;
        let tracks: Vec<Track> = self
            .library
            .tracks_under_path(&dir.to_string_lossy())
            .unwrap_or_default();
        // Collapse multi-CD variants ("Album CD1" / "Album Disc 2" …) to their
        // common base, so a multi-disc album counts as ONE album, not several —
        // otherwise it falls through to a generic folder. Compared
        // case-insensitively (sloppy rips tag "… besten CD 1" vs "… Besten Disc 2").
        let refs: Vec<&Track> = tracks.iter().collect();
        let distinct: std::collections::HashSet<String> = refs
            .iter()
            .filter_map(|t| t.album.as_deref().map(str::trim).filter(|a| !a.is_empty()))
            .map(|a| album_base(a).to_lowercase())
            .collect();
        if distinct.len() == 1 {
            // Album name + artist match `open_folder_tracks`/`render_album_tracks`
            // (most-common base + most-common artist) so the cover/EQ key — keyed
            // on (artist, album) — is the same whether set here or read there.
            let album = most_common_album_base(&refs).unwrap_or_default();
            let artist = most_common_artist(&tracks);
            return Some(FsKind::Album { artist, album });
        }
        None
    }

    /// EQ level `(heading, hint, scope, key)` of a filesystem folder,
    /// matching [`Self::open_eq_editor`] – derived from [`Self::fs_music_kind`].
    pub(crate) fn fs_eq_level(
        &self,
        entry: &FsEntry,
    ) -> Option<(
        &'static str,
        String,
        Option<&'static str>,
        &'static str,
        String,
    )> {
        match self.fs_music_kind(entry)? {
            FsKind::Artist(name) => Some((
                "the artist",
                name.clone(),
                Some("Also applies to this artist's albums and tracks."),
                "artist",
                name,
            )),
            FsKind::Album { artist, album } => {
                let key = category::album_key(&artist, &album);
                Some((
                    "the album",
                    album,
                    Some("Also applies to this album's tracks."),
                    "album",
                    key,
                ))
            }
        }
    }

    /// Whether a detail target lives in the Audiobooks area — used to relabel the
    /// menus ("Album"→"Audiobook", "songs"→"tracks") in that context.
    pub(crate) fn is_audiobook(&self, target: &CtxTarget) -> bool {
        use crate::core::category::Area;
        let areas = match target {
            CtxTarget::Album(m) => self.library.album_areas(&m.artist, &m.album),
            CtxTarget::Artist(m) => self.library.artist_areas(&m.name),
            CtxTarget::Fs(e) if e.is_dir() => e
                .path()
                .map(|p| self.library.folder_areas(&p.to_string_lossy()))
                .unwrap_or_default(),
            CtxTarget::Fs(e) => self
                .fs_album(e)
                .map(|(a, al)| self.library.album_areas(&a, &al))
                .unwrap_or_default(),
        };
        areas.contains(&Area::Audiobooks)
    }

    /// Album identity (artist, album) of the current context target, if it is an
    /// album (album card or folder recognized as an album).
    pub(crate) fn ctx_album(&self) -> Option<(String, String)> {
        match self.nav.context_target.as_ref()? {
            CtxTarget::Album(m) => Some((m.artist.clone(), m.album.clone())),
            CtxTarget::Fs(e) => match self.fs_music_kind(e)? {
                FsKind::Album { artist, album } => Some((artist, album)),
                FsKind::Artist(_) => None,
            },
            CtxTarget::Artist(_) => None,
        }
    }

    /// Artist name of the current context target, if it is an artist
    /// (artist card or folder recognized as an artist).
    pub(crate) fn ctx_artist(&self) -> Option<String> {
        match self.nav.context_target.as_ref()? {
            CtxTarget::Artist(m) => Some(m.name.clone()),
            CtxTarget::Fs(e) => match self.fs_music_kind(e)? {
                FsKind::Artist(name) => Some(name),
                FsKind::Album { .. } => None,
            },
            CtxTarget::Album(_) => None,
        }
    }

    /// Albums of an artist with (where known) release year from the
    /// album metadata. Tracks per album already by track number (see
    /// [`Self::artist_albums`]).
    pub(crate) fn artist_albums_dated(&self, name: &str) -> Vec<(Option<i32>, String, Vec<Track>)> {
        self.artist_albums(name)
            .into_iter()
            .map(|(album, tracks)| {
                let artist = tracks
                    .first()
                    .and_then(|t| t.artist.clone())
                    .unwrap_or_default();
                // Prefer the embedded tag year (earliest track = original release)
                // over the online match, which can be a reissue/remaster year.
                let year = tracks.iter().filter_map(|t| t.year).min().or_else(|| {
                    self.library
                        .get_album_meta(&artist, &album)
                        .ok()
                        .flatten()
                        .and_then(|m| m.year)
                });
                (year, album, tracks)
            })
            .collect()
    }

    /// All tracks of an artist in playback order: albums by year
    /// (oldest or newest first, unknown years to the end), each album from
    /// track 1 top-down.
    pub(crate) fn artist_files_ordered(&self, name: &str, newest_first: bool) -> Vec<PathBuf> {
        let mut albums = self.artist_albums_dated(name);
        albums.sort_by(|a, b| {
            use std::cmp::Ordering;
            let by_year = match (a.0, b.0) {
                (Some(x), Some(y)) => {
                    if newest_first {
                        y.cmp(&x)
                    } else {
                        x.cmp(&y)
                    }
                }
                // Known year before unknown (in both directions).
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            };
            by_year.then_with(|| a.1.cmp(&b.1))
        });
        albums
            .into_iter()
            .flat_map(|(_, _, tracks)| tracks.into_iter().map(|t| PathBuf::from(t.path)))
            .collect()
    }

    /// Year info of an artist's albums as `(label, value)`: with at least
    /// two **different** years "Years" + "from – to", with exactly one
    /// known year "Year" + single year. `None` if no year is known.
    pub(crate) fn artist_year_range(&self, name: &str) -> Option<(&'static str, String)> {
        let mut years: Vec<i32> = self
            .artist_albums_dated(name)
            .into_iter()
            .filter_map(|(year, _, _)| year)
            .collect();
        years.sort_unstable();
        years.dedup();
        match years.as_slice() {
            [] => None,
            [y] => Some(("Year", y.to_string())),
            _ => Some((
                "Years",
                format!("{} – {}", years[0], years[years.len() - 1]),
            )),
        }
    }

    /// (artist, album) of a local track entry. Lets a single song's detail share
    /// its **album's** cover candidates, so picking a cover there applies to the
    /// whole album (covers stay consistent per album). `None` for folders,
    /// remote entries or tracks without an album.
    pub(crate) fn fs_album(&self, e: &FsEntry) -> Option<(String, String)> {
        if e.is_dir() || e.is_remote() {
            return None;
        }
        let t = scanner::read_track(e.path()?).ok()?;
        let album = t.album.filter(|a| !a.trim().is_empty())?;
        Some((t.artist.unwrap_or_default(), album))
    }

    /// Detail lines for the "More info" expander.
    pub(crate) fn ctx_info_lines(&self, target: &CtxTarget) -> Vec<(String, String)> {
        // In the audiobook area the menus say "Audiobook"/"tracks", not
        // "Album"/"songs".
        let ab = self.is_audiobook(target);
        match target {
            CtxTarget::Fs(e) => self.info_lines(e),
            CtxTarget::Artist(m) => {
                let files = self.artist_files(&m.name);
                let mut lines = vec![(gettext("Artist"), m.name.clone())];
                // Year/years of the albums, depending on the album metadata.
                let year = self.artist_year_range(&m.name);
                let year_shown = year.is_some();
                if let Some((label, value)) = year {
                    lines.push((gettext(label), value));
                }
                lines.push((
                    gettext("Collection"),
                    Self::folder_summary(&files, !year_shown, ab).0,
                ));
                lines
            }
            CtxTarget::Album(m) => {
                let mut lines = Vec::new();
                if !m.artist.is_empty() {
                    lines.push((gettext("Artist"), m.artist.clone()));
                }
                lines.push((
                    if ab {
                        gettext("Audiobook")
                    } else {
                        gettext("Album")
                    },
                    m.album.clone(),
                ));
                let files = self.album_files(&m.artist, &m.album);
                let (summary, genre) = Self::folder_summary(&files, m.year.is_none(), ab);
                if let Some(g) = genre {
                    lines.push((gettext("Genre"), g));
                }
                if let Some(y) = m.year {
                    lines.push((gettext("Year"), y.to_string()));
                }
                lines.push((gettext("Collection"), summary));
                lines
            }
        }
    }

    /// "Available in" group of the detail target: multiple selection of the areas
    /// in which the content appears (empty = hidden). It is set at the
    /// appropriate level (track/album/artist); inheritance is handled by
    /// `resolve_areas`.
    pub(crate) fn ctx_merkmale(
        &self,
        target: &CtxTarget,
        sender: &ComponentSender<Self>,
    ) -> Option<adw::PreferencesGroup> {
        use crate::core::category::{album_key, Area};
        let (scope, key, effective): (&'static str, String, Vec<Area>) = match target {
            CtxTarget::Artist(m) => ("artist", m.name.clone(), self.library.artist_areas(&m.name)),
            CtxTarget::Album(m) => (
                "album",
                album_key(&m.artist, &m.album),
                self.library.album_areas(&m.artist, &m.album),
            ),
            CtxTarget::Fs(e) if !e.is_dir() => {
                let p = e.path()?;
                let track = scanner::read_track(p).ok()?;
                let path = p.to_string_lossy().into_owned();
                let mut eff = self.library.resolve_areas(
                    track.artist.as_deref(),
                    track.album.as_deref(),
                    &path,
                );
                // A song without an album never appears in the Albums overview
                // (its query skips album-less tracks), so leave the "Albums"
                // switch off for it initially instead of showing it ticked.
                if track.album.as_deref().is_none_or(|a| a.trim().is_empty()) {
                    eff.retain(|a| *a != Area::Albums);
                }
                ("track", path, eff)
            }
            CtxTarget::Fs(e) => {
                let dir_path = e.path()?.to_string_lossy().into_owned();
                // If this exact folder already carries an explicit folder-level
                // setting (e.g. it was filed under Concerts/Audiobooks *as a
                // folder*), keep editing it at the folder level. Otherwise
                // `fs_music_kind` may re-classify it as an album and write the
                // hide to a different row, leaving the original folder entry in
                // its category — the "I set it hidden but it still shows" bug.
                if self
                    .library
                    .get_category("folder", &dir_path)
                    .ok()
                    .flatten()
                    .is_some()
                {
                    let eff = self.library.folder_areas(&dir_path);
                    ("folder", dir_path, eff)
                } else {
                    match self.fs_music_kind(e) {
                        Some(FsKind::Album { artist, album }) => (
                            "album",
                            album_key(&artist, &album),
                            self.library.album_areas(&artist, &album),
                        ),
                        Some(FsKind::Artist(name)) => {
                            ("artist", name.clone(), self.library.artist_areas(&name))
                        }
                        // Generic folder (e.g. first level): folder level,
                        // inherited by everything below it.
                        None => {
                            let eff = self.library.folder_areas(&dir_path);
                            ("folder", dir_path, eff)
                        }
                    }
                }
            }
        };
        Some(self.build_area_group(scope, key, &effective, sender))
    }

    /// Type switch (Automatic / Album / Single / Compilation) for the album
    /// context menu — writes the manual `album_kind` override so the user can
    /// correct the heuristic. Automatic clears the override.
    pub(crate) fn ctx_album_kind_group(
        &self,
        target: &CtxTarget,
        sender: &ComponentSender<Self>,
    ) -> Option<adw::PreferencesGroup> {
        use crate::model::AlbumKind;
        let CtxTarget::Album(m) = target else {
            return None;
        };
        let album = m.album.clone();
        let group = adw::PreferencesGroup::builder()
            .title(gettext("Category"))
            .build();
        let row = adw::ComboRow::builder()
            .title(gettext("Type"))
            .subtitle(gettext(
                "Where this album is filed (Singles / Compilations)",
            ))
            .build();
        let auto = gettext("Automatic");
        let alb = gettext("Album");
        let sng = gettext("Single");
        let cmp = gettext("Compilation");
        let model = gtk::StringList::new(&[&auto, &alb, &sng, &cmp]);
        row.set_model(Some(&model));
        row.set_selected(match self.library.album_kind_override(&album) {
            None => 0,
            Some(AlbumKind::Album) => 1,
            Some(AlbumKind::Single) => 2,
            Some(AlbumKind::Compilation) => 3,
        });
        let sender = sender.clone();
        row.connect_selected_notify(move |r| {
            let kind = match r.selected() {
                1 => Some(AlbumKind::Album),
                2 => Some(AlbumKind::Single),
                3 => Some(AlbumKind::Compilation),
                _ => None,
            };
            sender.input(Msg::SetAlbumKind {
                album: album.clone(),
                kind,
            });
        });
        group.add(&row);
        Some(group)
    }

    /// Area selection (one switch per area) for a level. All switches
    /// off = hidden.
    fn build_area_group(
        &self,
        scope: &'static str,
        key: String,
        effective: &[crate::core::category::Area],
        sender: &ComponentSender<Self>,
    ) -> adw::PreferencesGroup {
        use crate::core::category::{areas_value, Area};
        use std::cell::RefCell;
        use std::rc::Rc;

        // Only show areas whose menu item is visible (audiobooks has no
        // menu item of its own and always stays selectable). Values of hidden
        // areas remain in the state and are not touched.
        let visible_areas: Rc<Vec<Area>> = Rc::new(
            Area::ALL
                .iter()
                .copied()
                .filter(|a| {
                    a.section()
                        .is_none_or(|s| !self.nav.hidden_sections.contains(s))
                })
                // Singles/Compilations are an album concept (the kind-aware
                // resolution only augments album areas): only offer them for an
                // album target, where they'd actually take effect.
                .filter(|a| !matches!(a, Area::Singles | Area::Compilations) || scope == "album")
                .collect(),
        );
        let group = adw::PreferencesGroup::builder().build();
        let expander = adw::ExpanderRow::builder()
            .title(gettext("Available in"))
            .build();
        let active: Vec<String> = visible_areas
            .iter()
            .filter(|a| effective.contains(a))
            .map(|a| gettext(a.label()))
            .collect();
        let subtitle = if active.is_empty() {
            gettext("Hidden")
        } else {
            active.join(", ")
        };
        expander.set_subtitle(&subtitle);

        let state = Rc::new(RefCell::new(effective.to_vec()));
        let syncing = Rc::new(std::cell::Cell::new(false));

        // "Hide": all visible areas off → invisible everywhere.
        let hide_row = adw::SwitchRow::builder()
            .title(gettext("Hide"))
            .active(!visible_areas.iter().any(|a| effective.contains(a)))
            .build();
        expander.add_row(&hide_row);

        // One switch per visible area.
        let area_rows: Rc<Vec<(Area, adw::SwitchRow)>> = Rc::new(
            visible_areas
                .iter()
                .map(|&area| {
                    let row = adw::SwitchRow::builder()
                        .title(gettext(area.label()))
                        .active(effective.contains(&area))
                        .build();
                    expander.add_row(&row);
                    (area, row)
                })
                .collect(),
        );

        // Hide: removes all visible areas or sets the visible
        // default areas and aligns the switches.
        {
            let (sender, key, state, syncing, area_rows, visible_areas) = (
                sender.clone(),
                key.clone(),
                state.clone(),
                syncing.clone(),
                area_rows.clone(),
                visible_areas.clone(),
            );
            hide_row.connect_active_notify(move |r| {
                if syncing.get() {
                    return;
                }
                {
                    let mut s = state.borrow_mut();
                    if r.is_active() {
                        s.retain(|a| !visible_areas.contains(a));
                    } else {
                        for a in Area::DEFAULT {
                            if visible_areas.contains(&a) && !s.contains(&a) {
                                s.push(a);
                            }
                        }
                    }
                }
                syncing.set(true);
                for (area, sw) in area_rows.iter() {
                    sw.set_active(state.borrow().contains(area));
                }
                syncing.set(false);
                sender.input(Msg::SetAreas {
                    scope,
                    key: key.clone(),
                    value: areas_value(&state.borrow()),
                });
            });
        }

        // Area switch: adjust the state and mirror "Hide".
        for (area, row) in area_rows.iter() {
            let area = *area;
            let (sender, key, state, syncing, hide_row, visible_areas) = (
                sender.clone(),
                key.clone(),
                state.clone(),
                syncing.clone(),
                hide_row.clone(),
                visible_areas.clone(),
            );
            row.connect_active_notify(move |r| {
                if syncing.get() {
                    return;
                }
                {
                    let mut s = state.borrow_mut();
                    if r.is_active() {
                        if !s.contains(&area) {
                            s.push(area);
                        }
                    } else {
                        s.retain(|a| *a != area);
                    }
                }
                syncing.set(true);
                let hidden = !visible_areas.iter().any(|a| state.borrow().contains(a));
                hide_row.set_active(hidden);
                syncing.set(false);
                sender.input(Msg::SetAreas {
                    scope,
                    key: key.clone(),
                    value: areas_value(&state.borrow()),
                });
            });
        }

        group.add(&expander);
        group
    }

    /// Short summary of a set of files: "N albums - M songs - 2001–2010".
    /// Short summary "N albums - M songs[ - year/range]". The year is only
    /// appended if `with_year` is set – as soon as a dedicated "Year"/"Years"
    /// line is shown, it is omitted here (to avoid duplication).
    /// One-pass folder summary: songs / album count / year span **and** the
    /// first genre, from a single tag parse per file (was two passes — one for
    /// the summary, one for the genre). The genre is `None` for callers that
    /// don't need it (it's read in the same pass either way).
    pub(crate) fn folder_summary(
        files: &[PathBuf],
        with_year: bool,
        audiobook: bool,
    ) -> (String, Option<String>) {
        let songs = files.len();
        let mut albums = std::collections::HashSet::new();
        let mut min_year: Option<u32> = None;
        let mut max_year: Option<u32> = None;
        let mut genre: Option<String> = None;
        for f in files {
            let (album, year, g) = scanner::read_album_year_genre(f);
            if let Some(a) = album {
                albums.insert(a);
            }
            if let Some(y) = year {
                min_year = Some(min_year.map_or(y, |m| m.min(y)));
                max_year = Some(max_year.map_or(y, |m| m.max(y)));
            }
            if genre.is_none() {
                genre = g;
            }
        }

        let mut value = String::new();
        let n = albums.len();
        if n > 0 {
            let count = if audiobook {
                ngettext_n("{n} audiobook", "{n} audiobooks", n as u32)
            } else {
                ngettext_n("{n} album", "{n} albums", n as u32)
            };
            value.push_str(&format!("{count} - "));
        }
        value.push_str(&if audiobook {
            // Context "audiobook" so this is "{n} Track(s)", not the generic
            // "{n} Titel" used for playlists/queue.
            npgettext_n("audiobook", "{n} track", "{n} tracks", songs as u32)
        } else {
            ngettext_n("{n} song", "{n} songs", songs as u32)
        });
        if with_year {
            if let (Some(a), Some(b)) = (min_year, max_year) {
                let span = if a == b {
                    a.to_string()
                } else {
                    format!("{a}\u{2013}{b}")
                };
                value.push_str(&format!(" - {span}"));
            }
        }
        (value, genre)
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
                if !undone.get() {
                    if let Some(m) = action.borrow_mut().take() {
                        sender.input(m);
                    }
                }
            });
        }
        self.toast_overlay.add_toast(toast);
    }

    /// Detail lines for the "More info" expander.
    pub(crate) fn info_lines(&self, entry: &FsEntry) -> Vec<(String, String)> {
        // Audiobook area → say "Audiobook"/"tracks" instead of "Album"/"songs".
        let ab = self.is_audiobook(&CtxTarget::Fs(entry.clone()));
        let mut lines = Vec::new();
        if entry.is_dir() {
            // Folders recognized as album/artist show matching info incl. year.
            let files = self.entry_files(entry);
            let kind = self.fs_music_kind(entry);
            let is_album = matches!(&kind, Some(FsKind::Album { .. }));
            let mut year_shown = false;
            match kind {
                Some(FsKind::Album { artist, album }) => {
                    if !artist.is_empty() {
                        lines.push((gettext("Artist"), artist.clone()));
                    }
                    lines.push((
                        if ab {
                            gettext("Audiobook")
                        } else {
                            gettext("Album")
                        },
                        album.clone(),
                    ));
                    if let Some(y) = self
                        .library
                        .get_album_meta(&artist, &album)
                        .ok()
                        .flatten()
                        .and_then(|m| m.year)
                    {
                        lines.push((gettext("Year"), y.to_string()));
                        year_shown = true;
                    }
                }
                Some(FsKind::Artist(name)) => {
                    lines.push((gettext("Artist"), name.clone()));
                    if let Some((label, value)) = self.artist_year_range(&name) {
                        lines.push((gettext(label), value));
                        year_shown = true;
                    }
                }
                None => {}
            }
            // One tag parse per file: collection summary + (for albums) the genre.
            let (summary, genre) = Self::folder_summary(&files, !year_shown, ab);
            if is_album {
                if let Some(g) = genre {
                    lines.push((gettext("Genre"), g));
                }
            }
            lines.push((gettext("Collection"), summary));
        } else if let Some(p) = entry.path() {
            // Single tag read for title/artist/album/genre/duration **and** the
            // composer (was previously two parses of the same file).
            if let Ok((t, composer)) = scanner::read_track_detailed(p) {
                lines.push((gettext("Title"), t.title));
                // Remember artist/album for the year resolution (consumed
                // when displaying).
                let (artist, album) = (t.artist.clone(), t.album.clone());
                // Composer is always shown when tagged (relevant for
                // classical/audio dramas); the genre whenever present.
                if let Some(a) = t.artist {
                    lines.push((gettext("Artist"), a));
                }
                if let Some(c) = composer {
                    lines.push((gettext("Composer"), c));
                }
                if let Some(al) = t.album {
                    lines.push((
                        if ab {
                            gettext("Audiobook")
                        } else {
                            gettext("Album")
                        },
                        al,
                    ));
                }
                if let Some(g) = t.genre {
                    lines.push((gettext("Genre"), g));
                }
                if let Some(d) = t.duration_ms {
                    lines.push((gettext("Duration"), fmt_duration(d)));
                }
                // Year (from the album metadata) directly under the duration.
                if let (Some(artist), Some(album)) = (artist, album) {
                    if let Some(y) = self
                        .library
                        .get_album_meta(&artist, &album)
                        .ok()
                        .flatten()
                        .and_then(|m| m.year)
                    {
                        lines.push((gettext("Year"), y.to_string()));
                    }
                }
            }

            // Suggestions detected via fingerprint (AcoustID) – display only,
            // not written into the file.
            if let Ok(Some(m)) = self.library.get_track_meta(&p.to_string_lossy()) {
                if m.status == "matched" {
                    if let Some(t) = m.title {
                        lines.push((gettext("Detected (title)"), t));
                    }
                    if let Some(a) = m.artist {
                        lines.push((gettext("Detected (artist)"), a));
                    }
                    if let Some(al) = m.album {
                        lines.push((gettext("Detected (album)"), al));
                    }
                }
            }
        } else {
            // Remote file: only the (possibly fetched) display values.
            lines.push((gettext("Title"), entry.display_title()));
            if let Some(a) = entry.effective_artist() {
                lines.push((gettext("Artist"), a));
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disc_from_segment_finds_marker_anywhere_with_boundary() {
        assert_eq!(disc_from_segment("CD1"), Some(1));
        assert_eq!(disc_from_segment("cd2"), Some(2));
        assert_eq!(disc_from_segment("Disc 3"), Some(3));
        // Marker in the middle (common for audiobooks):
        assert_eq!(disc_from_segment("Wie Google tickt CD1"), Some(1));
        assert_eq!(disc_from_segment("Teil 4 – Finale"), Some(4));
        // No match in the middle of a word or without a digit:
        assert_eq!(disc_from_segment("Discography"), None);
        assert_eq!(disc_from_segment("Soundtrack"), None);
        assert_eq!(disc_from_segment("Lockdown"), None);
        assert_eq!(disc_from_segment("Digitale Erschoepfung"), None);
    }

    fn track(path: &str, disc: Option<u32>, no: Option<u32>) -> Track {
        Track {
            id: 0,
            path: path.to_string(),
            title: String::new(),
            artist: None,
            album: None,
            genre: None,
            track_no: no,
            disc_no: disc,
            duration_ms: None,
            resume_ms: 0,
            year: None,
        }
    }

    #[test]
    fn natural_key_orders_numbers_numerically() {
        let lt = |a: &str, b: &str| natural_key(a) < natural_key(b);
        assert!(lt("OKR 3.2", "OKR 3.10")); // not zero-padded → numeric
        assert!(lt("CD2", "CD10"));
        assert!(lt("01 01", "01 02"));
        assert!(lt("01 09", "02 01")); // "Disc" in the filename
        assert!(lt("Buch CD1/01", "Buch CD2/01"));
    }

    #[test]
    fn sort_by_structure_keeps_cd_folders_in_order() {
        // Multi-CD audiobook without disc tags, CD marker in the folder name.
        let mut ts = vec![
            track("/Buch/Buch CD2/01.mp3", None, Some(1)),
            track("/Buch/Buch CD1/02.mp3", None, Some(2)),
            track("/Buch/Buch CD1/01.mp3", None, Some(1)),
            track("/Buch/Buch CD2/02.mp3", None, Some(2)),
        ];
        sort_by_structure(&mut ts);
        let order: Vec<&str> = ts.iter().map(|t| t.path.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "/Buch/Buch CD1/01.mp3",
                "/Buch/Buch CD1/02.mp3",
                "/Buch/Buch CD2/01.mp3",
                "/Buch/Buch CD2/02.mp3",
            ]
        );
    }
}
