//! Listening statistics as a standalone relm4 component (zero playback
//! coupling). Evaluates the raw `play_event` table and builds the page
//! imperatively. Purely local — the data never leaves the device.
//!
//! Extracted from the `App` god-object: the page owns its own DB connection
//! and period state; the parent only embeds its widget and tells it to refresh
//! when the section is opened.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::model::{StatEntry, StatTotals};
use crate::ui::app::{unix_now, StatsPeriod};

/// How many entries are shown per ranking at most.
const TOP_N: usize = 10;

impl StatsPeriod {
    /// Lower bound (Unix seconds) for the evaluation; `now` = now.
    pub(crate) fn since(self, now: i64) -> i64 {
        match self {
            StatsPeriod::Weeks4 => (now - 28 * 86_400).max(0),
            StatsPeriod::Year => (now - 365 * 86_400).max(0),
            StatsPeriod::All => 0,
        }
    }

    fn label(self) -> String {
        match self {
            StatsPeriod::Weeks4 => gettext("Last 4 weeks"),
            StatsPeriod::Year => gettext("Last 12 months"),
            StatsPeriod::All => gettext("All time"),
        }
    }
}

/// The statistics page component.
pub(crate) struct StatsPage {
    period: StatsPeriod,
    /// Root box; rebuilt in place on every refresh.
    container: gtk::Box,
}

/// All numbers for one render, computed off the UI thread by [`fetch`].
#[derive(Debug)]
pub(crate) struct StatsData {
    period: StatsPeriod,
    totals: StatTotals,
    artists: Vec<StatEntry>,
    albums: Vec<StatEntry>,
    tracks: Vec<StatEntry>,
    genres: Vec<StatEntry>,
    weekday: [i64; 7],
    hour: [i64; 24],
}

#[derive(Debug)]
pub(crate) enum StatsInput {
    SetPeriod(StatsPeriod),
    /// Recompute from the DB (sent by the parent when the section is opened).
    Refresh,
    /// A background computation finished → render it on the UI thread.
    Rendered(Box<StatsData>),
}

/// Runs all the statistics aggregations on a worker thread (own DB connection).
/// A large `play_event` history makes these slow, so they must not run on the
/// GTK main loop.
fn fetch(period: StatsPeriod) -> StatsData {
    let lib = Library::open_or_memory();
    let since = period.since(unix_now());
    let mut totals = lib.stats_totals(since).unwrap_or_default();
    // The (feat./album-name-folded) rankings are computed in full once: the
    // distinct counts are their lengths, the display takes the top N.
    let artists = lib.stats_top_artists(since, usize::MAX).unwrap_or_default();
    let albums = lib.stats_top_albums(since, usize::MAX).unwrap_or_default();
    totals.distinct_artists = artists.len() as i64;
    totals.distinct_albums = albums.len() as i64;
    StatsData {
        period,
        totals,
        artists: artists.into_iter().take(TOP_N).collect(),
        albums: albums.into_iter().take(TOP_N).collect(),
        tracks: lib.stats_top_tracks(since, TOP_N).unwrap_or_default(),
        genres: lib.stats_top_genres(since, TOP_N).unwrap_or_default(),
        weekday: lib.stats_by_weekday(since).unwrap_or([0; 7]),
        hour: lib.stats_by_hour(since).unwrap_or([0; 24]),
    }
}

#[relm4::component(pub(crate))]
impl SimpleComponent for StatsPage {
    type Init = ();
    type Input = StatsInput;
    type Output = ();

    view! {
        #[root]
        gtk::Box {
            set_orientation: gtk::Orientation::Vertical,
            set_vexpand: true,
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = StatsPage {
            period: StatsPeriod::All,
            container: root.clone(),
        };
        let widgets = view_output!();
        model.spawn_fetch(&sender);
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: StatsInput, sender: ComponentSender<Self>) {
        match msg {
            StatsInput::SetPeriod(period) => {
                self.period = period;
                self.spawn_fetch(&sender);
            }
            StatsInput::Refresh => self.spawn_fetch(&sender),
            StatsInput::Rendered(data) => {
                // Ignore a stale result from a period switch that was superseded
                // while its worker was still running.
                if data.period == self.period {
                    self.render(&data, &sender);
                }
            }
        }
    }
}

impl StatsPage {
    /// Recomputes the page off the UI thread (on open and on a period change),
    /// posting the result back as `Rendered`. The old content stays visible
    /// until the worker finishes.
    fn spawn_fetch(&self, sender: &ComponentSender<Self>) {
        let period = self.period;
        let sender = sender.clone();
        std::thread::spawn(move || {
            sender.input(StatsInput::Rendered(Box::new(fetch(period))));
        });
    }

    /// Rebuilds the page from already-computed data (UI thread; no DB access).
    fn render(&self, data: &StatsData, sender: &ComponentSender<Self>) {
        let container = &self.container;
        while let Some(child) = container.first_child() {
            container.remove(&child);
        }
        container.append(&self.period_selector(sender));

        let totals = &data.totals;

        let scroller = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        container.append(&scroller);

        let clamp = adw::Clamp::builder().maximum_size(640).build();
        scroller.set_child(Some(&clamp));

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(18)
            .margin_start(12)
            .margin_end(12)
            .build();
        clamp.set_child(Some(&content));

        // Empty state as long as nothing has been listened to.
        if totals.plays == 0 && totals.total_played_ms == 0 {
            let empty = adw::StatusPage::builder()
                .icon_name("emilia-stats-symbolic")
                .title(gettext("No listening data yet"))
                .description(gettext(
                    "Play some music — your listening statistics will appear here.",
                ))
                .vexpand(true)
                .build();
            content.append(&empty);
            return;
        }

        content.append(&summary_group(totals));
        content.append(&diversity_group(totals));

        if !data.artists.is_empty() {
            content.append(&top_group(&gettext("Top artists"), &data.artists, true));
        }
        if !data.albums.is_empty() {
            content.append(&top_group(&gettext("Top albums"), &data.albums, false));
        }
        if !data.tracks.is_empty() {
            content.append(&top_group(&gettext("Top tracks"), &data.tracks, false));
        }
        if !data.genres.is_empty() {
            content.append(&top_group(&gettext("Top genres"), &data.genres, true));
        }

        content.append(&weekday_group(&data.weekday));
        content.append(&clock_group(&data.hour));
    }

    /// Period selection (linked toggles, full width like the Podcast/Streaming
    /// tab switchers).
    fn period_selector(&self, sender: &ComponentSender<Self>) -> gtk::Box {
        let wrap = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .margin_top(2)
            .margin_bottom(10)
            .margin_start(12)
            .margin_end(12)
            .build();
        let group = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        group.add_css_class("linked");
        group.set_hexpand(true);
        let mut leader: Option<gtk::ToggleButton> = None;
        for period in [StatsPeriod::Weeks4, StatsPeriod::Year, StatsPeriod::All] {
            let btn = gtk::ToggleButton::with_label(&period.label());
            btn.set_hexpand(true);
            btn.set_active(period == self.period);
            match &leader {
                Some(l) => btn.set_group(Some(l)),
                None => leader = Some(btn.clone()),
            }
            let sender = sender.clone();
            btn.connect_clicked(move |b| {
                if b.is_active() {
                    sender.input(StatsInput::SetPeriod(period));
                }
            });
            group.append(&btn);
        }
        wrap.append(&group);
        wrap
    }
}

/// Overall metrics: listening time, plays, skip rate.
fn summary_group(t: &StatTotals) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder()
        .title(gettext("Overview"))
        .build();
    g.add(&stat_row(
        &gettext("Listening time"),
        &fmt_listen(t.total_played_ms),
    ));
    g.add(&stat_row(&gettext("Plays"), &t.plays.to_string()));
    let events = t.plays + t.skips;
    let skip_pct = if events > 0 {
        (t.skips as f64 * 100.0 / events as f64).round() as i64
    } else {
        0
    };
    g.add(&stat_row(&gettext("Skip rate"), &format!("{skip_pct} %")));
    g
}

/// Variety: distinct artists, albums, tracks.
fn diversity_group(t: &StatTotals) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder()
        .title(gettext("Variety"))
        .build();
    g.add(&stat_row(
        &gettext("Artists"),
        &t.distinct_artists.to_string(),
    ));
    g.add(&stat_row(
        &gettext("Albums"),
        &t.distinct_albums.to_string(),
    ));
    g.add(&stat_row(
        &gettext("Tracks"),
        &t.distinct_tracks.to_string(),
    ));
    g
}

/// A metric row: label on the left, value on the right.
fn stat_row(title: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let lbl = gtk::Label::new(Some(value));
    lbl.add_css_class("dim-label");
    row.add_suffix(&lbl);
    row
}

/// A ranking (top tracks/albums/artists). `time_subtitle`: the subtitle is the
/// listening time (artists) instead of the detail (artist for track/album).
fn top_group(title: &str, entries: &[StatEntry], time_subtitle: bool) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder().title(title).build();
    for (i, e) in entries.iter().enumerate() {
        let name = gtk::glib::markup_escape_text(&e.name);
        let row = adw::ActionRow::builder().title(name.as_str()).build();

        let subtitle = if time_subtitle {
            fmt_listen(e.played_ms)
        } else {
            e.detail.clone()
        };
        if !subtitle.trim().is_empty() {
            let sub = gtk::glib::markup_escape_text(&subtitle);
            row.set_subtitle(sub.as_str());
        }

        let rank = gtk::Label::new(Some(&format!("{}.", i + 1)));
        rank.add_css_class("dim-label");
        row.add_prefix(&rank);

        let plays = gtk::Label::new(Some(&ngettext_n("{n} play", "{n} plays", e.plays as u32)));
        plays.add_css_class("dim-label");
        row.add_suffix(&plays);

        g.add(&row);
    }
    g
}

/// Listening time per weekday (Monday first; DB index 0 = Sunday).
fn weekday_group(by_weekday: &[i64; 7]) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder()
        .title(gettext("By weekday"))
        .build();
    let max = by_weekday.iter().copied().max().unwrap_or(0).max(1);
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(6)
        .margin_end(6)
        .build();
    let days = [
        (1usize, gettext("Mon")),
        (2, gettext("Tue")),
        (3, gettext("Wed")),
        (4, gettext("Thu")),
        (5, gettext("Fri")),
        (6, gettext("Sat")),
        (0, gettext("Sun")),
    ];
    for (idx, name) in days {
        list.append(&hbar_row(&name, by_weekday[idx], max));
    }
    g.add(&list);
    g
}

/// A horizontal bar row: name | bar | value.
fn hbar_row(label: &str, value: i64, max: i64) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let lbl = gtk::Label::new(Some(label));
    lbl.set_width_chars(4);
    lbl.set_xalign(0.0);
    let bar = gtk::ProgressBar::new();
    bar.set_fraction((value as f64 / max as f64).clamp(0.0, 1.0));
    bar.set_hexpand(true);
    bar.set_valign(gtk::Align::Center);
    let val = gtk::Label::new(Some(&fmt_listen(value)));
    val.add_css_class("dim-label");
    val.set_width_chars(9);
    val.set_xalign(1.0);
    row.append(&lbl);
    row.append(&bar);
    row.append(&val);
    row
}

/// Listening time per hour of the day as a 24-bar chart. Drawn directly with
/// Cairo (a single `DrawingArea`) — robust even on narrow displays.
fn clock_group(by_hour: &[i64; 24]) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder()
        .title(gettext("By time of day"))
        .build();
    let data = *by_hour;
    let area = gtk::DrawingArea::builder()
        .height_request(120)
        .hexpand(true)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(6)
        .margin_end(6)
        .build();
    area.set_draw_func(move |widget, cr, width, height| {
        let max = data.iter().copied().max().unwrap_or(0).max(1) as f64;
        let w = width as f64;
        let h = height as f64;
        let label_h = 16.0; // space for the hour labels at the bottom
        let chart_h = (h - label_h).max(1.0);
        let n = 24.0;
        let gap = 2.0;
        let bar_w = ((w - gap * (n - 1.0)) / n).max(1.0);
        // Foreground color of the theme (adapts to light/dark).
        let c = widget.color();
        let (r, gc, b) = (c.red() as f64, c.green() as f64, c.blue() as f64);
        cr.set_source_rgba(r, gc, b, 0.85);
        for (i, &v) in data.iter().enumerate() {
            let frac = (v as f64 / max).clamp(0.0, 1.0);
            let bh = if v > 0 {
                (chart_h * frac).max(2.0)
            } else {
                0.0
            };
            if bh > 0.0 {
                let x = i as f64 * (bar_w + gap);
                cr.rectangle(x, chart_h - bh, bar_w, bh);
            }
        }
        let _ = cr.fill();
        // Hour labels every 6 hours (0, 6, 12, 18).
        cr.set_source_rgba(r, gc, b, 0.5);
        cr.set_font_size(10.0);
        for i in (0..24usize).step_by(6) {
            let x = i as f64 * (bar_w + gap);
            cr.move_to(x, h - 3.0);
            let _ = cr.show_text(&i.to_string());
        }
    });
    g.add(&area);
    g
}

/// Listening time human-readable: "3 h 12 min" or "12 min".
fn fmt_listen(ms: i64) -> String {
    let mins = ms.max(0) / 60_000;
    let (h, m) = (mins / 60, mins % 60);
    if h > 0 {
        gettext_f(
            "{h} h {m} min",
            &[("h", &h.to_string()), ("m", &m.to_string())],
        )
    } else {
        gettext_f("{m} min", &[("m", &m.to_string())])
    }
}
