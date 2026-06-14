//! Inline list filter: the header funnel button + a search bar that filters the
//! currently visible list (Files / Artists / Albums in list mode) live, without
//! touching the global search dialog. Kept separate from the sort/search code.

use adw::prelude::*;
use relm4::{adw, ComponentSender};

use crate::ui::app::{App, AppWidgets, Msg};

impl App {
    /// Wires the filter button + search bar (called once from `finish_init`).
    pub(crate) fn setup_inline_filter(
        &mut self,
        widgets: &AppWidgets,
        sender: &ComponentSender<Self>,
    ) {
        self.nav.filter_btn = widgets.filter_btn.clone();
        self.nav.filter_bar = widgets.filter_bar.clone();
        self.nav.filter_entry = widgets.filter_entry.clone();

        // Let the search bar own the entry (Escape closes it, key capture, …).
        widgets.filter_bar.connect_entry(&widgets.filter_entry);

        // The header button reveals / hides the bar …
        {
            let bar = widgets.filter_bar.clone();
            widgets
                .filter_btn
                .connect_toggled(move |b| bar.set_search_mode(b.is_active()));
        }
        // … and the bar keeps the button in step; closing it clears the filter.
        {
            let btn = widgets.filter_btn.clone();
            let sender = sender.clone();
            widgets
                .filter_bar
                .connect_search_mode_enabled_notify(move |bar| {
                    let on = bar.is_search_mode();
                    if btn.is_active() != on {
                        btn.set_active(on);
                    }
                    if !on {
                        sender.input(Msg::InlineFilter(String::new()));
                    }
                });
        }
        // Live filtering as the user types.
        {
            let sender = sender.clone();
            widgets.filter_entry.connect_search_changed(move |e| {
                sender.input(Msg::InlineFilter(e.text().to_string()));
            });
        }
    }

    /// Whether the visible section shows a filterable list (so the funnel button
    /// is offered): Artists/Albums/Concerts/Audiobooks only in list mode (their
    /// gallery tiles are not filtered). The file browser has no filter.
    fn filter_applies(&self) -> bool {
        match self.current_section().as_deref() {
            Some(s @ ("artists" | "albums" | "concerts" | "audiobooks")) => {
                !self.libview.gallery_on(s)
            }
            _ => false,
        }
    }

    /// Shows/hides the funnel button for the current section. When the section is
    /// not filterable, the bar is closed and any leftover filter is cleared.
    /// Called from [`App::rebuild_sort_menu`] on every section change.
    pub(crate) fn update_filter_chrome(&self) {
        let on = self.filter_applies();
        self.nav.filter_btn.set_visible(on);
        if !on {
            self.nav.filter_bar.set_search_mode(false);
            self.clear_inline_filters();
        }
    }

    /// Removes the row filter from every filterable list.
    fn clear_inline_filters(&self) {
        self.libview.entries.widget().set_filter_func(|_| true);
        self.libview.artists.widget().set_filter_func(|_| true);
        self.libview.albums.widget().set_filter_func(|_| true);
        self.concerts.concerts_list.set_filter_func(|_| true);
        self.favorites.audiobooks_list.set_filter_func(|_| true);
    }

    /// Applies the live filter `query` to the currently visible list by matching
    /// each row's title (case-insensitive substring). An empty query clears it.
    pub(crate) fn apply_inline_filter(&self, query: &str) {
        let list = match self.current_section().as_deref() {
            Some("files") => self.libview.entries.widget().clone(),
            Some("artists") => self.libview.artists.widget().clone(),
            Some("albums") => self.libview.albums.widget().clone(),
            Some("concerts") => self.concerts.concerts_list.clone(),
            Some("audiobooks") => self.favorites.audiobooks_list.clone(),
            _ => return,
        };
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            list.set_filter_func(|_| true);
            return;
        }
        list.set_filter_func(move |row| {
            row.downcast_ref::<adw::ActionRow>()
                .map(|r| r.title().to_lowercase().contains(&q))
                .unwrap_or(true)
        });
    }
}
