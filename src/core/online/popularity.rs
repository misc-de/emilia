//! How successful an artist's songs are, as a music database (Deezer) sees it:
//! the artist's fan count and their most popular tracks with Deezer's `rank`
//! (a popularity score up to ~1,000,000). Drives "Play hits only" and the
//! popularity lines in the detail view's info block.
//!
//! The rank of one track *version* is split across re-releases (the same song
//! on the original album, a best-of and a soundtrack each carry their own
//! rank), so hits are decided by the artist's **top list** – deduplicated by
//! song title – rather than by the ranks of an album's own tracks.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::matching::normalize_name;
use super::{OnlineClient, percent_encode};
use crate::core::net;

/// How many top tracks are requested per artist (Deezer's top list).
const TOP_LIMIT: usize = 50;
/// A song counts as a hit when its rank reaches this share of the artist's
/// most popular song. Deezer ranks are compressed (the 50th Queen song still
/// has half the rank of "Bohemian Rhapsody"), so the cut sits at one half.
const HIT_SHARE: f64 = 0.5;

/// The artist's popularity: fans plus the top tracks (Deezer order).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtistPopularity {
    pub fans: u64,
    pub top: Vec<PopTrack>,
}

/// One entry of an artist's top list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PopTrack {
    pub title: String,
    pub rank: u64,
}

/// A song of the artist that counts as a hit, best first.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// Matching key of the song title (see [`title_key`]).
    pub key: String,
    /// Display title (of the best-ranked version).
    pub title: String,
    pub rank: u64,
}

impl ArtistPopularity {
    /// The artist's hits, best first: the top list deduplicated by song (the
    /// best-ranked version wins), cut at [`HIT_SHARE`] of the top rank.
    pub fn hits(&self) -> Vec<Hit> {
        let songs = self.songs();
        let Some(best) = songs.first().map(|h| h.rank) else {
            return Vec::new();
        };
        let floor = (best as f64 * HIT_SHARE) as u64;
        songs.into_iter().filter(|h| h.rank >= floor).collect()
    }

    /// Every song of the top list (its best-known songs), deduplicated by
    /// title – versions of one song count once –, best first.
    pub fn songs(&self) -> Vec<Hit> {
        let mut out: Vec<Hit> = Vec::new();
        for t in &self.top {
            let key = title_key(&t.title);
            if key.is_empty() {
                continue;
            }
            match out.iter_mut().find(|h| h.key == key) {
                Some(h) if h.rank < t.rank => {
                    h.rank = t.rank;
                    h.title = t.title.clone();
                }
                Some(_) => {}
                None => out.push(Hit {
                    key,
                    title: t.title.clone(),
                    rank: t.rank,
                }),
            }
        }
        out.sort_by_key(|h| std::cmp::Reverse(h.rank));
        out
    }

    /// Rank of a song in the top list (best version), if it is listed.
    pub fn rank_of(&self, title: &str) -> Option<u64> {
        let key = title_key(title);
        self.songs()
            .into_iter()
            .find(|h| h.key == key)
            .map(|h| h.rank)
    }
}

/// Matching key of a song title: the title without a trailing version note –
/// "(Live at Wembley)", "[Remastered]", " - 2011 Remaster", "(feat. X)" – then
/// folded like [`normalize_name`]. So the studio song, its remaster and its
/// live take share one key. Falls back to the full title when stripping would
/// leave nothing ("(Untitled)").
pub fn title_key(title: &str) -> String {
    let cut = title.find(['(', '[']).map_or(title, |i| &title[..i]);
    let cut = cut.split(" - ").next().unwrap_or(cut);
    let key = normalize_name(cut);
    if key.is_empty() {
        normalize_name(title)
    } else {
        key
    }
}

/// Whether a local title is the plain song (no version note), preferred when
/// a hit exists several times locally (studio album vs. live / best-of).
fn is_plain(title: &str) -> bool {
    title_key(title) == normalize_name(title)
}

/// The local copy of a song (`key` as from [`title_key`]): among several
/// versions the plain title wins, otherwise the first one in `local` order.
pub fn find_local<T: Clone>(key: &str, local: &[(T, String)]) -> Option<T> {
    let mut versions = local.iter().filter(|(_, t)| title_key(t) == key);
    let first = versions.next()?;
    let plain = std::iter::once(first)
        .chain(versions)
        .find(|(_, t)| is_plain(t));
    Some(plain.unwrap_or(first).0.clone())
}

/// Picks the local tracks that are hits: one per hit song, in `hits` order.
/// `local` is `(item, title)` in the caller's preferred order (see
/// [`find_local`]).
pub fn pick_hits<T: Clone>(hits: &[Hit], local: &[(T, String)]) -> Vec<T> {
    hits.iter()
        .filter_map(|h| find_local(&h.key, local))
        .collect()
}

/// Popularity score 0–100 from a Deezer rank (which tops out near 1,000,000).
pub fn score(rank: u64) -> u64 {
    (rank / 10_000).min(100)
}

/// A score 0–100 as 0–5 stars in half steps (in tenths: 0, 5, 10 … 50).
pub fn half_stars(score: u64) -> u64 {
    (score.min(100) + 5) / 10 * 5
}

impl OnlineClient {
    /// Fan count and top tracks of `name` on Deezer. `Ok(None)` when Deezer has
    /// no artist of exactly that name – a fuzzy hit would credit the wrong
    /// artist's songs as hits.
    pub fn artist_popularity(&self, name: &str) -> Result<Option<ArtistPopularity>> {
        if crate::core::placeholder::is_placeholder(name) || name.trim().is_empty() {
            return Ok(None);
        }
        let Some(artist) = self.find_deezer_artist(name, true)? else {
            return Ok(None);
        };
        let url = format!(
            "https://api.deezer.com/artist/{}/top?limit={TOP_LIMIT}",
            artist.id
        );
        let top: DzTopList = self.get_json(&url)?.unwrap_or_default();
        Ok(Some(ArtistPopularity {
            fans: artist.nb_fan,
            top: top
                .data
                .into_iter()
                .filter(|t| !t.title.trim().is_empty())
                .map(|t| PopTrack {
                    title: t.title,
                    rank: t.rank,
                })
                .collect(),
        }))
    }

    /// Deezer rank of one song (best of its versions by that artist), for a
    /// song outside the artist's top list. `Ok(None)` when nothing matches.
    pub fn track_rank(&self, artist: &str, title: &str) -> Result<Option<u64>> {
        if artist.trim().is_empty() || title.trim().is_empty() {
            return Ok(None);
        }
        // Free text, not `artist:"…" track:"…"`: Deezer's field search misses
        // many tracks, and the filter below keeps only exact matches anyway.
        let q = format!("{artist} {title}");
        let url = format!(
            "https://api.deezer.com/search/track?q={}&limit=10",
            percent_encode(&q)
        );
        let search: DzTopList = self.get_json(&url)?.unwrap_or_default();
        let (want_artist, want_title) = (normalize_name(artist), title_key(title));
        Ok(search
            .data
            .into_iter()
            .filter(|t| {
                title_key(&t.title) == want_title
                    && t.artist
                        .as_ref()
                        .is_some_and(|a| normalize_name(&a.name) == want_artist)
            })
            .map(|t| t.rank)
            .max())
    }

    /// GET + capped JSON decode; `Ok(None)` for a 404.
    fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<Option<T>> {
        match self.call_get(url)? {
            Some(resp) => Ok(Some(net::json_capped(resp, net::MAX_JSON_BYTES)?)),
            None => Ok(None),
        }
    }
}

#[derive(Deserialize, Default)]
struct DzTopList {
    #[serde(default)]
    data: Vec<DzRankedTrack>,
}

#[derive(Deserialize)]
struct DzRankedTrack {
    #[serde(default)]
    title: String,
    #[serde(default)]
    rank: u64,
    #[serde(default)]
    artist: Option<DzName>,
}

#[derive(Deserialize)]
struct DzName {
    #[serde(default)]
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pop(top: &[(&str, u64)]) -> ArtistPopularity {
        ArtistPopularity {
            fans: 1,
            top: top
                .iter()
                .map(|(t, r)| PopTrack {
                    title: t.to_string(),
                    rank: *r,
                })
                .collect(),
        }
    }

    #[test]
    fn title_key_drops_version_notes() {
        assert_eq!(title_key("Bohemian Rhapsody"), "bohemian rhapsody");
        assert_eq!(
            title_key("Bohemian Rhapsody (Live At Wembley Stadium / July 1986)"),
            "bohemian rhapsody"
        );
        assert_eq!(
            title_key("Under Pressure (feat. David Bowie)"),
            "under pressure"
        );
        assert_eq!(title_key("Save Me - 2011 Remaster"), "save me");
        assert_eq!(title_key("Help [Remastered]"), "help");
        assert_eq!(title_key("Don't Stop Me Now"), "dont stop me now");
        // Nothing left after stripping → the full title.
        assert_eq!(title_key("(Untitled)"), "untitled");
    }

    #[test]
    fn hits_dedup_by_song_keep_best_version_and_cut_at_half() {
        let p = pop(&[
            ("Radio Ga Ga", 870_000),
            ("Bohemian Rhapsody (Live)", 580_000),
            ("Bohemian Rhapsody", 950_000),
            ("Spread Your Wings", 500_000),
            ("One Year Of Love", 440_000),
        ]);
        let hits = p.hits();
        let titles: Vec<_> = hits.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Bohemian Rhapsody", "Radio Ga Ga", "Spread Your Wings"]
        );
        // 440k < 950k / 2 → no hit, but still known with its rank.
        assert_eq!(p.rank_of("One Year of Love"), Some(440_000));
        assert_eq!(p.rank_of("Bohemian Rhapsody (Remastered)"), Some(950_000));
        assert_eq!(p.rank_of("Mustapha"), None);
    }

    #[test]
    fn hits_of_an_empty_top_list_are_empty() {
        assert!(pop(&[]).hits().is_empty());
    }

    #[test]
    fn pick_hits_follows_hit_order_and_prefers_the_plain_version() {
        let hits = pop(&[("Radio Ga Ga", 900_000), ("Killer Queen", 800_000)]).hits();
        let local = vec![
            (1, "Killer Queen (Live)".to_string()),
            (2, "Killer Queen".to_string()),
            (3, "Radio Ga Ga (Live Aid)".to_string()),
            (4, "Mustapha".to_string()),
        ];
        // Radio Ga Ga first (better rank), only a live take exists locally;
        // Killer Queen as the plain studio title.
        assert_eq!(pick_hits(&hits, &local), vec![3, 2]);
    }

    #[test]
    fn half_stars_round_to_half_steps() {
        assert_eq!(half_stars(0), 0);
        assert_eq!(half_stars(4), 0);
        assert_eq!(half_stars(5), 5);
        assert_eq!(half_stars(91), 45);
        assert_eq!(half_stars(95), 50);
        assert_eq!(half_stars(100), 50);
    }

    #[test]
    fn songs_count_versions_once() {
        let p = pop(&[
            ("Bohemian Rhapsody", 950_000),
            ("Bohemian Rhapsody (Live)", 580_000),
            ("One Year Of Love", 440_000),
        ]);
        let titles: Vec<_> = p.songs().into_iter().map(|h| h.title).collect();
        assert_eq!(titles, ["Bohemian Rhapsody", "One Year Of Love"]);
    }

    #[test]
    fn score_maps_rank_to_0_100() {
        assert_eq!(score(958_949), 95);
        assert_eq!(score(0), 0);
        assert_eq!(score(2_000_000), 100);
    }
}
