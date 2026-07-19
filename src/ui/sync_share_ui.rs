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

use adw::prelude::*;
use relm4::{adw, gtk, ComponentSender};

use crate::core::sync::share::{human_size, FileReview, FileStatus, ShareDecision, ShareManifest};
use crate::i18n::{gettext, gettext_f};
use crate::ui::sync_page::{SyncInput, SyncPage};

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
    if file_count > 0 {
        let names = manifest
            .files
            .iter()
            .take(4)
            .map(|f| {
                if f.rel_path.is_empty() {
                    f.title.clone()
                } else {
                    f.rel_path.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        g.add(
            &adw::ActionRow::builder()
                .title(gettext_f("{n} files", &[("n", &file_count.to_string())]))
                .subtitle(names)
                .build(),
        );
    }

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

    count_row(
        &g,
        yt,
        gettext_f("{n} YouTube items", &[("n", &yt.to_string())]),
    );
    count_row(
        &g,
        pod,
        gettext_f("{n} podcasts", &[("n", &pod.to_string())]),
    );
    count_row(
        &g,
        pl,
        gettext_f("{n} playlists", &[("n", &pl.to_string())]),
    );
    count_row(
        &g,
        fav,
        gettext_f("{n} favorites", &[("n", &fav.to_string())]),
    );
    count_row(
        &g,
        st,
        gettext_f("{n} radio stations", &[("n", &st.to_string())]),
    );
    count_row(
        &g,
        rec,
        gettext_f("{n} recordings", &[("n", &rec.to_string())]),
    );
    count_row(
        &g,
        memo,
        gettext_f("{n} voice memos", &[("n", &memo.to_string())]),
    );
    count_row(
        &g,
        cat,
        gettext_f("{n} categories", &[("n", &cat.to_string())]),
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

    let buttons = adw::PreferencesGroup::new();
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_halign(gtk::Align::Center);
    row.set_margin_top(12);
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
    buttons.add(&row);
    page.append(&buttons);

    scrolled(&page)
}

/// Adds a `"{n} …"` summary row to `g` when `n > 0` (skipped otherwise).
fn count_row(g: &adw::PreferencesGroup, n: usize, label: String) {
    if n > 0 {
        g.add(&adw::ActionRow::builder().title(label).build());
    }
}

// ---------------------------------------------------------------------------
// Receiver review
// ---------------------------------------------------------------------------

/// Handles read into a [`ShareDecision`] on accept.
#[derive(Default)]
pub(crate) struct ReviewHandles {
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
        .title(gettext_f(
            "{name} wants to share",
            &[("name", &manifest.device_name)],
        ))
        .description(gettext_f(
            "{n} files · {size} · {new} new, {have} already here, {coll} would overwrite",
            &[
                ("n", &reviews.len().to_string()),
                ("size", &human_size(manifest.total_size)),
                ("new", &new_n.to_string()),
                ("have", &have_n.to_string()),
                ("coll", &coll_n.to_string()),
            ],
        ))
        .build();
    page.append(&head);

    if !reviews.is_empty() {
        let files = adw::PreferencesGroup::builder()
            .title(gettext("Files"))
            .build();
        // Group by album so a whole album is confirmed as **one** unit (a single
        // checkbox) instead of one tick per track; loose songs stay individual
        // rows. Files without an album each form their own single-item group.
        for (album, idxs) in group_by_album(reviews) {
            if album.is_some() && idxs.len() >= 2 {
                let group: Vec<&FileReview> = idxs.iter().map(|&i| &reviews[i]).collect();
                let (row, handles) = album_expander(album.as_deref().unwrap_or_default(), &group);
                files.add(&row);
                h.files.extend(handles);
            } else {
                for &i in &idxs {
                    let r = &reviews[i];
                    let (row, check) = review_row(r);
                    files.add(&row);
                    h.files.push((check, r.file.rel_path.clone()));
                }
            }
        }
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

    // Actions: reject / accept.
    let buttons = adw::PreferencesGroup::new();
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_halign(gtk::Align::Center);
    row.set_margin_top(12);
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
    buttons.add(&row);
    page.append(&buttons);

    (scrolled(&page), h)
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
    let name = if r.file.rel_path.is_empty() {
        r.file.title.clone()
    } else {
        r.file.rel_path.clone()
    };
    let (row, check) = check_row(&name, Some(&human_size(r.file.size)));
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

/// Groups reviews by album, preserving first-seen order. Files without an album
/// each become their own single-item group (so they render as loose rows).
fn group_by_album(reviews: &[FileReview]) -> Vec<(Option<String>, Vec<usize>)> {
    let mut groups: Vec<(Option<String>, Vec<usize>)> = Vec::new();
    for (i, r) in reviews.iter().enumerate() {
        match r.file.album.as_deref().filter(|a| !a.trim().is_empty()) {
            Some(album) => {
                if let Some(g) = groups.iter_mut().find(|(k, _)| k.as_deref() == Some(album)) {
                    g.1.push(i);
                } else {
                    groups.push((Some(album.to_string()), vec![i]));
                }
            }
            None => groups.push((None, vec![i])),
        }
    }
    groups
}

/// An [`adw::ExpanderRow`] for a whole album: the prefix master checkbox selects or
/// deselects every track at once, so the album is confirmed as a single unit while
/// each track still keeps its own row + status marker inside the expander. The
/// master shows an inconsistent (tri-state) mark when only some tracks are picked.
/// Returns the row plus the per-track `(check, rel_path)` handles to fold into the
/// [`ReviewHandles`].
fn album_expander(
    album: &str,
    reviews: &[&FileReview],
) -> (adw::ExpanderRow, Vec<(gtk::CheckButton, String)>) {
    use std::cell::Cell;
    use std::rc::Rc;

    let total: u64 = reviews.iter().map(|r| r.file.size).sum();
    let artist = reviews
        .iter()
        .find_map(|r| r.file.artist.as_deref().filter(|a| !a.trim().is_empty()));

    let exp = adw::ExpanderRow::builder().title(album).build();
    let count = gettext_f("{n} files", &[("n", &reviews.len().to_string())]);
    let summary = format!("{count} · {}", human_size(total));
    exp.set_subtitle(&match artist {
        Some(a) => format!("{a} · {summary}"),
        None => summary,
    });

    let master = gtk::CheckButton::builder()
        .valign(gtk::Align::Center)
        .build();
    exp.add_prefix(&master);

    let mut handles = Vec::with_capacity(reviews.len());
    let mut checks = Vec::with_capacity(reviews.len());
    for r in reviews {
        let (row, check) = review_row(r);
        exp.add_row(&row);
        checks.push(check.clone());
        handles.push((check, r.file.rel_path.clone()));
    }

    // Keep the master and the per-track checks in sync without feedback loops: a
    // re-entrancy flag swallows the toggles each side triggers in the other.
    let syncing = Rc::new(Cell::new(false));
    {
        let checks = checks.clone();
        let syncing = syncing.clone();
        master.connect_toggled(move |m| {
            if syncing.replace(true) {
                return;
            }
            m.set_inconsistent(false);
            let active = m.is_active();
            for c in &checks {
                c.set_active(active);
            }
            syncing.set(false);
        });
    }
    let refresh_master = {
        let master = master.clone();
        let checks = checks.clone();
        let syncing = syncing.clone();
        move || {
            if syncing.replace(true) {
                return;
            }
            let on = checks.iter().filter(|c| c.is_active()).count();
            master.set_inconsistent(on != 0 && on != checks.len());
            master.set_active(on == checks.len());
            syncing.set(false);
        }
    };
    for c in &checks {
        let refresh = refresh_master.clone();
        c.connect_toggled(move |_| refresh());
    }
    refresh_master();

    (exp, handles)
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
        .max_content_height(620)
        .vexpand(true)
        .child(&clamp)
        .build();
    sw.upcast()
}
