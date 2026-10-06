//! Settings "Menu" (menu item order/visibility) and "Hidden" (hidden
//! library content) categories. Split out of [`crate::ui::app_settings`] –
//! pure reorganization, no functional change.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::gettext;
use crate::ui::app::{App, Msg, cover_widget};
use crate::ui::app_settings::SettingMsg;

impl App {
    /// Builds the "Menu" settings page.
    pub(super) fn build_menu_page(&self, sender: &ComponentSender<Self>) -> adw::PreferencesPage {
        // --- Category: Menu (manage menu items) ---
        let page = adw::PreferencesPage::builder()
            .title(gettext("Menu"))
            .icon_name("open-menu-symbolic")
            .name("menu")
            .build();
        let sections_group = adw::PreferencesGroup::builder()
            .title(gettext("Menu items"))
            .description(gettext(
                "Drag handle to reorder; the switch hides a menu item.",
            ))
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        // Shared, local state of the dialog (alongside the model).
        let order = std::rc::Rc::new(std::cell::RefCell::new(self.nav.section_order.clone()));
        let hidden = std::rc::Rc::new(std::cell::RefCell::new(self.nav.hidden_sections.clone()));
        rebuild_section_rows(&list, &order, &hidden, sender);
        sections_group.add(&list);
        page.add(&sections_group);
        page
    }

    /// Builds the "Hidden" settings page.
    pub(super) fn build_hidden_page(&self, sender: &ComponentSender<Self>) -> adw::PreferencesPage {
        // --- Category: Hidden (far right) ---
        let page = adw::PreferencesPage::builder()
            .title(gettext("Hidden"))
            .icon_name("view-conceal-symbolic")
            .name("hidden")
            .build();
        let hidden_group = adw::PreferencesGroup::builder()
            .title(gettext("Hidden content"))
            .description(gettext(
                "Artists, albums and tracks whose properties are visible nowhere – each the object that carries the setting. Use the eye to show them again.",
            ))
            .build();
        let entries = self.library.hidden_entries();
        if entries.is_empty() {
            hidden_group.add(
                &adw::ActionRow::builder()
                    .title(gettext("Nothing hidden"))
                    .build(),
            );
        }
        for (scope, key, title, is_dir) in entries {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&title))
                .subtitle(hidden_kind(&scope))
                .build();
            row.add_prefix(&cover_widget(
                self.entry_cover(&scope, &key, is_dir).as_deref(),
                hidden_icon(&scope),
            ));
            let reveal = gtk::Button::builder()
                .icon_name("view-reveal-symbolic")
                .tooltip_text(gettext("Show again"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let sender = sender.clone();
                let group = hidden_group.clone();
                let row = row.clone();
                reveal.connect_clicked(move |_| {
                    sender.input(Msg::Setting(SettingMsg::UnhideEntry {
                        scope: scope.clone(),
                        key: key.clone(),
                    }));
                    group.remove(&row);
                });
            }
            row.add_suffix(&reveal);
            hidden_group.add(&row);
        }
        page.add(&hidden_group);
        page
    }
}

/// Rebuilds the menu item rows (drag handle, label, visibility switch) in the
/// current order. Reorderable by dragging; every change updates the local dialog
/// state (`order`/`hidden`) and reports it to the model, which applies navigation
/// and order immediately.
fn rebuild_section_rows(
    list: &gtk::ListBox,
    order: &std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
    hidden: &std::rc::Rc<std::cell::RefCell<std::collections::HashSet<String>>>,
    sender: &ComponentSender<App>,
) {
    while let Some(c) = list.first_child() {
        list.remove(&c);
    }
    let names: Vec<&'static str> = order.borrow().clone();
    for (idx, &name) in names.iter().enumerate() {
        let Some((label, _icon)) = crate::ui::app::section_meta(name) else {
            continue;
        };
        let row = adw::ActionRow::builder()
            .title(gettext(label))
            .subtitle(crate::ui::app::section_description(name))
            .build();
        row.set_subtitle_lines(2);

        // Drag handle on the left (a hint); the whole row is dragged.
        let handle = gtk::Image::from_icon_name("list-drag-handle-symbolic");
        handle.set_tooltip_text(Some(&gettext("Drag to reorder")));
        row.add_prefix(&handle);

        let drag = gtk::DragSource::new();
        drag.set_actions(gtk::gdk::DragAction::MOVE);
        {
            let name = name.to_string();
            drag.connect_prepare(move |_, _, _| {
                Some(gtk::gdk::ContentProvider::for_value(&name.to_value()))
            });
        }
        row.add_controller(drag);

        // DropTarget on the whole row: move the source to this position.
        let drop = gtk::DropTarget::new(String::static_type(), gtk::gdk::DragAction::MOVE);
        {
            let (list, order, hidden, sender) =
                (list.clone(), order.clone(), hidden.clone(), sender.clone());
            drop.connect_drop(move |_, value, _, _| {
                let Ok(src) = value.get::<String>() else {
                    return false;
                };
                let to = idx;
                let from = order.borrow().iter().position(|n| *n == src.as_str());
                let (Some(from), Some(name_static)) = (
                    from,
                    crate::ui::app::SECTIONS
                        .iter()
                        .map(|(n, _, _)| *n)
                        .find(|n| *n == src.as_str()),
                ) else {
                    return false;
                };
                if from == to {
                    return false;
                }
                {
                    let mut o = order.borrow_mut();
                    o.remove(from);
                    o.insert(to, name_static);
                }
                sender.input(Msg::Setting(SettingMsg::MoveSection { from, to }));
                rebuild_section_rows(&list, &order, &hidden, &sender);
                true
            });
        }
        row.add_controller(drop);

        // Visibility switch on the right.
        let sw = gtk::Switch::builder()
            .active(!hidden.borrow().contains(name))
            .valign(gtk::Align::Center)
            .build();
        {
            let (hidden, sender) = (hidden.clone(), sender.clone());
            sw.connect_active_notify(move |s| {
                // At least one menu item must stay visible.
                if !s.is_active() {
                    let visible = crate::ui::app::SECTIONS
                        .iter()
                        .filter(|(n, _, _)| !hidden.borrow().contains(*n))
                        .count();
                    if visible <= 1 {
                        s.set_active(true);
                        return;
                    }
                }
                if s.is_active() {
                    hidden.borrow_mut().remove(name);
                } else {
                    hidden.borrow_mut().insert(name.to_string());
                }
                sender.input(Msg::Setting(SettingMsg::SetSectionVisible {
                    section: name,
                    visible: s.is_active(),
                }));
            });
        }
        row.add_suffix(&sw);

        list.append(&row);
    }
}

/// Placeholder icon per level in the "Hidden" overview.
fn hidden_icon(scope: &str) -> &'static str {
    match scope {
        "album" => "media-optical-symbolic",
        "artist" => "avatar-default-symbolic",
        "folder" => "folder-symbolic",
        _ => "audio-x-generic-symbolic",
    }
}

/// Subtitle label per level in the "Hidden" overview.
fn hidden_kind(scope: &str) -> String {
    match scope {
        "album" => gettext("Album"),
        "artist" => gettext("Artist"),
        "folder" => gettext("Folder"),
        _ => gettext("Track"),
    }
}

#[cfg(test)]
mod tests {
    use super::{hidden_icon, hidden_kind};

    #[test]
    fn hidden_icon_per_scope() {
        assert_eq!(hidden_icon("album"), "media-optical-symbolic");
        assert_eq!(hidden_icon("artist"), "avatar-default-symbolic");
        assert_eq!(hidden_icon("folder"), "folder-symbolic");
        assert_eq!(hidden_icon("track"), "audio-x-generic-symbolic");
        assert_eq!(hidden_icon(""), "audio-x-generic-symbolic");
    }

    #[test]
    fn hidden_kind_per_scope() {
        assert_eq!(hidden_kind("album"), "Album");
        assert_eq!(hidden_kind("artist"), "Artist");
        assert_eq!(hidden_kind("folder"), "Folder");
        assert_eq!(hidden_kind("track"), "Track");
        assert_eq!(hidden_kind("anything"), "Track");
    }
}
