//! Navigation-section metadata (names, icons, descriptions), the per-section
//! sort/grouping/gallery capabilities, the page view enums and the small
//! design-settings accessors. Pure data and lookups, no component logic —
//! split out of `app.rs` and re-exported from `crate::ui::app`.

use std::path::PathBuf;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::i18n::gettext;
use crate::ui::app::{App, Msg};

/// Navigation sections: (stack name, tooltip, icon). The **default** order;
/// the actual display/menu order is stored in `section_order`
/// and can be reordered by the user.
// The labels are English gettext `msgid`s; translate them at the display site
// with `gettext()` (see usage in `build_nav` / `win_title`).
pub(crate) const SECTIONS: [(&str, &str, &str); 14] = [
    ("favorites", "Favorites", "emilia-favorite-symbolic"),
    ("files", "Files", "folder-symbolic"),
    ("artists", "Artists", "avatar-default-symbolic"),
    ("singles", "Singles", "audio-x-generic-symbolic"),
    ("albums", "Albums", "media-optical-symbolic"),
    ("compilations", "Compilations", "view-grid-symbolic"),
    ("concerts", "Concerts", "ticket-special-symbolic"),
    ("podcasts", "Podcasts", "podcast-symbolic"),
    ("streaming", "Streaming", "internet-radio-symbolic"),
    ("youtube", "YouTube", "im-youtube-symbolic"),
    ("audiobooks", "Audiobooks", "emilia-audiobook-symbolic"),
    ("playlists", "Playlists", "view-list-symbolic"),
    ("memo", "Memo", "audio-input-microphone-symbolic"),
    ("stats", "Statistics", "emilia-stats-symbolic"),
];

/// Returns (tooltip/label as msgid, icon) of a section by its
/// stack name. Translate the label at the display site with `gettext()`.
pub(crate) fn section_meta(name: &str) -> Option<(&'static str, &'static str)> {
    SECTIONS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, label, icon)| (*label, *icon))
}

/// One- to two-sentence description of a menu section, already translated.
/// Shown as the secondary text of each row in the setup assistant and the
/// Settings → Menu list. Unknown sections yield an empty string — never pass
/// the fallback through `gettext()`, `gettext("")` returns the catalog header.
/// Each arm calls `gettext()` on its literal so `xtr` extracts the strings.
pub(crate) fn section_description(name: &str) -> String {
    match name {
        "favorites" => gettext("Quick access to the tracks, albums and artists you starred."),
        "files" => gettext("Browse your music folder — and any extra sources — as a file tree."),
        "artists" => gettext("Every artist in your library, each opening to their albums and tracks."),
        "singles" => {
            gettext("Releases by a single artist with just a few tracks, kept apart from full albums.")
        }
        "albums" => gettext("Every album in your library, sortable and grouped by initial or year."),
        "compilations" => {
            gettext("Albums with tracks by several artists, such as samplers and soundtracks.")
        }
        "concerts" => gettext("Live and concert recordings you marked, kept apart from your albums."),
        "podcasts" => gettext("Subscribe to podcast feeds and play or download their episodes."),
        "streaming" => {
            gettext("Internet radio stations, with an optional buffer to record what just played.")
        }
        "youtube" => {
            gettext("Search and play YouTube, follow channels and keep videos offline. Needs the yt-dlp tool.")
        }
        "audiobooks" => {
            gettext("Albums, folders or tracks you marked as audiobooks, resuming where you left off.")
        }
        "playlists" => gettext("Your own playlists, arranged in any order you like."),
        "memo" => gettext("Quick voice notes recorded with the microphone."),
        "stats" => gettext("Listening statistics and your most-played artists and tracks."),
        _ => String::new(),
    }
}

/// Safety prompt before destructive actions (delete/remove). Shows a
/// confirmation dialog relative to `parent` (any widget in the window) and
/// sends `msg` only after confirmation. `confirm_label` labels the
/// (destructive) confirm button, e.g. `gettext("Delete")` / `gettext("Remove")`.
pub(crate) fn confirm_destructive(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    confirm_label: &str,
    sender: ComponentSender<App>,
    msg: Msg,
) {
    let confirm = adw::AlertDialog::new(Some(heading), None);
    confirm.add_response("cancel", &gettext("Cancel"));
    confirm.add_response("ok", confirm_label);
    confirm.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
    confirm.set_default_response(Some("cancel"));
    confirm.set_close_response("cancel");
    // `connect_response` is `Fn`; so take the message only once.
    let msg = std::cell::RefCell::new(Some(msg));
    confirm.connect_response(None, move |_, resp| {
        if resp == "ok" {
            if let Some(m) = msg.borrow_mut().take() {
                sender.input(m);
            }
        }
    });
    confirm.present(Some(parent));
}

/// Re-exec the app in place (replace the process image) so gettext re-reads the
/// chosen UI language at startup — the language can only be picked up on a fresh
/// start. Uses `exec()` rather than spawn + exit because under Flatpak this
/// process is PID 1 of the sandbox's PID namespace: exiting it makes the kernel
/// kill every other process in the namespace, including a freshly spawned child,
/// leaving the app simply gone. `exec()` keeps the same PID, so the sandbox
/// stays alive and the new image starts. Only returns (via the spawn fallback)
/// if `exec()` itself fails; otherwise it never returns.
pub(crate) fn relaunch_for_language_change() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&exe).exec();
        // exec() only returns on failure; fall back to spawn.
        tracing::error!("re-exec for language change failed: {err}");
        let _ = std::process::Command::new(&exe).spawn();
    }
    std::process::exit(0);
}

/// Theme suffix for the per-theme design settings (the appearance — background +
/// colours — is stored separately for light and dark).
pub(crate) fn design_theme_suffix() -> &'static str {
    if adw::StyleManager::default().is_dark() {
        "dark"
    } else {
        "light"
    }
}

/// Read a per-theme design setting (`<base>_light`/`<base>_dark`), falling back
/// to the legacy global `<base>` key so values from before the split carry over
/// into both themes until the user changes them.
fn get_design(lib: &Library, base: &str) -> Option<String> {
    lib.get_setting(&format!("{base}_{}", design_theme_suffix()))
        .ok()
        .flatten()
        .or_else(|| lib.get_setting(base).ok().flatten())
}

/// Write a per-theme design setting under the current theme's key.
pub(crate) fn set_design(lib: &Library, base: &str, value: &str) {
    let _ = lib.set_setting(&format!("{base}_{}", design_theme_suffix()), value);
}

/// Build the [`DesignSettings`] for the *current* theme from the DB. Used at
/// startup and again whenever the light/dark theme flips (see `reload_design`),
/// so each theme keeps its own appearance.
pub(crate) fn read_design_settings(lib: &Library) -> crate::ui::theme::DesignSettings {
    // Fresh-install defaults differ per theme (the maintainer's shipped look):
    // dark gets a deep indigo field tint; light a plain white one that is a
    // touch more see-through. Both use a barely-there blur on the built-in
    // concert background.
    let dark = adw::StyleManager::default().is_dark();
    // No explicit filter set: default to a gentle Soft blur on the built-in
    // concert background.
    let bg_filter = match get_design(lib, "design_bg_filter").filter(|s| !s.is_empty()) {
        Some(k) => crate::ui::theme::BgFilter::from_key(&k),
        None => crate::ui::theme::BgFilter::Soft,
    };
    // Soft moved from a coarse 0..10 strength scale to a fine 0..30 one. A value
    // stored under the old scale is converted once — flagged per theme, since
    // every design setting is per theme — so an existing setup keeps its blur.
    let saved_strength =
        get_design(lib, "design_bg_filter_strength").and_then(|s| s.parse::<u32>().ok());
    let legacy_soft = bg_filter == crate::ui::theme::BgFilter::Soft
        && get_design(lib, "design_bg_soft_scale").is_none();
    let bg_filter_strength = match saved_strength {
        Some(v) if legacy_soft => {
            let v = crate::ui::theme::soft_strength_from_legacy(v.min(100));
            set_design(lib, "design_bg_filter_strength", &v.to_string());
            v
        }
        Some(v) => v.min(100),
        None => crate::ui::theme::SOFT_STRENGTH_DEFAULT,
    };
    if legacy_soft {
        set_design(lib, "design_bg_soft_scale", "1");
    }
    crate::ui::theme::DesignSettings {
        background_on: get_design(lib, "design_background_on")
            .map(|s| s != "0")
            .unwrap_or(true),
        custom_bg: get_design(lib, "design_bg_path")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
        use_cover_bg: matches!(get_design(lib, "design_use_cover_bg").as_deref(), Some("1")),
        bg_filter,
        bg_filter_strength,
        bg_nav: get_design(lib, "design_bg_nav")
            .map(|s| s != "0")
            .unwrap_or(true),
        bg_titlebar: get_design(lib, "design_bg_titlebar")
            .map(|s| s != "0")
            .unwrap_or(true),
        text_color: get_design(lib, "design_text_color").filter(|s| !s.is_empty()),
        entry_bg_on: get_design(lib, "design_entry_bg")
            .map(|s| s != "0")
            .unwrap_or(true),
        // Default field tint for a fresh install. `None` = never set; `Some("")`
        // = explicitly cleared via the reset button, which must stay cleared.
        field_color: match get_design(lib, "design_field_color") {
            None => Some(if dark { "#241f31" } else { "#ffffff" }.to_string()),
            Some(s) if s.is_empty() => None,
            Some(s) => Some(s),
        },
        field_transparency: get_design(lib, "design_chrome_transparency")
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(if dark { 60 } else { 90 })
            .min(100),
    }
}

/// Cadence of the quiet background backfill of missing artist photos & covers.
/// Deliberately low (~1 min) so new users quickly get an enriched overview;
/// the worker throttles the actual network requests itself.
pub(crate) const AUTO_ENRICH_INTERVAL_SECS: u32 = 60;

/// Which view the podcast page shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PodcastView {
    /// Recently (partly) heard episodes — "continue listening", with progress.
    Recent,
    /// Newest episodes (entries) across all subscriptions.
    Newest,
    /// Overview of the subscribed podcasts.
    Overview,
}

/// Which view the streaming page shows (tab switcher).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamView {
    /// Saved stations/channels.
    Channels,
    /// Timeshift recordings.
    Recordings,
    /// Songs recognized while streaming ("Recently heard").
    Heard,
}

/// What the (shared) waveform editor is currently editing. The editor body is
/// generic over "an audio file with a path"; this only distinguishes where the
/// item is looked up and where the cut result is written back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditKind {
    /// A radio timeshift recording (`recording` table).
    Recording,
    /// A voice memo (`memo` table).
    Memo,
}

/// Which view the Memo page shows (tab switcher): a flat "Recent" list or a
/// "Category" tree (categories alphanumeric, their memos nested underneath).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoView {
    Recent,
    Category,
}

/// Which view the YouTube page shows (tab switcher).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum YtView {
    /// Newest videos across all subscribed channels.
    Newest,
    /// Recently played videos (history).
    Recent,
    /// Overview of the subscribed channels.
    Channels,
}

/// Time period of the listening statistics. Deliberately sliding windows
/// (instead of a calendar year) – calendar-free and without an extra date dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatsPeriod {
    /// Last 4 weeks.
    Weeks4,
    /// Last 12 months.
    Year,
    /// Since the beginning.
    All,
}

/// A sort criterion of a library overview, chosen via the sort popover in the
/// title bar. Not every category offers every criterion (see
/// [`section_sort_criteria`]); the direction (asc/desc) is tracked per category
/// in [`LibView::sort`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortCrit {
    /// By name/title (natural order).
    Name,
    /// By the summed playback length of all tracks.
    Length,
    /// By the release year.
    Release,
    /// By the number of songs.
    Songs,
    /// Keep the user's own drag order (favorites). No sort is applied; the
    /// reorder handles stay active. Only offered where a manual order exists.
    Manual,
}

impl SortCrit {
    /// Stable token for persisting the choice in the settings DB.
    pub(crate) fn as_key(self) -> &'static str {
        match self {
            SortCrit::Name => "name",
            SortCrit::Length => "length",
            SortCrit::Release => "release",
            SortCrit::Songs => "songs",
            SortCrit::Manual => "manual",
        }
    }

    /// Parse the persisted token; falls back to [`SortCrit::Name`].
    pub(crate) fn from_key(s: &str) -> Self {
        match s {
            "length" => SortCrit::Length,
            "release" => SortCrit::Release,
            "songs" => SortCrit::Songs,
            "manual" => SortCrit::Manual,
            _ => SortCrit::Name,
        }
    }

    /// Localized label shown in the sort popover.
    pub(crate) fn label(self) -> String {
        match self {
            SortCrit::Name => gettext("Name"),
            SortCrit::Length => gettext("Length"),
            // Release year; sorting by it groups the album list under year headings.
            SortCrit::Release => gettext("Date"),
            SortCrit::Songs => gettext("Number of songs"),
            SortCrit::Manual => gettext("Custom order"),
        }
    }
}

/// The library sections that offer a sort control (with their own remembered
/// criterion + direction). The cover/entry overviews additionally group + offer
/// a gallery (see [`section_has_grouping`]/[`section_has_gallery`]); the flat
/// lists (playlists, memos) only sort. Files/Podcasts/YouTube/Stats have none.
pub(crate) const SORTABLE_SECTIONS: &[&str] = &[
    "files",
    "artists",
    "albums",
    "singles",
    "compilations",
    "concerts",
    "audiobooks",
    "favorites",
    "playlists",
    "memo",
];

/// The criteria a given section offers, in popover order. Category-appropriate:
/// artists carry no single release year, so they omit [`SortCrit::Release`];
/// albums/concerts/audiobooks derive a year from their tracks' tag metadata.
/// Playlists sort by name/track-count/runtime; memos by name/recording-date/length.
pub(crate) fn section_sort_criteria(section: &str) -> &'static [SortCrit] {
    use SortCrit::*;
    match section {
        // File browser: by name or by runtime; folders stay above files either way.
        "files" => &[Name, Length],
        "albums" | "singles" | "compilations" | "concerts" | "audiobooks" => {
            &[Name, Length, Release, Songs]
        }
        "artists" => &[Name, Songs, Length],
        "playlists" => &[Name, Songs, Length],
        // For memos `Release` is the recording date (label "Date"); no song count.
        "memo" => &[Name, Release, Length],
        // Favorites keep a manual drag order (the default); name is the alternative.
        "favorites" => &[Manual, Name],
        _ => &[],
    }
}

/// Whether a section offers the alphabetical "without grouping" toggle (section
/// headings) in its sort popover. Only the cover/entry overviews group; the flat
/// lists (playlists, memos) sort without headings.
pub(crate) fn section_has_grouping(section: &str) -> bool {
    matches!(
        section,
        "artists"
            | "albums"
            | "singles"
            | "compilations"
            | "concerts"
            | "audiobooks"
            | "favorites"
            | "playlists"
            | "memo"
            | "files"
    )
}

/// Whether a section offers the per-view "gallery" toggle in its sort popover.
/// Only the cover/photo overviews have a gallery variant (memos/files/recordings
/// carry no covers, so they group but never offer a gallery).
pub(crate) fn section_has_gallery(section: &str) -> bool {
    matches!(
        section,
        "artists"
            | "albums"
            | "singles"
            | "compilations"
            | "concerts"
            | "audiobooks"
            | "favorites"
            | "playlists"
    )
}

#[cfg(test)]
mod tests {
    use super::{section_description, SECTIONS};

    /// Every menu section needs a subtitle for the setup assistant and
    /// Settings → Menu. A missing arm used to fall through to the empty
    /// string, which — passed through `gettext()` — rendered the whole
    /// catalog header as the row's subtitle.
    #[test]
    fn every_section_has_a_description() {
        for (name, _, _) in SECTIONS {
            assert!(
                !section_description(name).is_empty(),
                "section {name:?} has no description"
            );
        }
    }

    /// The empty fallback must stay untranslated — `gettext("")` returns the
    /// catalog header, not an empty string.
    #[test]
    fn unknown_section_yields_empty_description() {
        assert_eq!(section_description("does-not-exist"), "");
    }
}
