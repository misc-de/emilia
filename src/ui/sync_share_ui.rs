//! Widgets for the selective-share flow: the **sender picker** (what to share),
//! the **size confirmation** and the **receiver review** (collision/dedup
//! markers + selective accept). Kept out of [`super::sync_page`] to keep that
//! component readable. The builders return the page widget plus a handle struct;
//! the [`SyncPage`](super::sync_page::SyncPage) reads the handles on confirm.

use adw::prelude::*;
use relm4::{adw, gtk, ComponentSender};

use crate::core::db::Library;
use crate::core::sync::protocol::Capabilities;
use crate::core::sync::share::{
    human_size, FileReview, FileStatus, Selection, ShareDecision, ShareManifest,
};
use crate::i18n::{gettext, gettext_f};
use crate::ui::sync_page::{SyncInput, SyncPage};

// ---------------------------------------------------------------------------
// Sender picker
// ---------------------------------------------------------------------------

/// Widget handles of the picker, read into a [`Selection`] on "Continue".
#[derive(Default)]
pub(crate) struct PickerHandles {
    whole: Option<adw::SwitchRow>,
    audiobooks: Option<adw::SwitchRow>,
    concerts: Option<adw::SwitchRow>,
    artists: Vec<(gtk::CheckButton, String)>,
    albums: Vec<(gtk::CheckButton, (String, String))>,
    yt_channels: Vec<(gtk::CheckButton, i64)>,
    yt_playlists: Vec<(gtk::CheckButton, String)>,
    favorites: Option<adw::SwitchRow>,
    playlists: Option<adw::SwitchRow>,
    podcasts: Option<adw::SwitchRow>,
    eq: Option<adw::SwitchRow>,
    categories: Option<adw::SwitchRow>,
}

impl PickerHandles {
    pub(crate) fn to_selection(&self) -> Selection {
        let on = |r: &Option<adw::SwitchRow>| r.as_ref().is_some_and(|s| s.is_active());
        Selection {
            whole_library: on(&self.whole),
            artists: self.artists.iter().filter(|(c, _)| c.is_active()).map(|(_, a)| a.clone()).collect(),
            albums: self.albums.iter().filter(|(c, _)| c.is_active()).map(|(_, a)| a.clone()).collect(),
            song_paths: Vec::new(),
            audiobooks: on(&self.audiobooks),
            concerts: on(&self.concerts),
            yt_channels: self.yt_channels.iter().filter(|(c, _)| c.is_active()).map(|(_, i)| *i).collect(),
            yt_playlists: self.yt_playlists.iter().filter(|(c, _)| c.is_active()).map(|(_, u)| u.clone()).collect(),
            yt_songs: Vec::new(),
            include_favorites: on(&self.favorites),
            include_playlists: on(&self.playlists),
            include_podcasts: on(&self.podcasts),
            include_eq: on(&self.eq),
            include_categories: on(&self.categories),
        }
    }
}

/// Builds the share picker page for `peer` and returns it plus the read handles.
pub(crate) fn build_picker(
    lib: &Library,
    peer: &str,
    peer_caps: &Capabilities,
    sender: &ComponentSender<SyncPage>,
) -> (gtk::Widget, PickerHandles) {
    let mut h = PickerHandles::default();
    let page = adw::PreferencesPage::new();

    // Whole library.
    let g0 = adw::PreferencesGroup::builder()
        .title(&gettext_f("Share with {peer}", &[("peer", peer)]))
        .build();
    let whole = adw::SwitchRow::builder().title(&gettext("Entire library")).build();
    g0.add(&whole);
    h.whole = Some(whole);
    page.add(&g0);

    // Music: artists + albums (each in an expander of check rows).
    let music = adw::PreferencesGroup::builder().title(&gettext("Music")).build();
    let artists = lib.distinct_artists().unwrap_or_default();
    if !artists.is_empty() {
        let exp = adw::ExpanderRow::builder()
            .title(&gettext("Artists"))
            .subtitle(&format!("{}", artists.len()))
            .build();
        for a in &artists {
            let (row, check) = check_row(a, None);
            exp.add_row(&row);
            h.artists.push((check, a.clone()));
        }
        music.add(&exp);
    }
    // Albums (flattened "Artist – Album").
    let mut albums: Vec<(String, String)> = Vec::new();
    for a in &artists {
        for alb in lib.albums_of_artist(a).unwrap_or_default() {
            albums.push((a.clone(), alb));
        }
    }
    if !albums.is_empty() {
        let exp = adw::ExpanderRow::builder()
            .title(&gettext("Albums"))
            .subtitle(&format!("{}", albums.len()))
            .build();
        for (artist, album) in &albums {
            let (row, check) = check_row(album, Some(artist));
            exp.add_row(&row);
            h.albums.push((check, (artist.clone(), album.clone())));
        }
        music.add(&exp);
    }
    page.add(&music);

    // Audiobooks / concerts (whole-area toggles).
    let areas = adw::PreferencesGroup::builder().title(&gettext("Collections")).build();
    let ab = adw::SwitchRow::builder().title(&gettext("Audiobooks")).build();
    let co = adw::SwitchRow::builder().title(&gettext("Concerts")).build();
    areas.add(&ab);
    areas.add(&co);
    h.audiobooks = Some(ab);
    h.concerts = Some(co);
    page.add(&areas);

    // YouTube — only if the peer can accept it.
    if peer_caps.youtube_enabled {
        let yt = adw::PreferencesGroup::builder().title(&gettext("YouTube")).build();
        let channels = lib.channels().unwrap_or_default();
        if !channels.is_empty() {
            let exp = adw::ExpanderRow::builder().title(&gettext("Channels")).build();
            for (id, title, _url, _thumb, _n) in &channels {
                let (row, check) = check_row(title, None);
                exp.add_row(&row);
                h.yt_channels.push((check, *id));
            }
            yt.add(&exp);
        }
        let pls: Vec<(String, String)> = lib
            .playlists_with_origin()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, name, _c, origin)| origin.map(|o| (name, o)))
            .collect();
        if !pls.is_empty() {
            let exp = adw::ExpanderRow::builder().title(&gettext("Playlists")).build();
            for (name, origin) in &pls {
                let (row, check) = check_row(name, None);
                exp.add_row(&row);
                h.yt_playlists.push((check, origin.clone()));
            }
            yt.add(&exp);
        }
        page.add(&yt);
    }

    // Library data.
    let libdata = adw::PreferencesGroup::builder().title(&gettext("Library data")).build();
    let fav = adw::SwitchRow::builder().title(&gettext("Favorites")).build();
    let pl = adw::SwitchRow::builder().title(&gettext("Playlists")).build();
    let pod = adw::SwitchRow::builder().title(&gettext("Podcasts")).build();
    let eq = adw::SwitchRow::builder().title(&gettext("Equalizer")).build();
    let cat = adw::SwitchRow::builder().title(&gettext("Categories")).build();
    for r in [&fav, &pl, &pod, &eq, &cat] {
        libdata.add(r);
    }
    h.favorites = Some(fav);
    h.playlists = Some(pl);
    h.podcasts = Some(pod);
    h.eq = Some(eq);
    h.categories = Some(cat);
    page.add(&libdata);

    // Bottom action.
    let btn = gtk::Button::builder()
        .label(&gettext("Continue"))
        .css_classes(["suggested-action", "pill"])
        .halign(gtk::Align::Center)
        .margin_top(12)
        .build();
    {
        let sender = sender.clone();
        btn.connect_clicked(move |_| sender.input(SyncInput::PreparePicked));
    }
    let actions = adw::PreferencesGroup::new();
    actions.add(&btn);
    page.add(&actions);

    (scrolled(&page), h)
}

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
    let g = adw::PreferencesGroup::builder().title(&gettext("Transfer summary")).build();
    let files_row = adw::ActionRow::builder()
        .title(&gettext_f("{n} files", &[("n", &file_count.to_string())]))
        .subtitle(&names.iter().take(4).cloned().collect::<Vec<_>>().join(", "))
        .build();
    let size_row = adw::ActionRow::builder()
        .title(&gettext("Total size"))
        .subtitle(&human_size(total_size))
        .build();
    g.add(&files_row);
    g.add(&size_row);
    page.add(&g);

    let buttons = adw::PreferencesGroup::new();
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_halign(gtk::Align::Center);
    row.set_margin_top(12);
    let cancel = gtk::Button::with_label(&gettext("Cancel"));
    let send = gtk::Button::builder().label(&gettext("Send")).css_classes(["suggested-action"]).build();
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
            files: self.files.iter().filter(|(c, _)| c.is_active()).map(|(_, p)| p.clone()).collect(),
            yt: self.yt.iter().filter(|(c, _)| c.is_active()).map(|(_, i)| i.clone()).collect(),
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

    let (new_n, have_n, coll_n) = reviews.iter().fold((0, 0, 0), |(n, h, c), r| match r.status {
        FileStatus::New => (n + 1, h, c),
        FileStatus::AlreadyHave => (n, h + 1, c),
        FileStatus::Collision => (n, h, c + 1),
    });
    let head = adw::PreferencesGroup::builder()
        .title(&gettext_f("{name} wants to share", &[("name", &manifest.device_name)]))
        .description(&gettext_f(
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
        let files = adw::PreferencesGroup::builder().title(&gettext("Files")).build();
        for r in reviews {
            let (row, check) = review_row(r);
            files.add(&row);
            h.files.push((check, r.file.rel_path.clone()));
        }
        page.add(&files);
    }

    if yt_enabled && !manifest.yt.is_empty() {
        let yt = adw::PreferencesGroup::builder().title(&gettext("YouTube")).build();
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
    if lb.favorites.is_some() || lb.playlists.is_some() || lb.podcasts.is_some()
        || lb.eq.is_some() || lb.categories.is_some()
    {
        let g = adw::PreferencesGroup::builder().title(&gettext("Library data")).build();
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
    let reject = gtk::Button::builder().label(&gettext("Reject all")).css_classes(["destructive-action"]).build();
    let accept = gtk::Button::builder().label(&gettext("Accept")).css_classes(["suggested-action"]).build();
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
    let row = adw::ActionRow::builder().title(title).activatable(true).build();
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
    let name = if r.file.rel_path.is_empty() { r.file.title.clone() } else { r.file.rel_path.clone() };
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
