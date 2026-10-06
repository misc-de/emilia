//! Settings subpage: the tab bar over the categories, plus the setting
//! messages. Each category page is built in its own module
//! (`app_settings_{view,design,sound,meta,menu}.rs`, the MCP group in
//! `app_settings_mcp.rs`). Split out of app_dialogs.rs – pure reordering, no
//! functional change.

use std::path::PathBuf;

use crate::i18n::gettext;
use crate::ui::app::{App, Msg};
use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

/// Navigation tag of the settings subpage — used to recognise it again (a
/// second tap on the settings button, a theme flip that rebuilds it).
pub(crate) const SETTINGS_TAG: &str = "settings";

/// Pin an `adw::SpinRow`'s value + "−/+" buttons flush right, as narrow as the
/// digits. libadwaita's row template gives its `GtkSpinButton` `hexpand`, so
/// it takes the row's spare width: the whole right part became a (tinted, see
/// `theme.rs`) slab, and once the inner `gtk::Text` (`hexpand` too) stopped
/// stretching, the "−/+" ended wherever the title happened to leave them — at
/// a different x in every row, never at the edge. Now the spin button is
/// end-aligned inside that allocation, so the trio sits at the row's end in
/// every row, with the text sized to a fixed few chars and the value
/// right-aligned against the "−" button.
pub(crate) fn narrow_spin_value(row: &adw::SpinRow) {
    fn find<T: IsA<gtk::Widget>>(w: &gtk::Widget) -> Option<T> {
        if let Ok(t) = w.clone().downcast::<T>() {
            return Some(t);
        }
        let mut child = w.first_child();
        while let Some(c) = child {
            if let Some(t) = find::<T>(&c) {
                return Some(t);
            }
            child = c.next_sibling();
        }
        None
    }
    let Some(spin) = find::<gtk::SpinButton>(row.upcast_ref()) else {
        return;
    };
    // Keep the template's `hexpand` (the row's title box does not take the
    // spare width on its own — with the spin button not expanding either, the
    // trio just trailed the title with the slack to its right) and let
    // `halign` place the natural-width trio at the far end of that allocation.
    spin.set_halign(gtk::Align::End);
    if let Some(text) = find::<gtk::Text>(spin.upcast_ref()) {
        text.set_hexpand(false);
        text.set_halign(gtk::Align::End);
        // Wide enough for every range here ("-50", "150"); the same width in
        // all rows keeps the digits' column steady.
        text.set_width_chars(4);
        text.set_max_width_chars(4);
        text.set_alignment(1.0);
    }
}

/// Index of `current` in a combo row's value list, for its preselection; an
/// unknown value selects the first entry.
pub(crate) fn combo_index<T: PartialEq>(items: &[T], current: &T) -> u32 {
    items.iter().position(|i| i == current).unwrap_or(0) as u32
}

/// Value behind a combo row's selected index; `fallback` for an index outside
/// the list (e.g. `gtk::INVALID_LIST_POSITION`).
pub(crate) fn combo_item<T: Copy>(items: &[T], selected: u32, fallback: T) -> T {
    items.get(selected as usize).copied().unwrap_or(fallback)
}

/// The remembered settings category, if it still names one of `names`.
fn known_page(saved: Option<String>, names: &[&str]) -> Option<String> {
    saved.filter(|n| names.iter().any(|name| name == n))
}

impl App {
    pub(crate) fn open_settings(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        // Already open? A second tap on the settings button must not stack a
        // second copy of the page onto the navigation.
        if self
            .nav
            .nav_view
            .visible_page()
            .and_then(|p| p.tag())
            .is_some_and(|t| t == SETTINGS_TAG)
        {
            return;
        }

        // The former "Library" page (music folder, extra sources, Nextcloud
        // connect) was removed: all of that is now managed directly on the Files
        // page – the "+" adds a folder/Nextcloud, and a tab's context menu
        // renames/edits/removes a source (the "Music" tab changes the primary
        // folder).

        let sound_page = self.build_sound_page(sender);
        let search_page = self.build_meta_page(sender);
        let view_page = self.build_view_page(root, sender);
        let design_page = self.build_design_page(root, sender);
        let menu_page = self.build_menu_page(sender);
        // (The former "Cache" page with the stream recording timeshift buffer
        // was removed from the settings.)
        let hidden_page = self.build_hidden_page(sender);

        // The categories are no longer the sidebar of a modal: they sit in a
        // linked tab bar over a stack, the same tab menu the other sections use
        // (Podcasts, Streaming, Files …), and the whole thing is pushed as an
        // ordinary subpage. Order: "View" first.
        let pages: [(&adw::PreferencesPage, &str, String, &str); 6] = [
            (&view_page, "view", gettext("View"), "view-list-symbolic"),
            (
                &design_page,
                "design",
                gettext("Design"),
                "applications-graphics-symbolic",
            ),
            (
                &sound_page,
                "sound",
                gettext("Sound"),
                "audio-speakers-symbolic",
            ),
            (
                &search_page,
                "meta",
                gettext("Meta/Lib"),
                "system-search-symbolic",
            ),
            (&menu_page, "menu", gettext("Menu"), "open-menu-symbolic"),
            (
                &hidden_page,
                "hidden",
                gettext("Hidden"),
                "view-conceal-symbolic",
            ),
        ];
        let names: Vec<&str> = pages.iter().map(|(_, name, _, _)| *name).collect();

        // Reopen on the category last viewed (an unknown/absent name falls back
        // to the first tab), and remember it on every switch.
        let saved = self
            .library
            .get_setting("settings_last_page")
            .ok()
            .flatten();
        let last = known_page(saved, &names).unwrap_or_else(|| "view".to_string());

        // Six categories never fit as labels on a phone, so the narrow layout
        // shows the icons alone — the tooltip still spells the category out.
        let content = settings_tabs(pages, &last, self.nav.narrow.get(), sender);
        // Same tab carousel as the other categories. The page is full of
        // sliders and switches, but those keep their own horizontal drags —
        // `attach_tab_swipe` ignores a press that starts on one.
        crate::ui::app::attach_tab_swipe(&content);
        let page = self.push_subpage_self_scrolling(&gettext("Settings"), SETTINGS_TAG, &content);

        // Leaving the page drops the yt-dlp status widgets a running probe or
        // download would otherwise keep updating.
        {
            let status_slot = self.youtube.settings_status.clone();
            let btn_slot = self.youtube.settings_dl_btn.clone();
            page.connect_hidden(move |_| {
                *status_slot.borrow_mut() = None;
                *btn_slot.borrow_mut() = None;
            });
        }
    }
}

/// The category tab bar over a stack of `pages`, preselecting `last`.
fn settings_tabs(
    pages: [(&adw::PreferencesPage, &str, String, &str); 6],
    last: &str,
    icons_only: bool,
    sender: &ComponentSender<App>,
) -> gtk::Box {
    let stack = gtk::Stack::builder().vexpand(true).build();
    let tab_bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(2)
        .margin_bottom(4)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["linked", "emilia-tabbar", "emilia-settings-tabs"])
        .build();
    let mut leader: Option<gtk::ToggleButton> = None;
    for (page, name, title, icon) in pages {
        stack.add_named(page, Some(name));
        let btn = gtk::ToggleButton::builder()
            .hexpand(true)
            .tooltip_text(&title)
            .build();
        if icons_only {
            btn.set_icon_name(icon);
        } else {
            // Labels only, like the other tab bars (Podcasts, Streaming …):
            // six categories plus their icons leave so little room per tab
            // that every name ends up an ellipsis. Ellipsizing anyway, so a
            // long name can't push the bar (and with it the window) wider
            // than the screen.
            let label = gtk::Label::new(Some(&title));
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            btn.set_child(Some(&label));
        }
        match &leader {
            Some(l) => btn.set_group(Some(l)),
            None => leader = Some(btn.clone()),
        }
        // Preselect BEFORE connecting, so restoring the last category does
        // not already count as a switch.
        btn.set_active(name == last);
        {
            let sender = sender.clone();
            let stack = stack.clone();
            let name = name.to_string();
            btn.connect_toggled(move |b| {
                if b.is_active() {
                    stack.set_visible_child_name(&name);
                    sender.input(Msg::Setting(SettingMsg::SetLastSettingsPage(name.clone())));
                }
            });
        }
        tab_bar.append(&btn);
    }
    stack.set_visible_child_name(last);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    content.append(&tab_bar);
    content.append(&stack);
    content
}

/// `Msg` sub-enum of the setting domain (split out of `App::update`).
#[derive(Debug)]
pub(crate) enum SettingMsg {
    SetMusicDir(PathBuf),
    /// The first-run setup assistant completed: persist the chosen language,
    /// music folder and enabled menu items, then scan (or restart for a language
    /// change).
    SetupFinished {
        lang_code: String,
        music_dir: PathBuf,
        enabled_sections: Vec<String>,
    },
    SetAcoustidKey(String),
    SetFanartKey(String),
    /// Turn the automatic online fetch on/off.
    SetAutoEnrich(bool),
    /// Change the display language ("system"/"de"/"en"); restarts the app.
    SetLanguage(String),
    /// Remember the last opened settings category (page name) so the settings
    /// dialog reopens on it.
    SetLastSettingsPage(String),
    /// Gapless playback on/off (settings); persisted + pushed to the player.
    SetGapless(bool),
    /// Crossfade window in seconds (settings); persisted + pushed to the player.
    SetCrossfade(f64),
    /// How compound artist credits are listed (settings); persisted + applied
    /// to the splitting, then the artist views are rebuilt.
    SetArtistCreditMode(crate::core::artist::CreditMode),
    /// Show/hide a navigation menu item (stack name).
    SetSectionVisible {
        section: &'static str,
        visible: bool,
    },
    /// Move a menu item in the order (indices in `section_order`).
    MoveSection {
        from: usize,
        to: usize,
    },
    /// Show a hidden content again (reset the override).
    UnhideEntry {
        scope: String,
        key: String,
    },
    /// Set a property of a level (or with `None` reset to "inherit").
    /// Set the areas (properties) of a level; empty value = hidden.
    SetAreas {
        scope: &'static str,
        key: String,
        value: String,
    },
    /// Override an album's classification (Singles/Compilations) from the album
    /// context menu; `kind` = `None` reverts to the automatic heuristic.
    SetAlbumKind {
        album: String,
        kind: Option<crate::model::AlbumKind>,
    },
}

impl App {
    /// Dispatch for [`SettingMsg`] (the former `App::update` arms, moved verbatim).
    pub(crate) fn update_setting(
        &mut self,
        msg: SettingMsg,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        match msg {
            SettingMsg::SetMusicDir(path) => self.on_set_music_dir(path, sender),
            SettingMsg::SetupFinished {
                lang_code,
                music_dir,
                enabled_sections,
            } => self.on_setup_finished(lang_code, music_dir, enabled_sections, sender),
            SettingMsg::SetAcoustidKey(key) => {
                let key = key.trim().to_string();
                let _ = self.library.set_secret_setting("acoustid_key", &key);
                self.enrich_state.acoustid_key = if key.is_empty() { None } else { Some(key) };
            }
            SettingMsg::SetFanartKey(key) => {
                let key = key.trim().to_string();
                let _ = self.library.set_secret_setting("fanart_key", &key);
                self.enrich_state.fanart_key = if key.is_empty() { None } else { Some(key) };
            }
            SettingMsg::SetAutoEnrich(on) => {
                self.enrich_state.auto_enrich = on;
                let _ = self
                    .library
                    .set_setting("auto_enrich", if on { "1" } else { "0" });
            }
            SettingMsg::SetLanguage(lang) => self.on_set_language(lang, root),
            SettingMsg::SetLastSettingsPage(name) => {
                let _ = self.library.set_setting("settings_last_page", &name);
            }
            SettingMsg::SetGapless(on) => {
                self.settings.gapless = on;
                let _ = self
                    .library
                    .set_setting("gapless", if on { "1" } else { "0" });
                self.apply_playback_prefs();
            }
            SettingMsg::SetCrossfade(secs) => {
                self.settings.crossfade_secs = secs.clamp(0.0, 12.0);
                let _ = self
                    .library
                    .set_setting("crossfade_secs", &self.settings.crossfade_secs.to_string());
                self.apply_playback_prefs();
            }
            SettingMsg::SetArtistCreditMode(mode) => {
                self.settings.artist_credit_mode = mode;
                let _ = self.library.set_setting("artist_credit_mode", mode.key());
                crate::core::artist::set_credit_mode(mode);
                // The mode decides which names exist at all, so the whole
                // artist overview (and its counts) has to be rebuilt.
                self.reload_artists();
            }
            SettingMsg::SetAreas { scope, key, value } => self.set_areas(sender, scope, key, value),
            SettingMsg::SetAlbumKind { album, kind } => {
                match kind {
                    Some(k) => {
                        let _ = self.library.set_album_kind(&album, k);
                    }
                    None => {
                        let _ = self.library.clear_album_kind(&album);
                    }
                }
                // Refresh all three album views so the moved album appears in its
                // new category (and disappears from the old one).
                self.reload_albums();
                self.reload_singles();
                self.reload_compilations();
            }
            SettingMsg::SetSectionVisible { section, visible } => {
                // The YouTube section is the opt-in feature; its menu switch is now
                // the single enable/disable control, so route it through
                // `set_youtube_enabled` (keeps the `youtube_enabled` flag + the
                // background channel load in step). All other sections just toggle
                // their menu visibility.
                if section == "youtube" {
                    self.set_youtube_enabled(visible, sender);
                } else {
                    self.set_section_visible(section, visible);
                }
            }
            SettingMsg::MoveSection { from, to } => {
                if from < self.nav.section_order.len()
                    && to < self.nav.section_order.len()
                    && from != to
                {
                    let name = self.nav.section_order.remove(from);
                    self.nav.section_order.insert(to, name);
                    let value = self.nav.section_order.join(",");
                    let _ = self.library.set_setting("section_order", &value);
                    // Apply the order to the existing buttons.
                    self.apply_section_order();
                }
            }
            SettingMsg::UnhideEntry { scope, key } => {
                // Delete the override → back to default (visible again).
                let _ = self.library.set_category(&scope, &key, None);
                self.reload_library_overviews();
                self.load_concerts(sender);
                self.load_audiobooks(sender);
                self.load_dir(sender);
                self.toast(&gettext("Shown again"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{combo_index, combo_item, known_page};

    #[test]
    fn combo_index_finds_value_or_falls_back_to_first() {
        let codes = ["system", "dark", "light"];
        assert_eq!(combo_index(&codes, &"dark"), 1);
        assert_eq!(combo_index(&codes, &"light"), 2);
        assert_eq!(combo_index(&codes, &"sepia"), 0);
        assert_eq!(combo_index::<&str>(&[], &"dark"), 0);
    }

    #[test]
    fn combo_item_maps_index_or_falls_back() {
        let codes = ["system", "dark", "light"];
        assert_eq!(combo_item(&codes, 0, "system"), "system");
        assert_eq!(combo_item(&codes, 2, "system"), "light");
        assert_eq!(combo_item(&codes, 3, "system"), "system");
        // `gtk::INVALID_LIST_POSITION` (nothing selected).
        assert_eq!(combo_item(&codes, u32::MAX, "system"), "system");
    }

    #[test]
    fn known_page_keeps_only_existing_categories() {
        let names = ["view", "design", "sound"];
        assert_eq!(
            known_page(Some("sound".into()), &names),
            Some("sound".to_string())
        );
        assert_eq!(known_page(Some("cache".into()), &names), None);
        assert_eq!(known_page(Some(String::new()), &names), None);
        assert_eq!(known_page(None, &names), None);
    }
}
