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

/// [`WORD_SEPARATORS`] with their padding stripped. A padded separator can only
/// occur in a credit that also contains its core, so scanning for these rules
/// out the whole set in one cheap pass. Deliberately over-eager: "Kraftwerk"
/// contains "ft" and "Smith" contains "mit", which only costs those credits the
/// fast path below — never correctness.
const SEPARATOR_CORES: &[&str] = &["feat", "ft", "with", "mit"];

/// ASCII-case-insensitive substring test that allocates nothing.
fn contains_ci_ascii(haystack: &str, needle: &str) -> bool {
    let (hb, nb) = (haystack.as_bytes(), needle.as_bytes());
    nb.len() <= hb.len() && hb.windows(nb.len()).any(|w| w.eq_ignore_ascii_case(nb))
}

/// Whether `raw` is a plain single-artist credit: no separator, no bracketed or
/// dashed qualifier, nothing for [`split_artists`] to do.
///
/// For such a credit `split_artists(raw)` provably returns exactly
/// `[raw.trim()]` — every stage of it is a no-op — so [`credit_matches`] can
/// compare directly instead of running the allocation-heavy split. The test is
/// deliberately stricter than the split's actual triggers (any bracket or dash
/// disqualifies, not just a qualifying one): being wrong in this direction only
/// forgoes the shortcut.
pub fn is_plain_credit(raw: &str) -> bool {
    let t = raw.trim();
    // An empty credit yields *no* names at all, which is not the same as one
    // empty name — leave that to `split_artists`.
    if t.is_empty() {
        return false;
    }
    // `strip_qualifiers` collapses runs of whitespace, so a credit carrying any
    // would not come back unchanged.
    if t.contains("  ") || t.chars().any(|c| c.is_whitespace() && c != ' ') {
        return false;
    }
    if t.chars().any(|c| {
        CHAR_SEPARATORS.contains(&c) || matches!(c, '(' | ')' | '[' | ']' | '-' | '–' | '—')
    }) {
        return false;
    }
    !SEPARATOR_CORES
        .iter()
        .any(|core| contains_ci_ascii(t, core))
}

/// Whether `credit` (a raw `track.artist` value) names the artist whose
/// [`norm_key`] is `target_key`, counting split "feat." credits.
///
/// This is the hot predicate of the artist view: it runs once per track in the
/// library on every artist opened, so the common case — a plain credit — takes
/// the allocation-free path through [`is_plain_credit`] and only genuinely
/// compound credits pay for [`split_artists`].
pub fn credit_matches(credit: &str, target_key: &str) -> bool {
    if is_plain_credit(credit) {
        return norm_key(credit) == target_key;
    }
    split_artists(credit)
        .iter()
        .any(|s| norm_key(s) == target_key)
}

/// Like [`credit_matches`], but only the **first** (main) artist of the credit
/// counts: "A feat. B" belongs to A's album, not B's. Same shortcut — a plain
/// credit is its own primary artist.
pub fn primary_credit_matches(credit: &str, target_key: &str) -> bool {
    if is_plain_credit(credit) {
        return norm_key(credit) == target_key;
    }
    split_artists(credit)
        .first()
        .is_some_and(|p| norm_key(p) == target_key)
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

    /// Credits the fast path may take, and credits it must not.
    #[test]
    fn plain_credit_recognises_what_the_split_leaves_alone() {
        use super::is_plain_credit;
        for plain in ["Adele", "Sigur Rós", "Die Ärzte", "björk", "  Prince  "] {
            assert!(is_plain_credit(plain), "{plain:?} should be plain");
        }
        for compound in [
            "Drake feat. Rihanna",
            "A & B",
            "Earth, Wind & Fire",
            "Sting with Shaggy",
            "Rammstein mit Till",
            "Metallica (Live)",
            "Eagles - Live",
            "Sigur Rós (Band)",
            "AC/DC",
            "Kraftwerk", // over-eager: contains "ft" — allowed, just not fast
        ] {
            assert!(!is_plain_credit(compound), "{compound:?} must not be plain");
        }
    }

    /// The fast path must be a pure optimisation: for every credit, matching
    /// through `credit_matches` has to agree with the original split-and-compare
    /// it replaces. This is the property that keeps a track from silently
    /// dropping out of an artist's view.
    #[test]
    fn credit_matches_agrees_with_the_split_it_shortcuts() {
        use super::{credit_matches, norm_key, split_artists};

        let credits = [
            "Adele",
            "Prince ",
            "RZA.",
            "Björk",
            "BJÖRK",
            "Kraftwerk",
            "Smith",
            "Drake feat. Rihanna & Future",
            "A FT. B",
            "Sting with Shaggy",
            "Rammstein mit Till",
            "Earth, Wind & Fire",
            "Metallica (Live)",
            "Queen [Live in Concert]",
            "Eagles - Live",
            "Sigur Rós (Band)",
            "ACDC (Live) feat. Bon Scott",
            "AC/DC",
            "A & a",
            "  A ,  , B /",
            "",
            "   ",
        ];
        // Every name the corpus can produce, plus a few that must never match.
        let mut targets: Vec<String> = credits
            .iter()
            .flat_map(|c| split_artists(c))
            .map(|s| norm_key(&s))
            .collect();
        targets.extend(["nobody".into(), "".into(), "a".into(), "björk".into()]);

        for credit in credits {
            for target in &targets {
                let expected = split_artists(credit).iter().any(|s| norm_key(s) == *target);
                assert_eq!(
                    credit_matches(credit, target),
                    expected,
                    "credit {credit:?} vs target {target:?}"
                );
            }
        }
    }
}
