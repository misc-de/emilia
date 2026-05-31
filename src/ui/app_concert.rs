//! Konzerte: markierte Live-/Unplugged-Aufnahmen auflisten und der
//! Import-Dialog für erkannte Kandidaten. Aus app.rs herausgelöst – reine
//! Umordnung, kein Funktionswechsel.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::ui::app::{App, Msg};

impl App {
    /// Lädt die markierten Konzerte aus der DB und baut die Liste neu auf.
    pub(crate) fn load_concerts(&mut self, sender: &ComponentSender<Self>) {
        self.concert_items = self.library.concerts().unwrap_or_default();

        while let Some(child) = self.concerts_list.first_child() {
            self.concerts_list.remove(&child);
        }
        for (i, (_, title, is_dir)) in self.concert_items.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(title))
                .subtitle(if *is_dir { "Album" } else { "Datei" })
                .activatable(true)
                .build();
            let icon = if *is_dir {
                "folder-symbolic"
            } else {
                "audio-x-generic-symbolic"
            };
            row.add_prefix(&gtk::Image::from_icon_name(icon));

            // Entfernen-Knopf (Markierung löschen, Dateien bleiben unberührt).
            let remove = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Konzert entfernen")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let sender = sender.clone();
                remove.connect_clicked(move |_| sender.input(Msg::ConcertRemove(i)));
            }
            row.add_suffix(&remove);
            row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));

            {
                let sender = sender.clone();
                row.connect_activated(move |_| sender.input(Msg::PlayConcert(i)));
            }

            // Langes Drücken: Detailansicht – wie unter „Dateisystem".
            let long_press = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                long_press.connect_pressed(move |gesture, _, _| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::ShowConcertDetail(i));
                });
            }
            row.add_controller(long_press);

            self.concerts_list.append(&row);
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
