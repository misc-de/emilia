//! Splitting compound artist entries into individual artists.
//!
//! "Drake feat. Rihanna & Future" → `["Drake", "Rihanna", "Future"]`. This way a
//! track is assigned to each participating artist individually (artist view,
//! photo fetch). Nothing about the file is changed in the process – only the display.

/// Word separators (with surrounding spaces), case-insensitive.
const WORD_SEPARATORS: &[&str] = &[
    " feat. ",
    " feat ",
    " ft. ",
    " ft ",
    " featuring ",
    " feature ",
    " with ", // English
    " mit ",  // German
];

/// Single-character separators (also apply without surrounding spaces).
const CHAR_SEPARATORS: &[char] = &['&', ',', '/', '+', ';', '×'];

/// Keywords that mark a performance variant. Bracketed additions containing
/// these words (e.g. "(Live)", "[Live in Concert]") are removed from the
/// display – a live recording is the same artist.
const QUALIFIER_KEYWORDS: &[&str] = &["live", "concert", "konzert", "unplugged"];

/// Splits an artist entry into individual, trimmed artist names.
/// Duplicates (case-insensitive) are removed, the order is preserved.
///
/// Note: band names with commas/`&` (e.g. "Earth, Wind & Fire") are also split
/// in the process – a deliberate compromise in favor of feat. resolution.
pub fn split_artists(raw: &str) -> Vec<String> {
    // 1) Normalize word separators to ';' (case-insensitive, ASCII-safe).
    let mut normalized = format!(" {} ", raw);
    for sep in WORD_SEPARATORS {
        normalized = replace_ci_ascii(&normalized, sep, " ; ");
    }

    // 2) Character separators likewise to ';'.
    let normalized: String = normalized
        .chars()
        .map(|c| if CHAR_SEPARATORS.contains(&c) { ';' } else { c })
        .collect();

    // 3) Split, remove performance additions, trim, dedup.
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for part in normalized.split(';') {
        let name = strip_qualifiers(part.trim());
        if name.is_empty() {
            continue;
        }
        if seen.insert(norm_key(&name)) {
            out.push(name);
        }
    }
    out
}

/// Comparison key for artist names: trimmed, without trailing dots, lowercased.
/// This way "RZA" and "RZA." (or "M.I.A" and "M.I.A.") count as the **same**
/// artist – a trailing abbreviation dot should not lead to two entries.
pub fn norm_key(name: &str) -> String {
    name.trim()
        .trim_end_matches(['.', ' '])
        .trim()
        .to_lowercase()
}

/// Primary artist of an entry (the first named, before "feat."). Used for
/// album grouping: "Beginner feat. X" belongs to the album by "Beginner".
pub fn primary_artist(raw: &str) -> String {
    split_artists(raw)
        .into_iter()
        .next()
        .unwrap_or_else(|| raw.trim().to_string())
}

/// Removes performance additions from an artist name:
/// round/square-bracketed groups with keywords (e.g. "(Live)", "[in Concert]")
/// and trailing "– Live …" suffixes.
pub fn strip_qualifiers(name: &str) -> String {
    let mut s = remove_qualifier_brackets(name, '(', ')');
    s = remove_qualifier_brackets(&s, '[', ']');

    // Trailing "- Live"/"– Concert …" addition after a dash.
    for dash in [" - ", " – ", " — "] {
        if let Some(idx) = s.find(dash) {
            let tail = s[idx + dash.len()..].to_lowercase();
            if QUALIFIER_KEYWORDS.iter().any(|k| tail.contains(k)) {
                s.truncate(idx);
            }
        }
    }
    s.trim().to_string()
}

/// Removes bracket groups `open … close` whose content contains a performance
/// keyword; other bracket groups remain unchanged. Multiple spaces are
/// collapsed afterwards.
fn remove_qualifier_brackets(s: &str, open: char, close: char) -> String {
    let mut out = String::with_capacity(s.len());
    let mut buf = String::new();
    let mut depth = 0u32;
    for c in s.chars() {
        if c == open {
            depth += 1;
            if depth == 1 {
                buf.clear();
                continue;
            }
        }
        if c == close && depth > 0 {
            depth -= 1;
            if depth == 0 {
                let low = buf.to_lowercase();
                if !QUALIFIER_KEYWORDS.iter().any(|k| low.contains(k)) {
                    // Not a performance bracket → keep unchanged.
                    out.push(open);
                    out.push_str(&buf);
                    out.push(close);
                }
                buf.clear();
                continue;
            }
        }
        if depth > 0 {
            buf.push(c);
        } else {
            out.push(c);
        }
    }
    // Unbalanced open bracket: keep the rest verbatim.
    if depth > 0 {
        out.push(open);
        out.push_str(&buf);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Replaces all (ASCII case-insensitive) occurrences of `needle` with `repl`.
/// Works byte-wise, but stays correct at UTF-8 boundaries, since matches can
/// only occur at pure-ASCII positions.
fn replace_ci_ascii(haystack: &str, needle: &str, repl: &str) -> String {
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if i + nb.len() <= hb.len() && hb[i..i + nb.len()].eq_ignore_ascii_case(nb) {
            out.push_str(repl);
            i += nb.len();
        } else if let Some(ch) = haystack[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::split_artists;

    #[test]
    fn feat_and_ampersand() {
        assert_eq!(
            split_artists("Drake feat. Rihanna & Future"),
            vec!["Drake", "Rihanna", "Future"]
        );
    }

    #[test]
    fn variants_and_case() {
        assert_eq!(split_artists("A FT. B"), vec!["A", "B"]);
        assert_eq!(split_artists("A Featuring B"), vec!["A", "B"]);
        assert_eq!(split_artists("A x B"), vec!["A x B"]); // no separator
    }

    #[test]
    fn single_and_dedup() {
        assert_eq!(split_artists("Adele"), vec!["Adele"]);
        assert_eq!(split_artists("A & a"), vec!["A"]); // case-insensitive dedup
    }

    #[test]
    fn trims_and_drops_empty() {
        assert_eq!(split_artists("  A ,  , B /"), vec!["A", "B"]);
    }

    #[test]
    fn mit_and_with_separators() {
        assert_eq!(
            split_artists("Rammstein mit Till"),
            vec!["Rammstein", "Till"]
        );
        assert_eq!(split_artists("Sting with Shaggy"), vec!["Sting", "Shaggy"]);
    }

    #[test]
    fn strips_live_and_concert() {
        assert_eq!(split_artists("Metallica (Live)"), vec!["Metallica"]);
        assert_eq!(split_artists("Queen [Live in Concert]"), vec!["Queen"]);
        assert_eq!(split_artists("Nirvana (Unplugged)"), vec!["Nirvana"]);
        assert_eq!(split_artists("Eagles - Live"), vec!["Eagles"]);
        // applied per individual artist
        assert_eq!(
            split_artists("ACDC (Live) feat. Bon Scott"),
            vec!["ACDC", "Bon Scott"]
        );
    }

    #[test]
    fn keeps_non_qualifier_brackets() {
        assert_eq!(split_artists("Sigur Rós (Band)"), vec!["Sigur Rós (Band)"]);
    }

    #[test]
    fn trailing_dot_is_same_artist() {
        use super::norm_key;
        assert_eq!(norm_key("RZA"), norm_key("RZA."));
        assert_eq!(norm_key("M.I.A"), norm_key("M.I.A."));
        assert_ne!(norm_key("RZA"), norm_key("GZA"));
        // Dedup also within a single entry.
        assert_eq!(split_artists("RZA & RZA."), vec!["RZA"]);
    }

    #[test]
    fn primary_is_first_artist() {
        use super::primary_artist;
        assert_eq!(primary_artist("Beginner feat. Megaloh"), "Beginner");
        assert_eq!(primary_artist("Sido feat. Genetikk & Marsimoto"), "Sido");
        assert_eq!(primary_artist("Adele"), "Adele");
    }
}
