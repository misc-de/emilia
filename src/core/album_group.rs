//! Which same-named albums form **one** album card.
//!
//! The album overviews group tracks by album name, so a soundtrack or sampler
//! with a dozen artists shows as one card, and so do the "feat." credit
//! variants of an album. Grouping by the name *alone* also merged unrelated
//! albums that merely share a title — "Greatest Hits" by Queen and by Gorillaz
//! became one card with one cover. So same-named tracks only join when they
//! share a **primary artist** or lie in the same **album folder** (a CD/disc
//! subfolder counts as its parent); everything else stays a separate card.

use std::collections::{HashMap, HashSet};

use crate::core::artist::primary_artist;

/// Disc number from a folder segment like "CD2", "CD 2", "Disc 03", "Part 2".
/// The disc keyword may sit anywhere in the segment (e.g. "Wie Google tickt
/// CD1") but only at a word boundary and followed by digits — so "Greatest
/// Hits" and the like trigger nothing. Otherwise `None`.
pub fn disc_from_segment(seg: &str) -> Option<u32> {
    let s = seg.trim().to_ascii_lowercase();
    let bytes = s.as_bytes();
    const MARKERS: [&str; 6] = ["cd", "disc", "disk", "teil", "part", "folge"];
    for kw in MARKERS {
        // Look for the marker **anywhere** in the segment, but only at a word
        // boundary (no match in the middle of a word like "abcd"), followed by
        // digits.
        let mut from = 0;
        while let Some(rel) = s[from..].find(kw) {
            let pos = from + rel;
            let boundary = pos == 0 || !bytes[pos - 1].is_ascii_alphabetic();
            if boundary {
                let digits: String = s[pos + kw.len()..]
                    .trim_start_matches([' ', '_', '.', '#', '-'])
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                if let Ok(n) = digits.parse::<u32>() {
                    return Some(n);
                }
            }
            from = pos + kw.len();
        }
    }
    None
}

/// The album folder of a track: its directory, or the one above when the
/// directory is a CD/disc subfolder. Empty for a path without a directory.
/// Works on synthetic remote paths (`nc:<id>:<rel>`) too — only `/` matters.
pub fn album_folder(path: &str) -> &str {
    let Some(i) = path.rfind('/') else {
        return "";
    };
    let dir = &path[..i];
    let last = dir.rsplit('/').next().unwrap_or(dir);
    if disc_from_segment(last).is_some() {
        dir.rfind('/').map_or("", |j| &dir[..j])
    } else {
        dir
    }
}

/// One album card among the same-named tracks.
#[derive(Debug, Clone)]
pub struct Card {
    /// Display artist: the primary artist with the most tracks (ties: the
    /// alphabetically first) — the same rule the overview applies.
    pub display: String,
    /// Lower-cased primary artists that belong to this card.
    pub members: HashSet<String>,
}

impl Card {
    /// Whether a track credited to `artist` belongs to this card.
    pub fn contains(&self, artist: &str) -> bool {
        self.members
            .contains(&primary_artist(artist).to_lowercase())
    }
}

/// Splits the tracks of one album name — `(artist credit, path)` pairs — into
/// cards: primary artists are joined when they share an album folder.
pub fn cards<'a>(tracks: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<Card> {
    fn find(parent: &mut HashMap<String, String>, k: &str) -> String {
        let mut root = k.to_string();
        while let Some(p) = parent.get(&root).filter(|p| **p != root) {
            root = p.clone();
        }
        // Path compression keeps the chains short.
        let mut cur = k.to_string();
        while cur != root {
            let next = parent.insert(cur.clone(), root.clone()).unwrap_or_default();
            cur = next;
        }
        root
    }

    let mut parent: HashMap<String, String> = HashMap::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut folder_owner: HashMap<&'a str, String> = HashMap::new();
    for (artist, path) in tracks {
        let primary = primary_artist(artist);
        let key = primary.to_lowercase();
        parent.entry(key.clone()).or_insert_with(|| key.clone());
        *counts.entry(primary).or_default() += 1;
        let folder = album_folder(path);
        if folder.is_empty() {
            continue;
        }
        match folder_owner.get(folder) {
            Some(owner) => {
                let (a, b) = (find(&mut parent, owner), find(&mut parent, &key));
                if a != b {
                    parent.insert(b, a);
                }
            }
            None => {
                folder_owner.insert(folder, key);
            }
        }
    }

    let mut by_root: HashMap<String, (HashSet<String>, Vec<(usize, String)>)> = HashMap::new();
    for (primary, n) in counts {
        let key = primary.to_lowercase();
        let root = find(&mut parent, &key);
        let slot = by_root.entry(root).or_default();
        slot.0.insert(key);
        slot.1.push((n, primary));
    }
    let mut out: Vec<Card> = by_root
        .into_values()
        .map(|(members, mut artists)| {
            artists.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
            });
            Card {
                display: artists
                    .into_iter()
                    .next()
                    .map(|(_, a)| a)
                    .unwrap_or_default(),
                members,
            }
        })
        .collect();
    out.sort_by_key(|c| c.display.to_lowercase());
    out
}

/// Stable identity of a card across the app (overview row ↔ running track):
/// album name plus display artist, both lower-cased.
pub fn card_key(album: &str, display: &str) -> String {
    format!("{}\u{1}{}", album.to_lowercase(), display.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn album_folder_skips_disc_subfolders() {
        assert_eq!(
            album_folder("/m/Queen/Live/disk 2of2/01.mp3"),
            "/m/Queen/Live"
        );
        assert_eq!(album_folder("/m/Queen/Live/CD1/01.mp3"), "/m/Queen/Live");
        assert_eq!(
            album_folder("/m/Queen/Greatest Hits/01.mp3"),
            "/m/Queen/Greatest Hits"
        );
        assert_eq!(album_folder("nc:3:Album/01.mp3"), "nc:3:Album");
        assert_eq!(album_folder("loose.mp3"), "");
    }

    #[test]
    fn same_title_by_unrelated_artists_stays_apart() {
        let c = cards([
            ("Queen", "/m/Queen/Greatest Hits/01.mp3"),
            ("Queen", "/m/Queen/Greatest Hits/02.mp3"),
            ("Gorillaz", "/m/Gorillaz/Greatest Hits/01.mp3"),
        ]);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].display, "Gorillaz");
        assert_eq!(c[1].display, "Queen");
        assert!(c[1].contains("Queen & David Bowie"));
        assert!(!c[1].contains("Gorillaz"));
    }

    #[test]
    fn soundtrack_and_feat_variants_merge() {
        // A soundtrack: many artists, one folder (with disc subfolders).
        let c = cards([
            ("Nancy Sinatra", "/m/OST/Kill Bill/CD1/01.mp3"),
            ("Isaac Hayes", "/m/OST/Kill Bill/CD2/02.mp3"),
            ("Isaac Hayes", "/m/OST/Kill Bill/CD2/03.mp3"),
        ]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].display, "Isaac Hayes");
        // feat. credits share the primary artist even across folders.
        let c = cards([
            ("Gorillaz", "/m/Gorillaz/Demon Days/01.mp3"),
            (
                "Gorillaz featuring De La Soul",
                "/m/Singles/Feel Good Inc/01.mp3",
            ),
        ]);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn folders_chain_artists_together() {
        // A and B share folder X, B and C share folder Y → one card.
        let c = cards([
            ("A", "/X/1.mp3"),
            ("B", "/X/2.mp3"),
            ("B", "/Y/1.mp3"),
            ("C", "/Y/2.mp3"),
            ("D", "/Z/1.mp3"),
        ]);
        assert_eq!(c.len(), 2);
    }
}
