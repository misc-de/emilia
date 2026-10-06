//! Track lookups by artist/title (Deezer) and the loose name matching that
//! decides whether a hit is plausible, plus the parsing of raw ICY stream
//! titles into search candidates for recordings.

use super::{TrackTags, recording_cover_file, shared_client};

/// Fetches a cover (+ album name) for a **clean** artist/title pair (no station
/// noise) – e.g. for an album-less single track. Network – background only.
pub fn track_cover(artist: &str, title: &str) -> Option<(Vec<u8>, Option<String>)> {
    if title.trim().is_empty() {
        return None;
    }
    shared_client()
        .fetch_track_cover(artist, title)
        .ok()
        .flatten()
}

/// Looks up the canonical tags of a track in a music database (Deezer), so a
/// download can be tagged from real metadata instead of a YouTube channel name
/// and video title. `artist` is only a *hint*: it narrows the search, but the
/// returned artist is the database's. Tried hint-first, then title-only — a
/// channel like "NoCopyrightSounds" would otherwise sink every search.
///
/// `None` when nothing plausible was found; the caller then keeps what it had.
pub fn track_tags(artist: Option<&str>, title: &str) -> Option<TrackTags> {
    let title = title.trim();
    if title.is_empty() {
        return None;
    }
    let client = shared_client();
    let hint = artist.map(str::trim).filter(|s| !s.is_empty());
    for query_artist in hint.into_iter().chain(std::iter::once("")) {
        // Deezer answers *something* for almost any fuzzy query, so only trust a
        // hit whose title really is the one we asked for.
        match client.search_track_tags(query_artist, title) {
            Ok(Some(tags)) if loose_match(&tags.title, title) => return Some(tags),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("Track lookup failed ({title}): {e}");
                return None;
            }
        }
    }
    None
}

/// Like [`track_tags`], but for a known artist only accepts a hit by that
/// artist: tried with the full artist, then with the main artist alone
/// ("Robin Schulz feat. Francesco Yates" → "Robin Schulz"), and never by title
/// alone — a title-only search happily returns a cover version or a tribute.
/// Without an artist it falls back to [`track_tags`]. Used by the detail
/// refresh, where a wrong cover is worse than none. **Network.**
pub fn track_tags_strict(artist: Option<&str>, title: &str) -> Option<TrackTags> {
    let title = title.trim();
    let Some(artist) = artist.map(str::trim).filter(|s| !s.is_empty()) else {
        return track_tags(None, title);
    };
    if title.is_empty() {
        return None;
    }
    let main = main_artist(artist);
    let client = shared_client();
    let mut hints = vec![artist];
    if main != artist && !main.is_empty() {
        hints.push(main.as_str());
    }
    let fits = |tags: &TrackTags| {
        loose_match(&tags.title, title)
            && tags
                .artist
                .as_deref()
                .is_some_and(|a| loose_match(&main_artist(a), &main))
    };
    // Structured queries first; Deezer's field search misses many tracks, so
    // then plain text ("Robin Schulz Sugar"), still checked the same way.
    let free = format!("{main} {title}");
    let queries = hints
        .into_iter()
        .map(|h| client.search_track_tags(h, title))
        .chain(std::iter::once_with(|| {
            client.search_track_query(&free, title)
        }));
    for result in queries {
        match result {
            Ok(Some(tags)) if fits(&tags) => return Some(tags),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("Track lookup failed ({title}): {e}");
                return None;
            }
        }
    }
    None
}

/// The leading artist of a credit, without featured guests or partners
/// ("A feat. B" → "A"), by the same rules the library uses for its
/// "main artist only" credit mode.
fn main_artist(credit: &str) -> String {
    crate::core::artist::split_artists_with(crate::core::artist::CreditMode::Primary, credit)
        .into_iter()
        .next()
        .unwrap_or_else(|| credit.trim().to_string())
}

/// Compares two track/artist names loosely: case- and punctuation-insensitive,
/// and tolerant of one carrying a suffix the other lacks ("Song" vs
/// "Song (Radio Edit)"). The suffix must start at a word boundary, so "Help"
/// does not match "Helpless".
fn loose_match(a: &str, b: &str) -> bool {
    let (a, b) = (normalize_name(a), normalize_name(b));
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    !short.is_empty()
        && long.starts_with(&short)
        && (long.len() == short.len() || long[short.len()..].starts_with(' '))
}

/// Lowercases, drops everything but letters/digits and collapses whitespace.
/// Apostrophes vanish without leaving a gap, so "Ain't" and "Aint" normalize
/// to the same string.
fn normalize_name(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| !"'\u{2019}\u{02BC}`\u{00B4}".contains(*c))
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fetches the cover (and album name) for a raw stream title – **network
/// access**, only call from worker/background threads. Best effort: `None` if
/// nothing is found. For subsequently tagging a streaming recording.
pub fn recording_cover(
    raw_title: &str,
    station: Option<&str>,
) -> Option<(Vec<u8>, Option<String>)> {
    let client = shared_client();
    let candidates = recording_query_candidates(raw_title, station);
    // The first candidate is also the best guess for storage/display – the cover
    // is cached under it so the recordings list finds it ([`recording_cover_path`]).
    let key = candidates.first().cloned();
    for (artist, title) in &candidates {
        if let Ok(Some(hit)) = client
            .search_track_tags(artist.as_deref().unwrap_or(""), title)
            .map(|t| t.and_then(TrackTags::into_cover))
        {
            if let Some((ka, kt)) = &key {
                let _ = std::fs::write(
                    recording_cover_file(ka.as_deref().unwrap_or(""), kt),
                    &hit.0,
                );
            }
            return Some(hit);
        }
    }
    None
}

/// Builds `(artist, title)` search candidates from a raw ICY stream title that
/// may also carry the **station name** (a frequent reason the lookup misses).
/// Best-first; the **first** element is also the best guess for storing/display.
///
/// Strategy: strip the station name and "now playing" noise, split on the common
/// separators, then offer the plausible pairings – including the swapped order
/// ("Title - Artist") and a title-only fallback.
pub fn recording_query_candidates(
    raw: &str,
    station: Option<&str>,
) -> Vec<(Option<String>, String)> {
    let cleaned = clean_stream_title(raw, station);
    let parts = split_title_parts(&cleaned, station);
    let mut out: Vec<(Option<String>, String)> = Vec::new();
    let mut push = |a: Option<&str>, t: &str| {
        let t = t.trim();
        if t.is_empty() {
            return;
        }
        let cand = (
            a.map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            t.to_string(),
        );
        if !out.contains(&cand) {
            out.push(cand);
        }
    };
    match parts.as_slice() {
        [] => {}
        [only] => push(None, only),
        [a, b] => {
            push(Some(a), b);
            push(Some(b), a);
        }
        // 3+ parts: one is likely the station or extra info – try adjacent pairs.
        [a, b, c, ..] => {
            push(Some(a), b);
            push(Some(b), a);
            push(Some(b), c);
            push(Some(a), c);
        }
    }
    // Always also try the whole cleaned string as a title-only search.
    push(None, &cleaned);
    out
}

/// Removes "now playing" noise and the station name from a raw stream title.
fn clean_stream_title(raw: &str, station: Option<&str>) -> String {
    let mut s = raw.trim().to_string();
    for p in [
        "now playing:",
        "now playing",
        "playing:",
        "live:",
        "on air:",
        "▶",
    ] {
        if s.to_ascii_lowercase().starts_with(p) {
            s = s[p.len()..].trim().to_string();
        }
    }
    if let Some(st) = station.map(str::trim).filter(|s| !s.is_empty()) {
        for (o, c) in [('(', ')'), ('[', ']')] {
            s = s.replace(&format!("{o}{st}{c}"), " ");
        }
        s = remove_ci(&s, st);
    }
    s.trim()
        .trim_matches(|c| "-–—|/ ".contains(c))
        .trim()
        .to_string()
}

/// Splits a cleaned title on the common separators (dash variants, pipe, slash),
/// dropping empty parts and any part that is just the station name.
fn split_title_parts(s: &str, station: Option<&str>) -> Vec<String> {
    let normalized = s.replace(['–', '—', '|'], "-").replace(" / ", " - ");
    normalized
        .split(" - ")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .filter(|p| station.is_none_or(|st| !p.eq_ignore_ascii_case(st.trim())))
        .map(str::to_string)
        .collect()
}

/// Case-insensitive (ASCII) removal of every whole-word occurrence of `needle`
/// from `haystack`; occurrences inside a longer word ("Bob" in "Bobby") stay.
fn remove_ci(haystack: &str, needle: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let (hl, nl) = (haystack.to_ascii_lowercase(), needle.to_ascii_lowercase());
    let is_word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    let mut result = String::new();
    let mut i = 0;
    while let Some(pos) = hl[i..].find(&nl) {
        let (start, end) = (i + pos, i + pos + nl.len());
        let bounded = !is_word(haystack[..start].chars().next_back())
            && !is_word(haystack[end..].chars().next());
        result.push_str(&haystack[i..if bounded { start } else { end }]);
        i = end;
    }
    result.push_str(&haystack[i..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn main_artist_drops_featured_guests() {
        assert_eq!(
            super::main_artist("Robin Schulz feat. Francesco Yates"),
            "Robin Schulz"
        );
        assert_eq!(
            super::main_artist("Calvin Harris & Dua Lipa"),
            "Calvin Harris"
        );
        assert_eq!(super::main_artist("Billie Eilish"), "Billie Eilish");
    }

    #[test]
    fn loose_match_ignores_case_and_punctuation() {
        assert!(loose_match(
            "Never Gonna Give You Up",
            "never gonna give you up"
        ));
        assert!(loose_match("Ain't No Sunshine", "Aint No Sunshine"));
        // One side carrying a version suffix must still match.
        assert!(loose_match("Take On Me (Radio Edit)", "Take On Me"));
        assert!(loose_match("Sky High", "Sky High - Remastered 2011"));
    }

    #[test]
    fn loose_match_rejects_a_different_song() {
        assert!(!loose_match("Take On Me", "Take My Breath Away"));
        assert!(!loose_match("Yesterday", "Help"));
        // Empty never matches — an empty query must not accept any hit.
        assert!(!loose_match("", "Yesterday"));
        assert!(!loose_match("Yesterday", "   "));
    }

    /// The prefix must end at a word boundary: a longer title merely starting
    /// with the same letters is a different song.
    #[test]
    fn loose_match_rejects_a_mere_string_prefix() {
        assert!(!loose_match("Help", "Helpless"));
        assert!(!loose_match("Rockstar", "Rock"));
        assert!(loose_match("Help!", "Help (Remastered)"));
    }

    #[test]
    fn normalize_name_folds_case_punctuation_and_whitespace() {
        assert_eq!(normalize_name("  Hello,   World! "), "hello world");
        assert_eq!(normalize_name("Ain't"), "aint");
        assert_eq!(normalize_name("Don\u{2019}t Stop"), "dont stop");
        assert_eq!(normalize_name("AC/DC"), "ac dc");
        // Non-ASCII letters are kept (lower-cased), not stripped.
        assert_eq!(normalize_name("BEYONCÉ"), "beyoncé");
        assert_eq!(normalize_name("!!! ???"), "");
        assert_eq!(normalize_name(""), "");
    }

    #[test]
    fn clean_stream_title_strips_now_playing_noise() {
        assert_eq!(
            clean_stream_title("Now Playing: Daft Punk - One More Time", None),
            "Daft Punk - One More Time"
        );
        assert_eq!(clean_stream_title("▶ Artist - Song", None), "Artist - Song");
        assert_eq!(
            clean_stream_title("  Artist - Song / ", None),
            "Artist - Song"
        );
    }

    #[test]
    fn clean_stream_title_removes_the_station_name() {
        assert_eq!(
            clean_stream_title("Radio Bob - AC/DC - Thunderstruck", Some("Radio Bob")),
            "AC/DC - Thunderstruck"
        );
        assert_eq!(
            clean_stream_title("Artist - Song [1LIVE]", Some("1LIVE")),
            "Artist - Song"
        );
        assert_eq!(
            clean_stream_title("RADIO X | Artist - Song", Some("Radio X")),
            "Artist - Song"
        );
        // A blank station name is ignored.
        assert_eq!(
            clean_stream_title("Artist - Song", Some("  ")),
            "Artist - Song"
        );
    }

    /// The station name is only removed as a whole word, never cut out of an
    /// artist or title that merely contains it.
    #[test]
    fn clean_stream_title_keeps_longer_words_containing_the_station() {
        assert_eq!(
            clean_stream_title("Bobby Brown - Every Little Step", Some("Bob")),
            "Bobby Brown - Every Little Step"
        );
        assert_eq!(
            clean_stream_title("Rocky - Theme", Some("Rock")),
            "Rocky - Theme"
        );
        assert_eq!(
            clean_stream_title("Rock | Rocky - Theme", Some("Rock")),
            "Rocky - Theme"
        );
    }

    #[test]
    fn split_title_parts_splits_on_spaced_separators() {
        assert_eq!(split_title_parts("A – B | C", None), ["A", "B", "C"]);
        assert_eq!(split_title_parts("A / B", None), ["A", "B"]);
        // Unspaced dashes/slashes belong to the names.
        assert_eq!(split_title_parts("Jay-Z - AC/DC", None), ["Jay-Z", "AC/DC"]);
        assert_eq!(
            split_title_parts("A - Station - B", Some("STATION")),
            ["A", "B"]
        );
        assert!(split_title_parts("", None).is_empty());
        assert!(split_title_parts(" -  - ", None).is_empty());
    }

    #[test]
    fn remove_ci_drops_every_occurrence_ignoring_ascii_case() {
        assert_eq!(remove_ci("Hello WORLD world", "world"), "Hello  ");
        assert_eq!(remove_ci("abc", ""), "abc");
        assert_eq!(remove_ci("abc", "xyz"), "abc");
        // Non-ASCII text around the needle survives intact.
        assert_eq!(remove_ci("ÄBC abc", "ABC"), "ÄBC ");
        // Only whole words go.
        assert_eq!(remove_ci("aaaa", "aa"), "aaaa");
        assert_eq!(remove_ci("Bobby Bob", "bob"), "Bobby ");
    }

    #[test]
    fn recording_query_candidates_offers_both_orders_and_title_only() {
        let c = recording_query_candidates("Daft Punk - One More Time", None);
        assert_eq!(
            c,
            [
                (Some("Daft Punk".to_string()), "One More Time".to_string()),
                (Some("One More Time".to_string()), "Daft Punk".to_string()),
                (None, "Daft Punk - One More Time".to_string()),
            ]
        );
    }

    #[test]
    fn recording_query_candidates_for_a_single_part_is_title_only() {
        assert_eq!(
            recording_query_candidates("Just A Title", None),
            [(None, "Just A Title".to_string())]
        );
        assert!(recording_query_candidates("   ", None).is_empty());
    }

    #[test]
    fn recording_query_candidates_pairs_three_parts_and_drops_the_station() {
        let c = recording_query_candidates("A - B - C", None);
        let pairs: Vec<(Option<&str>, &str)> =
            c.iter().map(|(a, t)| (a.as_deref(), t.as_str())).collect();
        assert_eq!(
            pairs,
            [
                (Some("A"), "B"),
                (Some("B"), "A"),
                (Some("B"), "C"),
                (Some("A"), "C"),
                (None, "A - B - C"),
            ]
        );
        let c = recording_query_candidates("Radio X | Artist - Song", Some("Radio X"));
        assert_eq!(c[0], (Some("Artist".to_string()), "Song".to_string()));
        assert!(
            c.iter()
                .all(|(a, t)| a.as_deref() != Some("Radio X") && t != "Radio X")
        );
    }
}
