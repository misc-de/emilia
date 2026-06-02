//! Konzerte: markierte Live-/Unplugged-Aufnahmen auflisten und der
//! Import-Dialog für erkannte Kandidaten. Aus app.rs herausgelöst – reine
//! Umordnung, kein Funktionswechsel.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, ngettext_n};
use crate::ui::app::{App, Msg};

impl App {
    /// Lädt die Konzerte und baut die Liste neu auf. Einzige Quelle sind die
    /// Inhalte, deren **Eigenschaften** den Bereich „Konzerte" enthalten
    /// (Alben/Ordner/Titel). So lässt sich ein Konzert allein über die
    /// Eigenschaften (Detailansicht) wieder entfernen. Markierte Ordner werden
    /// in ihre Alben/Einzelstücke aufgelöst (kein Ordner-Eintrag).
    pub(crate) fn load_concerts(&mut self, sender: &ComponentSender<Self>) {
        let raw = self
            .library
            .area_entries(crate::core::category::Area::Concerts, true, false);
        self.concert_items = self.expand_area_items(raw);
        let items = self.concert_items.clone();
        if self.gallery_view {
            let tiles = self.entry_gallery_items(&items);
            self.fill_gallery(
                &self.concerts_gallery,
                &tiles,
                Msg::OpenConcertEntry,
                Msg::ShowConcertDetail,
            );
        } else {
            self.fill_entry_list(
                &self.concerts_list,
                &items,
                sender,
                Msg::PlayConcert,
                // Kein Mülleimer in der Konzertliste – Entfernen läuft über die
                // Eigenschaften (Bereich „Konzerte" abwählen).
                None,
                Msg::ShowConcertDetail,
                None,
                false,
                true,
            );
        }
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
            .title(&gettext("Import concerts"))
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
            .title(&gettext("Select all"))
            .active(true)
            .build();
        all_group.add(&all);
        content.append(&all_group);

        // Kandidaten
        let group = adw::PreferencesGroup::builder()
            .title(ngettext_n(
                "{n} candidate",
                "{n} candidates",
                candidates.len() as u32,
            ))
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
            .label(&gettext("Add"))
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
