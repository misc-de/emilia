//! FM radio page (phones with a tuner, see [`crate::core::fm`]): the frequency
//! with its tuning dial, fine steps and seek, the saved stations and a band
//! scan.
//!
//! **Boundary** (as for the other media pages): the page tunes, seeks, scans
//! and keeps the favorites; playback stays on `App` (`app_fm.rs`), which plays
//! `emilia-fm://<freq>` through the normal player. The page asks for it via
//! [`FmOutput`] and is told the playback state back via
//! [`FmInput::PlaybackStateChanged`]. While FM plays, turning the dial retunes
//! the running tuner ([`FmOutput::Tune`]) instead of restarting playback.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::fm;
use crate::i18n::{gettext, gettext_f};
use crate::model::FmStation;
use crate::ui::fm_dial::FmDial;

pub(crate) struct FmPage {
    library: Library,
    window: Option<adw::ApplicationWindow>,
    /// The frequency on the dial (tuned to while FM plays).
    freq: u16,
    /// Mirror of the transport: FM is the current source (playing or paused).
    /// Its frequency is the page's own [`Self::freq`]: the page tunes, the
    /// transport follows; a change from outside arrives as [`FmInput::Follow`].
    active: bool,
    playing: bool,
    stations: Vec<FmStation>,
    /// Stations found by the last band scan.
    found: Vec<u16>,
    /// A seek or scan runs (the buttons wait for it).
    busy: bool,
    cancel: Arc<AtomicBool>,
    w: Widgets,
}

/// The widgets the page updates.
struct Widgets {
    dial: FmDial,
    freq_label: gtk::Label,
    name_label: gtk::Label,
    status: gtk::Label,
    progress: gtk::ProgressBar,
    play: gtk::Button,
    star: gtk::Button,
    seek_down: gtk::Button,
    seek_up: gtk::Button,
    scan: gtk::Button,
    favorites: gtk::ListBox,
    favorites_empty: gtk::Label,
    found_group: gtk::Box,
    found: gtk::ListBox,
}

#[derive(Debug)]
pub(crate) enum FmInput {
    SetWindow(adw::ApplicationWindow),
    /// The transport's state: FM is the current source (`active`), and plays.
    PlaybackStateChanged {
        active: bool,
        playing: bool,
    },
    /// The transport moved FM elsewhere (next/previous from a headset or
    /// MPRIS): the dial follows.
    Follow(u16),
    /// The dial passes a channel (live).
    Dial(u16),
    /// The dial came to rest.
    DialDone(u16),
    /// ±0.1 MHz.
    Step(i32),
    Seek {
        up: bool,
    },
    Scan,
    TogglePlay,
    PlayStation(u16),
    /// The star: save the current frequency (asks for a name) or forget it.
    Star,
    Save {
        freq: u16,
        name: String,
    },
    Rename(i64),
    RenameTo {
        id: i64,
        name: String,
    },
    Move {
        id: i64,
        dir: i32,
    },
    Remove(i64),
    RemoveConfirmed(i64),
    /// The transport could not play FM.
    PlaybackFailed,
}

#[derive(Debug)]
pub(crate) enum FmOutput {
    /// Play `freq` (or pause/resume if FM already plays there).
    Toggle(u16),
    /// FM plays: retune it to `freq`.
    Tune(u16),
    /// The favorites changed (the transport steps through them).
    StationsChanged,
}

#[derive(Debug)]
pub(crate) enum FmCmd {
    Seeked(Result<Option<u16>, String>),
    ScanProgress(u16),
    Scanned(Result<Vec<u16>, String>),
}

#[relm4::component(pub(crate))]
impl Component for FmPage {
    type Init = ();
    type Input = FmInput;
    type Output = FmOutput;
    type CommandOutput = FmCmd;

    view! {
        #[root]
        gtk::ScrolledWindow {
            set_vexpand: true,
            set_hscrollbar_policy: gtk::PolicyType::Never,
        }
    }

    fn init(_: (), root: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        let library = Library::open_or_memory();
        let freq = library.fm_last_freq();
        let w = build(&root, freq, &sender);
        let mut model = FmPage {
            library,
            window: None,
            freq,
            active: false,
            playing: false,
            stations: Vec::new(),
            found: Vec::new(),
            busy: false,
            cancel: Arc::new(AtomicBool::new(false)),
            w,
        };
        model.reload(&sender);
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: FmInput, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            FmInput::SetWindow(win) => self.window = Some(win),
            FmInput::PlaybackStateChanged { active, playing } => {
                if self.active != active || self.playing != playing {
                    self.active = active;
                    self.playing = playing;
                    self.refresh(&sender);
                }
            }
            FmInput::Follow(f) => {
                if f != self.freq {
                    self.freq = f;
                    self.w.dial.set_freq(f);
                    self.refresh(&sender);
                }
            }
            FmInput::Dial(f) => self.tune_to(f, false, &sender),
            FmInput::DialDone(f) => self.tune_to(f, true, &sender),
            FmInput::Step(n) => {
                let f = fm::step(self.freq, n);
                self.w.dial.set_freq(f);
                self.tune_to(f, true, &sender);
            }
            FmInput::Seek { up } => self.seek(up, &sender),
            FmInput::Scan => self.scan(&sender),
            FmInput::TogglePlay => {
                let _ = sender.output(FmOutput::Toggle(self.freq));
            }
            FmInput::PlayStation(f) => {
                if self.playing_freq() == Some(f) {
                    let _ = sender.output(FmOutput::Toggle(f));
                    return;
                }
                self.freq = f;
                self.w.dial.set_freq(f);
                self.library.set_fm_last_freq(f);
                if self.active && self.playing {
                    let _ = sender.output(FmOutput::Tune(f));
                } else {
                    let _ = sender.output(FmOutput::Toggle(f));
                }
                self.refresh(&sender);
            }
            FmInput::Star => match self.station_at(self.freq) {
                Some(st) => self.confirm_remove(st.id, &st.name, &sender),
                None => self.ask_name(self.freq, &sender),
            },
            FmInput::Save { freq, name } => {
                let name = name.trim();
                let name = if name.is_empty() {
                    fm::label(freq)
                } else {
                    name.to_string()
                };
                if let Err(e) = self.library.save_fm_station(freq, &name) {
                    tracing::error!("FM: cannot save {freq}: {e}");
                }
                self.stations_changed(&sender);
            }
            FmInput::Rename(id) => self.ask_rename(id, &sender),
            FmInput::RenameTo { id, name } => {
                if !name.trim().is_empty() {
                    let _ = self.library.rename_fm_station(id, name.trim());
                    self.stations_changed(&sender);
                }
            }
            FmInput::Move { id, dir } => {
                let _ = self.library.move_fm_station(id, dir);
                self.stations_changed(&sender);
            }
            FmInput::Remove(id) => {
                if let Some(st) = self.stations.iter().find(|s| s.id == id).cloned() {
                    self.confirm_remove(id, &st.name, &sender);
                }
            }
            FmInput::PlaybackFailed => self.w.status.set_label(&gettext(
                "FM radio could not start: the phone's audio service offers no FM sound. It needs the source “hal_fm_input” (audio-hal-bridge with FM support).",
            )),
            FmInput::RemoveConfirmed(id) => {
                let _ = self.library.delete_fm_station(id);
                self.stations_changed(&sender);
            }
        }
    }

    fn update_cmd(&mut self, cmd: FmCmd, sender: ComponentSender<Self>, _root: &Self::Root) {
        match cmd {
            FmCmd::Seeked(res) => {
                self.set_busy(false);
                match res {
                    Ok(Some(f)) => {
                        self.freq = f;
                        self.w.dial.set_freq(f);
                        self.library.set_fm_last_freq(f);
                        if self.active {
                            // The tuner is on the station already; the
                            // transport takes over the name.
                            let _ = sender.output(FmOutput::Tune(f));
                        }
                        self.w.status.set_label("");
                    }
                    Ok(None) => self.w.status.set_label(&gettext(
                        "No station found. Is a wired headset plugged in? Its cable is the antenna.",
                    )),
                    Err(e) => self.show_error(&e),
                }
                self.refresh(&sender);
            }
            FmCmd::ScanProgress(f) => {
                let frac = f64::from(f - fm::FREQ_MIN) / f64::from(fm::FREQ_MAX - fm::FREQ_MIN);
                self.w.progress.set_fraction(frac);
                self.w
                    .status
                    .set_label(&gettext_f("Scanning … {freq}", &[("freq", &fm::label(f))]));
            }
            FmCmd::Scanned(res) => {
                self.set_busy(false);
                self.w.progress.set_visible(false);
                match res {
                    Ok(found) => {
                        self.w.status.set_label(&if found.is_empty() {
                            gettext("No station found. Is a wired headset plugged in? Its cable is the antenna.")
                        } else {
                            crate::i18n::ngettext_n(
                                "{n} station found",
                                "{n} stations found",
                                found.len() as u32,
                            )
                        });
                        self.found = found;
                    }
                    Err(e) => self.show_error(&e),
                }
                self.refresh(&sender);
            }
        }
    }
}

/// Builds the page into `root`.
fn build(root: &gtk::ScrolledWindow, freq: u16, sender: &ComponentSender<FmPage>) -> Widgets {
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(12)
        .margin_end(12)
        .build();
    let clamp = adw::Clamp::builder().maximum_size(560).child(&page).build();
    root.set_child(Some(&clamp));

    // ---- Frequency display ----
    let freq_label = gtk::Label::builder()
        .label(fm::mhz(freq))
        .css_classes(["fm-freq"])
        .build();
    let unit = gtk::Label::builder()
        .label("MHz")
        .css_classes(["title-4", "dim-label"])
        .valign(gtk::Align::End)
        .margin_bottom(10)
        .build();
    let freq_row = gtk::Box::builder()
        .spacing(6)
        .halign(gtk::Align::Center)
        .build();
    freq_row.append(&freq_label);
    freq_row.append(&unit);
    let name_label = gtk::Label::builder()
        .css_classes(["title-4"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();
    header.append(&freq_row);
    header.append(&name_label);
    page.append(&header);

    // ---- Dial ----
    let dial = FmDial::new(freq);
    {
        let s = sender.input_sender().clone();
        dial.connect_change(move |f| s.emit(FmInput::Dial(f)));
        let s = sender.input_sender().clone();
        dial.connect_done(move |f| s.emit(FmInput::DialDone(f)));
    }
    let dial_frame = gtk::Frame::builder()
        .child(&dial.area)
        .css_classes(["fm-dial-frame"])
        .build();
    page.append(&dial_frame);

    // ---- Transport row: seek, fine step, play ----
    let button = |icon: &str, tooltip: &str, msg: fn() -> FmInput| {
        let b = gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(tooltip)
            .valign(gtk::Align::Center)
            .css_classes(["circular", "flat"])
            .build();
        let s = sender.input_sender().clone();
        b.connect_clicked(move |_| s.emit(msg()));
        b
    };
    let seek_down = button(
        "media-seek-backward-symbolic",
        &gettext("Search for the previous station"),
        || FmInput::Seek { up: false },
    );
    let step_down = button("go-previous-symbolic", &gettext("Down 0.1 MHz"), || {
        FmInput::Step(-1)
    });
    let play = gtk::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .tooltip_text(gettext("Play"))
        .css_classes(["circular", "suggested-action", "fm-play"])
        .valign(gtk::Align::Center)
        .build();
    {
        let s = sender.input_sender().clone();
        play.connect_clicked(move |_| s.emit(FmInput::TogglePlay));
    }
    let step_up = button("go-next-symbolic", &gettext("Up 0.1 MHz"), || {
        FmInput::Step(1)
    });
    let seek_up = button(
        "media-seek-forward-symbolic",
        &gettext("Search for the next station"),
        || FmInput::Seek { up: true },
    );
    let controls = gtk::Box::builder()
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();
    for b in [&seek_down, &step_down, &play, &step_up, &seek_up] {
        controls.append(b);
    }
    page.append(&controls);

    // ---- Save / scan ----
    let star = gtk::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .icon_name("non-starred-symbolic")
                .label(gettext("Save"))
                .build(),
        )
        .css_classes(["pill"])
        .build();
    {
        let s = sender.input_sender().clone();
        star.connect_clicked(move |_| s.emit(FmInput::Star));
    }
    let scan = gtk::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .icon_name("system-search-symbolic")
                .label(gettext("Scan band"))
                .build(),
        )
        .tooltip_text(gettext("Search the whole band for stations"))
        .css_classes(["pill"])
        .build();
    {
        let s = sender.input_sender().clone();
        scan.connect_clicked(move |_| s.emit(FmInput::Scan));
    }
    let actions = gtk::Box::builder()
        .spacing(12)
        .halign(gtk::Align::Center)
        .homogeneous(true)
        .build();
    actions.append(&star);
    actions.append(&scan);
    page.append(&actions);

    let progress = gtk::ProgressBar::builder().visible(false).build();
    page.append(&progress);
    let status = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label"])
        .build();
    page.append(&status);

    // ---- Saved stations ----
    let fav_title = gtk::Label::builder()
        .label(gettext("Saved stations"))
        .halign(gtk::Align::Start)
        .css_classes(["heading"])
        .build();
    let favorites = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let favorites_empty = gtk::Label::builder()
        .label(gettext(
            "Tune in to a station and tap “Save” to keep it here. The buttons of your headset step through the saved stations.",
        ))
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label"])
        .build();
    let fav_group = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    fav_group.append(&fav_title);
    fav_group.append(&favorites);
    fav_group.append(&favorites_empty);
    page.append(&fav_group);

    // ---- Scan results ----
    let found_title = gtk::Label::builder()
        .label(gettext("Found stations"))
        .halign(gtk::Align::Start)
        .css_classes(["heading"])
        .build();
    let found = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let found_group = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .visible(false)
        .build();
    found_group.append(&found_title);
    found_group.append(&found);
    page.append(&found_group);

    let hint = gtk::Label::builder()
        .label(gettext(
            "FM radio needs a wired headset: its cable is the antenna. The sound can still play on the speaker or over Bluetooth.",
        ))
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label", "caption"])
        .build();
    page.append(&hint);

    Widgets {
        dial,
        freq_label,
        name_label,
        status,
        progress,
        play,
        star,
        seek_down,
        seek_up,
        scan,
        favorites,
        favorites_empty,
        found_group,
        found,
    }
}

impl FmPage {
    /// The frequency FM plays on (or is paused on), if FM is the current source.
    fn playing_freq(&self) -> Option<u16> {
        self.active.then_some(self.freq)
    }

    fn station_at(&self, freq: u16) -> Option<FmStation> {
        self.stations.iter().find(|s| s.freq == freq).cloned()
    }

    fn reload(&mut self, sender: &ComponentSender<Self>) {
        self.stations = self.library.fm_stations().unwrap_or_default();
        self.refresh(sender);
    }

    fn stations_changed(&mut self, sender: &ComponentSender<Self>) {
        self.reload(sender);
        let _ = sender.output(FmOutput::StationsChanged);
    }

    /// The dial or a step moved to `f`: show it, retune a playing tuner, and
    /// remember it once it rests (`done`).
    fn tune_to(&mut self, f: u16, done: bool, sender: &ComponentSender<Self>) {
        let moved = self.freq != f;
        self.freq = f;
        if moved {
            if self.active {
                let _ = sender.output(FmOutput::Tune(f));
            }
            self.refresh_header();
        }
        if done {
            self.library.set_fm_last_freq(f);
            self.refresh(sender);
        }
    }

    fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
        for b in [&self.w.seek_down, &self.w.seek_up, &self.w.scan] {
            b.set_sensitive(!busy);
        }
    }

    fn show_error(&self, e: &str) {
        self.w.status.set_label(&gettext_f(
            "The FM tuner does not respond: {error}",
            &[("error", e)],
        ));
    }

    fn seek(&mut self, up: bool, sender: &ComponentSender<Self>) {
        if self.busy {
            return;
        }
        self.set_busy(true);
        self.w.status.set_label(&gettext("Searching …"));
        let (from, cancel) = (self.freq, self.cancel.clone());
        cancel.store(false, Ordering::Relaxed);
        sender.spawn_oneshot_command(move || {
            FmCmd::Seeked(fm::seek(from, up, &cancel).map_err(|e| e.to_string()))
        });
    }

    fn scan(&mut self, sender: &ComponentSender<Self>) {
        if self.busy {
            return;
        }
        self.set_busy(true);
        self.w.progress.set_fraction(0.0);
        self.w.progress.set_visible(true);
        let cancel = self.cancel.clone();
        cancel.store(false, Ordering::Relaxed);
        sender.spawn_command(move |out| {
            let progress = out.clone();
            let res = fm::scan(
                move |f| {
                    // One update per MHz is plenty for the bar.
                    if f % 100 == 0 {
                        let _ = progress.send(FmCmd::ScanProgress(f));
                    }
                },
                &cancel,
            );
            let _ = out.send(FmCmd::Scanned(res.map_err(|e| e.to_string())));
        });
    }

    /// Frequency, station name, play button and star.
    fn refresh_header(&self) {
        self.w.freq_label.set_label(&fm::mhz(self.freq));
        let saved = self.station_at(self.freq);
        let name = saved.as_ref().map(|s| s.name.as_str()).unwrap_or("");
        // A name that is just the frequency says nothing new.
        let name = if name == fm::label(self.freq) {
            ""
        } else {
            name
        };
        self.w.name_label.set_label(name);
        self.w.name_label.set_visible(!name.is_empty());
        let here = self.active;
        crate::ui::play_mark::apply(&self.w.play, here, self.playing);
        self.w.play.set_tooltip_text(Some(&if here && self.playing {
            gettext("Pause")
        } else {
            gettext("Play")
        }));
        if let Some(content) = self.w.star.child().and_downcast::<adw::ButtonContent>() {
            if saved.is_some() {
                content.set_icon_name("starred-symbolic");
                content.set_label(&gettext("Saved"));
            } else {
                content.set_icon_name("non-starred-symbolic");
                content.set_label(&gettext("Save"));
            }
        }
    }

    /// Everything: header and both lists.
    fn refresh(&self, sender: &ComponentSender<Self>) {
        self.refresh_header();
        self.fill_favorites(sender);
        self.fill_found(sender);
    }

    fn play_button(&self, freq: u16, sender: &ComponentSender<Self>) -> gtk::Button {
        let b = gtk::Button::builder()
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        crate::ui::play_mark::apply(&b, self.playing_freq() == Some(freq), self.playing);
        let s = sender.input_sender().clone();
        b.connect_clicked(move |_| s.emit(FmInput::PlayStation(freq)));
        b
    }

    fn fill_favorites(&self, sender: &ComponentSender<Self>) {
        let list = &self.w.favorites;
        list.remove_all();
        let n = self.stations.len();
        for (i, st) in self.stations.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&st.name).as_str())
                .subtitle(fm::label(st.freq))
                .activatable(true)
                .build();
            row.add_prefix(&self.play_button(st.freq, sender));
            if self.freq == st.freq {
                row.add_css_class("fm-current");
            }
            {
                let (s, f) = (sender.input_sender().clone(), st.freq);
                row.connect_activated(move |_| s.emit(FmInput::PlayStation(f)));
            }
            row.add_suffix(&self.row_menu(st.id, i > 0, i + 1 < n, sender));
            list.append(&row);
        }
        list.set_visible(n > 0);
        self.w.favorites_empty.set_visible(n == 0);
    }

    /// Rename / move / remove of a saved station.
    fn row_menu(
        &self,
        id: i64,
        can_up: bool,
        can_down: bool,
        sender: &ComponentSender<Self>,
    ) -> gtk::MenuButton {
        let pop_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        let popover = gtk::Popover::builder().child(&pop_box).build();
        let item = |label: String, enabled: bool, msg: FmInput| {
            let b = gtk::Button::builder()
                .label(label)
                .css_classes(["flat"])
                .sensitive(enabled)
                .build();
            if let Some(l) = b.child().and_downcast::<gtk::Label>() {
                l.set_xalign(0.0);
            }
            let (s, pop) = (sender.input_sender().clone(), popover.clone());
            let msg = std::cell::RefCell::new(Some(msg));
            b.connect_clicked(move |_| {
                pop.popdown();
                if let Some(m) = msg.borrow_mut().take() {
                    s.emit(m);
                }
            });
            pop_box.append(&b);
        };
        item(gettext("Rename"), true, FmInput::Rename(id));
        item(gettext("Move up"), can_up, FmInput::Move { id, dir: -1 });
        item(gettext("Move down"), can_down, FmInput::Move { id, dir: 1 });
        item(gettext("Remove"), true, FmInput::Remove(id));
        gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .tooltip_text(gettext("More"))
            .popover(&popover)
            .build()
    }

    fn fill_found(&self, sender: &ComponentSender<Self>) {
        let list = &self.w.found;
        list.remove_all();
        for &f in &self.found {
            let saved = self.station_at(f);
            let row = adw::ActionRow::builder()
                .title(fm::label(f))
                .activatable(true)
                .build();
            if let Some(st) = &saved {
                row.set_subtitle(&gtk::glib::markup_escape_text(&st.name));
            }
            row.add_prefix(&self.play_button(f, sender));
            {
                let s = sender.input_sender().clone();
                row.connect_activated(move |_| s.emit(FmInput::PlayStation(f)));
            }
            if saved.is_none() {
                let save = gtk::Button::builder()
                    .icon_name("non-starred-symbolic")
                    .tooltip_text(gettext("Save"))
                    .valign(gtk::Align::Center)
                    .css_classes(["flat"])
                    .build();
                let s = sender.input_sender().clone();
                save.connect_clicked(move |_| {
                    s.emit(FmInput::Save {
                        freq: f,
                        name: fm::label(f),
                    })
                });
                row.add_suffix(&save);
            }
            list.append(&row);
        }
        self.w.found_group.set_visible(!self.found.is_empty());
    }

    fn ask_name(&self, freq: u16, sender: &ComponentSender<Self>) {
        let dialog = adw::AlertDialog::new(
            Some(&gettext_f("Save {freq}", &[("freq", &fm::label(freq))])),
            Some(&gettext("A name for the station:")),
        );
        let entry = gtk::Entry::builder()
            .placeholder_text(fm::label(freq))
            .activates_default(true)
            .build();
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", &gettext("Cancel")), ("save", &gettext("Save"))]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        let s = sender.input_sender().clone();
        dialog.connect_response(None, move |_, resp| {
            if resp == "save" {
                s.emit(FmInput::Save {
                    freq,
                    name: entry.text().to_string(),
                });
            }
        });
        dialog.present(self.window.as_ref());
    }

    fn ask_rename(&self, id: i64, sender: &ComponentSender<Self>) {
        let Some(st) = self.stations.iter().find(|s| s.id == id) else {
            return;
        };
        let dialog = adw::AlertDialog::new(Some(&gettext("Rename station")), None);
        let entry = gtk::Entry::builder()
            .text(&st.name)
            .activates_default(true)
            .build();
        crate::ui::widgets::no_autofocus(&entry);
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[
            ("cancel", &gettext("Cancel")),
            ("rename", &gettext("Rename")),
        ]);
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");
        let s = sender.input_sender().clone();
        dialog.connect_response(None, move |_, resp| {
            if resp == "rename" {
                s.emit(FmInput::RenameTo {
                    id,
                    name: entry.text().to_string(),
                });
            }
        });
        dialog.present(self.window.as_ref());
    }

    fn confirm_remove(&self, id: i64, name: &str, sender: &ComponentSender<Self>) {
        let dialog = adw::AlertDialog::new(
            Some(&gettext_f("Remove “{name}”?", &[("name", name)])),
            None,
        );
        dialog.add_responses(&[
            ("cancel", &gettext("Cancel")),
            ("remove", &gettext("Remove")),
        ]);
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let s = sender.input_sender().clone();
        dialog.connect_response(None, move |_, resp| {
            if resp == "remove" {
                s.emit(FmInput::RemoveConfirmed(id));
            }
        });
        dialog.present(self.window.as_ref());
    }
}
