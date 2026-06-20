//! Per-category sorting of the library overviews (Artists/Albums/Concerts/
//! Audiobooks). Each section keeps its own criterion + direction (see
//! [`crate::ui::app::LibView::sort`]); the title-bar sort popover is rebuilt
//! per section here, and the reload functions call the matching `sort_*`.

use std::collections::HashMap;

use adw::prelude::*;
use relm4::gtk;
use relm4::prelude::*;

use crate::core::artist::{norm_key, split_artists};
use crate::core::db::Library;
use crate::i18n::gettext;
use crate::model::{AlbumMeta, ArtistMeta};
use crate::ui::app::{
    section_has_gallery, section_has_grouping, section_sort_criteria, App, Msg, SortCrit,
    SORTABLE_SECTIONS,
};
use crate::ui::app_views::natural_key;
use crate::ui::fs_row::FsEntry;

/// Shared hand-off for a page component's title-bar sort control. The component
/// (Podcasts/Streaming/YouTube) rebuilds it on every sort/view/list change; the
/// parent reads it to drive the one shared title-bar [`crate::ui::app::NavState::sort_btn`].
/// `None` hides the button, `Some((popover, desc))` shows it with that popover
/// and direction icon. (`gtk::Popover` is `!Send`, so it rides an `Rc` slot,
/// never a `Msg`.)
pub(crate) type SortSlot = std::rc::Rc<std::cell::RefCell<Option<(gtk::Popover, bool)>>>;

/// Alphabetical group heading for a name (Artists/Albums sorted by name):
/// digit-initial entries collapse into one `0–9` group, letters use their
/// uppercased initial (A, B, C …), anything else falls under `#`. Kept
/// consistent with [`natural_key`] so the grouping never repeats a heading:
/// digit and letter runs are each contiguous after the sort.
pub(crate) fn alpha_header(name: &str) -> String {
    match name.trim().chars().next() {
        Some(c) if c.is_ascii_digit() => "0–9".to_string(),
        Some(c) if c.is_alphabetic() => c.to_uppercase().to_string(),
        _ => "#".to_string(),
    }
}

/// Compares two file-browser entries for the chosen "files" sort: folders always
/// rank above files (the direction never sinks a folder below a file); within a
/// group, by name (natural) or by runtime, reversed for descending.
fn fs_entry_cmp(a: &FsEntry, b: &FsEntry, crit: SortCrit, desc: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.is_dir(), b.is_dir()) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    let ord = match crit {
        SortCrit::Length => a.runtime_ms().cmp(&b.runtime_ms()),
        // Name is the only other criterion offered for files.
        _ => natural_key(a.name()).cmp(&natural_key(b.name())),
    };
    if desc {
        ord.reverse()
    } else {
        ord
    }
}

/// Reads a component's persisted sort (`sort_<key>` / `sort_<key>_desc`) from the
/// settings DB, falling back to `default_crit`/`default_desc` when unset. Used by
/// the standalone components (podcasts/youtube/streaming) for their own sort state.
pub(crate) fn read_sort(
    lib: &Library,
    key: &str,
    default_crit: SortCrit,
    default_desc: bool,
) -> (SortCrit, bool) {
    let crit = lib
        .get_setting(&format!("sort_{key}"))
        .ok()
        .flatten()
        .map(|s| SortCrit::from_key(&s))
        .unwrap_or(default_crit);
    let desc = match lib
        .get_setting(&format!("sort_{key}_desc"))
        .ok()
        .flatten()
        .as_deref()
    {
        Some("1") => true,
        Some("0") => false,
        _ => default_desc,
    };
    (crit, desc)
}

/// Builds a standalone sort popover (criteria radio group + an ascending/
/// descending toggle) for the components that carry their **own** header sort
/// button instead of the shared title-bar one (podcasts/youtube/streaming).
///
/// `crits` pairs each criterion with its display label (so the same [`SortCrit`]
/// can read "Number of episodes"/"Number of videos" per context). `on_change` is
/// invoked with the chosen `(criterion, descending)` whenever either changes; the
/// caller persists + reloads + rebuilds this popover, so the build-time `active`/
/// `desc` captured below are always fresh on the next interaction.
/// One discreet bottom toggle of a [`sort_popover`] (e.g. "Without grouping",
/// "Gallery view"): its label, current state, and a callback fired on toggle.
pub(crate) struct SortToggle {
    pub label: String,
    pub active: bool,
    pub on_toggle: Box<dyn Fn(bool)>,
}

pub(crate) fn sort_popover(
    crits: &[(SortCrit, String)],
    active: SortCrit,
    desc: bool,
    on_change: impl Fn(SortCrit, bool) + 'static,
    toggles: Vec<SortToggle>,
) -> gtk::Popover {
    let on_change = std::rc::Rc::new(on_change);

    let bx = gtk::Box::new(gtk::Orientation::Vertical, 6);
    bx.set_margin_top(10);
    bx.set_margin_bottom(10);
    bx.set_margin_start(12);
    bx.set_margin_end(12);

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let title = gtk::Label::new(Some(&gettext("Sort by")));
    title.add_css_class("heading");
    title.set_xalign(0.0);
    title.set_hexpand(true);
    header.append(&title);
    let dir = gtk::ToggleButton::new();
    dir.add_css_class("flat");
    dir.set_active(desc);
    dir.set_icon_name(sort_dir_icon(desc));
    dir.set_tooltip_text(Some(&if desc {
        gettext("Descending")
    } else {
        gettext("Ascending")
    }));
    {
        let on_change = on_change.clone();
        dir.connect_toggled(move |b| {
            let desc = b.is_active();
            b.set_icon_name(sort_dir_icon(desc));
            b.set_tooltip_text(Some(&if desc {
                gettext("Descending")
            } else {
                gettext("Ascending")
            }));
            on_change(active, desc);
        });
    }
    header.append(&dir);
    bx.append(&header);

    let mut leader: Option<gtk::CheckButton> = None;
    for (crit, label) in crits {
        let crit = *crit;
        let cb = gtk::CheckButton::with_label(label);
        match &leader {
            Some(l) => cb.set_group(Some(l)),
            None => leader = Some(cb.clone()),
        }
        cb.set_active(crit == active);
        {
            let on_change = on_change.clone();
            cb.connect_toggled(move |b| {
                if b.is_active() {
                    on_change(crit, desc);
                }
            });
        }
        bx.append(&cb);
    }

    // Discreet bottom toggles (grouping / gallery), set off by a separator.
    if !toggles.is_empty() {
        let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
        sep.set_margin_top(4);
        sep.set_margin_bottom(2);
        bx.append(&sep);
        for t in toggles {
            let cb = gtk::CheckButton::with_label(&t.label);
            cb.add_css_class("dim-label");
            cb.set_active(t.active);
            cb.connect_toggled(move |b| (t.on_toggle)(b.is_active()));
            bx.append(&cb);
        }
    }

    let popover = gtk::Popover::new();
    popover.set_child(Some(&bx));
    popover
}

/// Icon for a sort direction – shared by the title-bar button and the popover
/// toggle so they always match.
pub(crate) fn sort_dir_icon(desc: bool) -> &'static str {
    if desc {
        "view-sort-descending-symbolic"
    } else {
        "view-sort-ascending-symbolic"
    }
}

/// Sort + gallery messages, dispatched by [`App::update_sort`]. Grouped out of
/// the flat `Msg` enum (see `app.rs`): the title-bar sort popover (criterion /
/// direction / grouping / per-section gallery), the global gallery view, and the
/// `*Changed` notifications mirrored from the extracted page components.
#[derive(Debug)]
pub(crate) enum SortMsg {
    /// Gallery view (cover grid) on/off; rebuilds the lists.
    GalleryView(bool),
    /// Tiles per row in the gallery view (2–8); rebuilds the lists.
    GalleryColumns(u32),
    /// Rebuild the title-bar sort popover for the current section (or hide it).
    MenuRefresh,
    /// Change the sort criterion of the current section; persists and re-sorts.
    SetCrit(SortCrit),
    /// Change the sort direction of the current section (`true` = descending).
    SetDir(bool),
    /// Toggle section grouping for the current section (`true` = no grouping).
    SetNoGroup(bool),
    /// Toggle the gallery view for the current section only.
    SectionGallery(bool),
    /// The PodcastsPage updated its sort slot → mirror onto the title-bar button.
    PodcastChanged,
    /// The StreamPage updated its sort slot → mirror onto the title-bar button.
    StreamChanged,
    /// The YtPage updated its sort slot → mirror onto the title-bar button.
    YtChanged,
}

impl App {
    /// Dispatch a [`SortMsg`]. Split out of the monolithic `App::update` match.
    pub(crate) fn update_sort(&mut self, msg: SortMsg, sender: &ComponentSender<Self>) {
        match msg {
            SortMsg::MenuRefresh => self.rebuild_sort_menu(),
            // A component page updated its sort slot; mirror it onto the shared
            // title-bar button, but only while that page's section is showing.
            SortMsg::PodcastChanged => {
                if self.current_section().as_deref() == Some("podcasts") {
                    self.apply_component_sort(&self.nav.podcast_sort);
                }
            }
            SortMsg::StreamChanged => {
                if self.current_section().as_deref() == Some("streaming") {
                    self.apply_component_sort(&self.nav.stream_sort);
                }
            }
            SortMsg::YtChanged => {
                if self.current_section().as_deref() == Some("youtube") {
                    self.apply_component_sort(&self.nav.yt_sort);
                }
            }
            SortMsg::SetCrit(crit) => {
                let Some(section) = self.current_section() else {
                    return;
                };
                let (cur, desc) = self.libview.sort_for(&section);
                if cur != crit {
                    self.set_section_sort(&section, crit, desc, sender);
                }
            }
            SortMsg::SetDir(desc) => {
                let Some(section) = self.current_section() else {
                    return;
                };
                let (crit, cur) = self.libview.sort_for(&section);
                if cur != desc {
                    self.set_section_sort(&section, crit, desc, sender);
                }
            }
            SortMsg::SetNoGroup(off) => {
                let Some(section) = self.current_section() else {
                    return;
                };
                if self.libview.grouping_off(&section) != off {
                    self.set_section_grouping(&section, off, sender);
                }
            }
            SortMsg::SectionGallery(on) => {
                let Some(section) = self.current_section() else {
                    return;
                };
                if self.libview.gallery_on(&section) != on {
                    self.set_section_gallery(&section, on, sender);
                }
            }
            SortMsg::GalleryView(on) => {
                self.libview.gallery_view = on;
                let _ = self
                    .library
                    .set_setting("gallery_view", if on { "1" } else { "0" });
                self.rebuild_all_lists(sender);
                self.podcasts_page
                    .emit(crate::ui::podcasts_page::PodcastsInput::SetGalleryView(on));
                self.yt_page
                    .emit(crate::ui::yt_page::YtInput::SetGalleryView(on));
            }
            SortMsg::GalleryColumns(n) => {
                self.libview.gallery_columns = n.clamp(2, 8);
                let _ = self
                    .library
                    .set_setting("gallery_columns", &self.libview.gallery_columns.to_string());
                if self.libview.gallery_view {
                    self.rebuild_all_lists(sender);
                }
                self.podcasts_page.emit(
                    crate::ui::podcasts_page::PodcastsInput::SetGalleryColumns(
                        self.libview.gallery_columns,
                    ),
                );
                self.yt_page
                    .emit(crate::ui::yt_page::YtInput::SetGalleryColumns(
                        self.libview.gallery_columns,
                    ));
            }
        }
    }

    /// Name of the section currently shown in the main view stack.
    pub(crate) fn current_section(&self) -> Option<String> {
        self.nav
            .view_stack
            .visible_child_name()
            .map(|s| s.to_string())
    }

    /// Stores a section's sort choice, persists it and rebuilds the affected
    /// overview. Called from the sort popover handlers.
    pub(crate) fn set_section_sort(
        &mut self,
        section: &str,
        crit: SortCrit,
        desc: bool,
        sender: &ComponentSender<Self>,
    ) {
        let Some(&key) = SORTABLE_SECTIONS.iter().find(|s| **s == section) else {
            return;
        };
        self.libview.sort.insert(key, (crit, desc));
        let _ = self
            .library
            .set_setting(&format!("sort_{key}"), crit.as_key());
        let _ = self
            .library
            .set_setting(&format!("sort_{key}_desc"), if desc { "1" } else { "0" });
        // Keep the title-bar icon in step with the chosen direction.
        self.nav.sort_btn.set_icon_name(sort_dir_icon(desc));
        match key {
            "files" => self.resort_entries(),
            "albums" => self.reload_albums(),
            "singles" => self.reload_singles(),
            "compilations" => self.reload_compilations(),
            "artists" => self.reload_artists(),
            "concerts" => self.load_concerts(sender),
            "audiobooks" => self.load_audiobooks(sender),
            "favorites" => self.load_favorites(sender),
            "playlists" => self.reload_playlists(sender),
            "memo" => self.reload_memos(sender),
            _ => {}
        }
    }

    /// Stores a section's "no grouping" choice, persists it and rebuilds the
    /// affected overview. The sort order is unchanged – only the section headings
    /// appear or vanish. Called from the sort popover's grouping toggle.
    pub(crate) fn set_section_grouping(
        &mut self,
        section: &str,
        off: bool,
        sender: &ComponentSender<Self>,
    ) {
        let Some(&key) = SORTABLE_SECTIONS.iter().find(|s| **s == section) else {
            return;
        };
        self.libview.no_group.insert(key, off);
        let _ = self
            .library
            .set_setting(&format!("nogroup_{key}"), if off { "1" } else { "0" });
        match key {
            "files" => self.resort_entries(),
            "albums" => self.reload_albums(),
            "singles" => self.reload_singles(),
            "compilations" => self.reload_compilations(),
            "artists" => self.reload_artists(),
            "concerts" => self.load_concerts(sender),
            "audiobooks" => self.load_audiobooks(sender),
            "favorites" => self.load_favorites(sender),
            "playlists" => self.reload_playlists(sender),
            "memo" => self.reload_memos(sender),
            _ => {}
        }
    }

    /// Stores a section's per-view "gallery" choice (overriding the global
    /// gallery setting for that one section), persists it and rebuilds the
    /// affected overview in the new mode. Called from the sort popover's gallery
    /// toggle; the inline-filter funnel only applies in list mode, so refresh it.
    pub(crate) fn set_section_gallery(
        &mut self,
        section: &str,
        on: bool,
        sender: &ComponentSender<Self>,
    ) {
        let Some(&key) = SORTABLE_SECTIONS.iter().find(|s| **s == section) else {
            return;
        };
        self.libview.section_gallery.insert(key, on);
        let _ = self
            .library
            .set_setting(&format!("gallery_{key}"), if on { "1" } else { "0" });
        match key {
            "albums" => self.reload_albums(),
            "singles" => self.reload_singles(),
            "compilations" => self.reload_compilations(),
            "artists" => self.reload_artists(),
            "concerts" => self.load_concerts(sender),
            "audiobooks" => self.load_audiobooks(sender),
            "favorites" => self.load_favorites(sender),
            "playlists" => self.reload_playlists(sender),
            _ => {}
        }
    }

    /// Drives the shared title-bar sort button from a component page's
    /// [`SortSlot`]: shows it with the page's popover + direction icon, or hides
    /// it. Used for the sections (Podcasts/Streaming/YouTube) that build their
    /// own sort popover off in the component instead of here.
    pub(crate) fn apply_component_sort(&self, slot: &SortSlot) {
        match &*slot.borrow() {
            Some((popover, desc)) => {
                self.nav.sort_btn.set_icon_name(sort_dir_icon(*desc));
                self.nav.sort_btn.set_popover(Some(popover));
                self.nav.sort_btn.set_visible(true);
            }
            None => self.nav.sort_btn.set_visible(false),
        }
    }

    /// (Re)builds the title-bar sort popover for the current section, or hides
    /// the button on sections without a sort control.
    pub(crate) fn rebuild_sort_menu(&self) {
        let section = match self.current_section() {
            Some(s) => s,
            None => {
                self.nav.sort_btn.set_visible(false);
                return;
            }
        };
        // Component pages (their own sort state lives in the component) hand their
        // popover over through a slot; the rest are built from the library state.
        match section.as_str() {
            "podcasts" => return self.apply_component_sort(&self.nav.podcast_sort),
            "streaming" => return self.apply_component_sort(&self.nav.stream_sort),
            "youtube" => return self.apply_component_sort(&self.nav.yt_sort),
            _ => {}
        }
        let crits = section_sort_criteria(&section);
        if crits.is_empty() {
            self.nav.sort_btn.set_visible(false);
            return;
        }
        self.nav.sort_btn.set_visible(true);
        let (active, desc) = self.libview.sort_for(&section);
        // The title-bar icon mirrors the selected direction (asc/desc).
        self.nav.sort_btn.set_icon_name(sort_dir_icon(desc));

        let bx = gtk::Box::new(gtk::Orientation::Vertical, 6);
        bx.set_margin_top(10);
        bx.set_margin_bottom(10);
        bx.set_margin_start(12);
        bx.set_margin_end(12);

        // Heading + direction toggle (ascending/descending).
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let title = gtk::Label::new(Some(&gettext("Sort by")));
        title.add_css_class("heading");
        title.set_xalign(0.0);
        title.set_hexpand(true);
        header.append(&title);
        let dir = gtk::ToggleButton::new();
        dir.add_css_class("flat");
        dir.set_active(desc);
        dir.set_icon_name(sort_dir_icon(desc));
        dir.set_tooltip_text(Some(&if desc {
            gettext("Descending")
        } else {
            gettext("Ascending")
        }));
        {
            let input = self.input.clone();
            dir.connect_toggled(move |b| {
                // Keep icon/tooltip in step without rebuilding (would close it).
                let desc = b.is_active();
                b.set_icon_name(sort_dir_icon(desc));
                b.set_tooltip_text(Some(&if desc {
                    gettext("Descending")
                } else {
                    gettext("Ascending")
                }));
                let _ = input.send(Msg::Sort(SortMsg::SetDir(desc)));
            });
        }
        header.append(&dir);
        bx.append(&header);

        // Criteria as a radio group; the active one reflects the stored choice.
        let mut leader: Option<gtk::CheckButton> = None;
        for &crit in crits {
            let cb = gtk::CheckButton::with_label(&crit.label());
            match &leader {
                Some(l) => cb.set_group(Some(l)),
                None => leader = Some(cb.clone()),
            }
            cb.set_active(crit == active);
            {
                let input = self.input.clone();
                cb.connect_toggled(move |b| {
                    if b.is_active() {
                        let _ = input.send(Msg::Sort(SortMsg::SetCrit(crit)));
                    }
                });
            }
            bx.append(&cb);
        }

        // Set apart at the very bottom: the discreet per-view toggles (grouping
        // and/or gallery), each only for the sections that support it. A single
        // separator keeps them visually off the criteria when either is shown.
        let has_grouping = section_has_grouping(&section);
        let has_gallery = section_has_gallery(&section);
        if has_grouping || has_gallery {
            let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
            sep.set_margin_top(4);
            sep.set_margin_bottom(2);
            bx.append(&sep);
        }
        // "Without grouping": sorts the rows as chosen but without the section
        // headings (the flat look from before grouping existed).
        if has_grouping {
            let no_group = gtk::CheckButton::with_label(&gettext("Without grouping"));
            no_group.add_css_class("dim-label");
            no_group.set_active(self.libview.grouping_off(&section));
            {
                let input = self.input.clone();
                no_group.connect_toggled(move |b| {
                    let _ = input.send(Msg::Sort(SortMsg::SetNoGroup(b.is_active())));
                });
            }
            bx.append(&no_group);
        }
        // Per-view gallery override: shows this one section as a cover grid (or a
        // list) regardless of the global gallery setting. Touching it pins the
        // choice for this section, so a later global toggle leaves it alone.
        if has_gallery {
            let gallery = gtk::CheckButton::with_label(&gettext("Gallery view"));
            gallery.add_css_class("dim-label");
            gallery.set_active(self.libview.gallery_on(&section));
            {
                let input = self.input.clone();
                gallery.connect_toggled(move |b| {
                    let _ = input.send(Msg::Sort(SortMsg::SectionGallery(b.is_active())));
                });
            }
            bx.append(&gallery);
        }

        let popover = gtk::Popover::new();
        popover.set_child(Some(&bx));
        self.nav.sort_btn.set_popover(Some(&popover));
    }

    /// Per-row section headings for the album overview in its current sort order:
    /// the alphabetical initial (`0–9`, `A`, …) when sorting by name, the release
    /// year (or "Unknown year") when sorting by date, and no grouping otherwise.
    /// Same length/order as `albums`, so it indexes the list rows and gallery
    /// tiles 1:1.
    pub(crate) fn album_section_headers(&self, albums: &[AlbumMeta]) -> Option<Vec<String>> {
        self.album_meta_headers("albums", albums)
    }

    /// Per-row section headings for an album-meta overview (albums / singles /
    /// compilations) by the given section's chosen sort.
    pub(crate) fn album_meta_headers(
        &self,
        section: &str,
        albums: &[AlbumMeta],
    ) -> Option<Vec<String>> {
        if self.libview.grouping_off(section) {
            return None;
        }
        match self.libview.sort_for(section).0 {
            SortCrit::Name => Some(albums.iter().map(|a| alpha_header(&a.album)).collect()),
            SortCrit::Release => Some(
                albums
                    .iter()
                    .map(|a| match a.year {
                        Some(y) => y.to_string(),
                        None => gettext("Unknown year"),
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    /// Per-row alphabetical headings for the artist overview when sorting by
    /// name; no grouping for the other criteria. Same length/order as `artists`.
    pub(crate) fn artist_section_headers(&self, artists: &[ArtistMeta]) -> Option<Vec<String>> {
        if self.libview.grouping_off("artists") {
            return None;
        }
        match self.libview.sort_for("artists").0 {
            SortCrit::Name => Some(artists.iter().map(|a| alpha_header(&a.name)).collect()),
            _ => None,
        }
    }

    /// Per-row headings for a concert/audiobook entry list in its current sort:
    /// the alphabetical initial (`0–9`, `A`, …) by name, the release year (from
    /// the track tags, or "Unknown year") by date, and none for length/song-count.
    /// `None` when the user turned grouping off. Same length/order as `items`.
    pub(crate) fn entry_section_headers(
        &self,
        section: &str,
        items: &[(String, String, String, bool)],
    ) -> Option<Vec<String>> {
        if self.libview.grouping_off(section) {
            return None;
        }
        match self.libview.sort_for(section).0 {
            SortCrit::Name => Some(items.iter().map(|e| alpha_header(&e.2)).collect()),
            SortCrit::Release => Some(
                items
                    .iter()
                    .map(|e| match self.entry_year(&e.0, &e.1) {
                        Some(y) => y.to_string(),
                        None => gettext("Unknown year"),
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    /// Orders the album overview in place by the section's chosen sort.
    pub(crate) fn sort_albums(&self, albums: &mut [AlbumMeta]) {
        self.sort_album_metas("albums", albums);
    }

    /// Orders an album-meta overview (albums / singles / compilations) in place
    /// by the given section's chosen sort.
    pub(crate) fn sort_album_metas(&self, section: &str, albums: &mut [AlbumMeta]) {
        let (crit, desc) = self.libview.sort_for(section);
        match crit {
            SortCrit::Name => albums.sort_by_cached_key(|a| natural_key(&a.album)),
            SortCrit::Length => albums.sort_by_key(|a| a.total_duration_ms.unwrap_or(0)),
            // Unknown year groups together at the start (ascending).
            SortCrit::Release => albums.sort_by_key(|a| a.year.unwrap_or(0)),
            SortCrit::Songs => albums.sort_by_key(|a| a.track_count),
            // Not offered for albums – leave the order untouched.
            SortCrit::Manual => {}
        }
        if desc {
            albums.reverse();
        }
    }

    /// Orders the artist overview in place by the section's chosen sort.
    pub(crate) fn sort_artists(&self, artists: &mut [ArtistMeta]) {
        let (crit, desc) = self.libview.sort_for("artists");
        match crit {
            SortCrit::Name => artists.sort_by_cached_key(|a| natural_key(&a.name)),
            SortCrit::Songs => {
                let stats = self.artist_play_stats();
                artists.sort_by_key(|a| stats.get(&norm_key(&a.name)).map_or(0, |s| s.0));
            }
            SortCrit::Length => {
                let stats = self.artist_play_stats();
                artists.sort_by_key(|a| stats.get(&norm_key(&a.name)).map_or(0, |s| s.1));
            }
            // Artists carry no single release year – criterion not offered.
            SortCrit::Release => {}
            // Not offered for artists – leave the order untouched.
            SortCrit::Manual => {}
        }
        if desc {
            artists.reverse();
        }
    }

    /// Orders a concert/audiobook entry list in place by the section's sort.
    pub(crate) fn sort_entries(&self, section: &str, items: &mut [(String, String, String, bool)]) {
        let (crit, desc) = self.libview.sort_for(section);
        match crit {
            SortCrit::Name => items.sort_by_cached_key(|e| natural_key(&e.2)),
            SortCrit::Length => items.sort_by_cached_key(|e| self.entry_stats(&e.0, &e.1).1),
            SortCrit::Songs => items.sort_by_cached_key(|e| self.entry_stats(&e.0, &e.1).0),
            // Year from the entry's track tags; unknown (0) groups first (ascending).
            SortCrit::Release => {
                items.sort_by_cached_key(|e| self.entry_year(&e.0, &e.1).unwrap_or(0))
            }
            // Not offered for concert/audiobook entries – leave the order untouched.
            SortCrit::Manual => {}
        }
        if desc {
            items.reverse();
        }
    }

    /// Orders a file-browser listing by the "files" section's chosen sort, always
    /// keeping folders above files (the browser convention).
    pub(crate) fn sort_fs_entries(&self, entries: &mut [FsEntry]) {
        let (crit, desc) = self.libview.sort_for("files");
        entries.sort_by(|a, b| fs_entry_cmp(a, b, crit, desc));
    }

    /// Re-orders the file browser to the section's chosen sort *without* re-reading
    /// the folder from disk/network: the current rows are pulled out (keeping each
    /// row's backfilled tags + queued flag), re-sorted and repopulated. The queue/
    /// playing markers are then re-applied. Called when the "files" sort changes.
    pub(crate) fn resort_entries(&mut self) {
        let mut rows: Vec<(FsEntry, crate::ui::fs_row::RowOpts, bool)> = {
            let guard = self.libview.entries.guard();
            (0..guard.len())
                .filter_map(|i| guard.get(i).map(|r| (r.entry.clone(), r.opts, r.queued)))
                .collect()
        };
        let (crit, desc) = self.libview.sort_for("files");
        rows.sort_by(|a, b| fs_entry_cmp(&a.0, &b.0, crit, desc));
        // Alphabetical headings (by name) for the re-sorted order.
        let entries: Vec<FsEntry> = rows.iter().map(|(e, _, _)| e.clone()).collect();
        *self.libview.files_headers.borrow_mut() = self.files_section_headers(&entries);
        {
            let mut guard = self.libview.entries.guard();
            guard.clear();
            for row in rows {
                guard.push_back(row);
            }
        }
        self.libview.entries.widget().invalidate_headers();
        self.refresh_queue_icons();
    }

    /// Orders the favorites list in place by the section's chosen sort. Only
    /// `Name` reorders (natural by title); `Manual` keeps the user's drag order
    /// and is handled by the caller (which then skips this). No other criteria.
    pub(crate) fn sort_favorites(&mut self) {
        let (crit, desc) = self.libview.sort_for("favorites");
        if matches!(crit, SortCrit::Name) {
            let items = &mut self.favorites.favorite_items;
            items.sort_by_cached_key(|e| natural_key(&e.2));
            if desc {
                items.reverse();
            }
        }
    }

    /// Per-row alphabetical headings for the playlist overview when sorting by
    /// name; none for the other criteria or when grouping is off. Same length/order
    /// as `playlist_items`.
    pub(crate) fn playlist_section_headers(&self) -> Option<Vec<String>> {
        if self.libview.grouping_off("playlists") {
            return None;
        }
        match self.libview.sort_for("playlists").0 {
            SortCrit::Name => Some(
                self.playlists
                    .playlist_items
                    .iter()
                    .map(|(_, name, _)| alpha_header(name))
                    .collect(),
            ),
            _ => None,
        }
    }

    /// Per-row alphabetical headings for the memo list when sorting by name; none
    /// for date/length or when grouping is off. Same length/order as `memo_items`.
    pub(crate) fn memo_section_headers(&self) -> Option<Vec<String>> {
        if self.libview.grouping_off("memo") {
            return None;
        }
        match self.libview.sort_for("memo").0 {
            SortCrit::Name => Some(
                self.memo
                    .memo_items
                    .iter()
                    .map(|m| alpha_header(&m.title))
                    .collect(),
            ),
            _ => None,
        }
    }

    /// Per-row alphabetical headings for the file browser when sorting by name;
    /// none for the runtime sort or when grouping is off. Folders and files are
    /// each grouped by initial (the folders-first order makes the letters restart
    /// once at the files, which is the intended boundary). Same length/order as the
    /// current `entries` factory.
    pub(crate) fn files_section_headers(&self, entries: &[FsEntry]) -> Option<Vec<String>> {
        if self.libview.grouping_off("files") {
            return None;
        }
        match self.libview.sort_for("files").0 {
            SortCrit::Name => Some(entries.iter().map(|e| alpha_header(e.name())).collect()),
            _ => None,
        }
    }

    /// Orders the playlist overview in place by the section's chosen sort:
    /// by name, by track count, or by total runtime (`durations` keyed by id).
    pub(crate) fn sort_playlists(&mut self, durations: &HashMap<i64, i64>) {
        let (crit, desc) = self.libview.sort_for("playlists");
        let items = &mut self.playlists.playlist_items;
        match crit {
            SortCrit::Name => items.sort_by_cached_key(|(_, name, _)| natural_key(name)),
            SortCrit::Songs => items.sort_by_key(|(_, _, count)| *count),
            SortCrit::Length => {
                items.sort_by_key(|(id, _, _)| durations.get(id).copied().unwrap_or(0))
            }
            // Neither a release year nor a manual order for playlists.
            SortCrit::Release | SortCrit::Manual => {}
        }
        if desc {
            items.reverse();
        }
    }

    /// Orders the memo list in place by the section's chosen sort: by title, by
    /// recording date (`Release`, labelled "Date"; the newest-first default), or
    /// by playback length. Drives both the Recent list and the within-category order.
    pub(crate) fn sort_memos(&mut self) {
        let (crit, desc) = self.libview.sort_for("memo");
        let items = &mut self.memo.memo_items;
        match crit {
            SortCrit::Name => items.sort_by_cached_key(|m| natural_key(&m.title)),
            SortCrit::Release => items.sort_by_key(|m| m.recorded_at),
            SortCrit::Length => items.sort_by_key(|m| m.duration_ms),
            // Memos have neither a song count nor a manual order.
            SortCrit::Songs | SortCrit::Manual => {}
        }
        if desc {
            items.reverse();
        }
    }

    /// Per-artist (track count, summed length in ms), keyed by normalized name.
    /// "feat." credits count for each contributing artist (as in the overview).
    fn artist_play_stats(&self) -> HashMap<String, (i64, i64)> {
        let mut map: HashMap<String, (i64, i64)> = HashMap::new();
        for t in self.library.all_tracks().unwrap_or_default() {
            let Some(artist) = t.artist.as_deref() else {
                continue;
            };
            let ms = t.duration_ms.unwrap_or(0);
            for s in split_artists(artist) {
                let e = map.entry(norm_key(&s)).or_insert((0, 0));
                e.0 += 1;
                e.1 += ms;
            }
        }
        map
    }

    /// Representative release year of a concert/audiobook entry, taken strictly
    /// from the track tag metadata (`track.year` in the DB), never the file
    /// timestamp – consistent with the album overview. For an album/folder the
    /// latest tagged year of its tracks; for a single track its own year. `None`
    /// when no track carries a year. Used for date sorting + the year headings.
    fn entry_year(&self, scope: &str, key: &str) -> Option<i32> {
        let tracks = match scope {
            "album" => {
                let mut parts = key.splitn(2, '\u{1}');
                let artist = parts.next().unwrap_or("");
                let album = parts.next().unwrap_or("");
                self.album_tracks_for_artist(artist, album)
            }
            "folder" => self.folder_tracks_ordered(key),
            "track" => {
                return self
                    .library
                    .track_by_path(key)
                    .ok()
                    .flatten()
                    .and_then(|t| t.year);
            }
            _ => return None,
        };
        tracks.iter().filter_map(|t| t.year).max()
    }

    /// (Track count, summed length in ms) of a concert/audiobook entry – used
    /// only for sorting. Mirrors the resolution of the duration suffix shown in
    /// the entry list.
    fn entry_stats(&self, scope: &str, key: &str) -> (i64, i64) {
        let tracks = match scope {
            "album" => {
                let mut parts = key.splitn(2, '\u{1}');
                let artist = parts.next().unwrap_or("");
                let album = parts.next().unwrap_or("");
                self.album_tracks_for_artist(artist, album)
            }
            "folder" => self.folder_tracks_ordered(key),
            "track" => {
                let ms = self
                    .library
                    .track_by_path(key)
                    .ok()
                    .flatten()
                    .and_then(|t| t.duration_ms)
                    .unwrap_or(0);
                return (1, ms);
            }
            _ => Vec::new(),
        };
        let count = tracks.len() as i64;
        let ms: i64 = tracks.iter().filter_map(|t| t.duration_ms).sum();
        (count, ms)
    }
}

#[cfg(test)]
mod tests {
    use super::alpha_header;
    use crate::ui::app_views::natural_key;

    #[test]
    fn alpha_header_groups_digits_letters_and_symbols() {
        assert_eq!(alpha_header("2Pac"), "0–9");
        assert_eq!(alpha_header("50 Cent"), "0–9");
        assert_eq!(alpha_header("ABBA"), "A");
        assert_eq!(alpha_header("aha"), "A"); // case-folded to the upper initial
        assert_eq!(alpha_header("Östro 430"), "Ö");
        assert_eq!(alpha_header("+44"), "#");
        assert_eq!(alpha_header("  Beatles"), "B"); // leading space ignored
        assert_eq!(alpha_header(""), "#");
    }

    /// The digit and letter groups must each stay contiguous under the natural
    /// sort, so a heading is never emitted twice for the same group.
    #[test]
    fn digit_and_letter_groups_are_contiguous_after_sort() {
        let mut names = vec!["Beatles", "2Pac", "ABBA", "50 Cent", "Coldplay", "aha"];
        names.sort_by_cached_key(|s| natural_key(s));
        // Collapse to the run of headings in sorted order.
        let mut runs: Vec<String> = Vec::new();
        for n in &names {
            let h = alpha_header(n);
            if runs.last() != Some(&h) {
                runs.push(h);
            }
        }
        // No heading repeats (digits first, then A, B, C …).
        assert_eq!(runs, vec!["0–9", "A", "B", "C"]);
    }
}
