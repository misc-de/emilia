//! Equalizer- und Eigenschaft-Dialoge: der 10-Band-EQ-Editor (Kaskade je Ebene und
//! Ausgabegerät) sowie die Eigenschaft-Auswahl (Musik/Konzert/Podcast/Hörbuch).
//! Aus app.rs herausgelöst – reine Umordnung, kein Funktionswechsel.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::category;
use crate::ui::app::{App, CtxTarget, Msg};

impl App {
    /// Equalizer-Dialog: oben **Ausgang** (Gerät/Bluetooth) und **Ebene**
    /// (Global/Interpret/Album/Titel) wählen, darunter zehn Frequenzregler.
    /// Änderungen wirken sofort und werden je Ausgang+Ebene gespeichert; beim
    /// Abspielen greift die Vererbung (Titel→Album→Interpret→Global, dann der
    /// Standard-Ausgang als Basis).
    pub(crate) fn open_eq_dialog(&self, root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) {
        let Some(entry) = self.context_target.as_ref() else {
            return;
        };

        // Genau eine Ebene je Ziel; die Vererbung nach unten (Interpret→Album→Titel)
        // übernimmt beim Abspielen `resolve_eq`. „Global" liegt in den Einstellungen.
        let (subject, name, note, scope, key): (
            &'static str,
            String,
            Option<&str>,
            &'static str,
            String,
        ) = match entry {
            CtxTarget::Artist(m) => (
                "den Interpreten",
                m.name.clone(),
                Some("Gilt auch für die Alben und Lieder dieses Interpreten."),
                "artist",
                m.name.clone(),
            ),
            CtxTarget::Album(m) => (
                "das Album",
                m.album.clone(),
                Some("Gilt auch für die Lieder dieses Albums."),
                "album",
                category::album_key(&m.artist, &m.album),
            ),
            CtxTarget::Fs(e) if !e.is_dir() => (
                "den Titel",
                e.display_title(),
                None,
                "track",
                e.path().to_string_lossy().into_owned(),
            ),
            // Ordner: als Interpret oder Album erkennen; sonst kein EQ.
            CtxTarget::Fs(e) => match self.fs_eq_level(e) {
                Some(level) => level,
                None => {
                    self.toast("Equalizer ist hier nicht verfügbar");
                    return;
                }
            },
        };

        self.open_eq_editor(root, sender, subject, &name, note, scope, key);
    }

    /// Globaler Equalizer (aus den Einstellungen): Basis für alles ohne eigene
    /// Festlegung auf Interpret-, Album- oder Titel-Ebene.
    pub(crate) fn open_global_eq(&self, root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) {
        self.open_eq_editor(
            root,
            sender,
            "den globalen Equalizer",
            "",
            Some("Gilt für alles ohne eigene Einstellung für Interpret, Album oder Titel."),
            "global",
            String::new(),
        );
    }

    /// Equalizer-Editor für genau eine Ebene (scope/key) mit Ausgang-Auswahl.
    /// Genutzt vom Detail-EQ (Interpret/Album/Titel) und vom globalen EQ.
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

        // Ausgänge: „Standard (alle)" als Basis + automatisch erkannte Geräte.
        let mut outputs: Vec<(String, String)> =
            vec![("Standard (alle Ausgänge)".to_string(), String::new())];
        for o in crate::core::output::list_outputs() {
            outputs.push((o.name, o.id));
        }
        let out_default = outputs
            .iter()
            .position(|(_, id)| !id.is_empty() && *id == self.active_output)
            .unwrap_or(0);

        // Bänder je Ausgang vorladen (kein DB-Zugriff in den Closures).
        let preloaded: Vec<[f64; 10]> = outputs
            .iter()
            .map(|(_, oid)| self.library.get_eq(oid, scope, &key).ok().flatten().unwrap_or([0.0; 10]))
            .collect();

        let outputs = Rc::new(outputs);
        let bands = Rc::new(RefCell::new(preloaded));
        let cur_out = Rc::new(Cell::new(out_default));
        let key = Rc::new(key);
        let loading = Rc::new(Cell::new(false));

        let dialog = adw::Dialog::builder()
            .title("Equalizer")
            .content_width(440)
            .content_height(620)
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Kopf: „Einstellungen für …" dezent, der Name darunter zentriert und
        // hervorgehoben. Beim globalen EQ (ohne Namen) trägt der Präfix selbst die
        // Überschrift.
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
            .label(format!("Einstellungen für {subject}"))
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
                .label(n)
                .halign(gtk::Align::Center)
                .justify(gtk::Justification::Center)
                .wrap(true)
                .css_classes(["dim-label", "caption"])
                .build();
            header.append(&note_label);
        }
        content.append(&header);

        // Ausgang-Auswahl (eigene Gruppe ohne Titel – Kopf steht darüber).
        let sel_group = adw::PreferencesGroup::new();

        let out_labels: Vec<&str> = outputs.iter().map(|(l, _)| l.as_str()).collect();
        let out_combo = adw::ComboRow::builder()
            .title("Ausgang")
            .subtitle("Gerät / Bluetooth")
            .model(&gtk::StringList::new(&out_labels))
            .build();
        out_combo.set_selected(out_default as u32);
        sel_group.add(&out_combo);
        content.append(&sel_group);

        // Zehn Frequenzregler.
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

        // Reglerbewegung → Wert merken + speichern (+ live anwenden via Msg).
        for (i, scale) in scales.iter().enumerate() {
            let bands = bands.clone();
            let cur_out = cur_out.clone();
            let loading = loading.clone();
            let outputs = outputs.clone();
            let key = key.clone();
            let sender = sender.clone();
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
            });
        }

        // Ausgang wechseln → Regler aus den Vorlade-Werten neu laden.
        {
            let bands = bands.clone();
            let cur_out = cur_out.clone();
            let loading = loading.clone();
            let scales = scales.clone();
            out_combo.connect_selected_notify(move |c| {
                cur_out.set(c.selected() as usize);
                loading.set(true);
                let arr = bands.borrow()[cur_out.get()];
                for (i, sc) in scales.iter().enumerate() {
                    sc.set_value(arr[i]);
                }
                loading.set(false);
            });
        }

        // Aktuelle Auswahl neutralstellen und auf „erben" zurücksetzen.
        let reset = gtk::Button::builder()
            .label("Zurücksetzen")
            .css_classes(["pill"])
            .halign(gtk::Align::Center)
            .build();
        {
            let bands = bands.clone();
            let cur_out = cur_out.clone();
            let loading = loading.clone();
            let scales = scales.clone();
            let outputs = outputs.clone();
            let key = key.clone();
            let sender = sender.clone();
            reset.connect_clicked(move |_| {
                let o = cur_out.get();
                bands.borrow_mut()[o] = [0.0; 10];
                loading.set(true);
                for sc in scales.iter() {
                    sc.set_value(0.0);
                }
                loading.set(false);
                let (_, oid) = &outputs[o];
                sender.input(Msg::ClearEq {
                    output: oid.clone(),
                    scope,
                    key: (*key).clone(),
                });
            });
        }
        content.append(&reset);

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
}
