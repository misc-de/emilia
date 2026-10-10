//! Popularity in the detail view: the success lines in the "Info" block (fans,
//! a song's popularity as stars), "Go to the hits" / "Best-known songs" among
//! an artist's actions, which open a song list (best first; songs missing
//! locally greyed with "+" to add them via YouTube), and "Play hits only" for
//! an album.
//! Data from a music database (Deezer, see `core::online::popularity`), cached
//! in the DB; a missing entry is fetched in the background and filled into
//! the still-open dialog.

use std::path::PathBuf;

use adw::prelude::*;
use relm4::{ComponentController, adw, gtk};

use crate::core::online::{
    ArtistPopularity, Hit, OnlineClient, find_local, half_stars, pick_hits, score, title_key,
};
use crate::core::scanner;
use crate::i18n::{gettext, gettext_f};
use crate::ui::app::{App, CtxTarget, FsKind, Msg};
use crate::ui::app_dialogs::CtxMsg;
use crate::ui::app_helpers::online_available;

/// Which song list of an artist an info row opens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum PopList {
    /// The artist's hits.
    Hits,
    /// Every song of the artist's top list.
    Known,
}

/// One popularity row of the "Info" block.
enum InfoRow {
    Text(String, String),
    /// Popularity score 0–100, shown as stars.
    Stars(String, u64),
}

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
    /// Rows to show (empty: known, but nothing worth showing).
    Ready(Vec<InfoRow>),
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

/// A fan count, compact: "12.8 M", "350 k", below ten thousand in full.
fn fmt_fans(n: u64) -> String {
    if n >= 1_000_000 {
        let tenths = (n + 50_000) / 100_000;
        let (int, frac) = (tenths / 10, tenths % 10);
        if frac == 0 || int >= 100 {
            gettext_f("{int} M", &[("int", &int.to_string())])
        } else {
            gettext_f(
                "{int}.{frac} M",
                &[("int", &int.to_string()), ("frac", &frac.to_string())],
            )
        }
    } else if n >= 10_000 {
        gettext_f("{int} k", &[("int", &((n + 500) / 1000).to_string())])
    } else {
        fmt_count(n)
    }
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
                lines.push(InfoRow::Stars(gettext("Popularity (Deezer)"), score(r)));
            }
            let key = title_key(&title);
            if let Some(place) = hits.iter().position(|h| h.key == key) {
                lines.push(InfoRow::Text(
                    gettext("Hit"),
                    gettext_f(
                        "No. {n} of the most popular songs by {artist}",
                        &[("n", &(place + 1).to_string()), ("artist", &artist)],
                    ),
                ));
            }
            return Some(PopLines::Ready(lines));
        }
        match self.hits_target(target) {
            Some(HitsTarget::Artist(_)) => {
                if pop.fans > 0 {
                    lines.push(InfoRow::Text(gettext("Fans (Deezer)"), fmt_fans(pop.fans)));
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
                lines.push(InfoRow::Text(
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
            Some(PopLines::Ready(lines)) => self.add_info_rows(expander, lines),
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
            self.add_info_rows(&expander, lines);
            *self.nav.pop_info.borrow_mut() = None;
        }
        self.refresh_pop_links();

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

impl App {
    /// Appends popularity rows to an "Info" expander (same look as the static
    /// info rows; a list row opens its song list on tap).
    fn add_info_rows(&self, expander: &adw::ExpanderRow, rows: Vec<InfoRow>) {
        for info in rows {
            let row = adw::ActionRow::new();
            match info {
                InfoRow::Text(label, value) => {
                    row.set_title(&label);
                    row.set_subtitle(&gtk::glib::markup_escape_text(&value));
                    row.set_subtitle_lines(2);
                }
                InfoRow::Stars(label, score) => {
                    row.set_title(&label);
                    row.add_suffix(&stars(score));
                }
            }
            expander.add_row(&row);
        }
    }

    /// Adds "Go to the hits" and "Best-known songs" to an artist's actions;
    /// hidden until the popularity data is known (see
    /// [`Self::refresh_pop_links`]), a tap opens the song list.
    pub(crate) fn add_pop_links(&self, group: &adw::PreferencesGroup, artist: &str) {
        let make = |label: String, icon: &str, list: PopList| {
            let row = adw::ActionRow::builder()
                .title(label)
                .activatable(true)
                .visible(false)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let (input, artist) = (self.input.clone(), artist.to_string());
            row.connect_activated(move |_| {
                let _ = input.send(Msg::Ctx(CtxMsg::OpenPopList {
                    artist: artist.clone(),
                    list,
                }));
            });
            group.add(&row);
            row
        };
        let hits = make(
            gettext("Go to the hits"),
            "emilia-hits-symbolic",
            PopList::Hits,
        );
        let known = make(
            gettext("Best-known songs"),
            "emilia-stats-symbolic",
            PopList::Known,
        );
        *self.nav.pop_links.borrow_mut() = Some((main_artist(artist), hits, known));
        self.refresh_pop_links();
    }

    /// Shows the open artist dialog's list rows that have songs to list.
    fn refresh_pop_links(&self) {
        let links = self.nav.pop_links.borrow();
        let Some((artist, hits, known)) = links.as_ref() else {
            return;
        };
        let pop = self.cached_artist_pop(artist).flatten();
        hits.set_visible(pop.as_ref().is_some_and(|p| !p.hits().is_empty()));
        known.set_visible(pop.as_ref().is_some_and(|p| !p.top.is_empty()));
    }

    /// Song list of an artist ("Go to the hits" / "Best-known songs"), most
    /// popular first. Songs in the library play on tap (the list's local songs
    /// from there on); missing ones are greyed and – with YouTube enabled –
    /// can be added like a missing album track.
    pub(crate) fn open_pop_list(
        &self,
        root: &adw::ApplicationWindow,
        artist: String,
        list: PopList,
    ) {
        let Some(Some(pop)) = self.cached_artist_pop(&artist) else {
            return;
        };
        let songs = match list {
            PopList::Hits => pop.hits(),
            PopList::Known => pop.songs(),
        };
        // Oldest first: among several copies of a song, the original album's
        // comes before a later best-of.
        let local: Vec<(PathBuf, String)> = self
            .artist_tracks_ordered(&artist, false)
            .into_iter()
            .map(|t| (PathBuf::from(t.path), t.title))
            .collect();
        let entries: Vec<(Hit, Option<PathBuf>)> = songs
            .into_iter()
            .map(|h| {
                let path = find_local(&h.key, &local);
                (h, path)
            })
            .collect();
        let present: Vec<PathBuf> = entries.iter().filter_map(|(_, p)| p.clone()).collect();
        let can_add = self.youtube.enabled;
        // With any "+" in the list, every other row gets an invisible one of
        // the same size, so the stars line up in one column.
        let any_add = can_add && entries.iter().any(|(_, p)| p.is_none());
        let add_icon = |visible: bool| {
            let img = gtk::Image::from_icon_name("list-add-symbolic");
            img.set_valign(gtk::Align::Center);
            if !visible {
                img.set_opacity(0.0);
            }
            img
        };

        let title = match list {
            PopList::Hits => gettext("Hits"),
            PopList::Known => gettext("Best-known songs"),
        };
        let dialog = adw::Dialog::builder().title(&title).build();
        dialog.set_content_width(600);
        self.adapt_detail_dialog(&dialog);
        let group = adw::PreferencesGroup::new();
        for (place, (hit, path)) in entries.into_iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&hit.title))
                .build();
            row.add_prefix(
                &gtk::Label::builder()
                    .label((place + 1).to_string())
                    .width_chars(2)
                    .xalign(1.0)
                    .css_classes(["dim-label", "numeric"])
                    .build(),
            );
            row.add_suffix(&stars(score(hit.rank)));
            let adds = path.is_none() && can_add;
            match path {
                Some(path) => {
                    row.set_activatable(true);
                    let start = present.iter().position(|p| *p == path).unwrap_or(0);
                    let (input, queue, dialog) =
                        (self.input.clone(), present.clone(), dialog.clone());
                    row.connect_activated(move |_| {
                        let _ = input.send(Msg::Ctx(CtxMsg::PlayPopList {
                            queue: queue.clone(),
                            start,
                        }));
                        dialog.close();
                    });
                }
                None => {
                    // Greyed like a missing album track: "not here yet".
                    row.set_opacity(0.55);
                    if can_add {
                        row.set_subtitle(&gettext("Missing — tap to add"));
                        row.set_activatable(true);
                        let (input, artist, title) =
                            (self.input.clone(), artist.clone(), hit.title.clone());
                        row.connect_activated(move |_| {
                            let _ = input.send(Msg::Ctx(CtxMsg::AddPopSong {
                                artist: artist.clone(),
                                title: title.clone(),
                            }));
                        });
                    } else {
                        row.set_subtitle(&gettext("Not in the library"));
                    }
                }
            }
            if any_add {
                row.add_suffix(&add_icon(adds));
            }
            group.add(&row);
        }
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&group);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .vexpand(true)
            .child(&content)
            .build();
        let header = crate::ui::widgets::detail_header();
        header.set_title_widget(Some(&adw::WindowTitle::new(&title, &artist)));
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        crate::ui::app_helpers::close_on_click_outside(&dialog);
        dialog.present(Some(root));
    }

    /// A song list row in the library was tapped: play the list's local songs
    /// from there (shuffle off) and close the detail view below.
    pub(crate) fn play_pop_list(&mut self, queue: Vec<PathBuf>, start: usize) {
        if queue.is_empty() {
            return;
        }
        self.transport.shuffle = false;
        self.transport.queue = queue;
        self.transport.queue_pos = start;
        self.play_current();
        self.refresh_queue_icons();
        let dialog = self.nav.ctx_dialog.borrow().clone();
        if let Some(d) = dialog {
            d.close();
        }
    }

    /// "+" on a missing song: search YouTube for it; the hits come back as
    /// [`CtxMsg::PopSongCandidates`] to pick a version from.
    pub(crate) fn add_pop_song(
        &mut self,
        root: &adw::ApplicationWindow,
        artist: String,
        title: String,
    ) {
        if !self.youtube.enabled || !crate::core::youtube::available() {
            self.toast(&gettext("Enable YouTube in the settings to use this"));
            return;
        }
        self.show_missing_busy(root, &gettext("Searching online …"));
        let input = self.input.clone();
        std::thread::spawn(move || {
            let query = format!("{artist} {title}");
            let results = crate::core::panic_guard::catch_or(
                "popular-song search",
                || {
                    crate::core::youtube::search(&query, crate::core::youtube::YtKind::Video, 10)
                        .unwrap_or_default()
                },
                Vec::new,
            );
            let _ = input.send(Msg::Ctx(CtxMsg::PopSongCandidates {
                artist,
                title,
                results,
            }));
        });
    }

    /// YouTube hits for a missing song: let the user pick a version, which is
    /// then added to the library like any YouTube download (tagged from the
    /// music database, filed under the artist).
    pub(crate) fn on_pop_song_candidates(
        &mut self,
        root: &adw::ApplicationWindow,
        artist: String,
        title: String,
        results: Vec<crate::core::youtube::YtResult>,
    ) {
        if let Some((d, _)) = self.libview.missing_busy.take() {
            d.close();
        }
        if results.is_empty() {
            self.toast(&gettext("Not found on YouTube"));
            return;
        }
        let yt = self.yt_page.sender().clone();
        let picked = title.clone();
        self.show_version_chooser(root, &title, &results, move |video_id| {
            yt.emit(crate::ui::yt_page::YtInput::AddToLibrary {
                video_id,
                title: picked.clone(),
                artist: Some(artist.clone()),
            });
        });
    }
}

/// A score 0–100 as five stars in half steps; the exact score as tooltip.
fn stars(score: u64) -> gtk::Box {
    let tenths = half_stars(score);
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(2)
        .valign(gtk::Align::Center)
        .tooltip_text(format!("{score} / 100"))
        .build();
    for i in 0..5u64 {
        let icon = match tenths.saturating_sub(i * 10) {
            10.. => "starred-symbolic",
            5..=9 => "semi-starred-symbolic",
            _ => "non-starred-symbolic",
        };
        let img = gtk::Image::from_icon_name(icon);
        if icon == "non-starred-symbolic" {
            img.add_css_class("dim-label");
        }
        row.append(&img);
    }
    row
}

#[cfg(test)]
mod tests {
    use super::{fmt_count, fmt_fans};

    #[test]
    fn fmt_fans_is_compact() {
        assert_eq!(fmt_fans(12_827_971), "12.8 M");
        assert_eq!(fmt_fans(2_000_000), "2 M");
        assert_eq!(fmt_fans(150_400_000), "150 M");
        assert_eq!(fmt_fans(349_600), "350 k");
        assert_eq!(fmt_fans(9_999), "9\u{202F}999");
    }

    #[test]
    fn fmt_count_groups_thousands() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(967), "967");
        assert_eq!(fmt_count(12_827_971), "12\u{202F}827\u{202F}971");
        assert_eq!(fmt_count(100_000), "100\u{202F}000");
    }
}
