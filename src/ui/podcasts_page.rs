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

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::ui::app::{PodcastView, SortCrit};

/// How many feeds "Refresh all" fetches at once (plain HTTP, cheap).
const FEED_REFRESH_THREADS: usize = 8;

/// Fetches a feed and stores podcast + episodes (runs in the worker thread,
/// its own DB connection). Returns the podcast title on success, plus how many
/// of the fetched episodes were **new** — so a refresh can report what it
/// actually brought in instead of leaving the user guessing.
pub(crate) fn fetch_and_store_podcast(feed_url: &str) -> Option<(String, usize)> {
    let lib = Library::open().ok()?;
    crate::core::podcast::subscribe_feed(&lib, feed_url)
        .ok()
        .map(|(_, title, fresh)| (title, fresh))
}

/// Fetches the feed images not yet in the cache (worker thread — network).
/// Returns whether any came in, i.e. whether a redraw would show something new.
fn cache_missing_feed_images() -> bool {
    let Ok(lib) = Library::open() else {
        return false;
    };
    lib.podcasts()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, _, image, _)| image)
        .filter(|url| crate::core::online::podcast_image_path(url).is_none())
        .filter(|url| crate::core::online::cache_podcast_image(url).is_some())
        .count()
        > 0
}

/// A listening-progress line of a list row, kept so the transport tick can
/// refresh it in place instead of rebuilding the whole list.
pub(super) struct EpisodeRow {
    /// Audio URL of the episode the line belongs to.
    pub(super) url: String,
    /// The line itself (emptied and refilled on every update).
    pub(super) row: gtk::Box,
    /// Episode length from the feed, if it states one.
    pub(super) total_secs: Option<i64>,
}

/// One-line outcome of a "refresh all", shown briefly in the loading overlay:
/// how many feeds were updated, what came in, and what failed.
fn refresh_summary_text(updated: usize, failed: usize, new_episodes: usize) -> String {
    let mut parts = Vec::new();
    if updated > 0 {
        parts.push(ngettext_n(
            "{n} podcast updated",
            "{n} podcasts updated",
            updated as u32,
        ));
    }
    if new_episodes > 0 {
        parts.push(ngettext_n(
            "{n} new episode",
            "{n} new episodes",
            new_episodes as u32,
        ));
    }
    if failed > 0 {
        parts.push(ngettext_n(
            "{n} feed failed",
            "{n} feeds failed",
            failed as u32,
        ));
    }
    if parts.is_empty() {
        return gettext("Nothing new");
    }
    parts.join(" · ")
}

/// Live state of one running episode download. `started`/`done` give the
/// average transfer rate, from which the remaining time is estimated — the
/// average (instead of the momentary rate) keeps the readout from jittering.
#[derive(Debug)]
pub(super) struct EpisodeDownload {
    started: std::time::Instant,
    /// Bytes written so far (last report from the worker).
    done: u64,
    /// Total size, if the server advertised a `Content-Length`.
    total: Option<u64>,
}

impl EpisodeDownload {
    pub(super) fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            done: 0,
            total: None,
        }
    }

    /// Completed share of the transfer (`None` while the total size is unknown).
    pub(super) fn fraction(&self) -> Option<f64> {
        let total = self.total.filter(|t| *t > 0)?;
        Some((self.done as f64 / total as f64).clamp(0.0, 1.0))
    }

    /// Estimated remaining time in milliseconds, from the average rate since the
    /// start. `None` until enough has been transferred for the estimate to mean
    /// anything (unknown total, or barely started).
    fn remaining_ms(&self) -> Option<i64> {
        let total = self.total.filter(|t| *t > self.done)?;
        let elapsed = self.started.elapsed().as_secs_f64();
        if self.done < 64 * 1024 || elapsed < 1.0 {
            return None;
        }
        let rate = self.done as f64 / elapsed; // bytes per second
        if rate <= 0.0 {
            return None;
        }
        // At least a second: "0:00 left" would read like it is already done.
        Some(((((total - self.done) as f64 / rate) * 1000.0) as i64).max(1000))
    }

    /// The one-line readout under the "Download" heading while the transfer runs:
    /// "45 % · 1:20 left", falling back to just the percentage (or the downloaded
    /// size when the server states no total).
    pub(super) fn status_text(&self) -> String {
        let Some(frac) = self.fraction() else {
            // Nothing transferred yet (connecting, or the server states no
            // length): a plain "Downloading …" until there is a real number.
            if self.done == 0 {
                return gettext("Downloading …");
            }
            return gettext_f(
                "Downloading … {size}",
                &[("size", &crate::core::sync::share::human_size(self.done))],
            );
        };
        let pct = (frac * 100.0).round() as u32;
        match self.remaining_ms() {
            Some(ms) => gettext_f(
                "{pct} % · {time} left",
                &[
                    ("pct", &pct.to_string()),
                    ("time", &crate::ui::app_helpers::fmt_duration(ms)),
                ],
            ),
            None => gettext_f("{pct} % downloaded", &[("pct", &pct.to_string())]),
        }
    }
}

/// The podcasts page component.
pub(crate) struct PodcastsPage {
    /// Own DB connection (WAL + per-thread, the project's established pattern).
    pub(super) library: Library,
    /// Window the dialogs are presented on (set on `SetWindow`).
    pub(super) window: Option<adw::ApplicationWindow>,
    /// Mirror of the transport's `playing_episode_url` (for the row icons).
    pub(super) playing_url: Option<String>,
    /// Mirror of the transport's play/pause state.
    pub(super) playing: bool,
    /// Gallery vs. list overview (mirror of the global `gallery_view` setting).
    pub(super) gallery_view: bool,
    /// Gallery columns (mirror of the global setting).
    pub(super) gallery_columns: u32,
    /// Narrow (mobile) layout → detail dialogs as bottom sheets.
    pub(super) mobile: bool,
    /// (id, title, image URL, episode count) per podcast.
    pub(super) podcast_items: Vec<(i64, String, Option<String>, i64)>,
    pub(super) podcasts_list: gtk::ListBox,
    /// Gallery variant of the podcast overview (cover grid).
    pub(super) podcasts_gallery: gtk::FlowBox,
    /// Which podcast view is visible: newest episodes or subscription overview.
    pub(super) podcast_view: PodcastView,
    /// Sort of the subscription overview (criterion + descending). Persisted as
    /// "sort_podcasts" / "sort_podcasts_desc". The "Newest" view is date-bucketed
    /// and not affected.
    pub(super) overview_sort: (SortCrit, bool),
    /// "Without grouping" for the overview list (no alphabetical headings).
    /// Persisted as "nogroup_podcasts".
    pub(super) overview_no_group: bool,
    /// Per-view gallery override (sort popover); `None` follows the global
    /// `gallery_view`. Persisted as "gallery_podcasts".
    pub(super) gallery_override: Option<bool>,
    /// "Show description" (sort popover): gallery tiles framed with their title
    /// instead of the bare cover. Persisted as "gallery_desc_podcasts";
    /// default off — podcast artwork already carries the show's name.
    pub(super) gallery_desc: bool,
    /// Per-row alphabetical headings of the overview list (name sort).
    pub(super) overview_headers: Rc<RefCell<Option<Vec<String>>>>,
    /// Hand-off for the shared title-bar sort button: [`Self::rebuild_sort`]
    /// writes the popover + direction here (or `None` to hide it), then signals
    /// the parent via [`PodcastsOutput::SortChanged`].
    pub(super) sort_slot: crate::ui::app_sort::SortSlot,
    /// Newest episodes across all subscriptions (for the "Newest" view).
    pub(super) newest_items: Vec<crate::model::EpisodeRef>,
    /// Container of the "Newest" list (filled imperatively in `reload_newest`).
    pub(super) newest_list: gtk::Box,
    /// Recently (partly) heard episodes (for the "Recently" view).
    pub(super) recent_items: Vec<crate::model::RecentEpisode>,
    /// Container of the "Recently" list (filled imperatively in `reload_recent`).
    pub(super) recent_list: gtk::Box,
    /// Hits of the last podcast search (iTunes), for the subscribe dialog.
    pub(super) podcast_search_results: Vec<crate::core::podcast::PodcastSearchResult>,
    /// The last podcast search hit a network/service error (vs. no hits).
    pub(super) podcast_search_failed: bool,
    /// While the subscribe search dialog is open: (dialog, hit list).
    pub(super) podcast_search: Rc<RefCell<Option<(adw::Dialog, gtk::ListBox)>>>,
    /// Play/pause buttons of the visible episode rows (audio URL → button).
    /// Play/pause controls of the episode rows, keyed by audio URL.
    pub(super) episode_marks: crate::ui::play_mark::Marks,
    /// Listening-progress lines of the visible rows, so the per-second transport
    /// tick can update the running episode's bar in place (rebuilding the list
    /// on every tick would be far too expensive — and made the progress look
    /// frozen until the user switched tabs).
    pub(super) episode_progress_rows: Rc<RefCell<Vec<EpisodeRow>>>,
    /// "Play" row of an open episode detail dialog (row, audio URL).
    pub(super) ctx_episode_play: Rc<RefCell<Option<(adw::ActionRow, String)>>>,
    /// "Download" column of an open episode detail dialog (value label, progress
    /// bar, audio URL).
    pub(super) ctx_episode_download: Rc<RefCell<Option<(gtk::Label, gtk::ProgressBar, String)>>>,
    /// Episodes whose download is currently running (audio URL → live progress).
    pub(super) downloading_episodes: HashMap<String, EpisodeDownload>,
    /// Hand-off slot for a built episode subpage. The parent owns the shared
    /// NavigationView; since its `Msg` must be `Send` it cannot carry the
    /// (`!Send`) `gtk::Box` through a message, so we park the built page here and
    /// only signal `PushSubpage` (a unit) — the parent then pushes it.
    pub(super) subpage_slot: Rc<RefCell<Option<(String, gtk::Box)>>>,
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
    /// Per-second position of the running episode (from the transport): update
    /// the progress line of every visible row of that episode in place.
    EpisodeProgressTick {
        url: String,
        position_ms: i64,
        duration_ms: i64,
    },
    /// The episode's resume point was written to the DB (5 s timer) — used to
    /// pull a freshly started episode into the "Recently" list, which only
    /// lists episodes that already have a stored position.
    EpisodeProgressPersisted {
        url: String,
    },
    /// The episode played to its end → show it as "Listened" right away.
    EpisodeFinished {
        url: String,
    },
    SetGalleryView(bool),
    SetGalleryColumns(u32),
    SetMobile(bool),
    SetWindow(adw::ApplicationWindow),
    // --- view-internal (from the page's own rows/dialogs) ---
    SetView(PodcastView),
    /// Change the overview sort (criterion + descending), from the header popover.
    SetSort(SortCrit, bool),
    /// Toggle alphabetical grouping of the overview list (`true` = no grouping).
    SetNoGroup(bool),
    /// Per-view gallery override for the overview (sort popover toggle).
    SetGallery(bool),
    /// Toggle the gallery tiles' title ("Show description").
    SetGalleryDesc(bool),
    Subscribe,
    Search(String),
    SubscribeUrl(String),
    OpenPodcast(i64),
    OpenPodcastAt(usize),
    ShowPodcastDetail(i64),
    ShowPodcastDetailAt(usize),
    ShowEpisodeDetail(usize),
    /// Detail views' refresh: fetch the feed (episodes, shownotes) and the
    /// cover again, then reopen the podcast's / the episode's (audio URL)
    /// detail view.
    RefreshPodcastDetail(i64),
    RefreshEpisodeDetail(String),
    ShowPodcastEpisodeDetail {
        podcast_id: i64,
        index: usize,
    },
    /// Episode detail resolved from the episode's audio URL — used when the
    /// now-playing track is a podcast started from a playlist (no podcast id /
    /// index at hand).
    ShowEpisodeDetailByUrl {
        url: String,
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
    /// Open the equalizer editor (a parent dialog) for a subscription
    /// (per-podcast EQ, inherited by its episodes).
    OpenPodcastEqualizer(i64),
    /// Open the equalizer editor (a parent dialog) for one episode
    /// (per-episode EQ, keyed by its audio URL).
    OpenEpisodeEqualizer { url: String, title: String },
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
    /// Live progress of the running "refresh all" (feed `done` of `total`, name
    /// of the feed being fetched) for the loading overlay's progress bar.
    RefreshProgress {
        done: usize,
        total: usize,
        label: String,
    },
    /// Outcome of a refresh, shown briefly in the overlay — the only feedback
    /// channel left, since informational toasts are disabled app-wide.
    RefreshSummary(String),
    /// The sort slot was rebuilt → the parent refreshes the shared title-bar
    /// sort button (if the Podcasts section is showing).
    SortChanged,
}

#[derive(Debug)]
pub(crate) enum PodcastsCmd {
    /// A detail refresh finished (`ok` = the feed loaded): reopen the podcast
    /// detail, or the episode's when `episode` (audio URL) is set.
    DetailRefreshed {
        podcast_id: i64,
        episode: Option<String>,
        ok: bool,
    },
    /// Feed fetch finished (subscribe/refresh): `Some(title)` on success.
    Fetched(Option<String>),
    /// Episode download finished.
    Downloaded {
        url: String,
        result: Result<String, String>,
    },
    /// Progress of a running episode download (throttled by the worker), so the
    /// detail dialog can show percent and the remaining time.
    DownloadProgress {
        url: String,
        done: u64,
        total: Option<u64>,
    },
    /// Search hits (still without covers).
    SearchResults(Vec<crate::core::podcast::PodcastSearchResult>),
    /// Search failed (service unreachable).
    SearchFailed,
    /// Search-hit covers cached → redraw the hit list.
    SearchCoversReady,
    /// One feed of a "refresh all" is about to be fetched.
    RefreshProgress {
        done: usize,
        total: usize,
        title: String,
    },
    /// All feeds (refresh-all) re-fetched, with what it brought in.
    Refreshed {
        updated: usize,
        failed: usize,
        new_episodes: usize,
    },
    /// Startup feed-image cache finished; `true` if it brought in an image
    /// that was missing → redraw the overview (it was built from the cache).
    CoversCached(bool),
}

#[relm4::component(pub(crate))]
impl Component for PodcastsPage {
    type Init = (
        Rc<RefCell<Option<(String, gtk::Box)>>>,
        crate::ui::app_sort::SortSlot,
    );
    type Input = PodcastsInput;
    type Output = PodcastsOutput;
    type CommandOutput = PodcastsCmd;

    view! {
        #[root]
        gtk::Box {
            set_orientation: gtk::Orientation::Vertical,

            // Header: linked tab switcher "Newest" / "Subscribed" and "+".
            gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 6,
                set_margin_top: 2,
                set_margin_bottom: 4,
                set_margin_start: 12,
                set_margin_end: 12,
                add_css_class: "linked",
                add_css_class: "emilia-tabbar",

                gtk::ToggleButton {
                    set_label: &gettext("Recently"),
                    set_hexpand: true,
                    #[watch]
                    set_active: model.podcast_view == PodcastView::Recent,
                    connect_clicked => PodcastsInput::SetView(PodcastView::Recent),
                },
                gtk::ToggleButton {
                    set_label: &gettext("Newest"),
                    set_hexpand: true,
                    #[watch]
                    set_active: model.podcast_view == PodcastView::Newest,
                    connect_clicked => PodcastsInput::SetView(PodcastView::Newest),
                },
                gtk::ToggleButton {
                    set_label: &gettext("Subscribed"),
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

            // "Recently": recently (partly) heard episodes, with progress.
            gtk::ScrolledWindow {
                set_vexpand: true,
                #[watch]
                set_visible: model.podcast_view == PodcastView::Recent && !model.recent_items.is_empty(),
                #[local_ref]
                recent_list -> gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 6,
                    set_valign: gtk::Align::Start,
                    set_margin_top: 10,
                    set_margin_bottom: 12,
                    set_margin_start: 12,
                    set_margin_end: 12,
                },
            },
            adw::StatusPage {
                set_icon_name: Some("podcast-symbolic"),
                set_title: &gettext("Nothing heard yet"),
                set_description: Some(&gettext("Episodes you have started appear here, showing how far you have listened.")),
                set_vexpand: true,
                #[watch]
                set_visible: model.podcast_view == PodcastView::Recent && model.recent_items.is_empty(),
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
                set_visible: model.podcast_view == PodcastView::Overview && !model.podcast_items.is_empty() && !model.gallery_on(),
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
                set_visible: model.podcast_view == PodcastView::Overview && !model.podcast_items.is_empty() && model.gallery_on(),
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
        (subpage_slot, sort_slot): Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // A failed second connection must not crash the whole app; degrade to a
        // temporary in-memory DB (logged) instead of panicking the UI thread.
        let library = Library::open_or_memory();
        let podcasts_list = gtk::ListBox::new();
        let newest_list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let recent_list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let podcasts_gallery = gtk::FlowBox::new();
        // Restore the persisted overview sort (default: by name, ascending) + the
        // grouping/gallery choices.
        let overview_sort =
            crate::ui::app_sort::read_sort(&library, "podcasts", SortCrit::Name, false);
        let overview_no_group = matches!(
            library
                .get_setting("nogroup_podcasts")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        let gallery_override = match library
            .get_setting("gallery_podcasts")
            .ok()
            .flatten()
            .as_deref()
        {
            Some("1") => Some(true),
            Some("0") => Some(false),
            _ => None,
        };
        let gallery_desc = library
            .get_setting("gallery_desc_podcasts")
            .ok()
            .flatten()
            .as_deref()
            == Some("1");
        let overview_headers = Rc::new(RefCell::new(None));
        podcasts_list.set_header_func(crate::ui::app_gallery::list_section_header_func(
            overview_headers.clone(),
        ));
        let mut model = PodcastsPage {
            library,
            window: None,
            playing_url: None,
            playing: false,
            gallery_view: false,
            gallery_columns: 4,
            mobile: false,
            podcast_items: Vec::new(),
            podcasts_list: podcasts_list.clone(),
            podcasts_gallery: podcasts_gallery.clone(),
            podcast_view: PodcastView::Newest,
            newest_items: Vec::new(),
            newest_list: newest_list.clone(),
            recent_items: Vec::new(),
            recent_list: recent_list.clone(),
            overview_sort,
            overview_no_group,
            gallery_override,
            gallery_desc,
            overview_headers,
            sort_slot,
            podcast_search_results: Vec::new(),
            podcast_search_failed: false,
            podcast_search: Rc::new(RefCell::new(None)),
            episode_marks: Default::default(),
            episode_progress_rows: Rc::new(RefCell::new(Vec::new())),
            ctx_episode_play: Rc::new(RefCell::new(None)),
            ctx_episode_download: Rc::new(RefCell::new(None)),
            downloading_episodes: HashMap::new(),
            subpage_slot,
        };
        // Fetch the feed images still missing from the cache in the background;
        // the overview is rebuilt only if one came in (no UI block at startup).
        sender.spawn_oneshot_command(|| PodcastsCmd::CoversCached(cache_missing_feed_images()));
        let widgets = view_output!();
        // Show the overview right away from the disk-cached images (which also
        // builds the header sort popover for the restored sort). Waiting for
        // the fetch above instead left the page empty for as long as a dead
        // image host took to time out.
        model.reload_podcasts(&sender);
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: PodcastsInput, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            PodcastsInput::Reload => self.reload_podcasts(&sender),
            PodcastsInput::RefreshAll => self.refresh_all_feeds(&sender),
            PodcastsInput::PlaybackStateChanged {
                playing_url,
                playing,
            } => {
                // A different episode than before means the previous one now has
                // a stored position: "Recently" has to be rebuilt for it to show
                // up (and in the right order).
                let switched = playing_url.is_some() && playing_url != self.playing_url;
                self.playing_url = playing_url;
                self.playing = playing;
                self.refresh_episode_icons();
                if switched {
                    self.reload_recent(&sender);
                }
            }
            PodcastsInput::EpisodeProgressTick {
                url,
                position_ms,
                duration_ms,
            } => self.apply_episode_progress(&url, position_ms, duration_ms, false),
            PodcastsInput::EpisodeProgressPersisted { url } => {
                // Only worth a rebuild while the episode is still missing from
                // "Recently" — afterwards the tick keeps its row current.
                if !self.recent_items.iter().any(|e| e.audio_url == url) {
                    self.reload_recent(&sender);
                }
            }
            PodcastsInput::EpisodeFinished { url } => {
                self.apply_episode_progress(&url, 0, 0, true);
                self.reload_recent(&sender);
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
            PodcastsInput::SetView(view) => {
                self.podcast_view = view;
                // Refresh the progress when entering "Recently" or "Newest"
                // (it changes as episodes are listened to; without this the
                // lists keep the state of the last full reload).
                match view {
                    PodcastView::Recent => self.reload_recent(&sender),
                    PodcastView::Newest => self.reload_newest(&sender),
                    _ => {}
                }
                // The sort button only shows on the subscription overview.
                self.rebuild_sort(&sender);
            }
            PodcastsInput::SetSort(crit, desc) => {
                if self.overview_sort != (crit, desc) {
                    self.overview_sort = (crit, desc);
                    let _ = self.library.set_setting("sort_podcasts", crit.as_key());
                    let _ = self
                        .library
                        .set_setting("sort_podcasts_desc", if desc { "1" } else { "0" });
                    self.reload_podcasts(&sender);
                }
            }
            PodcastsInput::SetNoGroup(off) => {
                if self.overview_no_group != off {
                    self.overview_no_group = off;
                    let _ = self
                        .library
                        .set_setting("nogroup_podcasts", if off { "1" } else { "0" });
                    self.reload_podcasts(&sender);
                }
            }
            PodcastsInput::SetGallery(on) => {
                if self.gallery_override != Some(on) {
                    self.gallery_override = Some(on);
                    let _ = self
                        .library
                        .set_setting("gallery_podcasts", if on { "1" } else { "0" });
                    self.reload_podcasts(&sender);
                }
            }
            PodcastsInput::SetGalleryDesc(on) => {
                if self.gallery_desc != on {
                    self.gallery_desc = on;
                    let _ = self
                        .library
                        .set_setting("gallery_desc_podcasts", if on { "1" } else { "0" });
                    self.reload_podcasts(&sender);
                }
            }
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
                        let fetched = fetch_and_store_podcast(&url).map(|(title, _)| title);
                        let _ = out.send(PodcastsCmd::Fetched(fetched));
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
            PodcastsInput::RefreshPodcastDetail(id) => self.refresh_detail(&sender, id, None),
            PodcastsInput::RefreshEpisodeDetail(url) => {
                if let Some(id) = self.library.podcast_id_for_episode_url(&url).ok().flatten() {
                    self.refresh_detail(&sender, id, Some(url));
                }
            }
            PodcastsInput::ShowPodcastEpisodeDetail { podcast_id, index } => {
                self.open_podcast_episode_detail(&sender, podcast_id, index)
            }
            PodcastsInput::ShowEpisodeDetailByUrl { url } => {
                self.open_episode_detail_by_url(&sender, &url)
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
            PodcastsCmd::DetailRefreshed {
                podcast_id,
                episode,
                ok,
            } => {
                if !ok {
                    let _ = sender.output(PodcastsOutput::Toast(gettext("Could not load feed")));
                }
                let image = self
                    .library
                    .podcasts()
                    .unwrap_or_default()
                    .into_iter()
                    .find(|p| p.0 == podcast_id)
                    .and_then(|p| p.2);
                crate::ui::widgets::forget_thumb(
                    image
                        .as_deref()
                        .and_then(crate::core::online::podcast_image_path)
                        .as_deref(),
                );
                self.reload_podcasts(&sender);
                match episode {
                    Some(url) => self.open_episode_detail_by_url(&sender, &url),
                    None => self.open_podcast_detail(&sender, podcast_id),
                }
            }
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
            PodcastsCmd::DownloadProgress { url, done, total } => {
                if let Some(dl) = self.downloading_episodes.get_mut(&url) {
                    dl.done = done;
                    dl.total = total;
                    self.refresh_download_row();
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
            PodcastsCmd::RefreshProgress { done, total, title } => {
                let _ = sender.output(PodcastsOutput::RefreshProgress {
                    done,
                    total,
                    label: title,
                });
            }
            PodcastsCmd::Refreshed {
                updated,
                failed,
                new_episodes,
            } => {
                let _ = sender.output(PodcastsOutput::RefreshFinished);
                let _ = sender.output(PodcastsOutput::RefreshSummary(refresh_summary_text(
                    updated,
                    failed,
                    new_episodes,
                )));
                self.reload_podcasts(&sender);
            }
            PodcastsCmd::CoversCached(fetched) => {
                if fetched {
                    self.reload_podcasts(&sender);
                }
            }
        }
    }
}

impl PodcastsPage {
    /// "Refresh all" from the header button: re-fetch every subscribed feed,
    /// several at once. Each step reports back so the loading overlay can show
    /// a progress bar with the feed being fetched — a bare spinner left the user
    /// unable to tell whether anything was happening at all. The cases that used
    /// to end in silence (no subscriptions, no network) now say so.
    fn refresh_all_feeds(&mut self, sender: &ComponentSender<Self>) {
        let feeds = self.library.podcast_feeds().unwrap_or_default();
        if feeds.is_empty() {
            let _ = sender.output(PodcastsOutput::RefreshSummary(gettext(
                "No podcasts subscribed",
            )));
            return;
        }
        if !crate::ui::app_helpers::online_available() {
            let _ = sender.output(PodcastsOutput::RefreshSummary(gettext(
                "No internet connection",
            )));
            return;
        }
        let total = feeds.len();
        let _ = sender.output(PodcastsOutput::RefreshStarted(true));
        let _ = sender.output(PodcastsOutput::RefreshProgress {
            done: 0,
            total,
            label: feeds[0].0.clone(),
        });
        sender.spawn_command(move |out| {
            // Several feeds at once: each one is mostly network wait, so a
            // serial loop over many subscriptions took ages.
            use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
            let [done, updated, failed, new_episodes] = [(); 4].map(|_| AtomicUsize::new(0));
            // A panic must not swallow `Refreshed` (it ends the refresh spinner).
            crate::core::panic_guard::catch_or(
                "podcast refresh",
                || {
                    crate::core::pool::for_each(&feeds, FEED_REFRESH_THREADS, |_, (title, url)| {
                        let _ = out.send(PodcastsCmd::RefreshProgress {
                            done: done.load(Relaxed),
                            total,
                            title: title.clone(),
                        });
                        match fetch_and_store_podcast(url) {
                            Some((_, fresh)) => {
                                updated.fetch_add(1, Relaxed);
                                new_episodes.fetch_add(fresh, Relaxed);
                            }
                            None => {
                                tracing::warn!("Podcast refresh failed for {url}");
                                failed.fetch_add(1, Relaxed);
                            }
                        }
                        done.fetch_add(1, Relaxed);
                    })
                },
                || (),
            );
            let _ = out.send(PodcastsCmd::Refreshed {
                updated: updated.into_inner(),
                failed: failed.into_inner(),
                new_episodes: new_episodes.into_inner(),
            });
        });
    }

    /// Show detail dialogs on the phone over the **full width** (bottom sheet);
    /// on the desktop floating as before (auto).
    pub(super) fn adapt_detail_dialog(&self, dialog: &adw::Dialog) {
        crate::ui::widgets::adapt_dialog(dialog, self.mobile);
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
    /// Effective gallery mode for the overview: the per-view override if set, else
    /// the global `gallery_view`.
    pub(super) fn gallery_on(&self) -> bool {
        self.gallery_override.unwrap_or(self.gallery_view)
    }
}

#[cfg(test)]
mod tests {
    use super::EpisodeDownload;
    use std::time::{Duration, Instant};

    /// Download state as if it had been running for `secs` with `done` of
    /// `total` bytes transferred.
    fn running(done: u64, total: Option<u64>, secs: u64) -> EpisodeDownload {
        EpisodeDownload {
            started: Instant::now() - Duration::from_secs(secs),
            done,
            total,
        }
    }

    #[test]
    fn fraction_needs_a_total_and_is_clamped() {
        assert_eq!(running(1_000, None, 5).fraction(), None);
        assert_eq!(running(0, Some(0), 5).fraction(), None);
        assert_eq!(running(500, Some(2_000), 5).fraction(), Some(0.25));
        // A server that under-reports its length must not push the bar past 1.
        assert_eq!(running(3_000, Some(2_000), 5).fraction(), Some(1.0));
    }

    #[test]
    fn remaining_time_estimates_from_the_average_rate() {
        // 1 MB in 10 s → 100 KB/s; 3 MB left → ~30 s.
        let ms = running(1024 * 1024, Some(4 * 1024 * 1024), 10)
            .remaining_ms()
            .expect("estimate");
        assert!((29_000..=31_000).contains(&ms), "estimated {ms} ms");
    }

    #[test]
    fn remaining_time_is_withheld_until_the_estimate_is_meaningful() {
        // Barely started: too little data for a rate.
        assert_eq!(
            running(1_024, Some(4 * 1024 * 1024), 5).remaining_ms(),
            None
        );
        // Just begun: elapsed time too short.
        assert_eq!(
            running(4 * 1024 * 1024, Some(80 * 1024 * 1024), 0).remaining_ms(),
            None
        );
        // Complete: nothing left to wait for.
        assert_eq!(
            running(4 * 1024 * 1024, Some(4 * 1024 * 1024), 10).remaining_ms(),
            None
        );
        // No total: no estimate possible.
        assert_eq!(running(4 * 1024 * 1024, None, 10).remaining_ms(), None);
    }

    #[test]
    fn status_text_falls_back_from_eta_to_percent_to_size() {
        // Untranslated in the test binary, so the msgids come back verbatim.
        assert_eq!(
            running(1024 * 1024, Some(4 * 1024 * 1024), 10).status_text(),
            "25 % · 0:30 left"
        );
        assert_eq!(
            running(1024 * 1024, Some(4 * 1024 * 1024), 0).status_text(),
            "25 % downloaded"
        );
        assert_eq!(
            running(5 * 1024 * 1024, None, 10).status_text(),
            "Downloading … 5.0 MB"
        );
        // Still connecting: no size worth showing yet.
        assert_eq!(running(0, None, 1).status_text(), "Downloading …");
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::refresh_summary_text;

    #[test]
    fn summary_lists_only_the_parts_that_happened() {
        assert_eq!(refresh_summary_text(1, 0, 0), "1 podcast updated");
        assert_eq!(
            refresh_summary_text(2, 0, 4),
            "2 podcasts updated · 4 new episodes"
        );
        assert_eq!(
            refresh_summary_text(1, 2, 0),
            "1 podcast updated · 2 feeds failed"
        );
        assert_eq!(refresh_summary_text(0, 0, 0), "Nothing new");
    }
}

/// The episode rows live in this component, so the state reaches them through
/// its message channel — the marking itself is the app-wide one.
impl crate::ui::play_mark::PlaybackSink for relm4::Controller<PodcastsPage> {
    fn apply_playback(&self, state: &crate::ui::play_mark::PlaybackState) {
        use relm4::ComponentController;
        self.emit(PodcastsInput::PlaybackStateChanged {
            playing_url: state.episode_url.clone(),
            playing: state.playing,
        });
    }
}
