//! Zerlegen zusammengesetzter Interpreten-Angaben in einzelne Künstler.
//!
//! „Drake feat. Rihanna & Future" → `["Drake", "Rihanna", "Future"]`. So wird
//! ein Titel jedem beteiligten Künstler einzeln zugeordnet (Interpreten-Ansicht,
//! Foto-Abruf). Verändert wird dabei nichts an der Datei – nur die Anzeige.

/// Wort-Trenner (mit umgebenden Leerzeichen), case-insensitiv.
const WORD_SEPARATORS: &[&str] = &[
    " feat. ",
    " feat ",
    " ft. ",
    " ft ",
    " featuring ",
    " feature ",
    " with ", // englisch
    " mit ",  // deutsch
];

/// Einzelzeichen-Trenner (gelten auch ohne umgebende Leerzeichen).
const CHAR_SEPARATORS: &[char] = &['&', ',', '/', '+', ';', '×'];

/// Schlagwörter, die eine Auftritts-Variante kennzeichnen. Klammer-Zusätze mit
/// diesen Wörtern (z. B. „(Live)", „[Live in Concert]") werden aus der Anzeige
/// entfernt – ein Live-Mitschnitt ist derselbe Interpret.
const QUALIFIER_KEYWORDS: &[&str] = &["live", "concert", "konzert", "unplugged"];

/// Zerlegt eine Interpreten-Angabe in einzelne, getrimmte Künstlernamen.
/// Dubletten (case-insensitiv) werden entfernt, die Reihenfolge bleibt erhalten.
///
/// Hinweis: Bandnamen mit Kommata/`&` (z. B. „Earth, Wind & Fire") werden dabei
/// ebenfalls getrennt – ein bewusster Kompromiss zugunsten der feat.-Auflösung.
pub fn split_artists(raw: &str) -> Vec<String> {
    // 1) Wort-Trenner auf ';' normalisieren (case-insensitiv, ASCII-sicher).
    let mut normalized = format!(" {} ", raw);
    for sep in WORD_SEPARATORS {
        normalized = replace_ci_ascii(&normalized, sep, " ; ");
    }

    // 2) Zeichen-Trenner ebenfalls zu ';'.
    let normalized: String = normalized
        .chars()
        .map(|c| if CHAR_SEPARATORS.contains(&c) { ';' } else { c })
        .collect();

    // 3) Aufteilen, Auftritts-Zusätze entfernen, trimmen, dedup.
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for part in normalized.split(';') {
        let name = strip_qualifiers(part.trim());
        if name.is_empty() {
            continue;
        }
        if seen.insert(name.to_lowercase()) {
            out.push(name);
        }
    }
    out
}

/// Entfernt Auftritts-Zusätze aus einem Interpretennamen:
/// Klammer-/eckige Gruppen mit Schlagwörtern (z. B. „(Live)", „[in Concert]")
/// und abschließende „– Live …"-Anhänge.
pub fn strip_qualifiers(name: &str) -> String {
    let mut s = remove_qualifier_brackets(name, '(', ')');
    s = remove_qualifier_brackets(&s, '[', ']');

    // Abschließender „- Live"/„– Konzert …"-Zusatz nach Gedankenstrich.
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

/// Entfernt Klammergruppen `open … close`, deren Inhalt ein Auftritts-Schlagwort
/// enthält; andere Klammergruppen bleiben unverändert. Mehrfach-Leerzeichen
/// werden anschließend zusammengefasst.
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
                    // Keine Auftritts-Klammer → unverändert behalten.
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
    // Unbalancierte offene Klammer: Rest wörtlich übernehmen.
    if depth > 0 {
        out.push(open);
        out.push_str(&buf);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Ersetzt alle (ASCII-case-insensitiven) Vorkommen von `needle` durch `repl`.
/// Arbeitet byte-weise, bleibt aber an UTF-8-Grenzen korrekt, da Treffer nur an
/// rein-ASCII-Stellen entstehen können.
fn replace_ci_ascii(haystack: &str, needle: &str, repl: &str) -> String {
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if i + nb.len() <= hb.len() && hb[i..i + nb.len()].eq_ignore_ascii_case(nb) {
            out.push_str(repl);
            i += nb.len();
        } else {
            let ch = haystack[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
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
        assert_eq!(split_artists("A x B"), vec!["A x B"]); // kein Trenner
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
        assert_eq!(split_artists("Rammstein mit Till"), vec!["Rammstein", "Till"]);
        assert_eq!(split_artists("Sting with Shaggy"), vec!["Sting", "Shaggy"]);
    }

    #[test]
    fn strips_live_and_concert() {
        assert_eq!(split_artists("Metallica (Live)"), vec!["Metallica"]);
        assert_eq!(split_artists("Queen [Live in Concert]"), vec!["Queen"]);
        assert_eq!(split_artists("Nirvana (Unplugged)"), vec!["Nirvana"]);
        assert_eq!(split_artists("Eagles - Live"), vec!["Eagles"]);
        // pro Einzelkünstler angewandt
        assert_eq!(
            split_artists("ACDC (Live) feat. Bon Scott"),
            vec!["ACDC", "Bon Scott"]
        );
    }

    #[test]
    fn keeps_non_qualifier_brackets() {
        assert_eq!(split_artists("Sigur Rós (Band)"), vec!["Sigur Rós (Band)"]);
    }
}
