//! Konzerte: markierte Live-/Unplugged-Aufnahmen auflisten und der
//! Import-Dialog für erkannte Kandidaten. Aus app.rs herausgelöst – reine
//! Umordnung, kein Funktionswechsel.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::ui::app::{App, Msg};

impl App {
    /// Lädt die Konzerte und baut die Liste neu auf. Quelle ist die **Vereinigung**
    /// aus importierten Markierungen (concert-Tabelle) und allen Inhalten, deren
    /// Eigenschaften den Bereich „Konzerte" enthalten (Alben/Ordner/Titel) –
    /// dedupliziert nach (scope, key), mit Album-/Interpreten-Cover.
    pub(crate) fn load_concerts(&mut self, sender: &ComponentSender<Self>) {
        use std::collections::HashSet;
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut items: Vec<(String, String, String, bool)> = Vec::new();

        // 1) Importierte/markierte Konzerte (Pfad → Ordner bzw. Titel).
        for (path, title, is_dir) in self.library.concerts().unwrap_or_default() {
            let scope = if is_dir { "folder" } else { "track" };
            if seen.insert((scope.to_string(), path.clone())) {
                items.push((scope.to_string(), path, title, is_dir));
            }
        }
        // 2) Über die Eigenschaften als „Konzerte" markierte Inhalte (live).
        for entry in
            self.library
                .area_entries(crate::core::category::Area::Concerts, true, false)
        {
            if seen.insert((entry.0.clone(), entry.1.clone())) {
                items.push(entry);
            }
        }

        self.concert_items = items;
        let items = self.concert_items.clone();
        self.fill_entry_list(
            &self.concerts_list,
            &items,
            sender,
            Msg::PlayConcert,
            Some(Msg::ConcertRemove),
            Msg::ShowConcertDetail,
            None,
        );
    }

    /// Import-Dialog: Liste der Kandidaten zum Markieren + „Hinzufügen".
    pub(crate) fn open_concert_import_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        candidates: Vec<crate::core::concert::Candidate>,
    ) {
        use std::rc::Rc;

        let dialog = adw::Dialog::builder()
            .title("Konzerte importieren")
            .content_width(440)
            .content_height(560)
            .build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Alle-auswählen-Schalter
        let all_group = adw::PreferencesGroup::new();
        let all = adw::SwitchRow::builder()
            .title("Alle auswählen")
            .active(true)
            .build();
        all_group.add(&all);
        content.append(&all_group);

        // Kandidaten
        let group = adw::PreferencesGroup::builder()
            .title(format!("{} Kandidaten", candidates.len()))
            .build();
        let mut rows = Vec::with_capacity(candidates.len());
        for c in candidates {
            let row = adw::SwitchRow::builder()
                .title(gtk::glib::markup_escape_text(&c.title))
                .subtitle(gtk::glib::markup_escape_text(&c.subtitle))
                .active(true)
                .build();
            group.add(&row);
            rows.push((c, row));
        }
        content.append(&group);
        let rows = Rc::new(rows);

        {
            let rows = rows.clone();
            all.connect_active_notify(move |s| {
                let on = s.is_active();
                for (_, r) in rows.iter() {
                    r.set_active(on);
                }
            });
        }

        let add = gtk::Button::builder()
            .label("Hinzufügen")
            .css_classes(["suggested-action", "pill"])
            .hexpand(true)
            .build();
        {
            let rows = rows.clone();
            let sender = sender.clone();
            let dialog = dialog.clone();
            add.connect_clicked(move |_| {
                let selected: Vec<(String, String, bool)> = rows
                    .iter()
                    .filter(|(_, r)| r.is_active())
                    .map(|(c, _)| (c.path.clone(), c.title.clone(), c.is_dir))
                    .collect();
                sender.input(Msg::ConcertAdd(selected));
                dialog.close();
            });
        }

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&content)
            .build();
        let bottom = gtk::Box::builder()
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        bottom.append(&add);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        toolbar.add_bottom_bar(&bottom);
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(root));
    }
}
