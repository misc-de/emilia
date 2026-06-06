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

use crate::core::sync::share::{
    human_size, FileReview, FileStatus, ShareDecision, ShareManifest,
};
use crate::i18n::{gettext, gettext_f};
use crate::ui::sync_page::{SyncInput, SyncPage};

// ---------------------------------------------------------------------------
// Size confirmation (sender)
// ---------------------------------------------------------------------------

/// Builds the "transfer summary" confirmation shown after the manifest is built.
pub(crate) fn build_confirm(
    total_size: u64,
    file_count: usize,
    names: &[String],
    sender: &ComponentSender<SyncPage>,
) -> gtk::Widget {
    let page = adw::PreferencesPage::new();
    let g = adw::PreferencesGroup::builder()
        .title(gettext("Transfer summary"))
        .build();
    let files_row = adw::ActionRow::builder()
        .title(gettext_f("{n} files", &[("n", &file_count.to_string())]))
        .subtitle(names.iter().take(4).cloned().collect::<Vec<_>>().join(", "))
        .build();
    let size_row = adw::ActionRow::builder()
        .title(gettext("Total size"))
        .subtitle(human_size(total_size))
        .build();
    g.add(&files_row);
    g.add(&size_row);
    page.add(&g);

    let buttons = adw::PreferencesGroup::new();
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_halign(gtk::Align::Center);
    row.set_margin_top(12);
    let cancel = gtk::Button::with_label(&gettext("Cancel"));
    let send = gtk::Button::builder()
        .label(gettext("Send"))
        .css_classes(["suggested-action"])
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
    page.add(&buttons);

    scrolled(&page)
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
    let page = adw::PreferencesPage::new();

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
    page.add(&head);

    if !reviews.is_empty() {
        let files = adw::PreferencesGroup::builder()
            .title(gettext("Files"))
            .build();
        for r in reviews {
            let (row, check) = review_row(r);
            files.add(&row);
            h.files.push((check, r.file.rel_path.clone()));
        }
        page.add(&files);
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
        page.add(&yt);
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
        page.add(&g);
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
    page.add(&buttons);

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

/// Wraps a preferences page in a vertically-scrolling container.
fn scrolled(page: &adw::PreferencesPage) -> gtk::Widget {
    let sw = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(page)
        .build();
    sw.upcast()
}
