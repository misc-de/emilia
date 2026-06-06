//! Concerts: listing marked live/unplugged recordings and the import dialog for
//! detected candidates. Extracted from app.rs – pure reorganization, no change
//! in functionality.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, ngettext_n};
use crate::ui::app::{App, Msg};

impl App {
    /// Loads the concerts and rebuilds the list. The only source is the content
    /// whose **properties** include the "Concerts" area (albums/folders/tracks).
    /// This way a concert can be removed again solely via the properties (detail
    /// view). Marked folders are resolved into their albums/individual pieces (no
    /// folder entry).
    pub(crate) fn load_concerts(&mut self, sender: &ComponentSender<Self>) {
        let raw = self
            .library
            .area_entries(crate::core::category::Area::Concerts, true, false);
        let mut items = self.expand_area_items(raw);
        self.sort_entries("concerts", &mut items);
        self.concerts.concert_items = items.clone();
        if self.libview.gallery_view {
            let tiles = self.entry_gallery_items(&items);
            self.fill_gallery(
                &self.concerts.concerts_gallery,
                &tiles,
                Msg::OpenConcertEntry,
                Msg::ShowConcertDetail,
            );
        } else {
            self.fill_entry_list(
                &self.concerts.concerts_list,
                &items,
                sender,
                Msg::PlayConcert,
                // No trash button in the concert list – removal goes via the
                // properties (deselect the "Concerts" area).
                None,
                Msg::ShowConcertDetail,
                None,
                false,
                true,
            );
        }
    }

    /// Import dialog: list of candidates to mark + "Add".
    pub(crate) fn open_concert_import_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        candidates: Vec<crate::core::concert::Candidate>,
    ) {
        use std::rc::Rc;

        let dialog = adw::Dialog::builder()
            .title(gettext("Import concerts"))
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

        // Select-all switch
        let all_group = adw::PreferencesGroup::new();
        let all = adw::SwitchRow::builder()
            .title(gettext("Select all"))
            .active(true)
            .build();
        all_group.add(&all);
        content.append(&all_group);

        // Candidates
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
            .label(gettext("Add"))
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

    /// Scan the primary music folder for likely concert recordings (background).
    pub(crate) fn concert_import(&mut self, sender: &ComponentSender<Self>) {
        let Some(root) = self.files.music_dir.as_ref().map(std::path::PathBuf::from) else {
            self.toast(&gettext("No music folder set"));
            return;
        };
        let existing = self.library.concert_paths().unwrap_or_default();
        self.toast(&gettext("Searching for concerts …"));
        sender.spawn_oneshot_command(move || {
            crate::ui::app::Cmd::Candidates(crate::core::concert::scan_candidates(&root, &existing))
        });
    }

    /// Add the chosen candidates as concerts (table + Concerts area markers on
    /// the contained albums/tracks, so they can be removed via the properties).
    pub(crate) fn concert_add(
        &mut self,
        sender: &ComponentSender<Self>,
        items: Vec<(String, String, bool)>,
    ) {
        let n = items.len();
        for (path, title, is_dir) in &items {
            // Table: only for the candidate filtering at the next import.
            let _ = self.library.add_concert(path, title, *is_dir);
            let entries = if *is_dir {
                self.folder_albums_and_tracks(path)
            } else {
                vec![("track".to_string(), path.clone(), title.clone(), false)]
            };
            for (scope, key, _, _) in entries {
                let _ = self.library.add_category_area(
                    &scope,
                    &key,
                    crate::core::category::Area::Concerts,
                );
            }
        }
        self.load_concerts(sender);
        self.toast(&ngettext_n(
            "Added {n} concert",
            "Added {n} concerts",
            n as u32,
        ));
    }
}
