use std::path::PathBuf;

use adw::prelude::*;
use relm4::gtk;

use crate::core::db::Library;
use crate::core::scanner;
use crate::i18n::ngettext_n;
use crate::model::Track;
use crate::ui::fs_row::FsEntry;

/// Before this position no resume is remembered (too close to the start).
const RESUME_MIN_POS_MS: i64 = 5_000;
/// This close to the end the track counts as finished -> reset resume to 0.
const RESUME_END_GUARD_MS: i64 = 10_000;

/// Current time in Unix seconds (for the listening statistics timestamps).
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Applies the color scheme ("system"/"dark"/"light") via the global
/// libadwaita StyleManager. "system" follows the desktop setting.
pub(crate) fn apply_color_scheme(code: &str) {
    let scheme = match code {
        "dark" => adw::ColorScheme::ForceDark,
        "light" => adw::ColorScheme::ForceLight,
        _ => adw::ColorScheme::Default,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

/// Default gallery tiles-per-row when the user has not chosen one yet: 3 on
/// phone-sized screens, 4 on the desktop.
pub(crate) fn initial_gallery_columns() -> u32 {
    let mobile = gtk::gdk::Display::default()
        .and_then(|d| d.monitors().item(0))
        .and_then(|obj| obj.downcast::<gtk::gdk::Monitor>().ok())
        .map(|mon| {
            let g = mon.geometry();
            g.width().min(g.height()) <= 550
        })
        .unwrap_or(false);
    if mobile {
        3
    } else {
        4
    }
}

/// Resume position with guards: near start or end it is set to 0,
/// so a nearly finished track starts from the beginning next time.
pub(crate) fn guarded_resume(pos_ms: i64, dur_ms: i64) -> i64 {
    let too_early = pos_ms < RESUME_MIN_POS_MS;
    let too_late = dur_ms > 0 && pos_ms > dur_ms - RESUME_END_GUARD_MS;
    if too_early || too_late {
        0
    } else {
        pos_ms
    }
}

/// Saves the window size/maximization and the most recently open navigation item
/// (own short-lived DB connection, since called in the close handler).
pub(crate) fn save_window_state(width: i32, height: i32, maximized: bool, section: Option<&str>) {
    if let Ok(lib) = Library::open() {
        let _ = lib.set_setting("win_width", &width.to_string());
        let _ = lib.set_setting("win_height", &height.to_string());
        let _ = lib.set_setting("win_maximized", if maximized { "1" } else { "0" });
        if let Some(sec) = section {
            let _ = lib.set_setting("active_section", sec);
        }
    }
}

/// Formats milliseconds as `m:ss` or `h:mm:ss` (negative -> 0).
pub(crate) fn fmt_duration(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Formats a playback rate compactly: `1.0 -> "1x"`, `1.5 -> "1.5x"`, `0.25 -> "0.25x"`.
pub(crate) fn fmt_rate(rate: f64) -> String {
    let s = format!("{rate:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}×")
}

/// Whether an online fetch makes sense: simply whether there is any connection
/// at all.
pub(crate) fn online_available() -> bool {
    use gtk::gio::prelude::NetworkMonitorExt;
    gtk::gio::NetworkMonitor::default().is_network_available()
}

/// Most common artist designation (raw tag string) of a set of tracks.
pub(crate) fn most_common_artist(tracks: &[Track]) -> String {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for t in tracks {
        if let Some(a) = t.artist.as_deref() {
            *counts.entry(a).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(a, _)| a.to_string())
        .unwrap_or_default()
}

/// Subtitle of an album row: "year · N songs" (year only if known).
pub(crate) fn album_subtitle(year: Option<i32>, track_count: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(y) = year {
        parts.push(y.to_string());
    }
    parts.push(ngettext_n("{n} song", "{n} songs", track_count as u32));
    parts.join(" · ")
}

/// Secondary line of an artist row: "N albums · M songs". The albums part is
/// omitted when the artist has no album (e.g. only loose tracks).
pub(crate) fn artist_count_subtitle(albums: u32, songs: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    if albums > 0 {
        parts.push(ngettext_n("{n} album", "{n} albums", albums));
    }
    parts.push(ngettext_n("{n} song", "{n} songs", songs));
    parts.join(" · ")
}

/// Right-aligned, subtle duration label for a track row.
pub(crate) fn duration_label(ms: i64) -> gtk::Label {
    gtk::Label::builder()
        .label(fmt_duration(ms))
        .css_classes(["dim-label", "numeric"])
        .build()
}

/// First `ScrolledWindow` in the widget subtree (depth-first search), e.g. to
/// find the scroll position of the currently visible overview section.
pub(crate) fn find_scroller(widget: &gtk::Widget) -> Option<gtk::ScrolledWindow> {
    if !widget.is_visible() {
        return None;
    }
    if let Some(sc) = widget.downcast_ref::<gtk::ScrolledWindow>() {
        return Some(sc.clone());
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(sc) = find_scroller(&c) {
            return Some(sc);
        }
        child = c.next_sibling();
    }
    None
}

pub(crate) fn cover_widget(path: Option<&str>, placeholder: &str) -> gtk::Widget {
    let texture = path.and_then(crate::ui::widgets::thumb_cached);
    crate::ui::widgets::rounded_image(texture.as_ref(), placeholder, 48)
}

/// If `sub` is a single-album folder (and not a known artist folder), returns
/// its album info (artist, album) so the row can offer a "play album" button.
/// `in_dir` are the library tracks already scoped to `sub`.
fn dir_album_info(
    lib: &Library,
    sub: &std::path::Path,
    in_dir: &[&Track],
) -> Option<crate::ui::fs_row::DirAlbum> {
    let name = sub.file_name().and_then(|n| n.to_str()).unwrap_or("");
    // A folder named like a known artist is shown as an artist, not an album.
    if matches!(lib.get_artist_meta(name), Ok(Some(_))) {
        return None;
    }
    // Exactly one distinct album in the folder → treat it as an album.
    let mut album: Option<&str> = None;
    for t in in_dir {
        if let Some(a) = t.album.as_deref().filter(|s| !s.is_empty()) {
            match album {
                None => album = Some(a),
                Some(x) if x == a => {}
                _ => return None,
            }
        }
    }
    let album = album?.to_string();
    let artist = in_dir
        .iter()
        .find_map(|t| t.artist.clone())
        .unwrap_or_default();
    Some(crate::ui::fs_row::DirAlbum { artist, album })
}

/// Reads subfolders and audio files of a folder (folders first, sorted).
/// Runs in a background thread - may therefore block.
pub(crate) fn read_entries(dir: PathBuf) -> Vec<FsEntry> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if scanner::is_audio(&path) {
                files.push(path);
            }
        }
    }
    dirs.sort();
    files.sort();

    let lib = Library::open().ok();
    // Load the library once so folders can show their summed runtime (and album
    // folders a play button). Scoped to tracks under the current folder; skipped
    // when there are no subfolders to classify.
    let all_tracks = if dirs.is_empty() {
        Vec::new()
    } else {
        lib.as_ref().and_then(|l| l.all_tracks().ok()).unwrap_or_default()
    };
    let under: Vec<&Track> = all_tracks
        .iter()
        .filter(|t| std::path::Path::new(&t.path).starts_with(&dir))
        .collect();
    let mut out = Vec::with_capacity(dirs.len() + files.len());
    for d in dirs {
        let visible = match &lib {
            Some(lib) => lib
                .folder_areas(&d.to_string_lossy())
                .contains(&crate::core::category::Area::Filesystem),
            None => true,
        };
        if visible {
            // Tracks under this subfolder → its summed runtime (shown for every
            // folder with songs) and single-album detection (for the play button).
            let in_dir: Vec<&Track> = under
                .iter()
                .copied()
                .filter(|t| std::path::Path::new(&t.path).starts_with(&d))
                .collect();
            let total_ms = in_dir.iter().filter_map(|t| t.duration_ms).sum();
            let album = lib.as_ref().and_then(|l| dir_album_info(l, &d, &in_dir));
            out.push(FsEntry::dir_album(d, album, total_ms));
        }
    }
    for f in files {
        let visible = match &lib {
            Some(lib) => match lib.track_by_path(&f.to_string_lossy()).ok().flatten() {
                Some(t) => lib
                    .resolve_areas(t.artist.as_deref(), t.album.as_deref(), &t.path)
                    .contains(&crate::core::category::Area::Filesystem),
                None => true,
            },
            None => true,
        };
        if visible {
            out.push(FsEntry::file(f));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{fmt_duration, guarded_resume};

    #[test]
    fn guarded_resume_clamps_start_and_end() {
        let dur = 3_600_000; // 1 h
        assert_eq!(guarded_resume(1_000_000, dur), 1_000_000);
        assert_eq!(guarded_resume(3_000, dur), 0);
        assert_eq!(guarded_resume(dur - 5_000, dur), 0);
        assert_eq!(guarded_resume(1_000_000, 0), 1_000_000);
    }

    #[test]
    fn fmt_duration_formats_minutes_and_hours() {
        assert_eq!(fmt_duration(0), "0:00");
        assert_eq!(fmt_duration(5_000), "0:05");
        assert_eq!(fmt_duration(65_000), "1:05");
        assert_eq!(fmt_duration(600_000), "10:00");
        assert_eq!(fmt_duration(3_661_000), "1:01:01");
        assert_eq!(fmt_duration(-1), "0:00");
    }
}
