//! Equalizer and property dialogs: the 10-band EQ editor (cascade per level and
//! output device) plus the property selection (music/concert/podcast/audiobook).
//! Split out of app.rs - pure reordering, no change in functionality.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::category;
use crate::i18n::{gettext, gettext_f};
use crate::ui::app::{App, CtxTarget, Msg};

/// Fixed genre equalizer presets. Each holds the gain (dB) of the ten bands
/// 29 Hz … 15 kHz, in the same order as the sliders. The names are English
/// `msgid`s, translated at the display site. Loading a preset fills the bands;
/// adjusting any slider afterwards drops the selection back to "Custom".
///
/// The curves keep the classic 10-band tonal shapes (Winamp/iTunes style) but
/// are **attenuation only**: the loudest band sits at 0 dB and every other band
/// is cut relative to it. Boosting bands above 0 dB amplifies the signal past
/// full scale and clips ("overdrive"), so the presets shape the sound purely by
/// lowering the other bands instead. The result is the same character at a
/// safe, distortion-free level (turn the volume up to taste).
pub(crate) const GENRE_PRESETS: &[(&str, [f64; 10])] = &[
    (
        "Rock",
        [-1.0, -3.0, -5.0, -11.0, -13.0, -11.0, -6.0, -3.0, -1.0, 0.0],
    ),
    (
        "Pop",
        [-8.0, -7.0, -5.0, -2.0, 0.0, 0.0, -3.0, -6.0, -7.0, -8.0],
    ),
    (
        "Jazz",
        [-1.0, -2.0, -4.0, -3.0, -8.0, -8.0, -6.0, -4.0, -1.0, 0.0],
    ),
    (
        "Classical",
        [0.0, -1.0, -2.0, -4.0, -6.0, -6.0, -7.0, -9.0, -10.0, -11.0],
    ),
    (
        "Dance",
        [
            0.0, -2.0, -6.0, -9.0, -10.0, -12.0, -13.0, -12.0, -8.0, -6.0,
        ],
    ),
    (
        "Electronic",
        [0.0, -1.0, -5.0, -7.0, -9.0, -5.0, -6.0, -5.0, -1.0, 0.0],
    ),
    (
        "Hip-Hop",
        [0.0, -1.0, -4.0, -5.0, -9.0, -10.0, -6.0, -8.0, -5.0, -4.0],
    ),
    (
        "R&B",
        [-2.0, 0.0, -2.0, -6.0, -10.0, -10.0, -6.0, -5.0, -4.0, -4.0],
    ),
    (
        "Metal",
        [0.0, -2.0, -3.0, -6.0, -8.0, -5.0, -6.0, -3.0, -1.0, 0.0],
    ),
    (
        "Acoustic",
        [0.0, -1.0, -2.0, -4.0, -3.0, -3.0, -2.0, -2.0, -3.0, -4.0],
    ),
    (
        "Vocal",
        [-9.0, -9.0, -8.0, -4.0, -1.0, 0.0, 0.0, -2.0, -5.0, -8.0],
    ),
    (
        "Bass boost",
        [
            0.0, -2.0, -4.0, -7.0, -9.0, -10.0, -10.0, -10.0, -10.0, -10.0,
        ],
    ),
    (
        "Treble boost",
        [-9.0, -9.0, -9.0, -9.0, -9.0, -7.0, -5.0, -3.0, -1.0, 0.0],
    ),
    (
        "Loudness",
        [0.0, -2.0, -6.0, -9.0, -12.0, -9.0, -10.0, -8.0, -3.0, 0.0],
    ),
    (
        "Soft",
        [0.0, -1.0, -2.0, -3.0, -4.0, -5.0, -6.0, -5.0, -3.0, -2.0],
    ),
];

/// Index (1-based, into the preset combo where 0 = "Custom") of the preset
/// whose bands match `bands`, or 0 if none matches.
fn match_preset(bands: &[f64; 10]) -> usize {
    for (i, (_, preset)) in GENRE_PRESETS.iter().enumerate() {
        if bands
            .iter()
            .zip(preset.iter())
            .all(|(a, b)| (a - b).abs() < 0.01)
        {
            return i + 1;
        }
    }
    0
}

impl App {
    /// Equalizer dialog: at the top choose **output** (device/Bluetooth) and
    /// **level** (global/artist/album/track), below them ten frequency sliders.
    /// Changes take effect immediately and are saved per output+level; during
    /// playback the inheritance applies (track→album→artist→global, then the
    /// default output as the base).
    pub(crate) fn open_eq_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let Some(entry) = self.nav.context_target.as_ref() else {
            return;
        };

        // Exactly one level per target; the downward inheritance (artist→album→track)
        // is handled by `resolve_eq` during playback. "Global" lives in the settings.
        let (subject, name, note, scope, key): (
            &'static str,
            String,
            Option<&str>,
            &'static str,
            String,
        ) = match entry {
            CtxTarget::Artist(m) => (
                "the artist",
                m.name.clone(),
                Some("Also applies to this artist's albums and tracks."),
                "artist",
                m.name.clone(),
            ),
            CtxTarget::Album(m) => (
                "the album",
                m.album.clone(),
                Some("Also applies to this album's tracks."),
                "album",
                category::album_key(&m.artist, &m.album),
            ),
            CtxTarget::Fs(e) if !e.is_dir() => (
                "the track",
                e.display_title(),
                None,
                "track",
                e.path()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| e.rel_path().unwrap_or_default().to_string()),
            ),
            // Folder: detect as artist or album; otherwise no EQ.
            CtxTarget::Fs(e) => match self.fs_eq_level(e) {
                Some(level) => level,
                None => {
                    self.toast(&gettext("Equalizer is not available here"));
                    return;
                }
            },
        };

        self.open_eq_editor(root, sender, subject, &name, note, scope, key);
    }

    /// Global equalizer (from the settings): the base for everything without its
    /// own setting at the artist, album or track level.
    pub(crate) fn open_global_eq(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        self.open_eq_editor(
            root,
            sender,
            "the global equalizer",
            "",
            Some("Applies to everything without its own artist, album or track setting."),
            "global",
            String::new(),
        );
    }

    /// Equalizer editor for exactly one level (scope/key) with output selection.
    /// Used by the detail EQ (artist/album/track) and by the global EQ.
    pub(crate) fn open_eq_editor(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        subject: &str,
        name: &str,
        note: Option<&str>,
        scope: &'static str,
        key: String,
    ) {
        use std::cell::{Cell, RefCell};
        use std::rc::Rc;

        // Outputs: "Default (all)" as the base + automatically detected devices.
        let mut outputs: Vec<(String, String)> =
            vec![(gettext("Default (all outputs)"), String::new())];
        for o in crate::core::output::list_outputs() {
            outputs.push((o.name, o.id));
        }
        let out_default = outputs
            .iter()
            .position(|(_, id)| !id.is_empty() && *id == self.settings.active_output)
            .unwrap_or(0);

        // Preload the bands per output (no DB access in the closures).
        let preloaded: Vec<[f64; 10]> = outputs
            .iter()
            .map(|(_, oid)| {
                self.library
                    .get_eq(oid, scope, &key)
                    .ok()
                    .flatten()
                    .unwrap_or([0.0; 10])
            })
            .collect();
        let preloaded_enabled: Vec<bool> = outputs
            .iter()
            .map(|(_, oid)| self.library.eq_enabled(oid, scope, &key).unwrap_or(true))
            .collect();

        let outputs = Rc::new(outputs);
        let bands = Rc::new(RefCell::new(preloaded));
        let enabled = Rc::new(RefCell::new(preloaded_enabled));
        let cur_out = Rc::new(Cell::new(out_default));
        let key = Rc::new(key);
        let loading = Rc::new(Cell::new(false));

        // No fixed height: the dialog grows to the natural height of its content
        // (header + presets + ten sliders + buttons) so nothing is cut off. On
        // screens too small for that, the ScrolledWindow below still scrolls.
        let dialog = adw::Dialog::builder()
            .title(gettext("Equalizer"))
            .content_width(440)
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Header: "Settings for …" subtle, the name below it centered and
        // highlighted. For the global EQ (without a name) the prefix itself
        // carries the heading.
        let has_name = !name.is_empty();
        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .build();
        let prefix_css: Vec<&str> = if has_name {
            vec!["dim-label", "caption"]
        } else {
            vec!["title-2"]
        };
        let prefix = gtk::Label::builder()
            .label(gettext_f(
                "Settings for {subject}",
                &[("subject", &gettext(subject))],
            ))
            .halign(gtk::Align::Center)
            .justify(gtk::Justification::Center)
            .wrap(true)
            .css_classes(prefix_css)
            .build();
        header.append(&prefix);
        if has_name {
            let name_label = gtk::Label::builder()
                .label(name)
                .halign(gtk::Align::Center)
                .justify(gtk::Justification::Center)
                .wrap(true)
                .css_classes(["title-2"])
                .build();
            header.append(&name_label);
        }
        if let Some(n) = note {
            let note_label = gtk::Label::builder()
                .label(gettext(n))
                .halign(gtk::Align::Center)
                .justify(gtk::Justification::Center)
                .wrap(true)
                .css_classes(["dim-label", "caption"])
                .build();
            header.append(&note_label);
        }
        content.append(&header);

        // Output selection (its own group without a title - the header is above it).
        let sel_group = adw::PreferencesGroup::new();

        let out_labels: Vec<&str> = outputs.iter().map(|(l, _)| l.as_str()).collect();
        let out_combo = adw::ComboRow::builder()
            .title(gettext("Output"))
            .subtitle(gettext("Device / Bluetooth"))
            .model(&gtk::StringList::new(&out_labels))
            .build();
        out_combo.set_selected(out_default as u32);
        sel_group.add(&out_combo);

        // Genre presets: pick a fixed preset to fill the ten bands. Adjusting any
        // slider afterwards drops the selection back to "Custom". The combo is
        // added here (above the sliders) but its handler is wired further down,
        // once the sliders exist. Selecting it before connecting avoids a
        // spurious load during setup.
        let preset_names: Vec<String> = std::iter::once(gettext("Custom"))
            .chain(GENRE_PRESETS.iter().map(|(n, _)| gettext(*n)))
            .collect();
        let preset_labels: Vec<&str> = preset_names.iter().map(|s| s.as_str()).collect();
        let preset_combo = adw::ComboRow::builder()
            .title(gettext("Genre preset"))
            .subtitle(gettext("Equalizer presets per genre"))
            .model(&gtk::StringList::new(&preset_labels))
            .build();
        preset_combo.set_selected(match_preset(&bands.borrow()[out_default]) as u32);
        sel_group.add(&preset_combo);
        content.append(&sel_group);

        // Ten frequency sliders.
        let freqs = [
            "29 Hz", "59 Hz", "119 Hz", "237 Hz", "474 Hz", "947 Hz", "1.9 kHz", "3.8 kHz",
            "7.5 kHz", "15 kHz",
        ];
        let bands_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();
        let mut scales = Vec::with_capacity(10);
        let start = bands.borrow()[out_default];
        for (i, freq) in freqs.iter().enumerate() {
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .build();
            let label = gtk::Label::builder()
                .label(*freq)
                .width_chars(7)
                .xalign(0.0)
                .css_classes(["caption", "numeric"])
                .build();
            let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, -24.0, 12.0, 1.0);
            scale.set_hexpand(true);
            scale.set_draw_value(true);
            scale.set_value_pos(gtk::PositionType::Right);
            scale.add_mark(0.0, gtk::PositionType::Top, None);
            scale.set_value(start[i]);
            row.append(&label);
            row.append(&scale);
            bands_box.append(&row);
            scales.push(scale);
        }
        let scales = Rc::new(scales);
        content.append(&bands_box);
        // Grey out the sliders while the EQ is bypassed ("Turn off") for the
        // current output; re-enabled on "Turn on", reset, or output switch.
        bands_box.set_sensitive(enabled.borrow()[out_default]);

        // Slider movement → remember value + save (+ apply live via Msg). A
        // manual move is no longer a clean preset, so drop the combo to "Custom".
        for (i, scale) in scales.iter().enumerate() {
            let bands = bands.clone();
            let cur_out = cur_out.clone();
            let loading = loading.clone();
            let outputs = outputs.clone();
            let key = key.clone();
            let sender = sender.clone();
            let preset_combo = preset_combo.clone();
            scale.connect_value_changed(move |s| {
                if loading.get() {
                    return;
                }
                let o = cur_out.get();
                bands.borrow_mut()[o][i] = s.value();
                let arr = bands.borrow()[o];
                let (_, oid) = &outputs[o];
                sender.input(Msg::SetEq {
                    output: oid.clone(),
                    scope,
                    key: (*key).clone(),
                    bands: arr,
                });
                // Drop to "Custom" (index 0 does nothing in the preset handler).
                preset_combo.set_selected(0);
            });
        }

        // Switch output → reload the sliders from the preloaded values and
        // re-match the genre preset for the new output (still under `loading`,
        // so neither the sliders nor the preset combo trigger a save).
        {
            let bands = bands.clone();
            let cur_out = cur_out.clone();
            let loading = loading.clone();
            let scales = scales.clone();
            let preset_combo = preset_combo.clone();
            out_combo.connect_selected_notify(move |c| {
                cur_out.set(c.selected() as usize);
                loading.set(true);
                let arr = bands.borrow()[cur_out.get()];
                for (i, sc) in scales.iter().enumerate() {
                    sc.set_value(arr[i]);
                }
                preset_combo.set_selected(match_preset(&arr) as u32);
                loading.set(false);
            });
        }

        let bypass_button: Rc<RefCell<Option<gtk::Button>>> = Rc::new(RefCell::new(None));

        // Shared: load a band set into the UI for the current output — fill the
        // in-memory bands, enable the level, refresh the sliders (under `loading`
        // so they don't re-save band-by-band) and the bypass label. Used by both
        // Reset (with zeros) and the genre presets; the caller emits the matching
        // ClearEq / SetEq afterwards.
        let load_bands: Rc<dyn Fn([f64; 10])> = {
            let bands = bands.clone();
            let enabled = enabled.clone();
            let bands_box = bands_box.clone();
            let scales = scales.clone();
            let loading = loading.clone();
            let cur_out = cur_out.clone();
            let bypass_button = bypass_button.clone();
            Rc::new(move |new_bands: [f64; 10]| {
                let o = cur_out.get();
                bands.borrow_mut()[o] = new_bands;
                enabled.borrow_mut()[o] = true;
                bands_box.set_sensitive(true);
                if let Some(button) = bypass_button.borrow().as_ref() {
                    button.set_label(&gettext("Turn off"));
                }
                loading.set(true);
                for (i, sc) in scales.iter().enumerate() {
                    sc.set_value(new_bands[i]);
                }
                loading.set(false);
            })
        };

        // Neutralize the current selection and reset it to "inherit".
        let reset = gtk::Button::builder()
            .label(gettext("Reset"))
            .css_classes(["pill"])
            .halign(gtk::Align::Center)
            .build();
        {
            let load_bands = load_bands.clone();
            let cur_out = cur_out.clone();
            let outputs = outputs.clone();
            let key = key.clone();
            let sender = sender.clone();
            let preset_combo = preset_combo.clone();
            reset.connect_clicked(move |_| {
                load_bands([0.0; 10]);
                // Cleared bands are no genre preset (no-op if already "Custom").
                preset_combo.set_selected(0);
                let o = cur_out.get();
                let (_, oid) = &outputs[o];
                sender.input(Msg::ClearEq {
                    output: oid.clone(),
                    scope,
                    key: (*key).clone(),
                });
            });
        }
        // Bypass the EQ for this level without changing its saved values. Unlike
        // "Reset" (delete → inherits album/artist/global), this is a flat
        // override for A/B comparison and can be turned back on.
        let off = gtk::Button::builder()
            .label(if enabled.borrow()[out_default] {
                gettext("Turn off")
            } else {
                gettext("Turn on")
            })
            .css_classes(["pill"])
            .halign(gtk::Align::Center)
            .build();
        *bypass_button.borrow_mut() = Some(off.clone());
        {
            let cur_out = cur_out.clone();
            let outputs = outputs.clone();
            let enabled = enabled.clone();
            let bands_box = bands_box.clone();
            let key = key.clone();
            let sender = sender.clone();
            off.connect_clicked(move |button| {
                let o = cur_out.get();
                let now_enabled = !enabled.borrow()[o];
                enabled.borrow_mut()[o] = now_enabled;
                let label = if now_enabled {
                    gettext("Turn off")
                } else {
                    gettext("Turn on")
                };
                button.set_label(&label);
                bands_box.set_sensitive(now_enabled);
                let (_, oid) = &outputs[o];
                sender.input(Msg::SetEqEnabled {
                    output: oid.clone(),
                    scope,
                    key: (*key).clone(),
                    enabled: now_enabled,
                });
            });
        }
        {
            let enabled = enabled.clone();
            let cur_out = cur_out.clone();
            let off = off.clone();
            let bands_box = bands_box.clone();
            out_combo.connect_selected_notify(move |_| {
                let on = enabled.borrow()[cur_out.get()];
                off.set_label(&if on {
                    gettext("Turn off")
                } else {
                    gettext("Turn on")
                });
                bands_box.set_sensitive(on);
            });
        }

        // Genre preset chosen → fill the ten bands for the current output,
        // enable the level so it is audible, and save (custom edits afterwards
        // revert the combo to "Custom" via the slider handlers).
        {
            let load_bands = load_bands.clone();
            let loading = loading.clone();
            let cur_out = cur_out.clone();
            let outputs = outputs.clone();
            let key = key.clone();
            let sender = sender.clone();
            preset_combo.connect_selected_notify(move |c| {
                let sel = c.selected() as usize;
                // Ignore programmatic changes (output switch / "Custom" reset).
                if loading.get() || sel == 0 {
                    return;
                }
                let preset = GENRE_PRESETS[sel - 1].1;
                load_bands(preset);
                let o = cur_out.get();
                let (_, oid) = &outputs[o];
                sender.input(Msg::SetEqEnabled {
                    output: oid.clone(),
                    scope,
                    key: (*key).clone(),
                    enabled: true,
                });
                sender.input(Msg::SetEq {
                    output: oid.clone(),
                    scope,
                    key: (*key).clone(),
                    bands: preset,
                });
            });
        }

        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .halign(gtk::Align::Center)
            .build();
        buttons.append(&reset);
        buttons.append(&off);
        content.append(&buttons);

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .vexpand(true)
            .child(&content)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(root));
    }

    /// Open the track-level equalizer for the currently running track.
    pub(crate) fn on_open_current_eq(
        &mut self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        if let Some(path) = self.transport.queue.get(self.transport.queue_pos).cloned() {
            let key = path.to_string_lossy().into_owned();
            // `display_name` resolves YouTube titles (yt:<id>) and the
            // "artist - title" of library tracks; the static helper would
            // show the raw path for YouTube.
            let name = self.display_name(&path);
            self.open_eq_editor(root, sender, "the track", &name, None, "track", key);
        }
    }
}
