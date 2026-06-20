//! Navigation sections: showing/hiding menu items, persisting the hidden
//! set, re-applying button visibility (collapsing the navigation when only a
//! single item is left) and applying the user's section order. Extracted from
//! app.rs – pure reordering, no change in behavior; the methods remain
//! inherent `impl App` methods.

use adw::prelude::*;
use relm4::{adw, gtk};

use crate::ui::app::{App, SECTIONS};

impl App {
    /// Shows/hides a navigation menu item: updates the state,
    /// saves it, toggles all associated buttons (sidebar +
    /// top bar) and, when hiding the active item, switches to the
    /// first visible one.
    pub(crate) fn set_section_visible(&mut self, section: &str, visible: bool) {
        // At least one menu item must stay visible.
        if !visible {
            let visible_count = SECTIONS
                .iter()
                .filter(|(n, _, _)| !self.nav.hidden_sections.contains(*n))
                .count();
            if visible_count <= 1 {
                return;
            }
        }
        if visible {
            self.nav.hidden_sections.remove(section);
        } else {
            self.nav.hidden_sections.insert(section.to_string());
        }
        let value = SECTIONS
            .iter()
            .map(|(n, _, _)| *n)
            .filter(|n| self.nav.hidden_sections.contains(*n))
            .collect::<Vec<_>>()
            .join(",");
        let _ = self.library.set_setting("hidden_sections", &value);

        // Re-apply button visibility and, when only one menu item is left,
        // suppress the navigation entirely (Settings then sits in the title bar).
        self.refresh_nav_visibility();

        // If the currently visible section is hidden, switch to the first
        // visible menu item (in the chosen order).
        if !visible {
            let cur = self.nav.view_stack.visible_child_name();
            if cur.as_deref() == Some(section) {
                if let Some(next) = self
                    .nav
                    .section_order
                    .iter()
                    .copied()
                    .find(|n| !self.nav.hidden_sections.contains(*n))
                {
                    self.nav.view_stack.set_visible_child_name(next);
                }
            }
        }
    }

    /// Re-applies the navigation visibility: hides the buttons of hidden
    /// sections, and when only a single menu item remains visible suppresses the
    /// whole navigation (sidebar + top bar) and moves Settings into the title
    /// bar (via [`NavState::apply_chrome`]).
    pub(crate) fn refresh_nav_visibility(&self) {
        let visible_count = SECTIONS
            .iter()
            .filter(|(n, _, _)| !self.nav.hidden_sections.contains(*n))
            .count();
        let single = visible_count <= 1;
        self.nav.nav_hidden.set(single);
        for (name, _is_sidebar, btn) in &self.nav.nav_buttons {
            btn.set_visible(!self.nav.hidden_sections.contains(*name) && !single);
        }
        (self.nav.apply_chrome)();
    }

    /// Applies `section_order` to the navigation containers by reordering the
    /// existing buttons (sidebar buttons before the
    /// spacer + "Settings", which stay untouched at the end).
    pub(crate) fn apply_section_order(&self) {
        for sidebar in [true, false] {
            let container = if sidebar {
                &self.nav.sidebar_nav
            } else {
                &self.nav.top_nav
            };
            let mut prev: Option<gtk::Widget> = None;
            for &name in &self.nav.section_order {
                if let Some((_, _, btn)) = self
                    .nav
                    .nav_buttons
                    .iter()
                    .find(|(n, s, _)| *n == name && *s == sidebar)
                {
                    container.reorder_child_after(btn, prev.as_ref());
                    prev = Some(btn.clone().upcast());
                }
            }
        }
    }
}
