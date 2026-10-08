//! Physical copies of the same song.
//!
//! A library often holds one recording several times: the album track and the
//! single release, a "Artist - Title.mp3" next to the album folder, two
//! pressings of the same single (UK CD1 / CD2). The album and artist views
//! should list such a song **once**. Two tracks count as copies when their
//! titles match (case, apostrophe style and spacing ignored) and their lengths
//! differ by at most [`DURATION_TOLERANCE_MS`] — different rips of one
//! recording vary by a second or two, a remix or live take of the same title
//! is clearly longer or shorter. The files themselves are never touched.

use std::collections::HashMap;

use crate::model::Track;

/// Largest length difference between two copies of one recording.
pub const DURATION_TOLERANCE_MS: i64 = 5_000;

/// Comparison key of a song title: lower-cased, typographic apostrophes
/// folded to `'`, whitespace collapsed.
pub fn title_key(title: &str) -> String {
    title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .map(|c| match c {
            '’' | '‘' | '`' | '´' => '\'',
            c => c,
        })
        .collect::<String>()
        .to_lowercase()
}

/// Whether two lengths fit one recording. An unknown length matches anything.
fn same_length(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() <= DURATION_TOLERANCE_MS,
        _ => true,
    }
}

/// Collects the songs seen so far; [`Self::is_new`] reports whether a
/// (title, length) is the first copy of its song.
#[derive(Default)]
pub struct SongSet {
    seen: HashMap<String, Vec<Option<i64>>>,
}

impl SongSet {
    /// `true` for the first copy of a song, `false` for every later copy.
    pub fn is_new(&mut self, title: &str, duration_ms: Option<i64>) -> bool {
        let lens = self.seen.entry(title_key(title)).or_default();
        if lens.iter().any(|l| same_length(*l, duration_ms)) {
            return false;
        }
        lens.push(duration_ms);
        true
    }
}

/// Keeps the first copy of every song, in the given order.
pub fn dedup_tracks(tracks: Vec<Track>) -> Vec<Track> {
    let mut set = SongSet::default();
    tracks
        .into_iter()
        .filter(|t| set.is_new(&t.title, t.duration_ms))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_collapse_variants_stay() {
        let mut set = SongSet::default();
        assert!(set.is_new("Kauf MICH!", Some(200_000)));
        // Same song from the single folder, slightly different rip.
        assert!(!set.is_new("Kauf mich! ", Some(201_500)));
        // Typographic apostrophe, collapsed spaces.
        assert!(set.is_new("Tout pour sauver l'amour", Some(180_000)));
        assert!(!set.is_new("Tout  pour sauver l’amour", Some(180_000)));
        // Same title, but clearly another recording (remix).
        assert!(set.is_new("Dare", Some(434_599)));
        assert!(!set.is_new("DARE", Some(436_000)));
        // Unknown length counts as a copy of a known title.
        assert!(!set.is_new("Dare", None));
    }
}
