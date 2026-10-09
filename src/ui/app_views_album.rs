//! Album/artist track views: track ordering helpers (disc/track structure,
//! natural sort, release order), the file lists of an artist/album/entry, the
//! artist → albums → tracks subpages and the album track page. Split out of
//! [`crate::ui::app_views`] – pure reorganization, no functional change.

use std::path::PathBuf;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::scanner;
use crate::i18n::{gettext, gettext_noop};
use crate::model::{ArtistMeta, Track};
use crate::ui::app::{ActiveSource, App, Cmd, Msg, album_subtitle, most_common_artist};
use crate::ui::entry_row::EntryRow;
use crate::ui::fs_row::FsEntry;

/// How a track tapped in an album track list is played back. Album contexts
/// (`Artist`/`Name`) play only the tapped track; a `Folder` (audiobook/concert)
/// keeps playing the whole folder so chapters continue.
#[derive(Clone)]
pub(crate) enum AlbumPlay {
    /// Artist context (artist → album).
    Artist,
    /// Album card of the overviews (album name across the card's artists;
    /// the page's `name` holds the card's artist).
    Name,
    /// Folder content (audiobook/concert): exactly the files in this folder.
    Folder(String),
}

/// Handle to the album track-list subpage currently rendered, so a late
/// MusicBrainz tracklist fetch — or a freshly downloaded missing track — can
/// refill the **same** content box in place (no navigation flicker).
#[derive(Clone)]
pub(crate) struct AlbumPageRef {
    /// Artist the page was opened for (the opener's argument; may be empty for
    /// the album-overview path).
    pub name: String,
    /// Display artist (most common across the album's tracks); the key for the
    /// canonical-tracklist lookup together with `album`.
    pub artist: String,
    pub album: String,
    pub play: AlbumPlay,
    /// The content box that lives inside the pushed navigation page.
    pub content: gtk::Box,
}

/// Worker-thread helper: resolve the album's MusicBrainz release (using the
/// stored mbid hint, else a fresh search) and cache its canonical tracklist.
/// Always records the fetch attempt (even on no match), so it runs at most once
/// per album until the cache is cleared.
fn fetch_and_store_tracklist(artist: &str, album: &str, mbid_hint: Option<&str>) {
    let Ok(lib) = Library::open() else { return };
    let client = crate::core::online::OnlineClient::new();
    let mbid = match mbid_hint {
        Some(m) if !m.trim().is_empty() => Some(m.to_string()),
        _ => client
            .match_release(artist, album)
            .ok()
            .flatten()
            .map(|m| m.mbid),
    };
    let tracks = match mbid {
        Some(m) => client.fetch_release_tracks(&m).unwrap_or_default(),
        None => Vec::new(),
    };
    let _ = lib.set_album_tracklist(artist, album, &tracks);
}

/// Album name without CD/disc suffix, so multi-CD albums collapse together:
/// "… Disc 2", "… CD 1", "… Cd 2 v 7", "… CD3" → the common base title.
pub(crate) fn album_base(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let clean = |w: &str| {
        w.to_lowercase()
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_string()
    };
    const MARKERS: [&str; 8] = [
        "disc", "disk", "cd", "teil", "part", "folge", "vol", "volume",
    ];
    let is_marker = |w: &str| {
        let c = clean(w);
        MARKERS.iter().any(|m| {
            c == *m
                || (c.starts_with(m)
                    && c.len() > m.len()
                    && c[m.len()..].chars().all(|d| d.is_ascii_digit()))
        })
    };
    const CONNECTORS: [&str; 6] = ["v", "von", "of", "x", "u", "und"];
    let is_suffix_tok = |w: &str| {
        let c = clean(w);
        c.is_empty()
            || c.chars().all(|d| d.is_ascii_digit())
            || CONNECTORS.contains(&c.as_str())
            || is_marker(w)
    };
    // First marker position from which on, until the end, only suffix tokens remain.
    let cut = (0..words.len())
        .find(|&i| is_marker(words[i]) && words[i..].iter().all(|w| is_suffix_tok(w)));
    let base = match cut {
        Some(i) => words[..i].join(" "),
        None => name.trim().to_string(),
    };
    let base = base.trim_matches(|c: char| c == '-' || c == ':' || c.is_whitespace());
    if base.is_empty() {
        name.trim().to_string()
    } else {
        base.to_string()
    }
}

pub(crate) use crate::core::album_group::disc_from_segment;

/// Effective disc number of a track. **File structure takes precedence:** a
/// CD/disc/part **subfolder** of the path is more reliable than a disc tag
/// (some audiobook rippers wrongly set `disc` = track number). Only real
/// "CD/Disc/Part…" folders count (see `disc_from_segment`), not the filename;
/// otherwise the `disc_no` tag, otherwise 1.
pub(crate) fn track_disc(t: &Track) -> u32 {
    if let Some(d) = std::path::Path::new(&t.path)
        .parent()
        .into_iter()
        .flat_map(|d| d.components())
        .filter_map(|c| c.as_os_str().to_str())
        .filter_map(disc_from_segment)
        .next_back()
    {
        return d;
    }
    t.disc_no.unwrap_or(1)
}

/// "Natural" sort key of a string: digit sequences are compared as numbers
/// (each digit block left-padded with zeros to a fixed width).
/// So "CD2" comes before "CD10", "3.2" before "3.10", and "01 01" before "02 01".
/// For the **file-structure sorting** of audiobooks/folder contents – robust
/// against missing zero-padding **and** unusable track tags.
pub(crate) fn natural_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut num = String::new();
            num.push(c);
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    num.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            let trimmed = num.trim_start_matches('0');
            let trimmed = if trimmed.is_empty() { "0" } else { trimmed };
            for _ in 0..16usize.saturating_sub(trimmed.len()) {
                out.push('0');
            }
            out.push_str(trimmed);
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Sorts tracks by **file structure**: subfolder (CD folder) first, then
/// disc, then track number, then path – consistent with playback
/// (`play_path`) and robust against wrong/missing disc tags (audiobooks).
fn sort_by_structure(tracks: &mut [Track]) {
    let parent = |t: &Track| {
        std::path::Path::new(&t.path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    tracks.sort_by(|a, b| {
        // Compare folder and file paths **naturally** (digit runs as numbers) so
        // that unpadded names ("Track 2" vs "Track 10", "CD2" vs "CD10") fall in
        // the right order. Without this the raw byte comparison was the only
        // tiebreak when track tags are missing/zero — the usual cause of an album
        // playing out of order.
        natural_key(&parent(a))
            .cmp(&natural_key(&parent(b)))
            .then(track_disc(a).cmp(&track_disc(b)))
            .then(a.track_no.unwrap_or(0).cmp(&b.track_no.unwrap_or(0)))
            .then_with(|| natural_key(&a.path).cmp(&natural_key(&b.path)))
    });
}

/// Folder holding one release of an album: the track's folder, or the one
/// above it when the track sits in a CD/disc subfolder of that release.
fn release_dir(t: &Track) -> &std::path::Path {
    let parent = std::path::Path::new(&t.path)
        .parent()
        .unwrap_or(std::path::Path::new(""));
    let in_disc_folder = parent
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(disc_from_segment)
        .is_some();
    match parent.parent() {
        Some(up) if in_disc_folder => up,
        _ => parent,
    }
}

/// Edge of the cover in an album page's header (logical px).
const ALBUM_HEADER_COVER: i32 = 128;

/// What an album page's header shows (see [`App::album_header`]).
struct AlbumHeader<'a> {
    cover: Option<&'a gtk::gdk::Texture>,
    album: &'a str,
    artist: &'a str,
    year: Option<i32>,
    /// The page's tracks in the order shown — the order "Play" queues.
    tracks: Vec<&'a Track>,
    /// Key of the "Play" icon in the page's play-mark registry.
    mark: String,
}

/// One section of an album page: the tracks of one release folder and disc,
/// by track number.
pub(crate) struct AlbumSection<'a> {
    /// The release folder — its name titles the section when an album spans
    /// several releases (a UK and a US single tagged with the same title).
    pub(crate) release: &'a std::path::Path,
    pub(crate) disc: u32,
    pub(crate) tracks: Vec<&'a Track>,
}

/// Splits album `tracks` (in [`sort_by_structure`] order) into the sections
/// the album page shows: per release folder, and within it per disc. The
/// flattened sections are the order "Play" queues, so the next track is
/// always the row below. Releases keep their folder order; discs and track
/// numbers ascend, ties keep the incoming order.
pub(crate) fn album_sections(tracks: &[Track]) -> Vec<AlbumSection<'_>> {
    let mut sections: Vec<AlbumSection> = Vec::new();
    for t in tracks {
        let (release, disc) = (release_dir(t), track_disc(t));
        match sections
            .iter_mut()
            .find(|s| s.release == release && s.disc == disc)
        {
            Some(s) => s.tracks.push(t),
            None => sections.push(AlbumSection {
                release,
                disc,
                tracks: vec![t],
            }),
        }
    }
    // Releases in order of first appearance, discs ascending within each.
    let releases: Vec<&std::path::Path> = sections.iter().fold(Vec::new(), |mut acc, s| {
        if !acc.contains(&s.release) {
            acc.push(s.release);
        }
        acc
    });
    sections.sort_by_key(|s| {
        let r = releases.iter().position(|r| *r == s.release).unwrap_or(0);
        (r, s.disc)
    });
    for s in &mut sections {
        s.tracks.sort_by_key(|t| t.track_no.unwrap_or(0));
    }
    sections
}

/// Most common album base title of a set of tracks (for the display title of a
/// subfolder grouped as an album).
pub(crate) fn most_common_album_base(tracks: &[&Track]) -> Option<String> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for t in tracks {
        if let Some(al) = t.album.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
            *counts.entry(album_base(al)).or_default() += 1;
        }
    }
    counts.into_iter().max_by_key(|(_, c)| *c).map(|(b, _)| b)
}

impl App {
    /// Returns the playable files of an entry: recursive for folders,
    /// only the single one for files.
    pub(crate) fn entry_files(&self, entry: &FsEntry) -> Vec<PathBuf> {
        // Remote (Nextcloud) entries: build the synthetic nc: paths so they can
        // be queued and played like local tracks (`start_track_playback` streams
        // them). Needs the source to be indexed (the DB holds the nc: tracks).
        if entry.is_remote() {
            let (Some(rel), ActiveSource::Source(id)) =
                (entry.rel_path(), &self.files.active_source)
            else {
                return Vec::new();
            };
            if entry.is_dir() {
                // All indexed tracks below this folder, in path (≈ track) order.
                let dir = crate::core::webdav::nc_path(*id, rel);
                let mut paths: Vec<PathBuf> = self
                    .library
                    .tracks_under_path(&dir)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| PathBuf::from(t.path))
                    .collect();
                paths.sort();
                return paths;
            }
            return vec![PathBuf::from(crate::core::webdav::nc_path(*id, rel))];
        }
        let Some(path) = entry.path() else {
            return Vec::new();
        };
        if entry.is_dir() {
            scanner::collect_audio_files(path)
        } else {
            vec![path.clone()]
        }
    }

    /// All files of an artist (from the library), in playback order.
    pub(crate) fn artist_files(&self, name: &str) -> Vec<PathBuf> {
        // Like the artist list (artist_sections/artist_albums): a track
        // counts toward the artist if their name appears in the artist
        // credit – possibly split from "feat." – (case-insensitive). Otherwise
        // the detail page would not count guest/composite tracks and would show "0
        // songs", even though the song list includes them.
        let target = crate::core::artist::norm_key(name);
        self.library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                t.artist
                    .as_deref()
                    .is_some_and(|a| crate::core::artist::credit_matches(a, &target))
            })
            .map(|t| PathBuf::from(t.path))
            .collect()
    }

    /// All files of an album (main artist + album), in playback order.
    /// Also counts feat. variants of the same main artist – matching the
    /// grouped albums overview.
    pub(crate) fn album_files(&self, artist: &str, album: &str) -> Vec<PathBuf> {
        self.album_tracks(artist, album)
            .into_iter()
            .map(|t| PathBuf::from(t.path))
            .collect()
    }

    /// The tracks behind [`Self::album_files`] (with their tags).
    pub(crate) fn album_tracks(&self, artist: &str, album: &str) -> Vec<Track> {
        let target = crate::core::artist::norm_key(artist);
        // Indexed album query instead of scanning the whole track table; the
        // main-artist refinement stays in Rust (split "feat." credits).
        self.library
            .tracks_by_album_name(album)
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                t.artist
                    .as_deref()
                    .is_some_and(|a| crate::core::artist::primary_credit_matches(a, &target))
            })
            .collect()
    }

    /// All tracks of an artist (possibly split from "feat."), grouped by
    /// album. A track counts toward the artist if their name appears in the
    /// track's split artist credit (case-insensitive) –
    /// matching the artist list, which also splits "feat." credits.
    /// Albums in the order from `all_tracks` (alphabetical), tracks per album
    /// by track number.
    pub(crate) fn artist_albums(&self, name: &str) -> Vec<(String, Vec<Track>)> {
        let target = crate::core::artist::norm_key(name);
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, Vec<Track>> =
            std::collections::HashMap::new();
        for t in self.library.all_tracks().unwrap_or_default() {
            let belongs = t
                .artist
                .as_deref()
                .is_some_and(|a| crate::core::artist::credit_matches(a, &target));
            if !belongs {
                continue;
            }
            let album = t.album.clone().unwrap_or_default();
            if !groups.contains_key(&album) {
                order.push(album.clone());
            }
            groups.entry(album).or_default().push(t);
        }
        order
            .into_iter()
            .map(|album| {
                let tracks = groups.remove(&album).unwrap_or_default();
                (album, tracks)
            })
            .collect()
    }

    /// Splits an artist's tracks into **own albums** and **singles**:
    ///
    /// * If **all** tracks of an album belong to the artist (per the library), it
    ///   is their album → its own album entry `(album, display artist, tracks)`.
    /// * If they appear only on **part** of the album (e.g. as a guest on
    ///   2–3 pieces), those tracks count as singles.
    /// * Tracks with no album at all are also singles.
    ///
    /// Albums in the order from `all_tracks`; tracks per album by track number.
    pub(crate) fn artist_sections(
        &self,
        name: &str,
    ) -> (Vec<(String, String, Vec<Track>)>, Vec<Track>) {
        let target = crate::core::artist::norm_key(name);
        let all = self.library.all_tracks().unwrap_or_default();

        // Group the artist's tracks by album name (preserving order).
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, Vec<Track>> =
            std::collections::HashMap::new();
        for t in all {
            let belongs = t
                .artist
                .as_deref()
                .is_some_and(|a| crate::core::artist::credit_matches(a, &target));
            if !belongs {
                continue;
            }
            let album = t.album.clone().unwrap_or_default();
            if !groups.contains_key(&album) {
                order.push(album.clone());
            }
            groups.entry(album).or_default().push(t);
        }

        let mut albums: Vec<(String, String, Vec<Track>)> = Vec::new();
        let mut singles: Vec<Track> = Vec::new();
        for album in order {
            let mine = groups.remove(&album).unwrap_or_default();
            if album.is_empty() {
                singles.extend(mine);
                continue;
            }
            // Only tracks where this artist is the **main artist**
            // form an album. Pure guest/feature tracks (name mentions) do
            // NOT feed into the album construction – they count as singles.
            let (own_tracks, guest_tracks): (Vec<Track>, Vec<Track>) =
                mine.into_iter().partition(|t| {
                    t.artist
                        .as_deref()
                        .is_some_and(|a| crate::core::artist::primary_credit_matches(a, &target))
                });
            // Album only from two own tracks up; otherwise they count as singles.
            // A song lying several times on disk is listed once.
            let own_tracks = crate::core::dupes::dedup_tracks(own_tracks);
            if own_tracks.len() >= 2 {
                let display_artist = most_common_artist(&own_tracks);
                albums.push((album, display_artist, own_tracks));
            } else {
                singles.extend(own_tracks);
            }
            singles.extend(guest_tracks);
        }
        let singles = crate::core::dupes::dedup_tracks(singles);
        (albums, singles)
    }

    /// Groups a playlist's tracks (given as paths, **order preserved**) into
    /// **albums** – an album name shared by 2+ entries – and standalone
    /// **songs**. Like [`Self::artist_sections`], but driven by an explicit
    /// path list, so entries the DB does not know (e.g. remote files) still
    /// show up as singles (with their display name).
    pub(crate) fn playlist_sections(
        &self,
        paths: &[String],
    ) -> (Vec<(String, String, Vec<Track>)>, Vec<Track>) {
        // Resolve each path to its track (order preserved); unknown paths
        // become a minimal single carrying just the path + a display name.
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, Vec<Track>> =
            std::collections::HashMap::new();
        for p in paths {
            let track = self
                .library
                .track_by_path(p)
                .ok()
                .flatten()
                .unwrap_or(Track {
                    id: 0,
                    path: p.clone(),
                    title: self.display_name(std::path::Path::new(p)),
                    artist: None,
                    album: None,
                    genre: None,
                    track_no: None,
                    disc_no: None,
                    duration_ms: None,
                    resume_ms: 0,
                    year: None,
                });
            let album = track.album.clone().unwrap_or_default();
            if !groups.contains_key(&album) {
                order.push(album.clone());
            }
            groups.entry(album).or_default().push(track);
        }

        let mut albums: Vec<(String, String, Vec<Track>)> = Vec::new();
        let mut singles: Vec<Track> = Vec::new();
        for album in order {
            let mine = groups.remove(&album).unwrap_or_default();
            // Untitled album, or just one track of it in the playlist → song.
            if album.is_empty() || mine.len() < 2 {
                singles.extend(mine);
                continue;
            }
            let display_artist = most_common_artist(&mine);
            albums.push((album, display_artist, mine));
        }
        (albums, singles)
    }

    /// Tracks that belong to "this album by this artist": all library
    /// tracks with the album name in whose (split) artist credit `name`
    /// appears. Sorted by file structure (CD folder → disc → track number);
    /// further copies of a song (see [`crate::core::dupes`]) are dropped.
    pub(crate) fn album_tracks_for_artist(&self, name: &str, album: &str) -> Vec<Track> {
        let target = crate::core::artist::norm_key(name);
        let mut tracks: Vec<Track> = self
            .library
            .tracks_by_album_name(album)
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                // Album membership via the main artist (like the
                // albums overview): "A feat. B" belongs to "A"'s album.
                t.artist
                    .as_deref()
                    .is_some_and(|a| crate::core::artist::primary_credit_matches(a, &target))
            })
            .collect();
        sort_by_structure(&mut tracks);
        crate::core::dupes::dedup_tracks(tracks)
    }

    /// The tracks of an album **card** of the overviews: the same-named tracks
    /// grouped with `artist` — across "feat." credits and the artists of a
    /// soundtrack, but not a foreign artist's album of the same title (see
    /// [`crate::core::album_group`]). Sorted by disc/track number, then path;
    /// further copies of a song are dropped.
    pub(crate) fn album_card_tracks(&self, artist: &str, album: &str) -> Vec<Track> {
        let mut tracks: Vec<Track> = self
            .library
            .album_card_tracks(artist, album)
            .unwrap_or_default();
        sort_by_structure(&mut tracks);
        crate::core::dupes::dedup_tracks(tracks)
    }

    /// Short tap on an artist: opens a subpage that first lists
    /// their **albums** (with cover) and then the **singles** (tracks without
    /// album, with cover). Tapping an album opens its tracks as
    /// a further subpage; tapping a single plays it.
    pub(crate) fn open_artist_tracks(&self, sender: &ComponentSender<Self>, meta: &ArtistMeta) {
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Separate own albums from the rest (guest tracks + tracks without album).
        let (album_groups, singles) = self.artist_sections(&meta.name);
        // Which album/track is running decides every row's play icon — one
        // lookup for the whole page instead of one per row.
        let playing_album = self.playing_album();

        if album_groups.is_empty() && singles.is_empty() {
            content.append(
                &adw::StatusPage::builder()
                    .icon_name("avatar-default-symbolic")
                    .title(gettext("No tracks"))
                    .description(gettext(
                        "There are no songs for this artist in the library.",
                    ))
                    .build(),
            );
        }

        // --- Albums first ---
        if !album_groups.is_empty() {
            let n = album_groups.len();
            let group = adw::PreferencesGroup::builder()
                .title(format!("{} ({n})", gettext("Albums")))
                .build();
            for (album, display_artist, tracks) in &album_groups {
                let album_meta = self
                    .library
                    .get_album_meta(display_artist, album)
                    .ok()
                    .flatten();
                // Tag year (earliest track = original release) first, online fallback.
                let year = tracks
                    .iter()
                    .filter_map(|t| t.year)
                    .min()
                    .or_else(|| album_meta.as_ref().and_then(|m| m.year));
                let cover_path = album_meta.as_ref().and_then(|m| m.cover_path.clone());

                // Total runtime of all album tracks + play button (layout as
                // for the singles). The button plays the whole album — or
                // pauses it while it is the one running; a tap on the row still
                // opens the album subpage.
                let total_ms: i64 = tracks.iter().filter_map(|t| t.duration_ms).sum();
                let key = crate::core::category::album_key(&meta.name, album);
                let row = EntryRow::new(album)
                    .subtitle(&album_subtitle(year, tracks.len()))
                    .cover(cover_path.as_deref(), "media-optical-symbolic")
                    .duration(total_ms)
                    .play_button(
                        &gettext("Play album"),
                        self.entry_is_active("album", &key, playing_album.as_deref()),
                        self.mini.playing,
                        {
                            let sender = sender.clone();
                            let (name, album) = (meta.name.clone(), album.to_string());
                            move || {
                                sender.input(Msg::PlayAlbum {
                                    artist: name.clone(),
                                    album: album.clone(),
                                })
                            }
                        },
                    )
                    .marked_in(
                        &self.libview.page_marks,
                        crate::ui::app_favorites::mark_key("album", &key),
                    )
                    // Short tap: album subpage (songs of the album).
                    .on_activate({
                        let sender = sender.clone();
                        let (name, album) = (meta.name.clone(), album.to_string());
                        move || {
                            sender.input(Msg::OpenAlbumTracks {
                                artist: name.clone(),
                                album: album.clone(),
                            })
                        }
                    })
                    // Long press (touch) / right click (mouse): album detail view.
                    .on_detail({
                        let sender = sender.clone();
                        let (display_artist, album) = (display_artist.clone(), album.to_string());
                        move || {
                            sender.input(Msg::ShowAlbumDetailFor {
                                artist: display_artist.clone(),
                                album: album.clone(),
                            })
                        }
                    })
                    .build();
                group.add(&row);
            }
            content.append(&group);
        }

        // --- then the singles (guest tracks + tracks without album) ---
        if !singles.is_empty() {
            let n = singles.len();
            let group = adw::PreferencesGroup::builder()
                .title(format!("{} ({n})", gettext("Singles")))
                .build();
            for t in &singles {
                // Cover order (never a foreign folder image):
                // 1) embedded image of the track itself,
                // 2) cover of the actual album (also for guest tracks),
                // 3) photo of the main artist.
                let cover_path = crate::core::online::local_track_cover(&t.path)
                    .or_else(|| {
                        let album = t.album.as_deref().filter(|a| !a.trim().is_empty())?;
                        let artist = t.artist.as_deref().unwrap_or("");
                        // First exact (artist, album), otherwise the same-named
                        // album of the same primary artist ("feat." variants).
                        self.library
                            .get_album_meta(artist, album)
                            .ok()
                            .flatten()
                            .and_then(|m| m.cover_path)
                            .or_else(|| {
                                self.library
                                    .album_cover_related(artist, album)
                                    .ok()
                                    .flatten()
                            })
                    })
                    .or_else(|| {
                        let artist = t.artist.as_deref().filter(|a| !a.trim().is_empty())?;
                        let primary = crate::core::artist::split_artists(artist)
                            .into_iter()
                            .next()?;
                        self.library
                            .get_artist_meta(&primary)
                            .ok()
                            .flatten()
                            .and_then(|m| m.image_path)
                    });
                // Not activatable: the track plays via its play button; the
                // detail view opens on long press / right click.
                let path = t.path.clone();
                let row = EntryRow::new(&t.title)
                    // Album as secondary info under the song name (if present).
                    .subtitle(t.album.as_deref().unwrap_or_default())
                    .cover(cover_path.as_deref(), "audio-x-generic-symbolic")
                    .duration(t.duration_ms.unwrap_or(0))
                    // Play button: plays this track but keeps the list open.
                    .play_button(
                        &gettext("Play"),
                        self.entry_is_active("track", &path, None),
                        self.mini.playing,
                        {
                            let sender = sender.clone();
                            let (name, path) = (meta.name.clone(), path.clone());
                            move || {
                                sender.input(Msg::PlayArtistTrack {
                                    name: name.clone(),
                                    path: path.clone(),
                                    close: false,
                                })
                            }
                        },
                    )
                    .marked_in(
                        &self.libview.page_marks,
                        crate::ui::app_favorites::mark_key("track", &path),
                    )
                    // Long press (touch) / right click (mouse): song detail view.
                    .on_detail({
                        let sender = sender.clone();
                        move || sender.input(Msg::ShowTrackDetail(path.clone()))
                    })
                    .build();
                group.add(&row);
            }
            content.append(&group);
        }

        self.push_subpage(&meta.name, &content);
    }

    /// Tapping an album in the artist subpage: lists its tracks
    /// (with album cover) as a further subpage. Tapping a track
    /// plays the entire album from that track on.
    pub(crate) fn open_album_tracks(
        &self,
        sender: &ComponentSender<Self>,
        name: &str,
        album: &str,
    ) {
        // Tracks of the album – `all_tracks` already returns them sorted by track number.
        let tracks = self.album_tracks_for_artist(name, album);
        self.render_album_tracks(sender, tracks, name, album, AlbumPlay::Artist);
    }

    /// Album card from the overviews (or search / player bar): the tracks of
    /// that card (see [`Self::album_card_tracks`]). Tapping a track plays the
    /// whole album from here.
    pub(crate) fn open_album_card(
        &self,
        sender: &ComponentSender<Self>,
        artist: &str,
        album: &str,
    ) {
        let tracks = self.album_card_tracks(artist, album);
        self.render_album_tracks(sender, tracks, artist, album, AlbumPlay::Name);
    }

    /// Tracks of a folder in playback order (CD/disc, track number, path).
    /// Basis for the track list of a folder presented as an album.
    pub(crate) fn folder_tracks_ordered(&self, folder: &str) -> Vec<Track> {
        let mut tracks: Vec<Track> = self.library.tracks_under_path(folder).unwrap_or_default();
        // **File structure as truth:** sort folder contents (audiobooks/concerts) by
        // **natural path** – the filenames/CD folders dictate the
        // correct order, even when disc/track tags are missing or wrong.
        // (Album entries still use the track number.)
        tracks.sort_by_cached_key(|t| natural_key(&t.path));
        tracks
    }

    /// Tapping a **folder** audiobook/concert presented as an album: lists
    /// its tracks. Tapping a track plays the folder from there.
    pub(crate) fn open_folder_tracks(&self, sender: &ComponentSender<Self>, folder: &str) {
        let tracks = self.folder_tracks_ordered(folder);
        let refs: Vec<&Track> = tracks.iter().collect();
        let album = most_common_album_base(&refs).unwrap_or_else(|| {
            std::path::Path::new(folder)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        let name = most_common_artist(&tracks);
        self.render_album_tracks(
            sender,
            tracks,
            &name,
            &album,
            AlbumPlay::Folder(folder.to_string()),
        );
    }

    /// Shared rendering of an album track list. `play` determines how a
    /// tapped track is played (artist-related or by album name).
    fn render_album_tracks(
        &self,
        sender: &ComponentSender<Self>,
        tracks: Vec<Track>,
        name: &str,
        album: &str,
        play: AlbumPlay,
    ) {
        let display_artist = most_common_artist(&tracks);

        // The content box is kept (in `libview.album_page`) so a late tracklist
        // fetch or a freshly downloaded track can refill it in place.
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        self.fill_album_content(sender, &content, &tracks, album, &play);

        *self.libview.album_page.borrow_mut() = Some(AlbumPageRef {
            name: name.to_string(),
            artist: display_artist.clone(),
            album: album.to_string(),
            play: play.clone(),
            content: content.clone(),
        });

        // For real albums (not folder audiobooks) fetch the canonical tracklist
        // once in the background, so locally-missing tracks can be flagged — only
        // when YouTube is enabled (otherwise the feature is hidden, so there is
        // nothing to fetch for). The result refills the page above via
        // `Cmd::AlbumTracklistFetched`.
        if self.youtube.enabled
            && !matches!(play, AlbumPlay::Folder(_))
            && !display_artist.is_empty()
            && !self.library.tracklist_fetched(&display_artist, album)
        {
            let artist = display_artist.clone();
            let alb = album.to_string();
            let mbid_hint = self
                .library
                .get_album_meta(&display_artist, album)
                .ok()
                .flatten()
                .and_then(|m| m.mbid);
            sender.spawn_command(move |out| {
                fetch_and_store_tracklist(&artist, &alb, mbid_hint.as_deref());
                let _ = out.send(Cmd::AlbumTracklistFetched { artist, album: alb });
            });
        }

        // Header line: preferably the album artist, otherwise the page artist.
        let header_artist = if display_artist.is_empty() {
            name
        } else {
            display_artist.as_str()
        };
        let title = if header_artist.is_empty() {
            album.to_string()
        } else {
            format!("{header_artist} – {album}")
        };
        self.push_subpage(&title, &content);
    }

    /// Re-fill the currently shown album page (same content box, no navigation)
    /// when it matches `(artist, album)` — used after the canonical tracklist
    /// arrives or a missing track was added.
    pub(crate) fn refill_album_page(
        &self,
        sender: &ComponentSender<Self>,
        artist: &str,
        album: &str,
    ) {
        let page = self.libview.album_page.borrow().clone();
        let Some(page) = page else { return };
        if page.artist != artist || page.album != album {
            return;
        }
        let tracks = match &page.play {
            AlbumPlay::Name => self.album_card_tracks(&page.name, album),
            AlbumPlay::Artist => self.album_tracks_for_artist(&page.name, album),
            AlbumPlay::Folder(f) => self.folder_tracks_ordered(f),
        };
        self.fill_album_content(sender, &page.content, &tracks, album, &page.play);
    }

    /// (Re)builds the rows of an album track-list `content` box: present tracks,
    /// plus greyed "missing" placeholders for tracks the canonical (MusicBrainz)
    /// tracklist has but the library lacks. Clears the box first so it can be
    /// called again to refresh in place.
    fn fill_album_content(
        &self,
        sender: &ComponentSender<Self>,
        content: &gtk::Box,
        tracks: &[Track],
        album: &str,
        play: &AlbumPlay,
    ) {
        use std::collections::HashSet;
        while let Some(child) = content.first_child() {
            content.remove(&child);
        }

        let display_artist = most_common_artist(tracks);
        let album_meta = self
            .library
            .get_album_meta(&display_artist, album)
            .ok()
            .flatten();
        let cover_path = album_meta
            .as_ref()
            .and_then(|m| m.cover_path.clone())
            .or_else(|| self.album_cover_for(&display_artist, album));
        // One cover for the whole page, in the header: decoded at twice its
        // size so it stays sharp at a scale factor of 2.
        let cover = cover_path
            .as_deref()
            .and_then(|p| crate::ui::widgets::decode_scaled(p, 2 * ALBUM_HEADER_COVER));

        // Missing-track detection: only for real albums whose present tracks are
        // all numbered (so canonical positions can be matched reliably). Gated on
        // YouTube being enabled — adding a missing track needs it, so without it
        // the greyed entries are hidden entirely (not just non-functional).
        let sections = album_sections(tracks);
        // Several release folders (e.g. a UK and a US single of the same title):
        // one section per release; canonical positions can't be matched then.
        let multi_release = sections.iter().any(|s| s.release != sections[0].release);
        let is_album = !matches!(play, AlbumPlay::Folder(_)) && self.youtube.enabled;
        let can_detect = is_album && !multi_release && tracks.iter().all(|t| t.track_no.is_some());
        let cached = if is_album {
            self.library
                .album_tracklist(&display_artist, album)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let present_pos: HashSet<(u32, u32)> = tracks
            .iter()
            .filter_map(|t| t.track_no.map(|n| (track_disc(t), n)))
            .collect();
        // (disc, position, title) for each canonical track with no local file.
        let missing: Vec<(u32, u32, String)> = if can_detect {
            cached
                .iter()
                .filter(|(d, p, _, _)| !present_pos.contains(&(*d, *p)))
                .map(|(d, p, title, _)| (*d, *p, title.clone()))
                .collect()
        } else {
            Vec::new()
        };
        // Still waiting for the first fetch → show a discreet hint at the bottom.
        let pending = is_album
            && cached.is_empty()
            && !display_artist.is_empty()
            && !self.library.tracklist_fetched(&display_artist, album);

        // Header: cover, title, facts and the buttons that play the whole page.
        let year = tracks
            .iter()
            .filter_map(|t| t.year)
            .min()
            .or_else(|| album_meta.as_ref().and_then(|m| m.year));
        let mark = match play {
            AlbumPlay::Folder(f) => crate::ui::app_favorites::mark_key("folder", f),
            _ => crate::ui::app_favorites::mark_key("album", album),
        };
        content.append(
            &self.album_header(
                sender,
                AlbumHeader {
                    cover: cover.as_ref(),
                    album,
                    artist: &display_artist,
                    year,
                    tracks: sections
                        .iter()
                        .flat_map(|s| s.tracks.iter().copied())
                        .collect(),
                    mark,
                },
            ),
        );

        // Discs: union of present tracks and any (whole-disc) missing entries.
        let mut discs: Vec<u32> = tracks.iter().map(track_disc).collect();
        discs.extend(missing.iter().map(|(d, _, _)| *d));
        discs.sort_unstable();
        discs.dedup();
        let multi_disc = discs.len() > 1;

        // Builds a present-track row (cover, track number, duration, play).
        let make_row = |t: &Track| -> adw::ActionRow {
            let path = t.path.clone();
            let build_msg = {
                let play = play.clone();
                move |path: String, close: bool| match &play {
                    AlbumPlay::Artist | AlbumPlay::Name => Msg::PlayOneTrack { path, close },
                    AlbumPlay::Folder(f) => Msg::PlayFolderTrack {
                        folder: f.clone(),
                        path,
                        close,
                    },
                }
            };
            // No cover per row: the header shows it once for the whole page.
            let mut row = EntryRow::new(&t.title)
                .duration(t.duration_ms.unwrap_or(0))
                .offline(self.is_offline_path(&t.path));
            if let Some(no) = t.track_no {
                row = row.number(no);
            }
            row.play_button(
                &gettext("Play"),
                self.entry_is_active("track", &path, None),
                self.mini.playing,
                {
                    let sender = sender.clone();
                    let (build_msg, path) = (build_msg.clone(), path.clone());
                    move || sender.input(build_msg(path.clone(), false))
                },
            )
            .marked_in(
                &self.libview.page_marks,
                crate::ui::app_favorites::mark_key("track", &path),
            )
            .on_detail({
                let sender = sender.clone();
                move || sender.input(Msg::ShowTrackDetail(path.clone()))
            })
            .build()
        };

        // Builds a greyed "missing" row: tapping it offers to fetch & add it.
        let make_missing_row = |disc: u32, pos: u32, title: &str| -> adw::ActionRow {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(title))
                .subtitle(gettext("Missing — tap to add"))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            // Greyed out so it reads as "not here yet" but still a real entry.
            row.set_opacity(0.55);
            row.add_prefix(
                &gtk::Label::builder()
                    .label(pos.to_string())
                    .width_chars(2)
                    .xalign(1.0)
                    .css_classes(["dim-label", "numeric"])
                    .build(),
            );
            let add_icon = gtk::Image::from_icon_name("list-add-symbolic");
            add_icon.set_valign(gtk::Align::Center);
            row.add_suffix(&add_icon);
            let (artist, album, title) =
                (display_artist.clone(), album.to_string(), title.to_string());
            let sender = sender.clone();
            row.connect_activated(move |_| {
                sender.input(Msg::ShowMissingTrack {
                    artist: artist.clone(),
                    album: album.clone(),
                    disc,
                    position: pos,
                    title: title.clone(),
                });
            });
            row
        };

        // One entry of a (merged) disc, ordered by position.
        enum Item<'a> {
            Present(&'a Track),
            Missing { pos: u32, title: String },
        }

        let render_disc = |group: &adw::PreferencesGroup, disc: u32| {
            let mut items: Vec<(u32, Item)> = Vec::new();
            for t in tracks.iter().filter(|t| track_disc(t) == disc) {
                items.push((t.track_no.unwrap_or(0), Item::Present(t)));
            }
            for (_, pos, title) in missing.iter().filter(|(d, _, _)| *d == disc) {
                items.push((
                    *pos,
                    Item::Missing {
                        pos: *pos,
                        title: title.clone(),
                    },
                ));
            }
            items.sort_by_key(|(p, _)| *p);
            for (_, item) in items {
                match item {
                    Item::Present(t) => group.add(&make_row(t)),
                    Item::Missing { pos, title } => group.add(&make_missing_row(disc, pos, &title)),
                }
            }
        };

        if multi_release {
            for section in &sections {
                // The disc only needs naming when this release has several.
                let release_discs = sections
                    .iter()
                    .filter(|s| s.release == section.release)
                    .count();
                let folder = section
                    .release
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let title = if release_discs > 1 {
                    format!("{folder} · CD {} ({})", section.disc, section.tracks.len())
                } else {
                    format!("{folder} ({})", section.tracks.len())
                };
                let group = adw::PreferencesGroup::builder()
                    .title(gtk::glib::markup_escape_text(&title).as_str())
                    .build();
                for t in &section.tracks {
                    group.add(&make_row(t));
                }
                content.append(&group);
            }
        } else if multi_disc {
            for disc in &discs {
                let present = tracks.iter().filter(|t| track_disc(t) == *disc).count();
                let group = adw::PreferencesGroup::builder()
                    .title(format!("CD {disc} ({present})"))
                    .build();
                render_disc(&group, *disc);
                content.append(&group);
            }
        } else {
            // Title and count are in the header already.
            let group = adw::PreferencesGroup::new();
            let disc = discs.first().copied().unwrap_or(1);
            render_disc(&group, disc);
            content.append(&group);
        }

        if pending {
            let lbl = gtk::Label::builder()
                .label(gettext("Checking for missing tracks …"))
                .xalign(0.0)
                .margin_start(4)
                .build();
            lbl.add_css_class("dim-label");
            content.append(&lbl);
        }
    }

    /// The head of an album page: cover, title, "artist · year · N songs ·
    /// length", and the buttons that play the whole page — in the order shown,
    /// or shuffled. Next to each other on the desktop, stacked on the phone.
    fn album_header(&self, sender: &ComponentSender<Self>, h: AlbumHeader) -> gtk::Widget {
        let narrow = self.nav.narrow.get();
        let header = gtk::Box::builder()
            .orientation(if narrow {
                gtk::Orientation::Vertical
            } else {
                gtk::Orientation::Horizontal
            })
            .spacing(18)
            .margin_bottom(6)
            .build();
        // The clamp holds the cover at its size: the frame would otherwise
        // follow the (twice as large) texture's natural width.
        let cover = adw::Clamp::builder()
            .maximum_size(ALBUM_HEADER_COVER)
            .tightening_threshold(ALBUM_HEADER_COVER)
            .child(&crate::ui::widgets::rounded_image(
                h.cover,
                "media-optical-symbolic",
                ALBUM_HEADER_COVER,
            ))
            .halign(if narrow {
                gtk::Align::Center
            } else {
                gtk::Align::Start
            })
            .valign(gtk::Align::Start)
            .build();
        header.append(&cover);

        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        let xalign = if narrow { 0.5 } else { 0.0 };
        let title = gtk::Label::builder()
            .label(h.album)
            .wrap(true)
            .xalign(xalign)
            .justify(if narrow {
                gtk::Justification::Center
            } else {
                gtk::Justification::Left
            })
            .css_classes(["title-2"])
            .build();
        text.append(&title);
        let total_ms: i64 = h.tracks.iter().filter_map(|t| t.duration_ms).sum();
        let mut facts: Vec<String> = Vec::new();
        if !h.artist.is_empty() {
            facts.push(h.artist.to_string());
        }
        facts.push(album_subtitle(h.year, h.tracks.len()));
        if total_ms > 0 {
            facts.push(crate::ui::app_helpers::fmt_duration(total_ms));
        }
        let facts = gtk::Label::builder()
            .label(facts.join(" · "))
            .wrap(true)
            .xalign(xalign)
            .css_classes(["dim-label"])
            .build();
        text.append(&facts);

        let paths: Vec<String> = h.tracks.iter().map(|t| t.path.clone()).collect();
        let buttons = gtk::Box::builder()
            .spacing(12)
            .margin_top(6)
            .halign(if narrow {
                gtk::Align::Center
            } else {
                gtk::Align::Start
            })
            .build();
        // "Play": its icon is a play mark like the rows' (registered under the
        // page's own key), so it shows a pause while this page is playing.
        let active = self.entry_is_active_key(&h.mark);
        let play_icon = crate::ui::play_mark::marker(active, self.mini.playing);
        self.libview.page_marks.add(h.mark.clone(), &play_icon);
        let play_box = gtk::Box::builder().spacing(6).build();
        play_box.append(&play_icon);
        play_box.append(&gtk::Label::new(Some(&gettext("Play"))));
        let play_btn = gtk::Button::builder()
            .child(&play_box)
            .css_classes(["pill", "suggested-action", "emilia-album-play"])
            .build();
        {
            let (sender, paths) = (sender.clone(), paths.clone());
            play_btn.connect_clicked(move |_| {
                sender.input(Msg::PlayTracks {
                    paths: paths.clone(),
                    shuffle: false,
                })
            });
        }
        let shuffle_btn = gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("media-playlist-shuffle-symbolic")
                    .label(gettext("Shuffle"))
                    .build(),
            )
            .css_classes(["pill"])
            .build();
        {
            let sender = sender.clone();
            shuffle_btn.connect_clicked(move |_| {
                sender.input(Msg::PlayTracks {
                    paths: paths.clone(),
                    shuffle: true,
                })
            });
        }
        buttons.append(&play_btn);
        buttons.append(&shuffle_btn);
        text.append(&buttons);
        header.append(&text);
        header.upcast()
    }

    /// [`Self::entry_is_active`] for a [`crate::ui::app_favorites::mark_key`].
    fn entry_is_active_key(&self, mark: &str) -> bool {
        let (scope, key) = mark.split_once('\u{1}').unwrap_or(("", mark));
        self.entry_is_active(scope, key, self.playing_album().as_deref())
    }

    /// Albums of an artist with (where known) release year from the
    /// album metadata. Tracks per album already by track number (see
    /// [`Self::artist_albums`]).
    pub(crate) fn artist_albums_dated(&self, name: &str) -> Vec<(Option<i32>, String, Vec<Track>)> {
        self.artist_albums(name)
            .into_iter()
            .map(|(album, tracks)| {
                let artist = tracks
                    .first()
                    .and_then(|t| t.artist.clone())
                    .unwrap_or_default();
                // Prefer the embedded tag year (earliest track = original release)
                // over the online match, which can be a reissue/remaster year.
                let year = tracks.iter().filter_map(|t| t.year).min().or_else(|| {
                    self.library
                        .get_album_meta(&artist, &album)
                        .ok()
                        .flatten()
                        .and_then(|m| m.year)
                });
                (year, album, tracks)
            })
            .collect()
    }

    /// All tracks of an artist in playback order by release (see
    /// [`release_order`]).
    pub(crate) fn artist_files_ordered(&self, name: &str, newest_first: bool) -> Vec<PathBuf> {
        self.artist_tracks_ordered(name, newest_first)
            .into_iter()
            .map(|t| PathBuf::from(t.path))
            .collect()
    }

    /// The tracks behind [`Self::artist_files_ordered`] (with their tags).
    pub(crate) fn artist_tracks_ordered(&self, name: &str, newest_first: bool) -> Vec<Track> {
        release_order(self.artist_albums_dated(name), newest_first)
    }

    /// Year info of an artist's albums as `(label, value)`: with at least
    /// two **different** years "Years" + "from – to", with exactly one
    /// known year "Year" + single year. `None` if no year is known.
    pub(crate) fn artist_year_range(&self, name: &str) -> Option<(&'static str, String)> {
        let mut years: Vec<i32> = self
            .artist_albums_dated(name)
            .into_iter()
            .filter_map(|(year, _, _)| year)
            .collect();
        years.sort_unstable();
        years.dedup();
        match years.as_slice() {
            [] => None,
            [y] => Some((gettext_noop("Year"), y.to_string())),
            _ => Some((
                gettext_noop("Years"),
                format!("{} – {}", years[0], years[years.len() - 1]),
            )),
        }
    }
}

/// Orders an artist's tracks by the release of **each song**: its own tag
/// year, else the year of its album (`albums` as from
/// [`App::artist_albums_dated`]); songs without any year go to the end in both
/// directions. Songs of the same year stay grouped by album (name) and keep
/// their album order, so an album without deviating years still plays in one
/// piece, while loose singles slot in between the albums by their own year.
fn release_order(albums: Vec<(Option<i32>, String, Vec<Track>)>, newest_first: bool) -> Vec<Track> {
    let mut songs: Vec<(Option<i32>, String, usize, Track)> = albums
        .into_iter()
        .flat_map(|(album_year, album, tracks)| {
            tracks
                .into_iter()
                .enumerate()
                .map(move |(i, t)| (t.year.or(album_year), album.clone(), i, t))
        })
        .collect();
    songs.sort_by(|a, b| {
        use std::cmp::Ordering;
        let by_year = match (a.0, b.0) {
            (Some(x), Some(y)) if newest_first => y.cmp(&x),
            (Some(x), Some(y)) => x.cmp(&y),
            // Known year before unknown (in both directions).
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        by_year.then_with(|| a.1.cmp(&b.1)).then(a.2.cmp(&b.2))
    });
    songs.into_iter().map(|(_, _, _, t)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disc_from_segment_finds_marker_anywhere_with_boundary() {
        assert_eq!(disc_from_segment("CD1"), Some(1));
        assert_eq!(disc_from_segment("cd2"), Some(2));
        assert_eq!(disc_from_segment("Disc 3"), Some(3));
        // Marker in the middle (common for audiobooks):
        assert_eq!(disc_from_segment("Wie Google tickt CD1"), Some(1));
        assert_eq!(disc_from_segment("Teil 4 – Finale"), Some(4));
        // No match in the middle of a word or without a digit:
        assert_eq!(disc_from_segment("Discography"), None);
        assert_eq!(disc_from_segment("Soundtrack"), None);
        assert_eq!(disc_from_segment("Lockdown"), None);
        assert_eq!(disc_from_segment("Digitale Erschoepfung"), None);
    }

    fn track(path: &str, disc: Option<u32>, no: Option<u32>) -> Track {
        Track {
            id: 0,
            path: path.to_string(),
            title: String::new(),
            artist: None,
            album: None,
            genre: None,
            track_no: no,
            disc_no: disc,
            duration_ms: None,
            resume_ms: 0,
            year: None,
        }
    }

    /// (release folder name, disc, track paths) of each section.
    fn sections_of(tracks: &[Track]) -> Vec<(String, u32, Vec<&str>)> {
        album_sections(tracks)
            .into_iter()
            .map(|s| {
                let name = s
                    .release
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                (
                    name,
                    s.disc,
                    s.tracks.iter().map(|t| t.path.as_str()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn two_releases_of_one_album_stay_apart_instead_of_interleaving() {
        // Structure order: the UK folder, then the US folder.
        let tracks = [
            track("/m/19-2000 (UK)/01.mp3", Some(1), Some(1)),
            track("/m/19-2000 (UK)/02.mp3", Some(1), Some(2)),
            track("/m/19-2000 (US)/01.mp3", Some(1), Some(1)),
            track("/m/19-2000 (US)/02.mp3", Some(1), Some(2)),
        ];
        assert_eq!(
            sections_of(&tracks),
            [
                (
                    "19-2000 (UK)".to_string(),
                    1,
                    vec!["/m/19-2000 (UK)/01.mp3", "/m/19-2000 (UK)/02.mp3"]
                ),
                (
                    "19-2000 (US)".to_string(),
                    1,
                    vec!["/m/19-2000 (US)/01.mp3", "/m/19-2000 (US)/02.mp3"]
                ),
            ]
        );
    }

    #[test]
    fn cd_subfolders_are_discs_of_one_release() {
        let tracks = [
            track("/m/Album/CD1/01.mp3", None, Some(1)),
            track("/m/Album/CD2/01.mp3", None, Some(1)),
        ];
        let s = sections_of(&tracks);
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].0.as_str(), s[0].1), ("Album", 1));
        assert_eq!((s[1].0.as_str(), s[1].1), ("Album", 2));
    }

    #[test]
    fn disc_tags_split_one_folder_and_track_numbers_order_it() {
        let tracks = [
            track("/m/Album/b.mp3", Some(2), Some(1)),
            track("/m/Album/c.mp3", Some(1), Some(2)),
            track("/m/Album/a.mp3", Some(1), Some(1)),
        ];
        assert_eq!(
            sections_of(&tracks),
            [
                (
                    "Album".to_string(),
                    1,
                    vec!["/m/Album/a.mp3", "/m/Album/c.mp3"]
                ),
                ("Album".to_string(), 2, vec!["/m/Album/b.mp3"]),
            ]
        );
    }

    #[test]
    fn release_order_sorts_each_song_by_its_own_year() {
        let song = |path: &str, year: Option<i32>| Track {
            year,
            ..track(path, None, None)
        };
        let albums = vec![
            // Album 1990 with a bonus track tagged 2005.
            (
                Some(1990),
                "Debut".to_string(),
                vec![
                    song("d1", Some(1990)),
                    song("d2", None),
                    song("d3", Some(2005)),
                ],
            ),
            (
                Some(2000),
                "Second".to_string(),
                vec![song("s1", Some(2000)), song("s2", Some(2000))],
            ),
            // Loose singles (no album) from different years, plus one without any year.
            (
                Some(1995),
                String::new(),
                vec![song("x1995", Some(1995)), song("x2010", Some(2010))],
            ),
            (None, "Unknown".to_string(), vec![song("u1", None)]),
        ];
        let paths = |newest| -> Vec<String> {
            release_order(albums.clone(), newest)
                .into_iter()
                .map(|t| t.path)
                .collect()
        };
        assert_eq!(
            paths(false),
            ["d1", "d2", "x1995", "s1", "s2", "d3", "x2010", "u1"]
        );
        assert_eq!(
            paths(true),
            ["x2010", "d3", "s1", "s2", "x1995", "d1", "d2", "u1"]
        );
    }

    #[test]
    fn natural_key_orders_numbers_numerically() {
        let lt = |a: &str, b: &str| natural_key(a) < natural_key(b);
        assert!(lt("OKR 3.2", "OKR 3.10")); // not zero-padded → numeric
        assert!(lt("CD2", "CD10"));
        assert!(lt("01 01", "01 02"));
        assert!(lt("01 09", "02 01")); // "Disc" in the filename
        assert!(lt("Buch CD1/01", "Buch CD2/01"));
    }

    #[test]
    fn sort_by_structure_keeps_cd_folders_in_order() {
        // Multi-CD audiobook without disc tags, CD marker in the folder name.
        let mut ts = vec![
            track("/Buch/Buch CD2/01.mp3", None, Some(1)),
            track("/Buch/Buch CD1/02.mp3", None, Some(2)),
            track("/Buch/Buch CD1/01.mp3", None, Some(1)),
            track("/Buch/Buch CD2/02.mp3", None, Some(2)),
        ];
        sort_by_structure(&mut ts);
        let order: Vec<&str> = ts.iter().map(|t| t.path.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "/Buch/Buch CD1/01.mp3",
                "/Buch/Buch CD1/02.mp3",
                "/Buch/Buch CD2/01.mp3",
                "/Buch/Buch CD2/02.mp3",
            ]
        );
    }
}
