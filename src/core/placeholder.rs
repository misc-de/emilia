//! Recognising placeholder values in tags: names that are technically present
//! but carry no information about the music — the platform a file was fetched
//! from ("YouTube"), or the filler a ripper/tag editor writes for a field it
//! does not know ("Unknown Artist", "no artist").
//!
//! Two rules follow from that: such a value must not be stored as metadata in
//! the first place (see [`crate::core::scanner`] and the YouTube download), and
//! it must never be sent to an online lookup — "release:YouTube" or
//! "artist:no artist" can only ever fail, yet it costs a request and burns the
//! attempt budget of the album it belongs to.

/// Platforms/tools that end up in an *album* tag when a downloader has no real
/// album to write. None of these is ever a real album title.
const PLATFORM_TAGS: &[&str] = &[
    "youtube",
    "youtube music",
    "yt music",
    "soundcloud",
    "bandcamp",
    "mixcloud",
    "spotify",
];

/// Generic fillers for an unknown artist/album, English and German, as written
/// by rippers, converters and tag editors.
const UNKNOWN_TAGS: &[&str] = &[
    "unknown",
    "unknown artist",
    "unknown album",
    "no artist",
    "no album",
    "untitled",
    "various",
    "various artists",
    "unbekannt",
    "unbekannter interpret",
    "unbekannter künstler",
    "unbekanntes album",
    "kein interpret",
    "diverse",
    "verschiedene interpreten",
    "n/a",
    "-",
    "--",
    "?",
    "???",
];

/// Comparison key: trimmed and lowercased, so "YouTube " and "youtube" match.
fn key(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Whether `name` is merely the platform a file came from (e.g. an album tag of
/// "YouTube" on a downloaded video). Those are dropped when reading and writing
/// tags, so they never enter the library as an album of their own.
pub fn is_platform_tag(name: &str) -> bool {
    PLATFORM_TAGS.contains(&key(name).as_str())
}

/// Whether `name` says nothing about the music: a platform name or a generic
/// "unknown" filler. Used to keep pointless queries off the online services.
pub fn is_placeholder(name: &str) -> bool {
    let k = key(name);
    k.is_empty() || PLATFORM_TAGS.contains(&k.as_str()) || UNKNOWN_TAGS.contains(&k.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_tags_are_recognised_case_insensitively() {
        assert!(is_platform_tag("YouTube"));
        assert!(is_platform_tag(" youtube music "));
        assert!(!is_platform_tag("YouTube Sessions"));
        assert!(!is_platform_tag("Rubber Soul"));
    }

    #[test]
    fn placeholders_cover_platforms_and_unknown_fillers() {
        assert!(is_placeholder("YouTube"));
        assert!(is_placeholder("no artist"));
        assert!(is_placeholder("Unknown Artist"));
        assert!(is_placeholder("   "));
        // A real, if unusual, title stays a real title.
        assert!(!is_placeholder("Wie Google tickt CD4"));
        assert!(!is_placeholder("Beginner"));
    }

    /// A platform tag is a placeholder, but not every placeholder is a platform
    /// tag — only the former is stripped from the files' own metadata.
    #[test]
    fn unknown_fillers_are_not_platform_tags() {
        assert!(!is_platform_tag("Unknown Artist"));
        assert!(is_placeholder("Unknown Artist"));
    }
}
