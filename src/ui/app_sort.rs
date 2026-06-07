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
        // Keep the inline-filter funnel button in step with the section, too.
        self.update_filter_chrome();
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

        // Set apart at the very bottom: a discreet "no grouping" toggle. Sorts the
        // rows as chosen but without the section headings (the flat look from
        // before grouping existed). A separator keeps it visually off the criteria.
        let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
        sep.set_margin_top(4);
        sep.set_margin_bottom(2);
        bx.append(&sep);
        let no_group = gtk::CheckButton::with_label(&gettext("Without grouping"));
        no_group.add_css_class("dim-label");
        no_group.set_active(self.libview.grouping_off(&section));
        {
            let input = self.input.clone();
            no_group.connect_toggled(move |b| {
                let _ = input.send(Msg::SetSortNoGroup(b.is_active()));
            });
        }
        bx.append(&no_group);

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
        if self.libview.grouping_off("albums") {
            return None;
        }
        match self.libview.sort_for("albums").0 {
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
            // Year from the entry's track tags; unknown (0) groups first (ascending).
            SortCrit::Release => {
                items.sort_by_cached_key(|e| self.entry_year(&e.0, &e.1).unwrap_or(0))
            }
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
