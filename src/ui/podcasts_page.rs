//! Podcasts as a standalone relm4 component: overview list (+ gallery variant),
//! "Newest" episodes across all subscriptions, the subscription/episode detail
//! dialogs, the subscribe-search dialog, and the background fetching of feeds.
//! Episodes are streamed directly. Extracted from the `App` god-object.
//!
//! **Boundary:** this component owns the *page* (lists, dialogs, search,
//! downloads); the actual *playback* of an episode stays in the parent
//! transport (`playing_episode_url` is the transport's truth). The page reaches
//! the transport through [`PodcastsOutput`] (`ToggleEpisode`/`EpisodeSeekTo`)
//! and is told the playback state back through
//! [`PodcastsInput::PlaybackStateChanged`] so it can keep the row play/pause
//! icons in sync. Subpage navigation and the (undo) toast live on the parent's
//! shared chrome, so they too go through `Output`.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::ui::app::PodcastView;
use crate::ui::app_gallery::{gallery_cell, size_gallery_tiles_when_ready, spawn_gallery_decode};
use crate::ui::app_helpers::{cover_widget, on_secondary_click};

/// Fetches a feed and stores podcast + episodes (runs in the worker thread,
/// its own DB connection). Returns the podcast title on success.
pub(crate) fn fetch_and_store_podcast(feed_url: &str) -> Option<String> {
    let feed = crate::core::podcast::fetch_feed(feed_url).ok()?;
    let lib = Library::open().ok()?;
    let id = lib
        .subscribe_podcast(&feed.title, feed_url, feed.image_url.as_deref())
        .ok()?;
    let _ = lib.set_episodes(id, &feed.episodes);
    // Load the feed image into the cache (worker thread, no UI block).
    if let Some(img) = feed.image_url.as_deref() {
        crate::core::online::cache_podcast_image(img);
    }
    Some(feed.title)
}

/// Content box for the detail dialogs (uniform margins).
fn detail_box() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build()
}

/// Activatable action row with an icon prefix (for the detail dialogs).
fn action_row(title: &str, icon: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row
}

/// Embeds the content scrollably in a dialog with a header bar and shows it.
fn present_detail(dialog: &adw::Dialog, content: &gtk::Box, root: &adw::ApplicationWindow) {
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .vexpand(true)
        .child(content)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    dialog.set_child(Some(&toolbar));
    // Use full width, but never wider than 600 px (on narrow windows the
    // dialog automatically shrinks to the window width).
    dialog.set_content_width(600);
    dialog.present(Some(root));
}

/// The podcasts page component.
pub(crate) struct PodcastsPage {
    /// Own DB connection (WAL + per-thread, the project's established pattern).
    library: Library,
    /// Window the dialogs are presented on (set on `SetWindow`).
    window: Option<adw::ApplicationWindow>,
    /// Mirror of the transport's `playing_episode_url` (for the row icons).
    playing_url: Option<String>,
    /// Mirror of the transport's play/pause state.
    playing: bool,
    /// Gallery vs. list overview (mirror of the global `gallery_view` setting).
    gallery_view: bool,
    /// Gallery columns (mirror of the global setting).
    gallery_columns: u32,
    /// One-time resize hook of the overview gallery already registered?
    gallery_hooked: Cell<bool>,
    /// Narrow (mobile) layout → detail dialogs as bottom sheets.
    mobile: bool,
    /// (id, title, image URL, episode count) per podcast.
    podcast_items: Vec<(i64, String, Option<String>, i64)>,
    podcasts_list: gtk::ListBox,
    /// Gallery variant of the podcast overview (cover grid).
    podcasts_gallery: gtk::FlowBox,
    /// Which podcast view is visible: newest episodes or subscription overview.
    podcast_view: PodcastView,
    /// Newest episodes across all subscriptions (for the "Newest" view).
    newest_items: Vec<crate::model::EpisodeRef>,
    /// Container of the "Newest" list (filled imperatively in `reload_newest`).
    newest_list: gtk::Box,
    /// Hits of the last podcast search (iTunes), for the subscribe dialog.
    podcast_search_results: Vec<crate::core::podcast::PodcastSearchResult>,
    /// The last podcast search hit a network/service error (vs. no hits).
    podcast_search_failed: bool,
    /// While the subscribe search dialog is open: (dialog, hit list).
    podcast_search: Rc<RefCell<Option<(adw::Dialog, gtk::ListBox)>>>,
    /// Play/pause buttons of the visible episode rows (audio URL → button).
    episode_play_buttons: Rc<RefCell<Vec<(String, gtk::Button)>>>,
    /// "Play" row of an open episode detail dialog (row, audio URL).
    ctx_episode_play: Rc<RefCell<Option<(adw::ActionRow, String)>>>,
    /// "Download" column of an open episode detail dialog (value label, audio URL).
    ctx_episode_download: Rc<RefCell<Option<(gtk::Label, String)>>>,
    /// Audio URLs of episodes whose download is currently running.
    downloading_episodes: HashSet<String>,
    /// Hand-off slot for a built episode subpage. The parent owns the shared
    /// NavigationView; since its `Msg` must be `Send` it cannot carry the
    /// (`!Send`) `gtk::Box` through a message, so we park the built page here and
    /// only signal `PushSubpage` (a unit) — the parent then pushes it.
    subpage_slot: Rc<RefCell<Option<(String, gtk::Box)>>>,
}

#[derive(Debug)]
pub(crate) enum PodcastsInput {
    // --- driven by the parent ---
    /// Rebuild overview + newest (init, after import, after feed-image caching).
    Reload,
    /// Global "refresh all" button: re-fetch every subscribed feed.
    RefreshAll,
    /// Playback state changed: update the icon mirrors + refresh row icons.
    PlaybackStateChanged {
        playing_url: Option<String>,
        playing: bool,
    },
    SetGalleryView(bool),
    SetGalleryColumns(u32),
    SetMobile(bool),
    SetWindow(adw::ApplicationWindow),
    // --- view-internal (from the page's own rows/dialogs) ---
    SetView(PodcastView),
    Subscribe,
    Search(String),
    SubscribeUrl(String),
    Refresh(i64),
    OpenPodcast(i64),
    OpenPodcastAt(usize),
    ShowPodcastDetail(i64),
    ShowPodcastDetailAt(usize),
    ShowEpisodeDetail(usize),
    ShowPodcastEpisodeDetail {
        podcast_id: i64,
        index: usize,
    },
    ToggleDownload {
        url: String,
        title: String,
    },
    /// "Remove podcast" tapped → show the confirmation alert.
    Delete(i64),
    /// Undo window elapsed → actually remove the podcast.
    DeleteConfirmed(i64),
}

#[derive(Debug)]
pub(crate) enum PodcastsOutput {
    /// Transport: start/pause this episode (parent owns the player).
    ToggleEpisode { url: String, title: String },
    /// Transport: jump to/start at a show-notes timestamp.
    EpisodeSeekTo { url: String, title: String, ms: i64 },
    /// A built episode subpage is parked in `subpage_slot`; ask the parent to
    /// push it onto the shared NavigationView. Unit, so the parent's `Send` `Msg`
    /// stays valid (the `!Send` widget travels through the shared slot instead).
    PushSubpage,
    /// Informational toast (parent owns the overlay; currently a no-op).
    Toast(String),
    /// Share a selection (a podcast) over device sync. Boxed: `Selection` is far
    /// larger than the other variants (`clippy::large_enum_variant`).
    Share(Box<crate::core::sync::share::Selection>),
    /// Show the "Podcast removed" undo toast; the parent defers the real
    /// deletion back to us via [`PodcastsInput::DeleteConfirmed`].
    DeletedUndoToast(i64),
    /// A "refresh all" worker was started → the parent counts it for the spinner.
    RefreshStarted(bool),
    /// The "refresh all" worker finished → the parent clears one spinner count.
    RefreshFinished,
}

#[derive(Debug)]
pub(crate) enum PodcastsCmd {
    /// Feed fetch finished (subscribe/refresh): `Some(title)` on success.
    Fetched(Option<String>),
    /// Episode download finished.
    Downloaded {
        url: String,
        result: Result<String, String>,
    },
    /// Search hits (still without covers).
    SearchResults(Vec<crate::core::podcast::PodcastSearchResult>),
    /// Search failed (service unreachable).
    SearchFailed,
    /// Search-hit covers cached → redraw the hit list.
    SearchCoversReady,
    /// All feeds (refresh-all) re-fetched.
    Refreshed,
    /// Startup feed-image cache finished → redraw the overview.
    CoversCached,
}

#[relm4::component(pub(crate))]
impl Component for PodcastsPage {
    type Init = Rc<RefCell<Option<(String, gtk::Box)>>>;
    type Input = PodcastsInput;
    type Output = PodcastsOutput;
    type CommandOutput = PodcastsCmd;

    view! {
        #[root]
        gtk::Box {
            set_orientation: gtk::Orientation::Vertical,

            // Header: linked tab switcher "Newest" / "Overview" and "+".
            gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 6,
                set_margin_top: 2,
                set_margin_bottom: 4,
                set_margin_start: 12,
                set_margin_end: 12,
                add_css_class: "linked",

                gtk::ToggleButton {
                    set_label: &gettext("Newest"),
                    set_hexpand: true,
                    #[watch]
                    set_active: model.podcast_view == PodcastView::Newest,
                    connect_clicked => PodcastsInput::SetView(PodcastView::Newest),
                },
                gtk::ToggleButton {
                    set_label: &gettext("Overview"),
                    set_hexpand: true,
                    #[watch]
                    set_active: model.podcast_view == PodcastView::Overview,
                    connect_clicked => PodcastsInput::SetView(PodcastView::Overview),
                },
                gtk::Button {
                    set_icon_name: "list-add-symbolic",
                    set_tooltip_text: Some(&gettext("Subscribe to podcast")),
                    add_css_class: "flat",
                    connect_clicked => PodcastsInput::Subscribe,
                },
            },

            // "Newest": newest episodes across all subscriptions.
            gtk::ScrolledWindow {
                set_vexpand: true,
                #[watch]
                set_visible: model.podcast_view == PodcastView::Newest && !model.newest_items.is_empty(),
                #[local_ref]
                newest_list -> gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 6,
                    set_valign: gtk::Align::Start,
                    set_margin_top: 0,
                    set_margin_bottom: 12,
                    set_margin_start: 12,
                    set_margin_end: 12,
                },
            },
            adw::StatusPage {
                set_icon_name: Some("podcast-symbolic"),
                set_title: &gettext("No episodes"),
                set_vexpand: true,
                #[watch]
                set_visible: model.podcast_view == PodcastView::Newest && model.newest_items.is_empty(),
            },

            // "Overview": subscribed podcasts (list variant).
            gtk::ScrolledWindow {
                set_vexpand: true,
                #[watch]
                set_visible: model.podcast_view == PodcastView::Overview && !model.podcast_items.is_empty() && !model.gallery_view,
                #[local_ref]
                podcasts_list -> gtk::ListBox {
                    set_valign: gtk::Align::Start,
                    set_margin_top: 10,
                    set_margin_bottom: 12,
                    set_margin_start: 12,
                    set_margin_end: 12,
                    set_css_classes: &["boxed-list"],
                },
            },
            // Gallery variant of the subscription overview.
            gtk::ScrolledWindow {
                set_vexpand: true,
                #[watch]
                set_visible: model.podcast_view == PodcastView::Overview && !model.podcast_items.is_empty() && model.gallery_view,
                #[local_ref]
                podcasts_gallery -> gtk::FlowBox {
                    set_valign: gtk::Align::Start,
                    set_margin_top: 10,
                    set_margin_bottom: 12,
                    set_margin_start: 12,
                    set_margin_end: 12,
                },
            },
            adw::StatusPage {
                set_icon_name: Some("podcast-symbolic"),
                set_title: &gettext("No podcasts"),
                set_description: Some(&gettext("Subscribe to a podcast via its feed address (RSS).")),
                set_vexpand: true,
                #[watch]
                set_visible: model.podcast_view == PodcastView::Overview && model.podcast_items.is_empty(),
            },
        }
    }

    fn init(
        subpage_slot: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // A failed second connection must not crash the whole app; degrade to a
        // temporary in-memory DB (logged) instead of panicking the UI thread.
        let library = Library::open_or_memory();
        let podcasts_list = gtk::ListBox::new();
        let newest_list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let podcasts_gallery = gtk::FlowBox::new();
        let model = PodcastsPage {
            library,
            window: None,
            playing_url: None,
            playing: false,
            gallery_view: false,
            gallery_columns: 4,
            gallery_hooked: Cell::new(false),
            mobile: false,
            podcast_items: Vec::new(),
            podcasts_list: podcasts_list.clone(),
            podcasts_gallery: podcasts_gallery.clone(),
            podcast_view: PodcastView::Newest,
            newest_items: Vec::new(),
            newest_list: newest_list.clone(),
            podcast_search_results: Vec::new(),
            podcast_search_failed: false,
            podcast_search: Rc::new(RefCell::new(None)),
            episode_play_buttons: Rc::new(RefCell::new(Vec::new())),
            ctx_episode_play: Rc::new(RefCell::new(None)),
            ctx_episode_download: Rc::new(RefCell::new(None)),
            downloading_episodes: HashSet::new(),
            subpage_slot,
        };
        // Cache the podcast feed images once in the background, then rebuild the
        // overview so the covers appear (no UI block at startup).
        sender.spawn_oneshot_command(|| {
            if let Ok(lib) = Library::open() {
                for (_, _, image, _) in lib.podcasts().unwrap_or_default() {
                    if let Some(url) = image {
                        crate::core::online::cache_podcast_image(&url);
                    }
                }
            }
            PodcastsCmd::CoversCached
        });
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: PodcastsInput, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            PodcastsInput::Reload => self.reload_podcasts(&sender),
            PodcastsInput::RefreshAll => {
                if self.podcast_items.is_empty() {
                    return;
                }
                sender.spawn_oneshot_command(|| {
                    if let Ok(lib) = Library::open() {
                        for url in lib.podcast_feed_urls().unwrap_or_default() {
                            let _ = fetch_and_store_podcast(&url);
                        }
                    }
                    PodcastsCmd::Refreshed
                });
                let _ = sender.output(PodcastsOutput::RefreshStarted(true));
            }
            PodcastsInput::PlaybackStateChanged {
                playing_url,
                playing,
            } => {
                self.playing_url = playing_url;
                self.playing = playing;
                self.refresh_episode_icons();
            }
            PodcastsInput::SetGalleryView(on) => {
                self.gallery_view = on;
                self.reload_podcasts(&sender);
            }
            PodcastsInput::SetGalleryColumns(n) => {
                self.gallery_columns = n.clamp(2, 8);
                if self.gallery_view {
                    self.reload_podcasts(&sender);
                }
            }
            PodcastsInput::SetMobile(b) => self.mobile = b,
            PodcastsInput::SetWindow(w) => self.window = Some(w),
            PodcastsInput::SetView(view) => self.podcast_view = view,
            PodcastsInput::Subscribe => self.open_subscribe_podcast_dialog(&sender),
            PodcastsInput::Search(term) => {
                let term = term.trim().to_string();
                if !term.is_empty() {
                    let _ = sender.output(PodcastsOutput::Toast(gettext("Searching …")));
                    sender.spawn_command(move |out| {
                        let results = match crate::core::podcast::search_podcasts(&term) {
                            Ok(r) => r,
                            Err(_) => {
                                let _ = out.send(PodcastsCmd::SearchFailed);
                                return;
                            }
                        };
                        // Show hits immediately (still without covers) …
                        let _ = out.send(PodcastsCmd::SearchResults(results.clone()));
                        // … and fetch the cover thumbnails afterwards in the background.
                        for r in &results {
                            if let Some(img) = r.image_url.as_deref() {
                                crate::core::online::cache_podcast_image(img);
                            }
                        }
                        let _ = out.send(PodcastsCmd::SearchCoversReady);
                    });
                }
            }
            PodcastsInput::SubscribeUrl(url) => {
                let url = url.trim().to_string();
                if !url.is_empty() {
                    let _ = sender.output(PodcastsOutput::Toast(gettext("Loading feed …")));
                    sender.spawn_command(move |out| {
                        let _ = out.send(PodcastsCmd::Fetched(fetch_and_store_podcast(&url)));
                    });
                }
            }
            PodcastsInput::Refresh(id) => {
                if let Ok(Some(url)) = self.library.podcast_feed_url(id) {
                    let _ = sender.output(PodcastsOutput::Toast(gettext("Updating feed …")));
                    sender.spawn_command(move |out| {
                        let _ = out.send(PodcastsCmd::Fetched(fetch_and_store_podcast(&url)));
                    });
                }
            }
            PodcastsInput::OpenPodcast(id) => {
                if let Some((_, title, _, _)) = self
                    .podcast_items
                    .iter()
                    .find(|(pid, _, _, _)| *pid == id)
                    .cloned()
                {
                    self.open_podcast(&sender, id, &title);
                }
            }
            PodcastsInput::OpenPodcastAt(index) => {
                if let Some(id) = self.podcast_items.get(index).map(|p| p.0) {
                    sender.input(PodcastsInput::OpenPodcast(id));
                }
            }
            PodcastsInput::ShowPodcastDetail(id) => self.open_podcast_detail(&sender, id),
            PodcastsInput::ShowPodcastDetailAt(index) => {
                if let Some(id) = self.podcast_items.get(index).map(|p| p.0) {
                    sender.input(PodcastsInput::ShowPodcastDetail(id));
                }
            }
            PodcastsInput::ShowEpisodeDetail(index) => self.open_episode_detail(&sender, index),
            PodcastsInput::ShowPodcastEpisodeDetail { podcast_id, index } => {
                self.open_podcast_episode_detail(&sender, podcast_id, index)
            }
            PodcastsInput::ToggleDownload { url, title } => {
                self.toggle_episode_download(&sender, url, title)
            }
            PodcastsInput::Delete(id) => self.confirm_remove(id, &sender),
            PodcastsInput::DeleteConfirmed(id) => {
                let _ = self.library.delete_podcast(id);
                self.reload_podcasts(&sender);
            }
        }
    }

    fn update_cmd(&mut self, cmd: PodcastsCmd, sender: ComponentSender<Self>, _root: &Self::Root) {
        match cmd {
            PodcastsCmd::Fetched(title) => {
                self.reload_podcasts(&sender);
                match title {
                    Some(t) => {
                        let _ = sender.output(PodcastsOutput::Toast(gettext_f(
                            "Subscribed: {t}",
                            &[("t", &t)],
                        )));
                    }
                    None => {
                        let _ =
                            sender.output(PodcastsOutput::Toast(gettext("Could not load feed")));
                    }
                }
            }
            PodcastsCmd::Downloaded { url, result } => {
                self.downloading_episodes.remove(&url);
                self.refresh_download_row();
                match result {
                    Ok(_) => {
                        let _ = sender.output(PodcastsOutput::Toast(gettext("Episode downloaded")));
                    }
                    Err(e) => {
                        tracing::warn!("Episode download failed: {e}");
                        let _ = sender.output(PodcastsOutput::Toast(gettext("Download failed")));
                    }
                }
            }
            PodcastsCmd::SearchResults(results) => {
                self.podcast_search_failed = false;
                self.podcast_search_results = results;
                self.rebuild_podcast_search_results(&sender);
            }
            PodcastsCmd::SearchFailed => {
                self.podcast_search_failed = true;
                self.podcast_search_results.clear();
                self.rebuild_podcast_search_results(&sender);
            }
            PodcastsCmd::SearchCoversReady => self.rebuild_podcast_search_results(&sender),
            PodcastsCmd::Refreshed => {
                let _ = sender.output(PodcastsOutput::RefreshFinished);
                self.reload_podcasts(&sender);
            }
            PodcastsCmd::CoversCached => self.reload_podcasts(&sender),
        }
    }
}

impl PodcastsPage {
    /// Show detail dialogs on the phone over the **full width** (bottom sheet);
    /// on the desktop floating as before (auto).
    fn adapt_detail_dialog(&self, dialog: &adw::Dialog) {
        if self.mobile {
            dialog.set_presentation_mode(adw::DialogPresentationMode::BottomSheet);
        }
    }

    /// Confirmation alert before removing a subscription. On confirm it asks the
    /// parent to show the undo toast (which defers the actual deletion back to
    /// us via [`PodcastsInput::DeleteConfirmed`]).
    fn confirm_remove(&self, id: i64, sender: &ComponentSender<Self>) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let confirm = adw::AlertDialog::new(Some(&gettext("Remove this podcast?")), None);
        confirm.add_response("cancel", &gettext("Cancel"));
        confirm.add_response("ok", &gettext("Remove"));
        confirm.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
        confirm.set_default_response(Some("cancel"));
        confirm.set_close_response("cancel");
        {
            let sender = sender.clone();
            confirm.connect_response(None, move |_, resp| {
                if resp == "ok" {
                    let _ = sender.output(PodcastsOutput::DeletedUndoToast(id));
                }
            });
        }
        confirm.present(Some(&root));
    }

    /// Rebuilds the overview of subscribed podcasts: cover, title, episode
    /// count. Tapping opens the episodes; **long press** opens the subscription
    /// detail view (refresh/remove). Afterwards also refreshes "Newest".
    fn reload_podcasts(&mut self, sender: &ComponentSender<Self>) {
        self.podcast_items = self.library.podcasts().unwrap_or_default();
        if self.gallery_view {
            self.fill_podcast_gallery(sender);
        } else {
            while let Some(child) = self.podcasts_list.first_child() {
                self.podcasts_list.remove(&child);
            }
            for (id, title, image, count) in self.podcast_items.clone() {
                // Episode count in parentheses on the heading, as with albums/songs.
                let row = adw::ActionRow::builder()
                    .title(format!("{} ({count})", gtk::glib::markup_escape_text(&title)).as_str())
                    .activatable(true)
                    .build();
                row.add_css_class("emilia-flush");
                let cover = image
                    .as_deref()
                    .and_then(crate::core::online::podcast_image_path);
                row.add_prefix(&cover_widget(cover.as_deref(), "microphone-symbolic"));
                {
                    let sender = sender.clone();
                    row.connect_activated(move |_| sender.input(PodcastsInput::OpenPodcast(id)));
                }
                // Long press (touch) / right click (mouse) → subscription detail view.
                on_secondary_click(&row, {
                    let sender = sender.clone();
                    move || sender.input(PodcastsInput::ShowPodcastDetail(id))
                });
                let lp = gtk::GestureLongPress::new();
                {
                    let sender = sender.clone();
                    lp.connect_pressed(move |g, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(PodcastsInput::ShowPodcastDetail(id));
                    });
                }
                row.add_controller(lp);
                self.podcasts_list.append(&row);
            }
        }
        self.reload_newest(sender);
    }

    /// Gallery variant of the podcast overview: cover grid; tap opens the
    /// episodes, long-press the subscription detail view.
    fn fill_podcast_gallery(&self, sender: &ComponentSender<Self>) {
        let fb = &self.podcasts_gallery;
        while let Some(c) = fb.first_child() {
            fb.remove(&c);
        }
        fb.set_min_children_per_line(1);
        fb.set_max_children_per_line(self.gallery_columns);
        fb.set_homogeneous(true);
        fb.set_row_spacing(8);
        fb.set_column_spacing(8);
        fb.set_selection_mode(gtk::SelectionMode::None);
        fb.set_activate_on_single_click(false);
        if !fb.has_css_class("emilia-gallery") {
            fb.add_css_class("emilia-gallery");
        }

        let mut to_decode: Vec<(String, gtk::Picture)> = Vec::new();
        for (i, (_, title, image, _)) in self.podcast_items.iter().enumerate() {
            let cover = image
                .as_deref()
                .and_then(crate::core::online::podcast_image_path);
            let (cell, pic) = gallery_cell(cover.as_deref(), "microphone-symbolic", title);
            if let (Some(path), Some(pic)) = (cover.as_deref(), pic) {
                if crate::ui::widgets::cached_thumb(path).is_none() {
                    to_decode.push((path.to_string(), pic));
                }
            }
            let click = gtk::GestureClick::new();
            {
                let sender = sender.clone();
                click.connect_released(move |g, n, _, _| {
                    if n == 1 {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(PodcastsInput::OpenPodcastAt(i));
                    }
                });
            }
            cell.add_controller(click);
            on_secondary_click(&cell, {
                let sender = sender.clone();
                move || sender.input(PodcastsInput::ShowPodcastDetailAt(i))
            });
            let long_press = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                long_press.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(PodcastsInput::ShowPodcastDetailAt(i));
                });
            }
            cell.add_controller(long_press);
            fb.append(&cell);
        }

        spawn_gallery_decode(to_decode);
        size_gallery_tiles_when_ready(fb);
        if !self.gallery_hooked.replace(true) {
            fb.connect_map(size_gallery_tiles_when_ready);
        }
    }

    /// Builds the "Newest" list: newest episodes across **all** subscriptions,
    /// chronologically by publication date. The **play button** streams the
    /// episode; **long press / right click** opens the entry detail view.
    fn reload_newest(&mut self, sender: &ComponentSender<Self>) {
        // Only show episodes from at most ~one month ago.
        let cutoff = crate::core::podcast::recent_cutoff_key();
        let mut eps: Vec<_> = self
            .library
            .all_episodes()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| crate::core::podcast::pubdate_key(e.published.as_deref()) >= cutoff)
            .collect();
        eps.sort_by(|a, b| {
            crate::core::podcast::pubdate_key(b.published.as_deref())
                .cmp(&crate::core::podcast::pubdate_key(a.published.as_deref()))
        });
        eps.truncate(150);
        self.newest_items = eps;
        while let Some(child) = self.newest_list.first_child() {
            self.newest_list.remove(&child);
        }

        // Sort by recency: Today / Yesterday / This week / This month.
        let (today, yesterday, week_start) = crate::core::podcast::recent_day_buckets();
        let bucket_of = |k: i64| -> usize {
            if k >= today {
                0
            } else if k >= yesterday {
                1
            } else if k >= week_start {
                2
            } else {
                3
            }
        };
        let bucket_title = |b: usize| match b {
            0 => gettext("Today"),
            1 => gettext("Yesterday"),
            2 => gettext("This week"),
            _ => gettext("This month"),
        };

        let mut cur_bucket: Option<usize> = None;
        let mut group: Option<adw::PreferencesGroup> = None;
        for (i, ep) in self.newest_items.iter().enumerate() {
            let b = bucket_of(crate::core::podcast::pubdate_key(ep.published.as_deref()));
            if cur_bucket != Some(b) {
                cur_bucket = Some(b);
                let g = adw::PreferencesGroup::builder()
                    .title(bucket_title(b))
                    .build();
                self.newest_list.append(&g);
                group = Some(g);
            }

            let mut subtitle = ep.podcast_title.clone();
            if let Some(p) = ep.published.as_deref().filter(|p| !p.trim().is_empty()) {
                subtitle.push_str(" · ");
                subtitle.push_str(&crate::core::podcast::pubdate_short(p));
            }
            // Not activatable: like a library track, the episode plays via its
            // play button; long press / right click opens the detail view.
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&ep.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .build();
            row.add_css_class("emilia-flush");
            let cover = ep
                .podcast_image
                .as_deref()
                .and_then(crate::core::online::podcast_image_path);
            row.add_prefix(&cover_widget(cover.as_deref(), "microphone-symbolic"));
            // Episode length as a subtle label, left of the play button.
            if let Some(d) = ep
                .duration
                .as_deref()
                .and_then(crate::core::podcast::format_duration)
            {
                let lbl = gtk::Label::new(Some(&d));
                lbl.set_valign(gtk::Align::Center);
                lbl.set_css_classes(&["dim-label", "numeric"]);
                row.add_suffix(&lbl);
            }
            row.add_suffix(&self.episode_play_button(sender, &ep.audio_url, &ep.title));
            on_secondary_click(&row, {
                let sender = sender.clone();
                move || sender.input(PodcastsInput::ShowEpisodeDetail(i))
            });
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(PodcastsInput::ShowEpisodeDetail(i));
                });
            }
            row.add_controller(lp);
            if let Some(g) = &group {
                g.add(&row);
            }
        }
        self.refresh_episode_icons();
    }

    /// Detail view of an entry (episode) from the "Newest" list.
    fn open_episode_detail(&self, sender: &ComponentSender<Self>, index: usize) {
        if let Some(ep) = self.newest_items.get(index).cloned() {
            self.show_episode_detail(sender, ep);
        }
    }

    /// Episode detail (incl. shownotes) of an episode from the episode list of
    /// an opened podcast (index = order in `episodes(id)`).
    fn open_podcast_episode_detail(
        &self,
        sender: &ComponentSender<Self>,
        podcast_id: i64,
        index: usize,
    ) {
        let Some(ep) = self
            .library
            .episodes(podcast_id)
            .unwrap_or_default()
            .into_iter()
            .nth(index)
        else {
            return;
        };
        let (podcast_title, podcast_image) = self
            .podcast_items
            .iter()
            .find(|(pid, _, _, _)| *pid == podcast_id)
            .map(|(_, t, img, _)| (t.clone(), img.clone()))
            .unwrap_or_default();
        self.show_episode_detail(
            sender,
            crate::model::EpisodeRef {
                podcast_title,
                podcast_image,
                title: ep.title,
                audio_url: ep.audio_url,
                published: ep.published,
                duration: ep.duration,
                description: ep.description,
            },
        );
    }

    /// Builds the episode detail dialog (shared by "Newest" and a podcast's
    /// episode list): podcast, date, duration, actions + shownotes.
    fn show_episode_detail(&self, sender: &ComponentSender<Self>, ep: crate::model::EpisodeRef) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let dialog = adw::Dialog::builder()
            .title(gtk::glib::markup_escape_text(&ep.title))
            .build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let info = adw::PreferencesGroup::new();
        let pod = adw::ActionRow::builder()
            .title(gettext("Podcast"))
            .subtitle(gtk::glib::markup_escape_text(&ep.podcast_title))
            .build();
        let cover = ep
            .podcast_image
            .as_deref()
            .and_then(crate::core::online::podcast_image_path);
        pod.add_prefix(&cover_widget(cover.as_deref(), "microphone-symbolic"));
        info.add(&pod);
        // Published and duration **side by side**, each about 50 % width.
        let pub_txt = ep
            .published
            .as_deref()
            .filter(|p| !p.trim().is_empty())
            .map(crate::core::podcast::pubdate_short);
        let dur_txt = ep
            .duration
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .map(|d| {
                crate::core::podcast::format_duration(d).unwrap_or_else(|| d.trim().to_string())
            });
        let meta = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .homogeneous(true)
            .spacing(12)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(14)
            .margin_end(14)
            .build();
        let cell = |title: &str, value: &str| {
            let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
            b.append(
                &gtk::Label::builder()
                    .label(title)
                    .xalign(0.0)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
            b.append(
                &gtk::Label::builder()
                    .label(value)
                    .xalign(0.0)
                    .wrap(true)
                    .build(),
            );
            b
        };
        if let Some(p) = &pub_txt {
            meta.append(&cell(&gettext("Published"), p));
        }
        if let Some(d) = &dur_txt {
            meta.append(&cell(&gettext("Duration"), d));
        }
        // Download column: "Download" heading over a tappable value label.
        let dl_cell = gtk::Box::new(gtk::Orientation::Vertical, 2);
        dl_cell.append(
            &gtk::Label::builder()
                .label(gettext("Download"))
                .xalign(0.0)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        let dl_value = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["accent"])
            .build();
        dl_cell.append(&dl_value);
        dl_cell.set_cursor_from_name(Some("pointer"));
        {
            let (sender, url, title) = (sender.clone(), ep.audio_url.clone(), ep.title.clone());
            let click = gtk::GestureClick::new();
            click.connect_released(move |g, _, _, _| {
                g.set_state(gtk::EventSequenceState::Claimed);
                sender.input(PodcastsInput::ToggleDownload {
                    url: url.clone(),
                    title: title.clone(),
                });
            });
            dl_cell.add_controller(click);
        }
        meta.append(&dl_cell);
        info.add(&meta);
        content.append(&info);

        *self.ctx_episode_download.borrow_mut() = Some((dl_value, ep.audio_url.clone()));
        self.refresh_download_row();

        // Shownotes (if present): timestamps become clickable jump markers.
        if let Some(notes) = ep.description.as_deref().filter(|s| !s.trim().is_empty()) {
            let notes_group = adw::PreferencesGroup::new();
            // Always wrap, including inside long unbreakable tokens (URLs), so a
            // shownote can never force the dialog wider than the screen.
            let label = gtk::Label::builder()
                .label(crate::core::podcast::linkify_timestamps(notes.trim()))
                .use_markup(true)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .selectable(true)
                .build();
            label.add_css_class("body");
            {
                let sender = sender.clone();
                let url = ep.audio_url.clone();
                let title = ep.title.clone();
                label.connect_activate_link(move |_, uri| {
                    if let Some(ms) = uri
                        .strip_prefix("emilia-seek:")
                        .and_then(|s| s.parse::<i64>().ok())
                    {
                        let _ = sender.output(PodcastsOutput::EpisodeSeekTo {
                            url: url.clone(),
                            title: title.clone(),
                            ms,
                        });
                        return gtk::glib::Propagation::Stop;
                    }
                    gtk::glib::Propagation::Proceed
                });
            }
            let wrap = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(6)
                .margin_top(10)
                .margin_bottom(10)
                .margin_start(14)
                .margin_end(14)
                .build();
            let heading = gtk::Label::builder()
                .label(gettext("Shownotes"))
                .xalign(0.0)
                .css_classes(["heading"])
                .build();
            wrap.append(&heading);
            wrap.append(&label);
            notes_group.add(&wrap);
            content.append(&notes_group);
        }

        present_detail(&dialog, &content, &root);
    }

    /// Detail view/management of a subscription: cover, episode count, and
    /// actions to open, refresh, and remove (with confirmation).
    fn open_podcast_detail(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let Some((_, title, image, count)) = self
            .podcast_items
            .iter()
            .find(|(p, _, _, _)| *p == id)
            .cloned()
        else {
            return;
        };
        let dialog = adw::Dialog::builder()
            .title(gtk::glib::markup_escape_text(&title))
            .build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let info = adw::PreferencesGroup::new();
        let head = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&title))
            .subtitle(ngettext_n("{n} episode", "{n} episodes", count as u32))
            .build();
        let cover = image
            .as_deref()
            .and_then(crate::core::online::podcast_image_path);
        head.add_prefix(&cover_widget(cover.as_deref(), "microphone-symbolic"));
        info.add(&head);
        content.append(&info);

        let actions = adw::PreferencesGroup::new();
        let open = action_row(&gettext("Open episodes"), "go-next-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            open.connect_activated(move |_| {
                sender.input(PodcastsInput::OpenPodcast(id));
                dialog.close();
            });
        }
        actions.add(&open);
        let refresh = action_row(&gettext("Refresh feed"), "view-refresh-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            refresh.connect_activated(move |_| {
                sender.input(PodcastsInput::Refresh(id));
                dialog.close();
            });
        }
        actions.add(&refresh);
        // Share the podcast (feed + episodes incl. show notes) over device sync.
        if let Some(feed) = self.library.podcast_feed_url(id).ok().flatten() {
            let share = action_row(&gettext("Share"), "emilia-share-symbolic");
            let (sender, dialog) = (sender.clone(), dialog.clone());
            share.connect_activated(move |_| {
                let _ = sender.output(PodcastsOutput::Share(Box::new(
                    crate::core::sync::share::Selection {
                        podcast_feeds: vec![feed.clone()],
                        ..Default::default()
                    },
                )));
                dialog.close();
            });
            actions.add(&share);
        }
        let remove = action_row(&gettext("Remove podcast"), "user-trash-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            remove.connect_activated(move |_| {
                dialog.close();
                sender.input(PodcastsInput::Delete(id));
            });
        }
        actions.add(&remove);
        content.append(&actions);

        present_detail(&dialog, &content, &root);
    }

    /// Episode subpage of a podcast (play button = stream episode, long press =
    /// detail view).
    fn open_podcast(&self, sender: &ComponentSender<Self>, id: i64, title: &str) {
        let episodes = self.library.episodes(id).unwrap_or_default();
        let cover = self
            .podcast_items
            .iter()
            .find(|(pid, _, _, _)| *pid == id)
            .and_then(|(_, _, img, _)| img.as_deref())
            .and_then(crate::core::online::podcast_image_path);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        let group = adw::PreferencesGroup::builder()
            .title(
                format!(
                    "{} ({})",
                    gtk::glib::markup_escape_text(title),
                    episodes.len()
                )
                .as_str(),
            )
            .build();

        if episodes.is_empty() {
            group.add(
                &adw::ActionRow::builder()
                    .title(gettext("No episodes"))
                    .build(),
            );
        }
        for (i, ep) in episodes.iter().enumerate() {
            let mut subtitle = String::new();
            if let Some(p) = &ep.published {
                subtitle.push_str(p.trim());
            }
            if let Some(d) = &ep.duration {
                if !subtitle.is_empty() {
                    subtitle.push_str(" · ");
                }
                subtitle.push_str(d.trim());
            }
            // Not activatable: like a library track, the episode plays via its
            // play button; long press / right click opens the detail view.
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&ep.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .build();
            row.add_css_class("emilia-flush");
            row.add_prefix(&cover_widget(cover.as_deref(), "microphone-symbolic"));
            row.add_suffix(&self.episode_play_button(sender, &ep.audio_url, &ep.title));
            on_secondary_click(&row, {
                let sender = sender.clone();
                move || {
                    sender.input(PodcastsInput::ShowPodcastEpisodeDetail {
                        podcast_id: id,
                        index: i,
                    });
                }
            });
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(PodcastsInput::ShowPodcastEpisodeDetail {
                        podcast_id: id,
                        index: i,
                    });
                });
            }
            row.add_controller(lp);
            group.add(&row);
        }
        content.append(&group);
        // Park the built page and ask the parent to push it. The play/pause
        // icons are refreshed only *after* the parent has mounted the subpage
        // (it echoes `PlaybackStateChanged` back), because `refresh_episode_icons`
        // drops rows whose widgets aren't realized yet.
        *self.subpage_slot.borrow_mut() =
            Some((gettext_f("Podcast – {title}", &[("title", title)]), content));
        let _ = sender.output(PodcastsOutput::PushSubpage);
    }

    /// Dialog for subscribing: a **search** (iTunes directory) at the top and a
    /// field for the **feed address** (RSS) below as the manual route.
    fn open_subscribe_podcast_dialog(&self, sender: &ComponentSender<Self>) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let dialog = adw::Dialog::builder()
            .title(gettext("Subscribe to podcast"))
            .build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        // --- Search (iTunes directory) ---
        let search_group = adw::PreferencesGroup::builder()
            .title(gettext("Search"))
            .description(gettext("Find a podcast by name"))
            .build();
        let search_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text(gettext("Podcast name …"))
            .hexpand(true)
            .build();
        crate::ui::widgets::no_autofocus(&search_entry);
        let search_btn = gtk::Button::builder().label(gettext("Search")).build();
        search_btn.add_css_class("suggested-action");
        search_row.append(&search_entry);
        search_row.append(&search_btn);
        search_group.add(&search_row);
        content.append(&search_group);

        {
            let (sender, entry) = (sender.clone(), search_entry.clone());
            search_entry.connect_activate(move |_| {
                let term = entry.text().to_string();
                if !term.trim().is_empty() {
                    sender.input(PodcastsInput::Search(term));
                }
            });
        }
        {
            let (sender, entry) = (sender.clone(), search_entry.clone());
            search_btn.connect_clicked(move |_| {
                let term = entry.text().to_string();
                if !term.trim().is_empty() {
                    sender.input(PodcastsInput::Search(term));
                }
            });
        }

        // Results list – initially empty/hidden, filled by `rebuild_*`.
        let results = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        results.add_css_class("boxed-list");
        results.set_visible(false);
        content.append(&results);

        // --- Manual: feed address (RSS) ---
        let url_group = adw::PreferencesGroup::builder()
            .title(gettext("Or enter feed address"))
            .build();
        let url_entry = adw::EntryRow::builder()
            .title(gettext("Feed address (RSS)"))
            .show_apply_button(true)
            .build();
        crate::ui::widgets::no_autofocus(&url_entry);
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            url_entry.connect_apply(move |e| {
                let url = e.text().to_string();
                if !url.trim().is_empty() {
                    sender.input(PodcastsInput::SubscribeUrl(url));
                    dialog.close();
                }
            });
        }
        url_group.add(&url_entry);
        content.append(&url_group);

        *self.podcast_search.borrow_mut() = Some((dialog.clone(), results.clone()));
        {
            let slot = self.podcast_search.clone();
            dialog.connect_closed(move |_| {
                *slot.borrow_mut() = None;
            });
        }

        present_detail(&dialog, &content, &root);
    }

    /// Redraws the results list in the open subscription search dialog.
    fn rebuild_podcast_search_results(&self, sender: &ComponentSender<Self>) {
        let guard = self.podcast_search.borrow();
        let Some((dialog, list)) = guard.as_ref() else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        list.set_visible(true);

        if self.podcast_search_results.is_empty() {
            let row = if self.podcast_search_failed {
                let r = adw::ActionRow::builder()
                    .title(gettext("Search service unreachable"))
                    .subtitle(gettext("Check your connection and try again"))
                    .build();
                r.set_subtitle_lines(2);
                r
            } else {
                adw::ActionRow::builder()
                    .title(gettext("No podcasts found"))
                    .build()
            };
            row.set_sensitive(false);
            list.append(&row);
            dialog.set_content_height(300);
            return;
        }

        let rows = self.podcast_search_results.len() as i32;
        dialog.set_content_height((320 + rows * 66).min(760));

        for r in &self.podcast_search_results {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&r.title))
                .activatable(true)
                .build();
            if let Some(a) = r.author.as_deref().filter(|a| !a.trim().is_empty()) {
                row.set_subtitle(&gtk::glib::markup_escape_text(a));
            }
            let cover = r
                .image_url
                .as_deref()
                .and_then(crate::core::online::podcast_image_path);
            row.add_prefix(&cover_widget(cover.as_deref(), "microphone-symbolic"));
            row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
            {
                let (sender, dialog, feed) = (sender.clone(), dialog.clone(), r.feed_url.clone());
                row.connect_activated(move |_| {
                    sender.input(PodcastsInput::SubscribeUrl(feed.clone()));
                    dialog.close();
                });
            }
            list.append(&row);
        }
    }

    /// Play/Pause button (suffix) for an entry row: tap = toggle episode.
    fn episode_play_button(
        &self,
        sender: &ComponentSender<Self>,
        url: &str,
        title: &str,
    ) -> gtk::Button {
        let btn = gtk::Button::builder()
            .icon_name("media-playback-start-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text(gettext("Play/Pause"))
            .build();
        btn.add_css_class("flat");
        {
            let (sender, url, title) = (sender.clone(), url.to_string(), title.to_string());
            btn.connect_clicked(move |_| {
                let _ = sender.output(PodcastsOutput::ToggleEpisode {
                    url: url.clone(),
                    title: title.clone(),
                });
            });
        }
        self.episode_play_buttons
            .borrow_mut()
            .push((url.to_string(), btn.clone()));
        btn
    }

    /// Updates the Play/Pause icons of all visible entry rows and the "Play" row
    /// of an open detail dialog. Detached rows are discarded in the process.
    fn refresh_episode_icons(&self) {
        let active = self.playing_url.clone();
        let playing = self.playing;
        let is_active = |url: &str| playing && active.as_deref() == Some(url);
        {
            let mut buttons = self.episode_play_buttons.borrow_mut();
            buttons.retain(|(_, btn)| btn.root().is_some());
            for (url, btn) in buttons.iter() {
                btn.set_icon_name(if is_active(url) {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                });
            }
        }
        if let Some((row, url)) = self.ctx_episode_play.borrow().as_ref() {
            row.set_visible(!is_active(url));
        }
    }

    /// Updates the download row of an open episode detail dialog to reflect the
    /// offline state of its episode.
    fn refresh_download_row(&self) {
        let guard = self.ctx_episode_download.borrow();
        let Some((label, url)) = guard.as_ref() else {
            return;
        };
        let downloading = self.downloading_episodes.contains(url);
        let downloaded =
            !downloading && self.library.episode_download(url).ok().flatten().is_some();
        if downloading {
            label.set_label(&gettext("Downloading …"));
        } else if downloaded {
            label.set_label(&gettext("Remove download"));
        } else {
            label.set_label(&gettext("For offline listening"));
        }
    }

    /// Download the episode for offline playback, or delete an existing copy.
    fn toggle_episode_download(
        &mut self,
        sender: &ComponentSender<Self>,
        url: String,
        title: String,
    ) {
        if self.downloading_episodes.contains(&url) {
            return;
        }
        if let Some(path) = self.library.delete_episode_download(&url).unwrap_or(None) {
            let _ = std::fs::remove_file(&path);
            self.refresh_download_row();
            let _ = sender.output(PodcastsOutput::Toast(gettext("Download removed")));
            return;
        }
        self.downloading_episodes.insert(url.clone());
        self.refresh_download_row();
        let _ = sender.output(PodcastsOutput::Toast(gettext_f(
            "Downloading “{title}” …",
            &[("title", &title)],
        )));
        let dl_url = url.clone();
        sender.spawn_command(move |out| {
            let dest = crate::core::online::episode_download_dest(&dl_url);
            let result = match crate::core::podcast::download_episode(&dl_url, &dest) {
                Ok(_) => {
                    let path = dest.to_string_lossy().into_owned();
                    if let Ok(lib) = Library::open() {
                        let _ = lib.set_episode_download(&dl_url, &path);
                    }
                    Ok(path)
                }
                Err(e) => Err(e.to_string()),
            };
            let _ = out.send(PodcastsCmd::Downloaded {
                url: dl_url.clone(),
                result,
            });
        });
    }
}
