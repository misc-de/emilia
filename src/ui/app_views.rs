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
use crate::core::{cover, scanner};
use crate::i18n::{gettext, ngettext_n};
use crate::model::{ArtistMeta, Track};
use crate::ui::app::{
    album_subtitle, cover_widget, duration_label, find_scroller, fmt_duration, most_common_artist,
    read_entries, ActiveSource, App, Cmd, CtxTarget, FsKind, Msg,
};
use crate::ui::enrich::enrich_worker;
use crate::ui::fs_row::FsEntry;

/// How a track tapped in an album track list is played back.
#[derive(Clone)]
enum AlbumPlay {
    /// Artist context (artist → album): only that artist's album tracks.
    Artist(String),
    /// Albums overview: all tracks of the album name (artist irrelevant).
    Name(String),
    /// Folder content (audiobook/concert): exactly the files in this folder.
    Folder(String),
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
    const MARKERS: [&str; 8] = ["disc", "disk", "cd", "teil", "part", "folge", "vol", "volume"];
    let is_marker = |w: &str| {
        let c = clean(w);
        MARKERS.iter().any(|m| {
            c == *m
                || (c.starts_with(m) && c.len() > m.len() && c[m.len()..].chars().all(|d| d.is_ascii_digit()))
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
    let cut = (0..words.len()).find(|&i| is_marker(words[i]) && words[i..].iter().all(|w| is_suffix_tok(w)));
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
                    .trim_start_matches(|c: char| matches!(c, ' ' | '_' | '.' | '#' | '-'))
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
        .last()
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
        parent(a)
            .cmp(&parent(b))
            .then(track_disc(a).cmp(&track_disc(b)))
            .then(a.track_no.unwrap_or(0).cmp(&b.track_no.unwrap_or(0)))
            .then_with(|| a.path.cmp(&b.path))
    });
}

/// Most common album base title of a set of tracks (for the display title of a
/// subfolder grouped as an album).
fn most_common_album_base(tracks: &[&Track]) -> Option<String> {
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
        self.entries
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
        // Remember the scroll position of the currently shown folder before it is replaced.
        if let (Some(dir), Some(sc)) = (self.shown_dir.clone(), self.fs_scroller()) {
            self.fs_scroll
                .borrow_mut()
                .insert(dir, sc.vadjustment().value());
        }
        match self.browse_dir.clone() {
            Some(dir) => {
                // Remember the current folder (for "continue where you left off").
                let _ = self.library.set_setting("browse_dir", &dir.to_string_lossy());
                self.loading = true;
                sender.spawn_oneshot_command(move || Cmd::Entries(read_entries(dir)));
            }
            None => {
                self.entries.guard().clear();
                self.loading = false;
            }
        }
    }

    /// Rebuilds the source tab bar: a "Music" tab for the primary
    /// directory plus one per additional source. The buttons are connected as a
    /// radio group; a click sends [`Msg::SelectSource`].
    pub(crate) fn rebuild_source_tabs(&mut self) {
        while let Some(c) = self.source_tabs.first_child() {
            self.source_tabs.remove(&c);
        }
        self.source_tab_buttons.clear();
        if self.sources.is_empty() {
            return;
        }
        let mut tabs: Vec<(ActiveSource, String)> = vec![(ActiveSource::Primary, gettext("Music"))];
        for s in &self.sources {
            tabs.push((ActiveSource::Source(s.id), s.name.clone()));
        }
        let mut group: Option<gtk::ToggleButton> = None;
        for (sel, label) in tabs {
            let btn = gtk::ToggleButton::with_label(&label);
            match &group {
                Some(g) => btn.set_group(Some(g)),
                None => group = Some(btn.clone()),
            }
            // Set the active state BEFORE the handler is connected – otherwise
            // the preselection already triggers a (superfluous) switch.
            btn.set_active(sel == self.active_source);
            let input = self.input.clone();
            let sel_cb = sel.clone();
            btn.connect_toggled(move |b| {
                if b.is_active() {
                    let _ = input.send(Msg::SelectSource(sel_cb.clone()));
                }
            });
            self.source_tabs.append(&btn);
            self.source_tab_buttons.push((sel, btn));
        }
    }

    /// Switches the active source, re-roots the file view accordingly and
    /// reloads the folder. Also mirrors the active state of the tabs.
    pub(crate) fn apply_source(&mut self, sel: ActiveSource, sender: &ComponentSender<Self>) {
        self.active_source = sel.clone();
        match &sel {
            ActiveSource::Primary => {
                self.root_dir = self.music_dir.as_ref().map(PathBuf::from);
                self.browse_dir = self.root_dir.clone();
            }
            ActiveSource::Source(id) => {
                if let Some(s) = self.sources.iter().find(|s| s.id == *id) {
                    match s.kind.as_str() {
                        "local" => {
                            let p = s.path.clone().map(PathBuf::from);
                            self.root_dir = p.clone();
                            self.browse_dir = p;
                            self.remote_browse = None;
                        }
                        // WebDAV: local paths empty, remote browser at the root.
                        _ => {
                            self.root_dir = None;
                            self.browse_dir = None;
                            self.remote_browse = Some(String::new());
                        }
                    }
                }
            }
        }
        if !matches!(self.active_source, ActiveSource::Source(_)) {
            self.remote_browse = None;
        }
        self.sync_source_tabs();
        self.load_dir(sender);
    }

    /// The active source as a WebDAV source (if it is one).
    pub(crate) fn active_remote_source(&self) -> Option<crate::model::Source> {
        let ActiveSource::Source(id) = self.active_source else {
            return None;
        };
        self.sources
            .iter()
            .find(|s| s.id == id && s.kind == "webdav")
            .cloned()
    }

    /// Loads a folder of the active remote source (PROPFIND in the background).
    fn load_remote_dir(&mut self, sender: &ComponentSender<Self>, source: crate::model::Source) {
        let rel = self.remote_browse.clone().unwrap_or_default();
        let Some(creds) = crate::core::webdav::Creds::from_source(&source) else {
            self.entries.guard().clear();
            self.loading = false;
            self.toast(&gettext("This source is not configured correctly"));
            return;
        };
        self.loading = true;
        let active = self.active_source.clone();
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
            let guard = self.entries.guard();
            (0..guard.len())
                .filter_map(|i| {
                    guard.get(i).and_then(|r| match &r.entry {
                        FsEntry::RemoteFile {
                            rel_path,
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
        for (sel, btn) in &self.source_tab_buttons {
            let want = *sel == self.active_source;
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

    pub(crate) fn reload_albums(&mut self) {
        let albums = self.library.albums_overview().unwrap_or_default();
        self.album_count = albums.len();
        // Mirror the overview so that gallery clicks (the factory is empty then) can
        // resolve the entry by index.
        self.albums_overview = albums.clone();
        let offline_keys = self.offline_album_keys();
        if self.gallery_view {
            let items: Vec<(Option<String>, &'static str, String)> = albums
                .iter()
                .map(|a| (a.cover_path.clone(), "media-optical-symbolic", a.album.clone()))
                .collect();
            self.fill_gallery(
                &self.albums_gallery,
                &items,
                Msg::ShowAlbumTracks,
                Msg::ShowAlbumDetail,
            );
        } else {
            let mut guard = self.albums.guard();
            guard.clear();
            for a in albums {
                let offline = offline_keys.contains(&(a.artist.clone(), a.album.clone()));
                guard.push_back((a, offline));
            }
        }
    }

    /// Reads the library (tags → DB) **in the background** – purely local, without
    /// network. `then_enrich`: afterwards optionally auto-fetch online (the
    /// `ScanDone` handler decides based on the switch + connection).
    pub(crate) fn start_scan(&self, sender: &ComponentSender<Self>, then_enrich: bool) {
        // Deliberately the **primary** music directory (not `root_dir`, which
        // switches when changing to an additional source) – library/scan stay on
        // the main folder.
        let Some(root) = self.music_dir.as_ref().map(PathBuf::from) else {
            return;
        };
        sender.spawn_oneshot_command(move || {
            match Library::open() {
                Ok(lib) => {
                    if let Err(e) = scanner::scan_into(&lib, &root) {
                        tracing::warn!("Library scan failed: {e}");
                    }
                }
                Err(e) => tracing::error!("Database unavailable for scan: {e}"),
            }
            Cmd::ScanDone { then_enrich }
        });
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
        let Some(root) = self.music_dir.as_ref().map(PathBuf::from) else {
            if !light {
                self.toast(&gettext("No music folder set – please choose one in the settings"));
            }
            return;
        };
        if self.enrich_state.enriching {
            return;
        }
        self.enrich_state.enrich_cancel.store(false, Ordering::Relaxed);
        let cancel = self.enrich_state.enrich_cancel.clone();
        self.enrich_state.enriching = true;
        sender.spawn_command(move |out| enrich_worker(root, cancel, scan_first, light, &out));
    }

    /// Loads the artists overview from the DB into the factory (incl. photo).
    /// If the artist photo is missing, an album cover is used as a substitute.
    pub(crate) fn reload_artists(&mut self) {
        let mut artists = self.library.artists_overview().unwrap_or_default();
        self.artist_count = artists.len();
        // Fallback cover (an album cover) for artists **without** their own photo.
        // Build the album assignment in ONE pass over `all_tracks` –
        // previously this called `artist_album_cover` → `all_tracks` per artist
        // (O(artists×tracks); dominated startup noticeably).
        if artists
            .iter()
            .any(|a| a.image_path.as_deref().map_or(true, |p| p.trim().is_empty()))
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
                if a.image_path.as_deref().map_or(true, |p| p.trim().is_empty()) {
                    if let Some(album) = first_album.get(&norm_key(&a.name)) {
                        a.image_path = self.album_cover_for(&a.name, album);
                    }
                }
            }
        }
        // Mirror the overview (for gallery index resolution, see reload_albums).
        self.artists_overview = artists.clone();
        if self.gallery_view {
            let items: Vec<(Option<String>, &'static str, String)> = artists
                .iter()
                .map(|a| (a.image_path.clone(), "avatar-default-symbolic", a.name.clone()))
                .collect();
            self.fill_gallery(
                &self.artists_gallery,
                &items,
                Msg::OpenArtistTracks,
                Msg::ShowArtistDetail,
            );
        } else {
            let offline_names = self.offline_artist_names_lc();
            let mut guard = self.artists.guard();
            guard.clear();
            for a in artists {
                let name_lc = a.name.to_lowercase();
                let offline = offline_names.iter().any(|n| n.contains(&name_lc));
                guard.push_back((a, offline));
            }
        }
    }

    /// Returns the playable files of an entry: recursive for folders,
    /// only the single one for files.
    pub(crate) fn entry_files(&self, entry: &FsEntry) -> Vec<PathBuf> {
        // Remote entries have no local path – these helpers work
        // exclusively on the local library.
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
        self.library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                t.album.as_deref() == Some(album)
                    && t.artist.as_deref().is_some_and(|a| {
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
    pub(crate) fn artist_sections(&self, name: &str) -> (Vec<(String, String, Vec<Track>)>, Vec<Track>) {
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

    /// Tracks that belong to "this album by this artist": all library
    /// tracks with the album name in whose (split) artist credit `name`
    /// appears. Sorted by file structure (CD folder → disc → track number).
    pub(crate) fn album_tracks_for_artist(&self, name: &str, album: &str) -> Vec<Track> {
        let target = crate::core::artist::norm_key(name);
        let mut tracks: Vec<Track> = self
            .library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                // Album membership via the main artist (like the
                // albums overview): "A feat. B" belongs to "A"'s album.
                t.album.as_deref() == Some(album)
                    && t.artist.as_deref().is_some_and(|a| {
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
        let target = album.to_lowercase();
        let mut tracks: Vec<Track> = self
            .library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                t.album
                    .as_deref()
                    .is_some_and(|a| a.to_lowercase() == target)
            })
            .collect();
        sort_by_structure(&mut tracks);
        tracks
    }

    /// Wraps a content into a scrollable subpage (with header bar +
    /// back arrow) and pushes it onto the navigation stack.
    pub(crate) fn push_subpage(&self, title: &str, content: &gtk::Box) {
        // If we are leaving the root overview, remember the current scroll position
        // of the visible section (restored when returning).
        let leaving_root = self
            .nav_view
            .visible_page()
            .and_then(|p| p.tag())
            .is_some_and(|t| t == "main");
        if leaving_root {
            if let Some(sc) = self
                .view_stack
                .visible_child()
                .and_then(|c| find_scroller(&c))
            {
                let value = sc.vadjustment().value();
                *self.overview_scroll.borrow_mut() = Some((sc, value));
            }
        }

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(content)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        let page = adw::NavigationPage::builder()
            .title(title)
            .child(&toolbar)
            .build();
        self.nav_view.push(&page);
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
                    .title(&gettext("No tracks"))
                    .description(&gettext("There are no songs for this artist in the library."))
                    .build(),
            );
        }

        // --- Albums first ---
        if !album_groups.is_empty() {
            let n = album_groups.len();
            let group = adw::PreferencesGroup::builder()
                .title(&format!("{} ({n})", gettext("Albums")))
                .build();
            for (album, display_artist, tracks) in &album_groups {
                let album_meta = self
                    .library
                    .get_album_meta(display_artist, album)
                    .ok()
                    .flatten();
                let year = album_meta.as_ref().and_then(|m| m.year);
                let cover_path = album_meta.as_ref().and_then(|m| m.cover_path.clone());

                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(album))
                    .subtitle(album_subtitle(year, tracks.len()))
                    .activatable(true)
                    .build();
                row.add_css_class("emilia-flush");
                row.add_prefix(&cover_widget(cover_path.as_deref(), "media-optical-symbolic"));

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
                // Long press: album detail view.
                {
                    let sender = sender.clone();
                    let gesture = gtk::GestureLongPress::new();
                    gesture.connect_pressed(move |g, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(Msg::ShowAlbumDetailFor {
                            artist: display_artist.clone(),
                            album: album.clone(),
                        });
                    });
                    row.add_controller(gesture);
                }
                group.add(&row);
            }
            content.append(&group);
        }

        // --- then the singles (guest tracks + tracks without album) ---
        if !singles.is_empty() {
            let n = singles.len();
            let group = adw::PreferencesGroup::builder()
                .title(&format!("{} ({n})", gettext("Singles")))
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
                        let primary =
                            crate::core::artist::split_artists(artist).into_iter().next()?;
                        self.library
                            .get_artist_meta(&primary)
                            .ok()
                            .flatten()
                            .and_then(|m| m.image_path)
                    });
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(&t.title))
                    .activatable(true)
                    .build();
                // Album as secondary info under the song name (if present).
                if let Some(al) = t.album.as_deref().filter(|a| !a.trim().is_empty()) {
                    row.set_subtitle(&gtk::glib::markup_escape_text(al));
                }
                row.add_css_class("emilia-flush");
                row.add_prefix(&cover_widget(cover_path.as_deref(), "audio-x-generic-symbolic"));
                if let Some(ms) = t.duration_ms {
                    if ms > 0 {
                        row.add_suffix(&duration_label(ms));
                    }
                }
                row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));

                let path = t.path.clone();
                // Short tap: play track.
                {
                    let sender = sender.clone();
                    let name = meta.name.clone();
                    let path = path.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::PlayArtistTrack {
                            name: name.clone(),
                            path: path.clone(),
                        });
                    });
                }
                // Long press: detail view of the song.
                {
                    let sender = sender.clone();
                    let gesture = gtk::GestureLongPress::new();
                    gesture.connect_pressed(move |g, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(Msg::ShowTrackDetail(path.clone()));
                    });
                    row.add_controller(gesture);
                }
                group.add(&row);
            }
            content.append(&group);
        }

        self.push_subpage(&meta.name, &content);
    }

    /// Tapping an album in the artist subpage: lists its tracks
    /// (with album cover) as a further subpage. Tapping a track
    /// plays the entire album from that track on.
    pub(crate) fn open_album_tracks(&self, sender: &ComponentSender<Self>, name: &str, album: &str) {
        // Tracks of the album – `all_tracks` already returns them sorted by track number.
        let tracks = self.album_tracks_for_artist(name, album);
        self.render_album_tracks(sender, tracks, name, album, AlbumPlay::Artist(name.to_string()));
    }

    /// Album from the albums overview: **all** tracks of this album name
    /// (artist irrelevant). Tapping a track plays the whole album from here.
    pub(crate) fn open_album_by_name(&self, sender: &ComponentSender<Self>, album: &str) {
        let tracks = self.album_tracks_by_name(album);
        self.render_album_tracks(sender, tracks, "", album, AlbumPlay::Name(album.to_string()));
    }

    /// Tracks of a folder in playback order (CD/disc, track number, path).
    /// Basis for the track list of a folder presented as an album.
    pub(crate) fn folder_tracks_ordered(&self, folder: &str) -> Vec<Track> {
        let prefix = format!("{}/", folder.trim_end_matches('/'));
        let mut tracks: Vec<Track> = self
            .library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.path.starts_with(&prefix))
            .collect();
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
        self.render_album_tracks(sender, tracks, &name, &album, AlbumPlay::Folder(folder.to_string()));
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
        // Cover/year live under the (most common) raw artist credit.
        let display_artist = most_common_artist(&tracks);
        let album_meta = self
            .library
            .get_album_meta(&display_artist, album)
            .ok()
            .flatten();
        let cover_path = album_meta.as_ref().and_then(|m| m.cover_path.clone());
        // Decode the album cover once and reuse it in all track rows.
        let cover = cover_path
            .as_deref()
            .and_then(crate::ui::widgets::thumb_cached);

        // Audiobook? Then the title is shown on top instead of the number of songs.
        let is_audiobook = {
            use crate::core::category::Area;
            let areas = match &play {
                AlbumPlay::Folder(f) => self.library.folder_areas(f),
                _ => self.library.album_areas(&display_artist, album),
            };
            areas.contains(&Area::Audiobooks)
        };

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Line **above** the heading: the title only for audiobooks. For
        // normal albums no header – the song count is in the heading
        // ("Album (N)"), and the year is hidden in the song view.
        let header_text = if is_audiobook {
            album.to_string()
        } else {
            String::new()
        };
        if !header_text.trim().is_empty() {
            let lbl = gtk::Label::builder()
                .label(gtk::glib::markup_escape_text(&header_text).as_str())
                .xalign(0.0)
                .wrap(true)
                .margin_start(4)
                .build();
            lbl.add_css_class(if is_audiobook { "title-4" } else { "dim-label" });
            content.append(&lbl);
        }

        // Determine the existing discs (None counts as CD 1). More than one → the
        // tracks are shown split by "CD 1" / "CD 2" ….
        let mut discs: Vec<u32> = tracks.iter().map(track_disc).collect();
        discs.sort_unstable();
        discs.dedup();
        let multi_disc = discs.len() > 1;

        // Builds a track row (cover, track number, duration, play + gestures).
        let make_row = |t: &Track| -> adw::ActionRow {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&t.title))
                .activatable(true)
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
            // Red "disconnected" indicator if the track comes from an offline source.
            if self.is_offline_path(&t.path) {
                let badge = gtk::Image::from_icon_name("network-offline-symbolic");
                badge.add_css_class("emilia-offline");
                badge.set_pixel_size(14);
                badge.set_valign(gtk::Align::Center);
                row.add_suffix(&badge);
            }
            row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));

            let path = t.path.clone();
            // Short tap: play track (whole album from here).
            {
                let sender = sender.clone();
                let play = play.clone();
                let album = album.to_string();
                let path = path.clone();
                row.connect_activated(move |_| {
                    sender.input(match &play {
                        AlbumPlay::Artist(a) => Msg::PlayAlbumTrack {
                            artist: a.clone(),
                            album: album.clone(),
                            path: path.clone(),
                        },
                        AlbumPlay::Name(al) => Msg::PlayAlbumByNameTrack {
                            album: al.clone(),
                            path: path.clone(),
                        },
                        AlbumPlay::Folder(f) => Msg::PlayFolderTrack {
                            folder: f.clone(),
                            path: path.clone(),
                        },
                    });
                });
            }
            // Long press: detail view of the song.
            {
                let sender = sender.clone();
                let gesture = gtk::GestureLongPress::new();
                gesture.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::ShowTrackDetail(path.clone()));
                });
                row.add_controller(gesture);
            }
            row
        };

        if multi_disc {
            // Multiple CDs → one group per "CD 1" / "CD 2" … (the song count or
            // the title is already shown above the sections).
            for disc in &discs {
                let disc_tracks: Vec<&Track> =
                    tracks.iter().filter(|t| track_disc(t) == *disc).collect();
                let group = adw::PreferencesGroup::builder()
                    .title(format!("CD {disc} ({})", disc_tracks.len()))
                    .build();
                for t in disc_tracks {
                    group.add(&make_row(t));
                }
                content.append(&group);
            }
        } else {
            // Single CD: for audiobooks without a repeated title heading (already
            // shown on top), otherwise the album name as the group title.
            let group = if is_audiobook {
                adw::PreferencesGroup::new()
            } else {
                adw::PreferencesGroup::builder()
                    .title(format!("{} ({})", gtk::glib::markup_escape_text(album), tracks.len()).as_str())
                    .build()
            };
            for t in &tracks {
                group.add(&make_row(t));
            }
            content.append(&group);
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

    // ---- Target-dependent helpers for the detail view (file/folder, artist, album) ----

    /// Playable files of the detail target.
    pub(crate) fn ctx_files(&self, target: &CtxTarget) -> Vec<PathBuf> {
        match target {
            CtxTarget::Fs(e) => self.entry_files(e),
            CtxTarget::Artist(m) => self.artist_files(&m.name),
            CtxTarget::Album(m) => self.album_files(&m.artist, &m.album),
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
        out.sort_by(|a, b| a.2.to_lowercase().cmp(&b.2.to_lowercase()));
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
    pub(crate) fn folder_albums_and_tracks(&self, dir: &str) -> Vec<(String, String, String, bool)> {
        use crate::core::category::album_key;
        use std::collections::BTreeMap;

        let base = dir.trim_end_matches('/');
        let prefix = format!("{base}/");
        let tracks: Vec<Track> = self
            .library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.path.starts_with(&prefix))
            .collect();

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
        // Each subfolder = one album (all CDs/parts together).
        for (sub, grp) in &subfolders {
            let key = format!("{base}/{sub}");
            let title = most_common_album_base(grp).unwrap_or_else(|| sub.clone());
            out.push(("folder".to_string(), key, title, true));
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
            let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
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
            out.push(("album".to_string(), album_key(&artist, al), al.clone(), false));
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
        let Some(dir) = entry.path() else {
            return None;
        };
        let tracks: Vec<Track> = self
            .library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| std::path::Path::new(&t.path).starts_with(dir))
            .collect();
        let albums: std::collections::HashSet<&str> = tracks
            .iter()
            .filter_map(|t| t.album.as_deref())
            .filter(|a| !a.is_empty())
            .collect();
        if albums.len() == 1 {
            let album = albums.into_iter().next().unwrap().to_string();
            let artist = tracks
                .iter()
                .find_map(|t| t.artist.clone())
                .unwrap_or_default();
            return Some(FsKind::Album { artist, album });
        }
        None
    }

    /// EQ level `(heading, hint, scope, key)` of a filesystem folder,
    /// matching [`Self::open_eq_editor`] – derived from [`Self::fs_music_kind`].
    pub(crate) fn fs_eq_level(
        &self,
        entry: &FsEntry,
    ) -> Option<(&'static str, String, Option<&'static str>, &'static str, String)> {
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

    /// Album identity (artist, album) of the current context target, if it is an
    /// album (album card or folder recognized as an album).
    pub(crate) fn ctx_album(&self) -> Option<(String, String)> {
        match self.context_target.as_ref()? {
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
        match self.context_target.as_ref()? {
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
                let year = self
                    .library
                    .get_album_meta(&artist, &album)
                    .ok()
                    .flatten()
                    .and_then(|m| m.year);
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
            _ => Some(("Years", format!("{} – {}", years[0], years[years.len() - 1]))),
        }
    }

    pub(crate) fn ctx_cover(&self, target: &CtxTarget) -> (Option<gtk::gdk::Texture>, &'static str) {
        match target {
            CtxTarget::Fs(e) => {
                // First an (album) cover: cover file, embedded, or online
                // via tags. Applies to album folders and individual tracks.
                if let Some(tex) = self.cover_texture(e) {
                    (Some(tex), "media-optical-symbolic")
                } else {
                    // No cover found: next best – the artist photo.
                    // Folder → folder name, file → artist from the tags.
                    let artist = if e.is_dir() {
                        Some(e.name().to_string())
                    } else {
                        e.path()
                            .and_then(|p| scanner::read_track(p).ok())
                            .and_then(|t| t.artist)
                    };
                    let photo = artist
                        .filter(|a| !a.trim().is_empty())
                        .and_then(|a| self.library.get_artist_meta(&a).ok().flatten())
                        .and_then(|m| m.image_path)
                        .and_then(|p| gtk::gdk::Texture::from_filename(&p).ok());
                    match photo {
                        Some(tex) => (Some(tex), "avatar-default-symbolic"),
                        None => (None, "media-optical-symbolic"),
                    }
                }
            }
            CtxTarget::Artist(m) => {
                // Photo, otherwise an album cover of the artist as a substitute.
                let img = m.image_path.clone().or_else(|| self.artist_album_cover(&m.name));
                let tex = img.and_then(|p| gtk::gdk::Texture::from_filename(&p).ok());
                (tex, "avatar-default-symbolic")
            }
            CtxTarget::Album(m) => {
                let tex = m
                    .cover_path
                    .as_deref()
                    .and_then(|p| gtk::gdk::Texture::from_filename(p).ok());
                (tex, "media-optical-symbolic")
            }
        }
    }


    /// Appends the cover/photo: with multiple images a carousel with dots,
    /// otherwise the single (primary) image as before.
    pub(crate) fn append_cover_or_gallery(
        &self,
        content: &gtk::Box,
        entry: &CtxTarget,
        sender: &ComponentSender<Self>,
        dialog: &adw::Dialog,
    ) {
        let (texture, placeholder) = self.ctx_cover(entry);
        let mut paths = self.ctx_gallery_paths(entry);

        // Long press or right click on the image: choose your own cover/photo.
        let attach_upload = |w: &gtk::Box| {
            let click = gtk::GestureClick::new();
            click.set_button(gtk::gdk::BUTTON_SECONDARY);
            {
                let sender = sender.clone();
                click.connect_pressed(move |g, _, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::UploadCover);
                });
            }
            w.add_controller(click);
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::UploadCover);
                });
            }
            w.add_controller(lp);
        };

        // Move the current primary image to the front so the carousel starts on it
        // (so that closing without scrolling changes nothing unintentionally).
        let primary = match entry {
            CtxTarget::Album(m) => m.cover_path.clone(),
            CtxTarget::Artist(m) => m.image_path.clone(),
            CtxTarget::Fs(_) => None,
        };
        if let Some(pos) = primary.and_then(|p| paths.iter().position(|x| *x == p)) {
            let p = paths.remove(pos);
            paths.insert(0, p);
        }

        if paths.len() > 1 {
            let carousel = adw::Carousel::new();
            carousel.set_halign(gtk::Align::Center);
            for path in &paths {
                let tex = gtk::gdk::Texture::from_filename(path).ok();
                let img = crate::ui::widgets::rounded_image(tex.as_ref(), placeholder, 180);
                carousel.append(&img);
            }
            let dots = adw::CarouselIndicatorDots::new();
            dots.set_carousel(Some(&carousel));

            let gallery = gtk::Box::new(gtk::Orientation::Vertical, 6);
            gallery.set_halign(gtk::Align::Center);
            gallery.append(&carousel);
            gallery.append(&dots);
            content.append(&gallery);
            attach_upload(&gallery);

            // When closing the detail view, immediately adopt the image last shown
            // in the carousel as the primary cover/photo (applies everywhere then).
            let album_id = match entry {
                CtxTarget::Album(m) => Some((m.artist.clone(), m.album.clone())),
                _ => None,
            };
            let artist_id = match entry {
                CtxTarget::Artist(m) => Some(m.name.clone()),
                _ => None,
            };
            let sender = sender.clone();
            dialog.connect_closed(move |_| {
                let idx = carousel.position().round().max(0.0) as usize;
                let Some(path) = paths.get(idx).cloned() else {
                    return;
                };
                if let Some((artist, album)) = &album_id {
                    sender.input(Msg::SetAlbumCover {
                        artist: artist.clone(),
                        album: album.clone(),
                        path,
                    });
                } else if let Some(name) = &artist_id {
                    sender.input(Msg::SetArtistImage {
                        name: name.clone(),
                        path,
                    });
                }
            });
        } else {
            let cover = crate::ui::widgets::rounded_image(texture.as_ref(), placeholder, 180);
            let cover_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            cover_box.set_halign(gtk::Align::Center);
            cover_box.set_hexpand(false);
            cover_box.append(&cover);
            content.append(&cover_box);
            attach_upload(&cover_box);
        }
    }

    /// Stored gallery image paths of a target (only existing files).
    pub(crate) fn ctx_gallery_paths(&self, entry: &CtxTarget) -> Vec<String> {
        let stored = match entry {
            CtxTarget::Artist(m) => self.library.artist_images(&m.name).unwrap_or_default(),
            CtxTarget::Album(m) => {
                self.library.album_images(&m.artist, &m.album).unwrap_or_default()
            }
            CtxTarget::Fs(_) => Vec::new(),
        };
        stored
            .into_iter()
            .filter(|p| std::path::Path::new(p).exists())
            .collect()
    }

    /// Detail lines for the "More info" expander.
    pub(crate) fn ctx_info_lines(&self, target: &CtxTarget) -> Vec<(String, String)> {
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
                lines.push((gettext("Collection"), Self::files_summary(&files, !year_shown)));
                lines
            }
            CtxTarget::Album(m) => {
                let mut lines = Vec::new();
                if !m.artist.is_empty() {
                    lines.push((gettext("Artist"), m.artist.clone()));
                }
                lines.push((gettext("Album"), m.album.clone()));
                let files = self.album_files(&m.artist, &m.album);
                if let Some(g) = Self::first_genre(&files) {
                    lines.push((gettext("Genre"), g));
                }
                if let Some(y) = m.year {
                    lines.push((gettext("Year"), y.to_string()));
                }
                lines.push((gettext("Collection"), Self::files_summary(&files, m.year.is_none())));
                lines
            }
        }
    }

    /// "Properties" group of the detail target: multiple selection of the areas in
    /// which the content appears (empty = hidden). It is set at the
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
                let eff =
                    self.library
                        .resolve_areas(track.artist.as_deref(), track.album.as_deref(), &path);
                ("track", path, eff)
            }
            CtxTarget::Fs(e) => match self.fs_music_kind(e) {
                Some(FsKind::Album { artist, album }) => (
                    "album",
                    album_key(&artist, &album),
                    self.library.album_areas(&artist, &album),
                ),
                Some(FsKind::Artist(name)) => {
                    ("artist", name.clone(), self.library.artist_areas(&name))
                }
                // Generic folder (e.g. first level): folder level, inherited
                // by everything below it.
                None => {
                    let path = e.path()?.to_string_lossy().into_owned();
                    let eff = self.library.folder_areas(&path);
                    ("folder", path, eff)
                }
            },
        };
        Some(self.build_area_group(scope, key, &effective, sender))
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
                .filter(|a| a.section().map_or(true, |s| !self.hidden_sections.contains(s)))
                .collect(),
        );
        let group = adw::PreferencesGroup::builder().build();
        let expander = adw::ExpanderRow::builder().title(&gettext("Properties")).build();
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
            .title(&gettext("Hide"))
            .active(!visible_areas.iter().any(|a| effective.contains(a)))
            .build();
        expander.add_row(&hide_row);

        // One switch per visible area.
        let area_rows: Rc<Vec<(Area, adw::SwitchRow)>> = Rc::new(
            visible_areas
                .iter()
                .map(|&area| {
                    let row = adw::SwitchRow::builder()
                        .title(&gettext(area.label()))
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
    pub(crate) fn files_summary(files: &[PathBuf], with_year: bool) -> String {
        let songs = files.len();
        let mut albums = std::collections::HashSet::new();
        let mut min_year: Option<u32> = None;
        let mut max_year: Option<u32> = None;
        for f in files {
            let (album, year) = scanner::read_album_year(f);
            if let Some(a) = album {
                albums.insert(a);
            }
            if let Some(y) = year {
                min_year = Some(min_year.map_or(y, |m| m.min(y)));
                max_year = Some(max_year.map_or(y, |m| m.max(y)));
            }
        }

        let mut value = String::new();
        let n = albums.len();
        if n > 0 {
            value.push_str(&format!("{} - ", ngettext_n("{n} album", "{n} albums", n as u32)));
        }
        value.push_str(&ngettext_n("{n} song", "{n} songs", songs as u32));
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
        value
    }

    pub(crate) fn toast(&self, _msg: &str) {
        // On-screen messages at the bottom edge are disabled by request –
        // deliberately a no-op (the calls remain, easily reactivatable).
    }

    /// Obtains a cover as a texture. For a **folder** the folder cover
    /// (= album image); for a **single file** deliberately **no** folder image, so
    /// that a track does not inherit a foreign cover from a shared folder – instead
    /// the embedded image of the file or the online-assigned album cover.
    /// `None` if nothing suitable is found.
    pub(crate) fn cover_texture(&self, entry: &FsEntry) -> Option<gtk::gdk::Texture> {
        // Cover resolution works on the local filesystem; remote
        // entries have none.
        let epath = entry.path()?;
        if entry.is_dir() {
            if let Some(path) = cover::find_cover_file(epath) {
                if let Ok(texture) = gtk::gdk::Texture::from_filename(&path) {
                    return Some(texture);
                }
            }
        }

        let audio = if entry.is_dir() {
            std::fs::read_dir(epath)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| scanner::is_audio(p))
                .min()
        } else {
            Some(epath.clone())
        };

        if let Some(audio) = &audio {
            if let Some(bytes) = cover::embedded_cover(audio) {
                if let Ok(tex) =
                    gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(bytes.as_slice()))
                {
                    return Some(tex);
                }
            }
        }

        // Last: online-loaded cover from the cache (assigned via the tags).
        let track = scanner::read_track(audio.as_ref()?).ok()?;
        let (artist, album) = (track.artist?, track.album?);
        let meta = self.library.get_album_meta(&artist, &album).ok().flatten()?;
        let path = meta.cover_path?;
        gtk::gdk::Texture::from_filename(&path).ok()
    }

    /// First set genre of a set of files (for the album display). Albums
    /// are usually genre-uniform, so the first match suffices.
    fn first_genre(files: &[PathBuf]) -> Option<String> {
        files
            .iter()
            .find_map(|f| scanner::read_genre_composer(f).0)
    }

    /// Detail lines for the "More info" expander.
    pub(crate) fn info_lines(&self, entry: &FsEntry) -> Vec<(String, String)> {
        let mut lines = Vec::new();
        if entry.is_dir() {
            // Folders recognized as album/artist show matching info incl. year.
            let files = self.entry_files(entry);
            let mut year_shown = false;
            match self.fs_music_kind(entry) {
                Some(FsKind::Album { artist, album }) => {
                    if !artist.is_empty() {
                        lines.push((gettext("Artist"), artist.clone()));
                    }
                    lines.push((gettext("Album"), album.clone()));
                    if let Some(g) = Self::first_genre(&files) {
                        lines.push((gettext("Genre"), g));
                    }
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
            lines.push((gettext("Collection"), Self::files_summary(&files, !year_shown)));
        } else if let Some(p) = entry.path() {
            match scanner::read_track(p) {
                Ok(t) => {
                    lines.push((gettext("Title"), t.title));
                    // Remember artist/album for the year resolution (consumed
                    // when displaying).
                    let (artist, album) = (t.artist.clone(), t.album.clone());
                    // Genre + composer from the file tags (display only). The
                    // composer is always shown when it is tagged (relevant
                    // for classical/audio dramas); the genre whenever present.
                    let (genre, composer) = scanner::read_genre_composer(p);
                    if let Some(a) = t.artist {
                        lines.push((gettext("Artist"), a));
                    }
                    if let Some(c) = composer {
                        lines.push((gettext("Composer"), c));
                    }
                    if let Some(al) = t.album {
                        lines.push((gettext("Album"), al));
                    }
                    if let Some(g) = genre {
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
                Err(_) => {}
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
