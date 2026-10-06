//! Context/detail helpers (`ctx_*`): the playable files, share selection and
//! area expansion of a detail target, the folder/album classification, the
//! "More info" lines and the area/kind groups of the detail view. Split out of
//! [`crate::ui::app_views`] – pure reorganization, no functional change.

use std::path::PathBuf;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::category;
use crate::core::scanner;
use crate::i18n::{gettext, gettext_noop, ngettext_n, npgettext_n};
use crate::model::Track;
use crate::ui::app::{fmt_duration, most_common_artist, App, CtxTarget, FsKind, Msg};
use crate::ui::app_settings::SettingMsg;
use crate::ui::app_views_album::{album_base, disc_from_segment, most_common_album_base};
use crate::ui::fs_row::FsEntry;

impl App {
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
        area: crate::core::category::Area,
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
                // Honor a more specific (album/track-level) override: a child that
                // was individually hidden or re-filed no longer resolves to `area`,
                // even though the parent folder still carries the marker. Without
                // this, a hidden audiobook/concert inside a marked folder keeps
                // showing ("I set it hidden but it still shows").
                if !self.entry_in_area(&e.0, &e.1, e.3, area) {
                    continue;
                }
                if seen.insert((e.0.clone(), e.1.clone())) {
                    out.push(e);
                }
            }
        }
        out.sort_by_cached_key(|a| a.2.to_lowercase());
        out
    }

    /// Whether an expanded entry stays in `area` after honoring a **more
    /// specific** hide/recategorization. The entry only got here because a parent
    /// (folder) marker put it into `area`; the user can still hide or re-file the
    /// individual album/track/subfolder via its detail view. We resolve the exact
    /// level that detail view writes to ([`Self::ctx_area_level`]) and honor an
    /// explicit override stored there. With no own override the entry stays
    /// (trust the inherited marker — re-deriving it can misfire for loose-file
    /// albums and wrongly hide a visible book).
    fn entry_in_area(
        &self,
        scope: &str,
        key: &str,
        is_dir: bool,
        area: crate::core::category::Area,
    ) -> bool {
        let target = self.entry_target(scope, key, is_dir);
        let Some((w_scope, w_key, _)) = self.ctx_area_level(&target) else {
            return true; // unclassifiable (e.g. the file vanished) → keep listed
        };
        match self.library.get_category(w_scope, &w_key).ok().flatten() {
            Some(v) => crate::core::category::parse_areas(&v).contains(&area),
            None => true, // no explicit override at the write level → trust marker
        }
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
                gettext_noop("the artist"),
                name.clone(),
                Some(gettext_noop(
                    "Also applies to this artist's albums and tracks.",
                )),
                "artist",
                name,
            )),
            FsKind::Album { artist, album } => {
                let key = category::album_key(&artist, &album);
                Some((
                    gettext_noop("the album"),
                    album,
                    Some(gettext_noop("Also applies to this album's tracks.")),
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
        let (scope, key, effective) = self.ctx_area_level(target)?;
        Some(self.build_area_group(scope, key, &effective, sender))
    }

    /// The category level — `(scope, key)` — a properties/hide change for
    /// `target` is written to, plus the areas currently in force there. Single
    /// source of truth for the level + inheritance resolution, shared by the
    /// detail dialog and the concert/audiobook views, so a per-item hide is read
    /// back exactly where it was written. `None` if the target can't be
    /// classified (e.g. the underlying file vanished).
    fn ctx_area_level(
        &self,
        target: &CtxTarget,
    ) -> Option<(&'static str, String, Vec<crate::core::category::Area>)> {
        use crate::core::category::{album_key, Area};
        let res: (&'static str, String, Vec<Area>) = match target {
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
        Some(res)
    }

    /// Category switch (Automatic / Album / Single / Compilation) for the album
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
        let group = adw::PreferencesGroup::new();
        let row = adw::ComboRow::builder()
            .title(gettext("Category"))
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
            sender.input(Msg::Setting(SettingMsg::SetAlbumKind {
                album: album.clone(),
                kind,
            }));
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
                sender.input(Msg::Setting(SettingMsg::SetAreas {
                    scope,
                    key: key.clone(),
                    value: areas_value(&state.borrow()),
                }));
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
                sender.input(Msg::Setting(SettingMsg::SetAreas {
                    scope,
                    key: key.clone(),
                    value: areas_value(&state.borrow()),
                }));
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
