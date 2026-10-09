//! Popularity in the detail view: "Play hits only" for an artist/album and
//! the success lines (fans, best-known songs, hits, song popularity) in the
//! "Info" block. Data from a music database (Deezer, see
//! `core::online::popularity`), cached in the DB; a missing entry is fetched
//! in the background and filled into the still-open dialog.

use std::path::PathBuf;

use adw::prelude::*;
use relm4::adw;

use crate::core::online::{ArtistPopularity, Hit, OnlineClient, pick_hits, score, title_key};
use crate::core::scanner;
use crate::i18n::{gettext, gettext_f};
use crate::ui::app::{App, CtxTarget, FsKind, Msg};
use crate::ui::app_dialogs::CtxMsg;
use crate::ui::app_helpers::online_available;

/// How many hits the artist's info line names.
const NAMED_HITS: usize = 5;

/// What "Play hits only" plays.
#[derive(Debug, Clone)]
pub(crate) enum HitsTarget {
    Artist(String),
    Album { artist: String, album: String },
}

/// A finished background lookup. `None` in a result means the request failed
/// (offline, timeout) – nothing is cached then, so the next open retries.
#[derive(Debug)]
pub(crate) struct PopFetch {
    /// The (main) artist looked up.
    artist: String,
    /// `Some(None)`: Deezer knows no such artist. Absent when it was cached.
    artist_pop: Option<Option<ArtistPopularity>>,
    /// A song outside the top list: its title and rank lookup.
    track: Option<(String, Option<Option<u64>>)>,
    /// Play these hits once the data is there ("Play hits only" clicked
    /// before the lookup finished).
    play: Option<HitsTarget>,
}

/// The popularity info of a detail target.
enum PopLines {
    /// Lines to show (empty: known, but nothing worth showing).
    Ready(Vec<(String, String)>),
    /// Not cached yet: look up this artist (and this song title).
    Fetch {
        artist: String,
        title: Option<String>,
    },
}

/// The leading artist of a credit ("A feat. B" → "A") – the one Deezer lists.
fn main_artist(credit: &str) -> String {
    use crate::core::artist::{CreditMode, split_artists_with};
    split_artists_with(CreditMode::Primary, credit)
        .into_iter()
        .next()
        .unwrap_or_else(|| credit.trim().to_string())
}

fn artist_key(name: &str) -> String {
    crate::core::artist::norm_key(name)
}

fn track_key(artist: &str, title: &str) -> String {
    format!("{}\u{1f}{}", artist_key(artist), title_key(title))
}

/// Thousands grouped by a narrow no-break space ("12 827 971"), readable in
/// every language without locale data.
fn fmt_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{202F}');
        }
        out.push(c);
    }
    out
}

impl App {
    /// Cached popularity of an artist: `None` = not cached (fetch),
    /// `Some(None)` = Deezer doesn't know the artist.
    fn cached_artist_pop(&self, artist: &str) -> Option<Option<ArtistPopularity>> {
        self.library
            .cached_popularity("artist", &artist_key(artist))
            .map(|data| data.and_then(|json| serde_json::from_str(&json).ok()))
    }

    /// Cached rank of a song outside the artist's top list (same shape).
    fn cached_track_rank(&self, artist: &str, title: &str) -> Option<Option<u64>> {
        self.library
            .cached_popularity("track", &track_key(artist, title))
            .map(|data| data.and_then(|s| s.parse().ok()))
    }

    /// The artist a hits lookup / the popularity lines are about, plus the
    /// song title for a single track. `None` for folders without a music
    /// kind, remote entries and audiobooks.
    fn pop_subject(&self, target: &CtxTarget) -> Option<(String, Option<String>)> {
        if self.is_audiobook(target) {
            return None;
        }
        match target {
            CtxTarget::Artist(m) => Some((m.name.clone(), None)),
            CtxTarget::Album(m) => Some((main_artist(&m.artist), None)),
            CtxTarget::Fs(e) if e.is_dir() => match self.fs_music_kind(e)? {
                FsKind::Artist(name) => Some((name, None)),
                FsKind::Album { artist, .. } => Some((main_artist(&artist), None)),
            },
            CtxTarget::Fs(e) => {
                let t = scanner::read_track(e.path()?).ok()?;
                let artist = main_artist(t.artist.as_deref()?);
                Some((artist, Some(t.title)))
            }
        }
        .filter(|(a, _)| !a.is_empty() && !crate::core::placeholder::is_placeholder(a))
    }

    /// The local hits of an artist (one file per hit song, best first) or of
    /// an album (in album order).
    fn local_hits(&self, target: &HitsTarget, pop: &ArtistPopularity) -> Vec<PathBuf> {
        let hits = pop.hits();
        match target {
            HitsTarget::Artist(name) => {
                // Oldest first: among several copies of a song, the original
                // album's comes before a later best-of.
                let local: Vec<(PathBuf, String)> = self
                    .artist_tracks_ordered(name, false)
                    .into_iter()
                    .map(|t| (PathBuf::from(t.path), t.title))
                    .collect();
                pick_hits(&hits, &local)
            }
            HitsTarget::Album { artist, album } => {
                let tracks = self.album_tracks(artist, album);
                let is_hit = |title: &str| {
                    let key = title_key(title);
                    hits.iter().any(|h| h.key == key)
                };
                tracks
                    .into_iter()
                    .filter(|t| is_hit(&t.title))
                    .map(|t| PathBuf::from(t.path))
                    .collect()
            }
        }
    }

    /// The popularity lines of a detail target, or what to fetch for them.
    fn popularity_lines(&self, target: &CtxTarget) -> Option<PopLines> {
        let (artist, title) = self.pop_subject(target)?;
        let pop = match self.cached_artist_pop(&artist) {
            None => return Some(PopLines::Fetch { artist, title }),
            Some(None) => return Some(PopLines::Ready(Vec::new())),
            Some(Some(p)) => p,
        };
        let hits = pop.hits();
        let mut lines = Vec::new();
        if let Some(title) = title {
            // A single song: its popularity score, plus its place if it's a hit.
            let rank = match pop.rank_of(&title) {
                Some(r) => Some(r),
                None => match self.cached_track_rank(&artist, &title) {
                    None => {
                        return Some(PopLines::Fetch {
                            artist,
                            title: Some(title),
                        });
                    }
                    Some(r) => r,
                },
            };
            if let Some(r) = rank {
                lines.push((
                    gettext("Popularity (Deezer)"),
                    format!("{} / 100", score(r)),
                ));
            }
            let key = title_key(&title);
            if let Some(place) = hits.iter().position(|h| h.key == key) {
                lines.push((
                    gettext("Hit"),
                    gettext_f(
                        "No. {n} of the most popular songs by {artist}",
                        &[("n", &(place + 1).to_string()), ("artist", &artist)],
                    ),
                ));
            }
            return Some(PopLines::Ready(lines));
        }
        let names = |hs: &[Hit]| {
            hs.iter()
                .map(|h| h.title.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        match self.hits_target(target) {
            Some(t @ HitsTarget::Artist(_)) => {
                if pop.fans > 0 {
                    lines.push((gettext("Fans (Deezer)"), fmt_count(pop.fans)));
                }
                if !hits.is_empty() {
                    lines.push((
                        gettext("Best-known songs"),
                        names(&hits[..hits.len().min(NAMED_HITS)]),
                    ));
                    let n = self.local_hits(&t, &pop).len();
                    lines.push((
                        gettext("Hits in the library"),
                        gettext_f(
                            "{n} of {total}",
                            &[("n", &n.to_string()), ("total", &hits.len().to_string())],
                        ),
                    ));
                }
            }
            Some(HitsTarget::Album { artist, album }) if !hits.is_empty() => {
                // The album's hits in album order, by their local titles.
                let found: Vec<String> = self
                    .album_tracks(&artist, &album)
                    .into_iter()
                    .filter(|t| {
                        let key = title_key(&t.title);
                        hits.iter().any(|h| h.key == key)
                    })
                    .map(|t| t.title)
                    .collect();
                lines.push((
                    gettext("Hits on this album"),
                    if found.is_empty() {
                        "—".to_string()
                    } else {
                        found.join(", ")
                    },
                ));
            }
            _ => {}
        }
        Some(PopLines::Ready(lines))
    }

    /// What "Play hits only" plays for a target: an artist or an album
    /// (cards and folders recognized as such); nothing for audiobooks.
    pub(crate) fn hits_target(&self, target: &CtxTarget) -> Option<HitsTarget> {
        if self.is_audiobook(target) {
            return None;
        }
        match target {
            CtxTarget::Artist(m) => Some(HitsTarget::Artist(m.name.clone())),
            CtxTarget::Album(m) => Some(HitsTarget::Album {
                artist: m.artist.clone(),
                album: m.album.clone(),
            }),
            CtxTarget::Fs(e) if e.is_dir() => match self.fs_music_kind(e)? {
                FsKind::Artist(name) => Some(HitsTarget::Artist(name)),
                FsKind::Album { artist, album } => Some(HitsTarget::Album { artist, album }),
            },
            CtxTarget::Fs(_) => None,
        }
    }

    /// Adds the target's popularity lines to the detail view's "Info"
    /// expander – right away from the cache, otherwise once the background
    /// lookup returns (see [`Self::on_popularity_fetched`]).
    pub(crate) fn attach_popularity(&self, target: &CtxTarget, expander: &adw::ExpanderRow) {
        *self.nav.pop_info.borrow_mut() = None;
        match self.popularity_lines(target) {
            Some(PopLines::Ready(lines)) => add_info_rows(expander, lines),
            Some(PopLines::Fetch { artist, title }) if online_available() => {
                *self.nav.pop_info.borrow_mut() = Some(expander.clone());
                self.fetch_popularity(artist, title, None);
            }
            Some(PopLines::Fetch { .. }) => {}
            None => {}
        }
    }

    /// Looks up an artist's popularity (and a song's rank) in the background;
    /// the result arrives as [`CtxMsg::PopularityFetched`].
    fn fetch_popularity(&self, artist: String, title: Option<String>, play: Option<HitsTarget>) {
        let cached = self.cached_artist_pop(&artist);
        let input = self.input.clone();
        std::thread::spawn(move || {
            let client = OnlineClient::new();
            let artist_pop = match cached {
                Some(_) => None,
                None => client.artist_popularity(&artist).ok(),
            };
            let known = cached.or_else(|| artist_pop.clone()).flatten();
            // A song needs its own lookup only when the artist is known but the
            // song is outside the top list.
            let track = title
                .filter(|t| known.as_ref().is_some_and(|p| p.rank_of(t).is_none()))
                .map(|t| {
                    let rank = client.track_rank(&artist, &t).ok();
                    (t, rank)
                });
            let _ = input.send(Msg::Ctx(CtxMsg::PopularityFetched(Box::new(PopFetch {
                artist,
                artist_pop,
                track,
                play,
            }))));
        });
    }

    /// A popularity lookup returned: cache it, fill the open detail view's
    /// info lines and – if "Play hits only" was waiting for it – play.
    pub(crate) fn on_popularity_fetched(&mut self, fetch: PopFetch) {
        let PopFetch {
            artist,
            artist_pop,
            track,
            play,
        } = fetch;
        if let Some(pop) = &artist_pop {
            let json = pop.as_ref().and_then(|p| serde_json::to_string(p).ok());
            self.library
                .store_popularity("artist", &artist_key(&artist), json.as_deref());
        }
        if let Some((title, Some(rank))) = &track {
            let data = rank.map(|r| r.to_string());
            self.library
                .store_popularity("track", &track_key(&artist, title), data.as_deref());
        }

        // Fill the waiting info expander, if its dialog is still open and its
        // data is complete now (another target's lookup may still be running).
        let pending = self.nav.pop_info.borrow().clone();
        if let Some(expander) = pending
            && self.nav.ctx_dialog.borrow().is_some()
            && let Some(target) = self.nav.context_target.clone()
            && let Some(PopLines::Ready(lines)) = self.popularity_lines(&target)
        {
            add_info_rows(&expander, lines);
            *self.nav.pop_info.borrow_mut() = None;
        }

        if let Some(target) = play {
            match self.cached_artist_pop(&artist) {
                Some(pop) => self.play_hits(&target, pop),
                None => self.set_hits_status(&gettext("Could not look up the hits")),
            }
        }
    }

    /// "Play hits only": plays the hits of the open artist/album, looking them
    /// up first when they aren't cached yet.
    pub(crate) fn on_ctx_play_hits(&mut self) {
        let Some(target) = self
            .nav
            .context_target
            .clone()
            .and_then(|t| self.hits_target(&t))
        else {
            return;
        };
        let artist = match &target {
            HitsTarget::Artist(name) => name.clone(),
            HitsTarget::Album { artist, .. } => main_artist(artist),
        };
        match self.cached_artist_pop(&artist) {
            Some(pop) => self.play_hits(&target, pop),
            None if online_available() => {
                self.set_hits_status(&gettext("Looking up the hits …"));
                self.fetch_popularity(artist, None, Some(target));
            }
            None => self.set_hits_status(&gettext("Needs an internet connection")),
        }
    }

    /// Plays the local hits (shuffle off) and closes the detail view; reports
    /// on the action row when there are none.
    fn play_hits(&mut self, target: &HitsTarget, pop: Option<ArtistPopularity>) {
        let Some(pop) = pop else {
            self.set_hits_status(&gettext("No popularity data for this artist"));
            return;
        };
        let files = self.local_hits(target, &pop);
        if files.is_empty() {
            self.set_hits_status(&gettext("No hits in the library"));
            return;
        }
        self.transport.shuffle = false;
        self.transport.queue = files;
        self.transport.queue_pos = 0;
        self.play_current();
        self.refresh_queue_icons();
        let dialog = self.nav.ctx_dialog.borrow().clone();
        if let Some(d) = dialog {
            d.close();
        }
    }

    /// Shows a status on the open detail view's "Play hits only" row.
    fn set_hits_status(&self, text: &str) {
        if let Some(row) = self.nav.hits_row.borrow().as_ref() {
            row.set_subtitle(text);
        }
    }
}

/// Appends `(label, value)` rows to an "Info" expander (same look as the
/// static info rows).
fn add_info_rows(expander: &adw::ExpanderRow, lines: Vec<(String, String)>) {
    for (label, value) in lines {
        let row = adw::ActionRow::builder()
            .title(&label)
            .subtitle(relm4::gtk::glib::markup_escape_text(&value))
            .build();
        row.set_subtitle_lines(2);
        expander.add_row(&row);
    }
}

#[cfg(test)]
mod tests {
    use super::fmt_count;

    #[test]
    fn fmt_count_groups_thousands() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(967), "967");
        assert_eq!(fmt_count(12_827_971), "12\u{202F}827\u{202F}971");
        assert_eq!(fmt_count(100_000), "100\u{202F}000");
    }
}
