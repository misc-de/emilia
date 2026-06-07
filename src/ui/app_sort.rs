//! Per-category sorting of the library overviews (Artists/Albums/Concerts/
//! Audiobooks). Each section keeps its own criterion + direction (see
//! [`crate::ui::app::LibView::sort`]); the title-bar sort popover is rebuilt
//! per section here, and the reload functions call the matching `sort_*`.

use std::collections::HashMap;

use adw::prelude::*;
use relm4::gtk;
use relm4::prelude::*;

use crate::core::artist::{norm_key, split_artists};
use crate::i18n::gettext;
use crate::model::{AlbumMeta, ArtistMeta};
use crate::ui::app::{section_sort_criteria, App, Msg, SortCrit, SORTABLE_SECTIONS};
use crate::ui::app_views::natural_key;

/// Icon for a sort direction – shared by the title-bar button and the popover
/// toggle so they always match.
pub(crate) fn sort_dir_icon(desc: bool) -> &'static str {
    if desc {
        "view-sort-descending-symbolic"
    } else {
        "view-sort-ascending-symbolic"
    }
}

impl App {
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
            "albums" => self.reload_albums(),
            "artists" => self.reload_artists(),
            "concerts" => self.load_concerts(sender),
            "audiobooks" => self.load_audiobooks(sender),
            _ => {}
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
                let _ = input.send(Msg::SetSortDir(desc));
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
                        let _ = input.send(Msg::SetSortCrit(crit));
                    }
                });
            }
            bx.append(&cb);
        }

        let popover = gtk::Popover::new();
        popover.set_child(Some(&bx));
        self.nav.sort_btn.set_popover(Some(&popover));
    }

    /// Fills the album gallery. When `group` (date sort) is set, the gallery box
    /// holds year-grouped sections (a heading + a `FlowBox` per year); otherwise
    /// a single grid. `items`/`albums` are in the same sorted order, so the
    /// per-section base index maps back to the full overview for clicks.
    pub(crate) fn fill_albums_gallery(
        &self,
        items: &[(Option<String>, &'static str, String)],
        albums: &[AlbumMeta],
        group: bool,
    ) {
        let bx = &self.libview.albums_gallery_box;
        while let Some(c) = bx.first_child() {
            bx.remove(&c);
        }
        if !group {
            let fb = &self.libview.albums_gallery;
            bx.append(fb);
            self.fill_gallery(fb, items, Msg::ShowAlbumTracks, Msg::ShowAlbumDetail);
            return;
        }
        let mut i = 0;
        while i < albums.len() {
            let year = albums[i].year;
            let mut j = i;
            while j < albums.len() && albums[j].year == year {
                j += 1;
            }
            bx.append(&crate::ui::app_gallery::year_header_label(year));
            let fb = gtk::FlowBox::new();
            bx.append(&fb);
            self.fill_gallery_into(
                &fb,
                &items[i..j],
                i,
                Msg::ShowAlbumTracks,
                Msg::ShowAlbumDetail,
                false,
            );
            i = j;
        }
    }

    /// Orders the album overview in place by the section's chosen sort.
    pub(crate) fn sort_albums(&self, albums: &mut [AlbumMeta]) {
        let (crit, desc) = self.libview.sort_for("albums");
        match crit {
            SortCrit::Name => albums.sort_by_cached_key(|a| natural_key(&a.album)),
            SortCrit::Length => albums.sort_by_key(|a| a.total_duration_ms.unwrap_or(0)),
            // Unknown year groups together at the start (ascending).
            SortCrit::Release => albums.sort_by_key(|a| a.year.unwrap_or(0)),
            SortCrit::Songs => albums.sort_by_key(|a| a.track_count),
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
            // Release date isn't reliably available for these entries.
            SortCrit::Release => {}
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
