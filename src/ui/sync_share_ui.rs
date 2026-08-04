//! Widgets for the share flow: the **size confirmation** (sender) and the
//! **receiver review** (collision/dedup markers + selective accept). Kept out of
//! [`super::sync_page`] to keep that component readable. The builders return the
//! page widget plus a handle struct; the [`SyncPage`](super::sync_page::SyncPage)
//! reads the handles on confirm.
//!
//! There is no in-dialog "what to share" picker: a share is always started from
//! an item's detail view (long-press → Share), which hands the SyncPage a ready
//! [`Selection`](crate::core::sync::share::Selection) straight to the
//! confirmation below.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use relm4::{adw, gtk, ComponentSender};

use crate::core::sync::share::{
    group_files, human_size, ArtistGroup, FileReview, FileStatus, ManifestFile, ShareDecision,
    ShareManifest,
};
use crate::core::sync::MEMO_PREFIX;
use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::ui::app_helpers::artist_count_subtitle;
use crate::ui::sync_page::{SyncInput, SyncPage};

/// Cap on artist rows in a summary; everything beyond is folded into one line, so
/// even a whole-library offer stays a page instead of a scroll marathon.
const MAX_ARTIST_ROWS: usize = 12;

// ---------------------------------------------------------------------------
// Size confirmation (sender)
// ---------------------------------------------------------------------------

/// Builds the "transfer summary" confirmation shown after the manifest is built.
///
/// Lists every kind of content the offer carries — not just audio files — so a
/// library-only share (podcasts, playlists, stations, …) reads as what it is
/// instead of the misleading "0 files · 0 B".
pub(crate) fn build_confirm(
    manifest: &ShareManifest,
    sender: &ComponentSender<SyncPage>,
) -> gtk::Widget {
    let page = page_box();
    let g = adw::PreferencesGroup::builder()
        .title(gettext("Transfer summary"))
        .description(gettext(
            "Send to offer this to the other device — it then reviews the list \
             and chooses what to keep.",
        ))
        .build();

    // Audio files are the only rows that carry bytes; the rest is metadata or
    // subscriptions the receiver re-fetches itself.
    let file_count = manifest.files.len();
    add_music_rows(&g, &manifest.files);

    let lib = &manifest.library;
    let pod = lib.podcasts.as_ref().map_or(0, Vec::len);
    let pl = lib.playlists.as_ref().map_or(0, Vec::len);
    let fav = lib.favorites.as_ref().map_or(0, Vec::len);
    let cat = lib.categories.as_ref().map_or(0, Vec::len);
    let eq = lib.eq.as_ref().map_or(0, Vec::len);
    let yt = manifest.yt.len();
    let st = manifest.stations.len();
    let rec = manifest.recordings.len();
    let memo = manifest.memos.len();

    // Each row names what it carries (up to three, then "+n more") — "3 playlists"
    // alone doesn't tell the sender *which* ones are about to leave the device.
    count_row(
        &g,
        yt,
        gettext_f("{n} YouTube items", &[("n", &yt.to_string())]),
        &names_of(&manifest.yt, |i| i.title.clone()),
    );
    count_row(
        &g,
        pod,
        gettext_f("{n} podcasts", &[("n", &pod.to_string())]),
        &names_of(lib.podcasts.as_deref().unwrap_or_default(), |p| {
            p.title.clone()
        }),
    );
    count_row(
        &g,
        pl,
        gettext_f("{n} playlists", &[("n", &pl.to_string())]),
        &names_of(lib.playlists.as_deref().unwrap_or_default(), |p| {
            p.name.clone()
        }),
    );
    count_row(
        &g,
        fav,
        gettext_f("{n} favorites", &[("n", &fav.to_string())]),
        &names_of(lib.favorites.as_deref().unwrap_or_default(), |f| {
            f.title.clone()
        }),
    );
    count_row(
        &g,
        st,
        gettext_f("{n} radio stations", &[("n", &st.to_string())]),
        &names_of(&manifest.stations, |s| s.name.clone()),
    );
    count_row(
        &g,
        rec,
        gettext_f("{n} recordings", &[("n", &rec.to_string())]),
        &names_of(&manifest.recordings, |r| r.title.clone()),
    );
    count_row(
        &g,
        memo,
        gettext_f("{n} voice memos", &[("n", &memo.to_string())]),
        &names_of(&manifest.memos, |m| m.title.clone()),
    );
    count_row(
        &g,
        cat,
        gettext_f("{n} categories", &[("n", &cat.to_string())]),
        // Category rows are (scope, key, value) assignments, not named items.
        &[],
    );
    if eq > 0 {
        g.add(
            &adw::ActionRow::builder()
                .title(gettext("Equalizer settings"))
                .build(),
        );
    }

    if manifest.total_size > 0 {
        g.add(
            &adw::ActionRow::builder()
                .title(gettext("Total size"))
                .subtitle(human_size(manifest.total_size))
                .build(),
        );
    }

    // Guard against an offer that resolved to nothing: spell it out instead of
    // showing an empty group, and don't let the user "Send" emptiness.
    let visible = file_count + yt + pod + pl + fav + st + rec + memo + cat + usize::from(eq > 0);
    if visible == 0 {
        g.add(
            &adw::ActionRow::builder()
                .title(gettext("Nothing to share"))
                .subtitle(gettext("The selection did not resolve to any content."))
                .build(),
        );
    }
    page.append(&g);

    // Actions pinned below the scrolling list, so they stay reachable.
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_halign(gtk::Align::Center);
    let cancel = gtk::Button::with_label(&gettext("Cancel"));
    let send = gtk::Button::builder()
        .label(gettext("Send"))
        .css_classes(["suggested-action"])
        .sensitive(visible > 0)
        .build();
    {
        let sender = sender.clone();
        cancel.connect_clicked(move |_| sender.input(SyncInput::CancelShare));
    }
    {
        let sender = sender.clone();
        send.connect_clicked(move |_| sender.input(SyncInput::ConfirmSend));
    }
    row.append(&cancel);
    row.append(&send);

    action_shell(&page, &row)
}

/// Display names of a record list, for the summary row below it.
fn names_of<T>(items: &[T], f: impl Fn(&T) -> String) -> Vec<String> {
    items.iter().map(f).collect()
}

/// Adds a `"{n} …"` summary row to `g` when `n > 0` (skipped otherwise), naming
/// up to three of the items below it.
fn count_row(g: &adw::PreferencesGroup, n: usize, label: String, names: &[String]) {
    if n == 0 {
        return;
    }
    let row = adw::ActionRow::builder()
        .title(label)
        .use_markup(false)
        .build();
    let shown: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
        .take(3)
        .collect();
    if !shown.is_empty() {
        let mut sub = shown.join(", ");
        let rest = names.len().saturating_sub(shown.len());
        if rest > 0 {
            sub = format!(
                "{sub}, {}",
                gettext_f("+{n} more", &[("n", &rest.to_string())])
            );
        }
        row.set_subtitle(&sub);
    }
    g.add(&row);
}

/// Sender-side summary of the audio payload: one row per artist — an album share
/// becomes a single album row, an artist share a single expandable artist row
/// listing its albums. Individual file names only ever appear for files that
/// carry no album tag at all, so sharing a band with 300 tracks reads as
/// "5 albums · 62 songs · 1.4 GB" instead of 300 paths.
fn add_music_rows(g: &adw::PreferencesGroup, files: &[ManifestFile]) {
    // Voice memos ride along as files but already have their own summary row.
    let music: Vec<&ManifestFile> = files
        .iter()
        .filter(|f| !f.rel_path.starts_with(MEMO_PREFIX))
        .collect();
    if music.is_empty() {
        return;
    }
    let groups = group_files(music.iter().copied());

    for grp in groups.iter().take(MAX_ARTIST_ROWS) {
        // Exactly one album and nothing loose: the album *is* the offer.
        if let (1, true) = (grp.albums.len(), grp.loose.is_empty()) {
            let a = &grp.albums[0];
            let sub = with_artist(grp.artist.as_deref(), &counts_line(0, a.idxs.len(), a.size));
            g.add(&summary_row(&a.album, &sub));
            continue;
        }
        let title = group_title(grp);
        if grp.albums.is_empty() {
            g.add(&summary_row(&title, &counts_line(0, grp.tracks, grp.size)));
            continue;
        }
        // Several albums (a whole-artist share): collapsed by default, the album
        // breakdown one tap away.
        let exp = adw::ExpanderRow::builder()
            .title(&title)
            .subtitle(counts_line(grp.albums.len(), grp.tracks, grp.size))
            .use_markup(false)
            .build();
        for a in &grp.albums {
            exp.add_row(&summary_row(
                &a.album,
                &counts_line(0, a.idxs.len(), a.size),
            ));
        }
        if !grp.loose.is_empty() {
            let size: u64 = grp.loose.iter().map(|&i| music[i].size).sum();
            exp.add_row(&summary_row(
                &gettext("Individual songs"),
                &counts_line(0, grp.loose.len(), size),
            ));
        }
        g.add(&exp);
    }

    if groups.len() > MAX_ARTIST_ROWS {
        let rest = &groups[MAX_ARTIST_ROWS..];
        let tracks: usize = rest.iter().map(|g| g.tracks).sum();
        let size: u64 = rest.iter().map(|g| g.size).sum();
        g.add(&summary_row(
            &gettext_f("{n} more artists", &[("n", &rest.len().to_string())]),
            &counts_line(0, tracks, size),
        ));
    }
}

/// Row title for a group: the artist, or a neutral label for untagged files.
fn group_title(grp: &ArtistGroup) -> String {
    grp.artist
        .clone()
        .unwrap_or_else(|| gettext("Individual songs"))
}

/// `"N albums · M songs · size"` (the albums part only when there are albums).
fn counts_line(albums: usize, songs: usize, size: u64) -> String {
    let counts = if albums > 0 {
        artist_count_subtitle(albums as u32, songs as u32)
    } else {
        ngettext_n("{n} song", "{n} songs", songs as u32)
    };
    format!("{counts} · {}", human_size(size))
}

/// Prefixes a summary line with the artist name (when known).
fn with_artist(artist: Option<&str>, line: &str) -> String {
    match artist {
        Some(a) => format!("{a} · {line}"),
        None => line.to_string(),
    }
}

/// A plain title/subtitle summary row (no interaction).
///
/// `use_markup(false)` throughout this file: titles here are song, album and
/// artist names straight from the tags, and Pango would swallow every one that
/// contains an `&` ("Bonnie & Clyde" rendered as an empty row).
fn summary_row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .use_markup(false)
        .build()
}

// ---------------------------------------------------------------------------
// Receiver review
// ---------------------------------------------------------------------------

/// Handles read into a [`ShareDecision`] on accept.
#[derive(Default)]
pub(crate) struct ReviewHandles {
    /// Kept alive here, not just inside the widgets: the group checkboxes talk to
    /// each other through it for as long as the review page is shown.
    links: MasterLinks,
    files: Vec<(gtk::CheckButton, String)>,
    yt: Vec<(gtk::CheckButton, String)>,
    favorites: Option<adw::SwitchRow>,
    playlists: Option<adw::SwitchRow>,
    podcasts: Option<adw::SwitchRow>,
    eq: Option<adw::SwitchRow>,
    categories: Option<adw::SwitchRow>,
}

impl ReviewHandles {
    pub(crate) fn to_decision(&self) -> ShareDecision {
        let on = |r: &Option<adw::SwitchRow>| r.as_ref().is_some_and(|s| s.is_active());
        ShareDecision {
            accept: true,
            files: self
                .files
                .iter()
                .filter(|(c, _)| c.is_active())
                .map(|(_, p)| p.clone())
                .collect(),
            yt: self
                .yt
                .iter()
                .filter(|(c, _)| c.is_active())
                .map(|(_, i)| i.clone())
                .collect(),
            favorites: on(&self.favorites),
            playlists: on(&self.playlists),
            podcasts: on(&self.podcasts),
            eq: on(&self.eq),
            categories: on(&self.categories),
        }
    }
}

/// Builds the receiver review for `manifest` (already classified by `reviews`),
/// returns the page plus the handles read on accept. `yt_enabled` is the local
/// capability (hide YT if off).
pub(crate) fn build_review(
    manifest: &ShareManifest,
    reviews: &[FileReview],
    yt_enabled: bool,
    sender: &ComponentSender<SyncPage>,
) -> (gtk::Widget, ReviewHandles) {
    let mut h = ReviewHandles::default();
    let page = page_box();

    let (new_n, have_n, coll_n) = reviews
        .iter()
        .fold((0, 0, 0), |(n, h, c), r| match r.status {
            FileStatus::New => (n + 1, h, c),
            FileStatus::AlreadyHave => (n, h + 1, c),
            FileStatus::Collision => (n, h, c + 1),
        });
    let head = adw::PreferencesGroup::builder()
        // A group title *is* markup and there is no opt-out — escape the peer's
        // freely chosen device name instead of losing the heading to an "&".
        .title(gettext_f(
            "{name} wants to share",
            &[(
                "name",
                &gtk::glib::markup_escape_text(&manifest.device_name),
            )],
        ))
        .description({
            // Only spell out the non-zero parts — "0 already here, 0 would
            // overwrite" is noise.
            let mut bits: Vec<String> = Vec::new();
            if new_n > 0 {
                bits.push(gettext_f("{n} new", &[("n", &new_n.to_string())]));
            }
            if have_n > 0 {
                bits.push(gettext_f("{n} already here", &[("n", &have_n.to_string())]));
            }
            if coll_n > 0 {
                bits.push(gettext_f(
                    "{n} would overwrite",
                    &[("n", &coll_n.to_string())],
                ));
            }
            let base = gettext_f(
                "{n} files · {size}",
                &[
                    ("n", &reviews.len().to_string()),
                    ("size", &human_size(manifest.total_size)),
                ],
            );
            if bits.is_empty() {
                base
            } else {
                format!("{base} · {}", bits.join(", "))
            }
        })
        .build();
    page.append(&head);

    if !reviews.is_empty() {
        let files = adw::PreferencesGroup::builder()
            .title(gettext("Files"))
            .build();
        // Mirror the sender's grouping — artist → album → track — so an incoming
        // artist share is reviewed (and accepted) as a handful of collapsed rows
        // instead of hundreds of file names, while every track keeps its own
        // checkbox and status marker one level down.
        for grp in group_files(reviews.iter().map(|r| &r.file)) {
            add_review_group(&files, &grp, reviews, &mut h);
        }
        h.links.refresh();
        page.append(&files);
    }

    if yt_enabled && !manifest.yt.is_empty() {
        let yt = adw::PreferencesGroup::builder()
            .title(gettext("YouTube"))
            .build();
        for item in &manifest.yt {
            let (row, check) = check_row(&item.title, None);
            check.set_active(true);
            yt.add(&row);
            h.yt.push((check, item.id.clone()));
        }
        page.append(&yt);
    }

    // Library-data switches, only for facets actually present in the offer.
    let lb = &manifest.library;
    if lb.favorites.is_some()
        || lb.playlists.is_some()
        || lb.podcasts.is_some()
        || lb.eq.is_some()
        || lb.categories.is_some()
    {
        let g = adw::PreferencesGroup::builder()
            .title(gettext("Library data"))
            .build();
        let add = |present: bool, title: String| -> Option<adw::SwitchRow> {
            present.then(|| {
                let s = adw::SwitchRow::builder().title(&title).active(true).build();
                g.add(&s);
                s
            })
        };
        h.favorites = add(lb.favorites.is_some(), gettext("Favorites"));
        h.playlists = add(lb.playlists.is_some(), gettext("Playlists"));
        h.podcasts = add(lb.podcasts.is_some(), gettext("Podcasts"));
        h.eq = add(lb.eq.is_some(), gettext("Equalizer"));
        h.categories = add(lb.categories.is_some(), gettext("Categories"));
        page.append(&g);
    }

    // Actions: reject / accept — pinned below the scrolling list.
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_halign(gtk::Align::Center);
    let reject = gtk::Button::builder()
        .label(gettext("Reject all"))
        .css_classes(["destructive-action"])
        .build();
    let accept = gtk::Button::builder()
        .label(gettext("Accept"))
        .css_classes(["suggested-action"])
        .build();
    {
        let sender = sender.clone();
        reject.connect_clicked(move |_| sender.input(SyncInput::RejectOffer));
    }
    {
        let sender = sender.clone();
        accept.connect_clicked(move |_| sender.input(SyncInput::AcceptOffer));
    }
    row.append(&reject);
    row.append(&accept);

    (action_shell(&page, &row), h)
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// A check row: a leading `CheckButton` plus a title (and optional subtitle).
fn check_row(title: &str, subtitle: Option<&str>) -> (adw::ActionRow, gtk::CheckButton) {
    let check = gtk::CheckButton::new();
    let row = adw::ActionRow::builder()
        .title(title)
        .activatable(true)
        .use_markup(false)
        .build();
    if let Some(s) = subtitle {
        row.set_subtitle(s);
    }
    row.add_prefix(&check);
    let c = check.clone();
    row.connect_activated(move |_| c.set_active(!c.is_active()));
    (row, check)
}

/// A file review row with a status marker (collision = warning, already-have = dim).
fn review_row(r: &FileReview) -> (adw::ActionRow, gtk::CheckButton) {
    // Inside an album/artist group the track title is what identifies the row;
    // the full path stays reachable as a tooltip (and is the fallback for files
    // that arrived without tags).
    let name = if r.file.title.trim().is_empty() {
        r.file.rel_path.clone()
    } else {
        r.file.title.clone()
    };
    let (row, check) = check_row(&name, Some(&human_size(r.file.size)));
    if !r.file.rel_path.is_empty() {
        row.set_tooltip_text(Some(&r.file.rel_path));
    }
    check.set_active(r.selected);
    match r.status {
        FileStatus::New => {}
        FileStatus::AlreadyHave => {
            row.add_css_class("dim-label");
            row.set_subtitle(&gettext("Already on this device"));
        }
        FileStatus::Collision => {
            let warn = gtk::Image::from_icon_name("dialog-warning-symbolic");
            warn.add_css_class("warning");
            row.add_suffix(&warn);
            row.set_subtitle(&gettext("Would overwrite a different file"));
        }
    }
    (row, check)
}

/// Renders one artist group of the review, collapsing it as far as it stays
/// unambiguous:
///
/// * a single album (the album-share case) → one album expander,
/// * several albums (the artist-share case) → an artist expander holding one
///   album expander each,
/// * album-less files → one row each when there is a single one, otherwise a
///   group row so a pile of loose tracks doesn't flood the page.
///
/// Every level carries a master checkbox, so a whole artist or album is accepted
/// with one tap while single tracks can still be unticked inside.
fn add_review_group(
    files: &adw::PreferencesGroup,
    grp: &ArtistGroup,
    reviews: &[FileReview],
    h: &mut ReviewHandles,
) {
    // A lone untagged file stays a plain row — wrapping one track in a group
    // would add a level without adding information.
    if grp.albums.is_empty() && grp.loose.len() == 1 {
        let r = &reviews[grp.loose[0]];
        let (row, check) = review_row(r);
        h.links.track(&check);
        files.add(&row);
        h.files.push((check, r.file.rel_path.clone()));
        return;
    }

    if grp.albums.len() == 1 && grp.loose.is_empty() {
        let a = &grp.albums[0];
        let sub = with_artist(grp.artist.as_deref(), &counts_line(0, a.idxs.len(), a.size));
        let (exp, master) = group_expander(&a.album, &sub, &a.idxs, reviews, 0);
        let checks = add_track_rows(&exp, &a.idxs, reviews, h, 1);
        h.links.master(&master, checks);
        files.add(&exp);
        return;
    }

    let idxs = grp.idxs();
    let title = group_title(grp);
    let sub = counts_line(grp.albums.len(), grp.tracks, grp.size);
    let (exp, master) = group_expander(&title, &sub, &idxs, reviews, 0);
    let mut checks = Vec::with_capacity(idxs.len());

    for a in &grp.albums {
        let (inner, inner_master) = group_expander(
            &a.album,
            &counts_line(0, a.idxs.len(), a.size),
            &a.idxs,
            reviews,
            1,
        );
        let album_checks = add_track_rows(&inner, &a.idxs, reviews, h, 2);
        h.links.master(&inner_master, album_checks.clone());
        checks.extend(album_checks);
        exp.add_row(&inner);
    }
    checks.extend(add_track_rows(&exp, &grp.loose, reviews, h, 1));

    h.links.master(&master, checks);
    files.add(&exp);
}

/// An expander for a group of files: master checkbox, summary subtitle and the
/// group's status (dimmed when everything is already here, warning icon when
/// accepting would overwrite something). `level` indents the checkbox, which is
/// what makes the artist → album → track nesting readable (libadwaita renders
/// nested rows flush otherwise).
fn group_expander(
    title: &str,
    subtitle: &str,
    idxs: &[usize],
    reviews: &[FileReview],
    level: i32,
) -> (adw::ExpanderRow, gtk::CheckButton) {
    let count = |s: FileStatus| idxs.iter().filter(|&&i| reviews[i].status == s).count();
    let have = count(FileStatus::AlreadyHave);
    let coll = count(FileStatus::Collision);

    let mut sub = subtitle.to_string();
    if have > 0 {
        sub = format!(
            "{sub} · {}",
            gettext_f("{n} already here", &[("n", &have.to_string())])
        );
    }
    if coll > 0 {
        sub = format!(
            "{sub} · {}",
            gettext_f("{n} would overwrite", &[("n", &coll.to_string())])
        );
    }

    let exp = adw::ExpanderRow::builder()
        .title(title)
        .subtitle(&sub)
        .use_markup(false)
        .build();
    if have == idxs.len() {
        exp.add_css_class("dim-label");
    }
    if coll > 0 {
        let warn = gtk::Image::from_icon_name("dialog-warning-symbolic");
        warn.add_css_class("warning");
        exp.add_suffix(&warn);
    }

    let master = gtk::CheckButton::builder()
        .valign(gtk::Align::Center)
        .margin_start(level * 16)
        .build();
    exp.add_prefix(&master);
    (exp, master)
}

/// Adds one track row per index to `exp`, registering each check with the review
/// handles, and returns the checks (for the enclosing master). `level` indents
/// the checks one step further than their group.
fn add_track_rows(
    exp: &adw::ExpanderRow,
    idxs: &[usize],
    reviews: &[FileReview],
    h: &mut ReviewHandles,
    level: i32,
) -> Vec<gtk::CheckButton> {
    let mut checks = Vec::with_capacity(idxs.len());
    for &i in idxs {
        let r = &reviews[i];
        let (row, check) = review_row(r);
        check.set_margin_start(level * 16);
        h.links.track(&check);
        exp.add_row(&row);
        checks.push(check.clone());
        h.files.push((check, r.file.rel_path.clone()));
    }
    checks
}

/// Keeps the group master checkboxes in sync with the per-track checks below
/// them — across nesting levels, where an artist master, its album masters and
/// the tracks all move together.
///
/// One shared re-entrancy flag for the whole tree: whoever starts a cascade
/// silences every other handler, then all masters are recomputed from their
/// tracks in one pass. Masters hold the registry weakly, so the widgets don't
/// keep it (and each other) alive; [`ReviewHandles`] owns it instead.
#[derive(Clone, Default)]
struct MasterLinks {
    groups: Rc<RefCell<Vec<(gtk::CheckButton, Vec<gtk::CheckButton>)>>>,
    updating: Rc<Cell<bool>>,
}

impl MasterLinks {
    /// Registers a per-track check: any manual tick refreshes the masters above.
    fn track(&self, check: &gtk::CheckButton) {
        let groups = Rc::downgrade(&self.groups);
        let updating = self.updating.clone();
        check.connect_toggled(move |_| {
            if updating.get() {
                return;
            }
            let Some(groups) = groups.upgrade() else {
                return;
            };
            updating.set(true);
            refresh_masters(&groups.borrow());
            updating.set(false);
        });
    }

    /// Registers a master over `checks`: toggling it applies to all of them.
    fn master(&self, master: &gtk::CheckButton, checks: Vec<gtk::CheckButton>) {
        self.groups
            .borrow_mut()
            .push((master.clone(), checks.clone()));
        let groups = Rc::downgrade(&self.groups);
        let updating = self.updating.clone();
        master.connect_toggled(move |m| {
            if updating.get() {
                return;
            }
            let Some(groups) = groups.upgrade() else {
                return;
            };
            updating.set(true);
            m.set_inconsistent(false);
            let active = m.is_active();
            for c in &checks {
                c.set_active(active);
            }
            refresh_masters(&groups.borrow());
            updating.set(false);
        });
    }

    /// Initial master states, once the whole tree is built.
    fn refresh(&self) {
        self.updating.set(true);
        refresh_masters(&self.groups.borrow());
        self.updating.set(false);
    }
}

/// Sets every master to all/none/tri-state from its tracks.
fn refresh_masters(groups: &[(gtk::CheckButton, Vec<gtk::CheckButton>)]) {
    for (m, checks) in groups {
        let on = checks.iter().filter(|c| c.is_active()).count();
        m.set_inconsistent(on != 0 && on != checks.len());
        m.set_active(!checks.is_empty() && on == checks.len());
    }
}

/// The vertical container the share pages fill with their groups.
///
/// Deliberately a plain box rather than an [`adw::PreferencesPage`]: that widget
/// carries its own internal scroller, whose natural height is a tiny minimum —
/// wrapping it in [`scrolled`] therefore propagated *that* minimum upwards and
/// collapsed the natural-sized dialog to a few lines. A box reports the real
/// height of its children, which is what the dialog has to follow.
fn page_box() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 18);
    b.set_margin_top(18);
    b.set_margin_bottom(18);
    b.set_margin_start(12);
    b.set_margin_end(12);
    b
}

/// Wraps a [`page_box`] in a vertically-scrolling, clamped container.
///
/// `propagate_natural_height` is essential: without it the scroller reports its
/// own tiny minimum and the natural-sized dialog collapses to a single line. With
/// it the dialog grows to the content's natural height, and `max_content_height`
/// caps how far a long file list may push it before scrolling takes over.
fn scrolled(content: &gtk::Box) -> gtk::Widget {
    // Same clamp width AdwPreferencesPage would have applied.
    let clamp = adw::Clamp::builder()
        .maximum_size(600)
        .tightening_threshold(400)
        .child(content)
        .build();
    let sw = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        // Generous cap: as a bottom sheet there is room to show the whole summary
        // instead of cutting it off after a few rows; the dialog itself is still
        // clamped to the window height on small screens.
        .max_content_height(820)
        .vexpand(true)
        .child(&clamp)
        .build();
    sw.upcast()
}

/// Puts the scrolling page above a pinned action bar, so the primary buttons
/// stay visible however long the content is. The dialog still follows the
/// content's natural height; only when the list is taller than the sheet does
/// the scroller take over — and the buttons, being outside it, are never the
/// part that gets cut off at the bottom.
fn action_shell(content: &gtk::Box, actions: &gtk::Box) -> gtk::Widget {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.append(&scrolled(content));
    outer.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    actions.set_margin_top(10);
    actions.set_margin_bottom(10);
    actions.set_margin_start(12);
    actions.set_margin_end(12);
    outer.append(actions);
    outer.upcast()
}
