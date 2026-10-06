//! GTK-free logic of the [`crate::ui::stream_page`]: subtitles and other
//! data → display-text mapping, the per-sub-view sort orders and alphabetical
//! headings, setting parsing and the library destination of a recording. Kept
//! free of widget types so it can be unit-tested without a display.

use std::path::PathBuf;

use crate::core::streaming::StationResult;
use crate::model::{HeardItem, RecordingItem, StreamItem};
use crate::ui::app::{SortCrit, StreamView};
use crate::ui::app_sort::alpha_header;
use crate::ui::app_views::natural_key;

/// `s` unless it is missing or blank (whitespace only).
pub(super) fn nonblank(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.trim().is_empty())
}

/// The first of `primary` / `fallback` that is not blank.
pub(super) fn first_nonblank(primary: Option<String>, fallback: Option<String>) -> Option<String> {
    primary
        .filter(|a| !a.trim().is_empty())
        .or(fallback)
        .filter(|a| !a.trim().is_empty())
}

/// At most `max` of the comma-separated `tags`, trimmed, empty ones skipped,
/// joined with " · ". `None` when nothing is left.
pub(super) fn tag_list(tags: Option<&str>, max: usize) -> Option<String> {
    let t = nonblank(tags)?;
    let tags: Vec<&str> = t
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .take(max)
        .collect();
    (!tags.is_empty()).then(|| tags.join(" · "))
}

/// Subtitle of a station: genre/country, as far as available.
pub(super) fn stream_subtitle(st: &StreamItem) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(tags) = tag_list(st.tags.as_deref(), 3) {
        parts.push(tags);
    }
    if let Some(c) = nonblank(st.country.as_deref()) {
        parts.push(c.to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" — "))
    }
}

/// Subtitle of a directory search hit: country first, then up to two tags.
pub(super) fn search_result_subtitle(r: &StationResult) -> Option<String> {
    let mut sub: Vec<String> = Vec::new();
    if let Some(c) = nonblank(r.country.as_deref()) {
        sub.push(c.to_string());
    }
    if let Some(tags) = tag_list(r.tags.as_deref(), 2) {
        sub.push(tags);
    }
    (!sub.is_empty()).then(|| sub.join(" — "))
}

/// Subtitle of a recording / recognized-song row: artist and station (each
/// only when not blank), then `tail` (a date or "Recording …"), joined by " · ".
pub(super) fn song_subtitle(artist: Option<&str>, station: Option<&str>, tail: &str) -> String {
    let mut sub: Vec<&str> = Vec::new();
    if let Some(a) = nonblank(artist) {
        sub.push(a);
    }
    if let Some(s) = nonblank(station) {
        sub.push(s);
    }
    sub.push(tail);
    sub.join(" · ")
}

/// `(artist, title)` of the live "currently recording" entry: the best guess
/// parsed from the station's ICY title, or `fallback` while no title is known.
pub(super) fn live_entry_title(
    current_title: Option<&str>,
    station: Option<&str>,
    fallback: &str,
) -> (Option<String>, String) {
    match current_title {
        Some(t) => crate::core::online::recording_query_candidates(t, station)
            .into_iter()
            .next()
            .unwrap_or((None, t.trim().to_string())),
        None => (None, fallback.to_string()),
    }
}

/// Placeholder icon of a recording row without a cover (incomplete recordings
/// get a distinct one).
pub(super) fn recording_placeholder(incomplete: bool) -> &'static str {
    if incomplete {
        "media-playlist-consecutive-symbolic"
    } else {
        "audio-x-generic-symbolic"
    }
}

/// Key fragment of a sub-view's persisted sort/grouping settings
/// ("sort_<key>[_desc]", "nogroup_<key>").
pub(super) fn view_setting_key(view: StreamView) -> &'static str {
    match view {
        StreamView::Channels => "stations",
        StreamView::Recordings => "recordings",
        StreamView::Heard => "heard",
    }
}

/// A logo URL as entered: trimmed, `None` when empty (= remove the logo).
pub(super) fn normalize_logo_url(url: Option<String>) -> Option<String> {
    url.map(|u| u.trim().to_string()).filter(|u| !u.is_empty())
}

/// Tiles per gallery row from the stored "gallery_columns" setting (default 4,
/// clamped to 2..=8).
pub(super) fn parse_gallery_columns(raw: Option<&str>) -> u32 {
    raw.and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(4)
        .clamp(2, 8)
}

/// Per-row alphabetical headings (by name) for the stations list; none when
/// grouping is off (stations only ever sort by name).
pub(super) fn station_headers(items: &[StreamItem], no_group: bool) -> Option<Vec<String>> {
    if no_group {
        return None;
    }
    Some(items.iter().map(|s| alpha_header(&s.name)).collect())
}

/// Per-row alphabetical headings (by name) for the recordings list; none for
/// the date/length sorts or when grouping is off. The live entry has no row
/// here (it is prepended separately), so the labels align with the saved rows
/// only — which is why grouping is cleared while a live recording shows.
pub(super) fn recording_headers(
    items: &[RecordingItem],
    crit: SortCrit,
    no_group: bool,
    live: bool,
) -> Option<Vec<String>> {
    if no_group || live {
        return None;
    }
    match crit {
        SortCrit::Name => Some(items.iter().map(|r| alpha_header(&r.title)).collect()),
        _ => None,
    }
}

/// Per-row alphabetical headings (by title) for the "Recently heard" list;
/// none for the date sort or when grouping is off.
pub(super) fn heard_headers(
    items: &[HeardItem],
    crit: SortCrit,
    no_group: bool,
) -> Option<Vec<String>> {
    if no_group {
        return None;
    }
    match crit {
        SortCrit::Name => Some(items.iter().map(|h| alpha_header(&h.title)).collect()),
        _ => None,
    }
}

/// Orders the stations by name (natural order); the direction applies.
pub(super) fn sort_stations(items: &mut [StreamItem], desc: bool) {
    items.sort_by_cached_key(|s| natural_key(&s.name));
    if desc {
        items.reverse();
    }
}

/// Orders the recordings by length, recording date or (otherwise) name.
pub(super) fn sort_recordings(items: &mut [RecordingItem], crit: SortCrit, desc: bool) {
    match crit {
        SortCrit::Length => items.sort_by_key(|r| r.duration_ms),
        SortCrit::Release => items.sort_by_key(|r| r.recorded_at),
        // Name is the remaining criterion.
        _ => items.sort_by_cached_key(|r| natural_key(&r.title)),
    }
    if desc {
        items.reverse();
    }
}

/// Orders the recognized songs by last-heard date or (otherwise) title.
pub(super) fn sort_heard(items: &mut [HeardItem], crit: SortCrit, desc: bool) {
    match crit {
        SortCrit::Release => items.sort_by_key(|h| h.heard_at),
        // Name is the remaining criterion.
        _ => items.sort_by_cached_key(|h| natural_key(&h.title)),
    }
    if desc {
        items.reverse();
    }
}

/// Where a recording is copied in the music library:
/// `<music_dir>/<artist or "Recordings">/[<album>/]<title>.<ext>`, each
/// component sanitized; blank artist/album count as missing.
pub(super) fn library_dest(
    music_dir: &str,
    artist: Option<&str>,
    album: Option<&str>,
    title: &str,
    ext: &str,
) -> PathBuf {
    use crate::core::youtube::sanitize_filename;
    let mut dest = PathBuf::from(music_dir);
    match nonblank(artist) {
        Some(a) => dest.push(sanitize_filename(a)),
        None => dest.push("Recordings"),
    }
    if let Some(al) = nonblank(album) {
        dest.push(sanitize_filename(al));
    }
    dest.push(format!("{}.{ext}", sanitize_filename(title)));
    dest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn station(tags: Option<&str>, country: Option<&str>) -> StreamItem {
        StreamItem {
            id: 1,
            name: "Radio".into(),
            url: "http://example.invalid/stream".into(),
            favicon: None,
            tags: tags.map(str::to_string),
            country: country.map(str::to_string),
        }
    }

    fn named(id: i64, name: &str) -> StreamItem {
        StreamItem {
            id,
            name: name.into(),
            ..station(None, None)
        }
    }

    fn result(tags: Option<&str>, country: Option<&str>) -> StationResult {
        StationResult {
            name: "Radio".into(),
            url: "http://example.invalid/stream".into(),
            favicon: None,
            tags: tags.map(str::to_string),
            country: country.map(str::to_string),
            codec: None,
            bitrate: None,
        }
    }

    fn rec(id: i64, title: &str, recorded_at: i64, duration_ms: i64) -> RecordingItem {
        RecordingItem {
            id,
            path: format!("/rec/{id}.mp3"),
            artist: None,
            title: title.into(),
            station: None,
            recorded_at,
            duration_ms,
            incomplete: false,
        }
    }

    fn heard(id: i64, title: &str, heard_at: i64) -> HeardItem {
        HeardItem {
            id,
            artist: None,
            title: title.into(),
            station: None,
            heard_at,
            count: 1,
        }
    }

    fn ids<T>(items: &[T], id: impl Fn(&T) -> i64) -> Vec<i64> {
        items.iter().map(id).collect()
    }

    #[test]
    fn no_tags_and_no_country_means_no_subtitle() {
        assert_eq!(stream_subtitle(&station(None, None)), None);
        assert_eq!(stream_subtitle(&station(Some("  "), Some(""))), None);
        assert_eq!(stream_subtitle(&station(Some(", ,"), None)), None);
    }

    #[test]
    fn tags_are_trimmed_and_capped_at_three() {
        assert_eq!(
            stream_subtitle(&station(Some("rock, pop"), None)),
            Some("rock · pop".into())
        );
        assert_eq!(
            stream_subtitle(&station(Some("a,b,c,d,e"), None)),
            Some("a · b · c".into())
        );
        assert_eq!(
            stream_subtitle(&station(Some(", ,rock,"), None)),
            Some("rock".into())
        );
    }

    #[test]
    fn country_is_appended_after_a_dash() {
        assert_eq!(
            stream_subtitle(&station(Some("rock, pop"), Some("Germany"))),
            Some("rock · pop — Germany".into())
        );
        assert_eq!(
            stream_subtitle(&station(None, Some("Germany"))),
            Some("Germany".into())
        );
        assert_eq!(
            stream_subtitle(&station(Some("  "), Some("Germany"))),
            Some("Germany".into())
        );
    }

    #[test]
    fn tag_list_honours_the_cap() {
        assert_eq!(tag_list(Some("a, b, c"), 2), Some("a · b".into()));
        assert_eq!(tag_list(Some(" , "), 2), None);
        assert_eq!(tag_list(None, 2), None);
    }

    #[test]
    fn search_result_subtitle_puts_country_first_and_caps_tags_at_two() {
        assert_eq!(search_result_subtitle(&result(None, None)), None);
        assert_eq!(
            search_result_subtitle(&result(Some(" , "), Some(" "))),
            None
        );
        assert_eq!(
            search_result_subtitle(&result(Some("jazz, soul, funk"), Some("France"))),
            Some("France — jazz · soul".into())
        );
        assert_eq!(
            search_result_subtitle(&result(Some("jazz"), None)),
            Some("jazz".into())
        );
    }

    #[test]
    fn song_subtitle_skips_blank_parts_but_keeps_the_tail() {
        assert_eq!(
            song_subtitle(Some("Artist"), Some("Station"), "today"),
            "Artist · Station · today"
        );
        assert_eq!(song_subtitle(Some("  "), None, "today"), "today");
        assert_eq!(song_subtitle(None, Some("Station"), "x"), "Station · x");
        // An empty tail still gets its separator (callers always pass text).
        assert_eq!(song_subtitle(Some("A"), None, ""), "A · ");
    }

    #[test]
    fn first_nonblank_prefers_the_primary() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(first_nonblank(s("A"), s("B")), s("A"));
        assert_eq!(first_nonblank(s(" "), s("B")), s("B"));
        assert_eq!(first_nonblank(None, s("B")), s("B"));
        assert_eq!(first_nonblank(s(""), s("  ")), None);
        assert_eq!(first_nonblank(None, None), None);
    }

    #[test]
    fn live_entry_title_falls_back_without_an_icy_title() {
        assert_eq!(
            live_entry_title(None, Some("Radio X"), "Current recording"),
            (None, "Current recording".to_string())
        );
    }

    #[test]
    fn live_entry_title_parses_artist_and_title() {
        let (artist, title) = live_entry_title(Some("Queen - Bohemian Rhapsody"), None, "-");
        assert_eq!(artist.as_deref(), Some("Queen"));
        assert_eq!(title, "Bohemian Rhapsody");
    }

    #[test]
    fn live_entry_title_of_a_blank_icy_title_is_empty() {
        // Current behaviour: a whitespace-only ICY title yields no candidate
        // and falls back to the trimmed raw title — an empty row title rather
        // than the "Current recording" fallback.
        assert_eq!(
            live_entry_title(Some("   "), None, "Current recording"),
            (None, String::new())
        );
    }

    #[test]
    fn recording_placeholder_marks_incomplete_recordings() {
        assert_eq!(
            recording_placeholder(true),
            "media-playlist-consecutive-symbolic"
        );
        assert_eq!(recording_placeholder(false), "audio-x-generic-symbolic");
    }

    #[test]
    fn view_setting_keys_are_stable() {
        assert_eq!(view_setting_key(StreamView::Channels), "stations");
        assert_eq!(view_setting_key(StreamView::Recordings), "recordings");
        assert_eq!(view_setting_key(StreamView::Heard), "heard");
    }

    #[test]
    fn logo_url_is_trimmed_and_empty_means_none() {
        assert_eq!(
            normalize_logo_url(Some("  https://x/logo.png ".into())),
            Some("https://x/logo.png".into())
        );
        assert_eq!(normalize_logo_url(Some("   ".into())), None);
        assert_eq!(normalize_logo_url(None), None);
    }

    #[test]
    fn gallery_columns_default_and_clamp() {
        assert_eq!(parse_gallery_columns(None), 4);
        assert_eq!(parse_gallery_columns(Some("abc")), 4);
        assert_eq!(parse_gallery_columns(Some("6")), 6);
        assert_eq!(parse_gallery_columns(Some("1")), 2);
        assert_eq!(parse_gallery_columns(Some("20")), 8);
        // Negative numbers don't parse as u32 → default.
        assert_eq!(parse_gallery_columns(Some("-3")), 4);
    }

    #[test]
    fn station_headers_follow_names_unless_ungrouped() {
        let items = [named(1, "Antenne"), named(2, "1Live"), named(3, "bayern 3")];
        assert_eq!(
            station_headers(&items, false),
            Some(vec!["A".into(), "0–9".into(), "B".into()])
        );
        assert_eq!(station_headers(&items, true), None);
        assert_eq!(station_headers(&[], false), Some(vec![]));
    }

    #[test]
    fn recording_headers_only_for_name_sort_without_live_entry() {
        let items = [rec(1, "Zoo", 0, 0), rec(2, "apple", 0, 0)];
        assert_eq!(
            recording_headers(&items, SortCrit::Name, false, false),
            Some(vec!["Z".into(), "A".into()])
        );
        assert_eq!(recording_headers(&items, SortCrit::Name, true, false), None);
        assert_eq!(recording_headers(&items, SortCrit::Name, false, true), None);
        assert_eq!(
            recording_headers(&items, SortCrit::Release, false, false),
            None
        );
        assert_eq!(
            recording_headers(&items, SortCrit::Length, false, false),
            None
        );
    }

    #[test]
    fn heard_headers_only_for_name_sort() {
        let items = [heard(1, "Yesterday", 0)];
        assert_eq!(
            heard_headers(&items, SortCrit::Name, false),
            Some(vec!["Y".into()])
        );
        assert_eq!(heard_headers(&items, SortCrit::Name, true), None);
        assert_eq!(heard_headers(&items, SortCrit::Release, false), None);
    }

    #[test]
    fn stations_sort_naturally_and_reverse_when_descending() {
        let mut items = vec![named(1, "Radio 10"), named(2, "Radio 9"), named(3, "Alpha")];
        sort_stations(&mut items, false);
        assert_eq!(ids(&items, |s| s.id), vec![3, 2, 1]);
        sort_stations(&mut items, true);
        assert_eq!(ids(&items, |s| s.id), vec![1, 2, 3]);
    }

    #[test]
    fn recordings_sort_by_each_criterion() {
        let base = vec![
            rec(1, "Track 10", 300, 5_000),
            rec(2, "Track 9", 100, 9_000),
            rec(3, "Another", 200, 1_000),
        ];
        let sorted = |crit, desc| {
            let mut v = base.clone();
            sort_recordings(&mut v, crit, desc);
            ids(&v, |r| r.id)
        };
        assert_eq!(sorted(SortCrit::Name, false), vec![3, 2, 1]);
        assert_eq!(sorted(SortCrit::Release, false), vec![2, 3, 1]);
        assert_eq!(sorted(SortCrit::Release, true), vec![1, 3, 2]);
        assert_eq!(sorted(SortCrit::Length, false), vec![3, 1, 2]);
        // Any other criterion falls back to the name order.
        assert_eq!(sorted(SortCrit::Songs, false), vec![3, 2, 1]);
    }

    #[test]
    fn heard_sorts_by_date_or_title() {
        let base = vec![heard(1, "b", 30), heard(2, "a", 10), heard(3, "c", 20)];
        let sorted = |crit, desc| {
            let mut v = base.clone();
            sort_heard(&mut v, crit, desc);
            ids(&v, |h| h.id)
        };
        assert_eq!(sorted(SortCrit::Release, true), vec![1, 3, 2]);
        assert_eq!(sorted(SortCrit::Name, false), vec![2, 1, 3]);
        assert_eq!(sorted(SortCrit::Name, true), vec![3, 1, 2]);
    }

    #[test]
    fn library_dest_groups_by_artist_and_album() {
        assert_eq!(
            library_dest(
                "/music",
                Some("AC/DC"),
                Some("Back in Black"),
                "Hells Bells",
                "mp3"
            ),
            PathBuf::from("/music/AC_DC/Back in Black/Hells Bells.mp3")
        );
        assert_eq!(
            library_dest("/music", None, None, "Song", "ogg"),
            PathBuf::from("/music/Recordings/Song.ogg")
        );
        assert_eq!(
            library_dest("/music", Some("  "), Some(" "), "Song", "mp3"),
            PathBuf::from("/music/Recordings/Song.mp3")
        );
        // A blank title is sanitized to "untitled".
        assert_eq!(
            library_dest("/music", Some("A"), None, "", "mp3"),
            PathBuf::from("/music/A/untitled.mp3")
        );
    }
}
