//! Hörstatistik-Seite: wertet die rohe `play_event`-Tabelle aus und baut die
//! Seite imperativ auf (wie die Favoriten-/Konzertlisten). Rein lokal – die
//! Daten verlassen das Gerät nie.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::model::{StatEntry, StatTotals};
use crate::ui::app::{App, Msg, StatsPeriod};

/// Wie viele Einträge je Rangliste höchstens gezeigt werden.
const TOP_N: usize = 10;

impl StatsPeriod {
    /// Untergrenze (Unix-Sekunden) für die Auswertung; `now` = jetzt.
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

impl App {
    /// Baut die Statistik-Seite neu auf (beim Öffnen des Bereichs und bei
    /// Zeitraumwechsel). Liest nur – verändert nichts.
    pub(crate) fn refresh_stats(&self, sender: &ComponentSender<Self>) {
        let container = &self.stats_box;
        while let Some(child) = container.first_child() {
            container.remove(&child);
        }
        container.append(&self.stats_period_selector(sender));

        let since = self.stats_period.since(crate::ui::app::unix_now());
        let totals = self.library.stats_totals(since).unwrap_or_default();

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

        // Leerzustand, solange nichts gehört wurde.
        if totals.plays == 0 && totals.total_played_ms == 0 {
            let empty = adw::StatusPage::builder()
                .icon_name("emilia-stats-symbolic")
                .title(&gettext("No listening data yet"))
                .description(&gettext("Play some music — your listening statistics will appear here."))
                .vexpand(true)
                .build();
            content.append(&empty);
            return;
        }

        content.append(&summary_group(&totals));
        content.append(&diversity_group(&totals));

        let artists = self.library.stats_top_artists(since, TOP_N).unwrap_or_default();
        if !artists.is_empty() {
            content.append(&top_group(&gettext("Top artists"), &artists, true));
        }
        let albums = self.library.stats_top_albums(since, TOP_N).unwrap_or_default();
        if !albums.is_empty() {
            content.append(&top_group(&gettext("Top albums"), &albums, false));
        }
        let tracks = self.library.stats_top_tracks(since, TOP_N).unwrap_or_default();
        if !tracks.is_empty() {
            content.append(&top_group(&gettext("Top tracks"), &tracks, false));
        }

        content.append(&weekday_group(
            &self.library.stats_by_weekday(since).unwrap_or([0; 7]),
        ));
        content.append(&clock_group(
            &self.library.stats_by_hour(since).unwrap_or([0; 24]),
        ));
    }

    /// Zeitraum-Auswahl (verlinkte Umschalter, oben zentriert).
    fn stats_period_selector(&self, sender: &ComponentSender<Self>) -> gtk::Box {
        let wrap = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .halign(gtk::Align::Center)
            .margin_top(2)
            // Etwas Luft unter den Zeitraum-Schaltern vor dem Inhalt.
            .margin_bottom(10)
            .build();
        let group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        group.add_css_class("linked");
        let mut leader: Option<gtk::ToggleButton> = None;
        for period in [StatsPeriod::Weeks4, StatsPeriod::Year, StatsPeriod::All] {
            let btn = gtk::ToggleButton::with_label(&period.label());
            btn.set_active(period == self.stats_period);
            match &leader {
                Some(l) => btn.set_group(Some(l)),
                None => leader = Some(btn.clone()),
            }
            let sender = sender.clone();
            btn.connect_clicked(move |b| {
                if b.is_active() {
                    sender.input(Msg::SetStatsPeriod(period));
                }
            });
            group.append(&btn);
        }
        wrap.append(&group);
        wrap
    }
}

/// Gesamt-Kennzahlen: gehörte Zeit, Wiedergaben, Skip-Rate.
fn summary_group(t: &StatTotals) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder().title(&gettext("Overview")).build();
    g.add(&stat_row(&gettext("Listening time"), &fmt_listen(t.total_played_ms)));
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

/// Vielfalt: unterschiedliche Interpreten, Alben, Titel.
fn diversity_group(t: &StatTotals) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder().title(&gettext("Variety")).build();
    g.add(&stat_row(&gettext("Artists"), &t.distinct_artists.to_string()));
    g.add(&stat_row(&gettext("Albums"), &t.distinct_albums.to_string()));
    g.add(&stat_row(&gettext("Tracks"), &t.distinct_tracks.to_string()));
    g
}

/// Eine Kennzahl-Zeile: Beschriftung links, Wert rechts.
fn stat_row(title: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let lbl = gtk::Label::new(Some(value));
    lbl.add_css_class("dim-label");
    row.add_suffix(&lbl);
    row
}

/// Eine Rangliste (Top-Titel/-Alben/-Interpreten). `time_subtitle`: Untertitel
/// ist die gehörte Zeit (Interpreten) statt des Zusatzes (Interpret bei Titel/Album).
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

/// Gehörte Zeit je Wochentag (Montag zuerst; DB-Index 0 = Sonntag).
fn weekday_group(by_weekday: &[i64; 7]) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder().title(&gettext("By weekday")).build();
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

/// Eine waagerechte Balkenzeile: Name | Balken | Wert.
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

/// Gehörte Zeit je Stunde des Tages als kompaktes 24-Balken-Diagramm.
fn clock_group(by_hour: &[i64; 24]) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder()
        .title(&gettext("By time of day"))
        .build();
    let max = by_hour.iter().copied().max().unwrap_or(0).max(1);
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(2)
        .homogeneous(true)
        .height_request(110)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(6)
        .margin_end(6)
        .build();
    for h in 0..24usize {
        let col = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let bar = gtk::ProgressBar::new();
        bar.set_orientation(gtk::Orientation::Vertical);
        bar.set_inverted(true); // von unten füllen
        bar.set_fraction((by_hour[h] as f64 / max as f64).clamp(0.0, 1.0));
        bar.set_vexpand(true);
        bar.set_valign(gtk::Align::Fill);
        // Balken füllen ihre (gleich breiten) Spalte; die Mindestbreite wird per
        // CSS aufgehoben, damit alle 24 Stunden auch auf schmalen Displays passen.
        bar.set_halign(gtk::Align::Fill);
        bar.add_css_class("emilia-hourbar");
        col.append(&bar);
        // Beschriftung nur alle 6 Stunden (0, 6, 12, 18).
        let txt = if h % 6 == 0 { format!("{h}") } else { String::new() };
        let lbl = gtk::Label::new(Some(&txt));
        lbl.add_css_class("dim-label");
        lbl.add_css_class("caption");
        col.append(&lbl);
        row.append(&col);
    }
    g.add(&row);
    g
}

/// Gehörte Zeit menschenlesbar: „3 h 12 min" bzw. „12 min".
fn fmt_listen(ms: i64) -> String {
    let mins = ms.max(0) / 60_000;
    let (h, m) = (mins / 60, mins % 60);
    if h > 0 {
        gettext_f("{h} h {m} min", &[("h", &h.to_string()), ("m", &m.to_string())])
    } else {
        gettext_f("{m} min", &[("m", &m.to_string())])
    }
}
