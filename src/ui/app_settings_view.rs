//! Settings "View" category: display language, scaling, list display, artist
//! credits, the system tray and the MCP server. Split out of
//! [`crate::ui::app_settings`] – pure reorganization, no functional change.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::gettext;
use crate::ui::app::{App, Msg};
use crate::ui::app_settings::{SettingMsg, combo_index, combo_item, narrow_spin_value};
use crate::ui::app_sort::SortMsg;
use crate::ui::app_tray::TrayMsg;
use crate::ui::theme::DesignMsg;

impl App {
    /// Builds the "View" settings page.
    pub(super) fn build_view_page(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) -> adw::PreferencesPage {
        // --- Category: View ---
        let page = adw::PreferencesPage::builder()
            .title(gettext("View"))
            .icon_name("view-list-symbolic")
            .name("view")
            .build();

        // Display language at the very top (takes effect after restarting the app).
        let lang_group = adw::PreferencesGroup::builder()
            .title(gettext("Language"))
            .build();
        // The shared language list ([`crate::i18n::LANGUAGES`], codes + endonyms),
        // with the "System default" choice prepended so it stays on top. The
        // endonyms are shown untranslated; English is the source language.
        let mut lang_codes: Vec<&str> = vec!["system"];
        lang_codes.extend(crate::i18n::LANGUAGES.iter().map(|(c, _)| *c));
        let mut lang_labels: Vec<String> = vec![gettext("System default")];
        lang_labels.extend(crate::i18n::LANGUAGES.iter().map(|(_, l)| (*l).to_string()));
        let lang_label_refs: Vec<&str> = lang_labels.iter().map(String::as_str).collect();
        let lang_row = adw::ComboRow::builder()
            .title(gettext("Display language"))
            .subtitle(gettext("Takes effect after a restart"))
            .model(&gtk::StringList::new(&lang_label_refs))
            .build();
        lang_row.set_selected(combo_index(
            &lang_codes,
            &self.settings.ui_language.as_str(),
        ));
        {
            // Connect the handler only after `set_selected`, so the preselection
            // doesn't trigger a language change.
            let sender = sender.clone();
            lang_row.connect_selected_notify(move |r| {
                let code = combo_item(&lang_codes, r.selected(), "system");
                sender.input(Msg::Setting(SettingMsg::SetLanguage(code.to_string())));
            });
        }
        lang_group.add(&lang_row);
        page.add(&lang_group);

        // Gallery view (cover grid) instead of a list + tiles per row.
        let gallery_group = adw::PreferencesGroup::builder()
            .title(gettext("List display"))
            .build();
        let gallery_row = adw::SwitchRow::builder()
            .title(gettext("Gallery view"))
            .subtitle(gettext("Show lists as a grid of cover thumbnails"))
            .active(self.libview.gallery_view)
            .build();
        {
            let sender = sender.clone();
            gallery_row.connect_active_notify(move |r| {
                sender.input(Msg::Sort(SortMsg::GalleryView(r.is_active())));
            });
        }
        gallery_group.add(&gallery_row);
        let cols_row = adw::SpinRow::builder()
            .title(gettext("Tiles per row"))
            .adjustment(&gtk::Adjustment::new(
                self.libview.gallery_columns as f64,
                2.0,
                8.0,
                1.0,
                1.0,
                0.0,
            ))
            .build();
        {
            let sender = sender.clone();
            cols_row.connect_value_notify(move |r| {
                sender.input(Msg::Sort(SortMsg::GalleryColumns(r.value() as u32)));
            });
        }
        narrow_spin_value(&cols_row);
        gallery_group.add(&cols_row);
        // Added to this "View" page below, right after "Scaling".

        // App scaling (whole UI, not just text): -50% .. +50% in 10% steps.
        let scale_group = adw::PreferencesGroup::builder()
            .title(gettext("Scaling"))
            .build();
        let scale_row = adw::SpinRow::builder()
            .title(gettext("App size"))
            .subtitle(gettext("Scales the whole interface (percent)"))
            .adjustment(&gtk::Adjustment::new(
                (self.theme.ui_scale * 100.0).round(),
                50.0,
                150.0,
                10.0,
                10.0,
                0.0,
            ))
            .build();
        {
            let sender = sender.clone();
            scale_row.connect_value_notify(move |r| {
                sender.input(Msg::Design(DesignMsg::UiScale(r.value() / 100.0)));
            });
        }
        narrow_spin_value(&scale_row);
        scale_group.add(&scale_row);
        // Mobile only: scale just the top-bar menu icons, -50 .. +50 % in 10 %
        // steps. Hidden on desktop, where the sidebar shows icon + label.
        let menu_scale_row = adw::SpinRow::builder()
            .title(gettext("Menu icon size"))
            .subtitle(gettext("Scales the menu icons (mobile, percent)"))
            .adjustment(&gtk::Adjustment::new(
                self.library
                    .get_setting("menu_icon_scale")
                    .ok()
                    .flatten()
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0),
                -50.0,
                50.0,
                10.0,
                10.0,
                0.0,
            ))
            .build();
        menu_scale_row.set_visible(self.nav.narrow.get());
        narrow_spin_value(&menu_scale_row);
        {
            let sender = sender.clone();
            menu_scale_row.connect_value_notify(move |r| {
                sender.input(Msg::Design(DesignMsg::MenuIconScale(r.value() as i32)));
            });
        }
        scale_group.add(&menu_scale_row);
        page.add(&scale_group);
        // "List display" sits right after "Scaling" on the View page.
        page.add(&gallery_group);

        // How compound credits ("A feat. B & C") appear in the artist list.
        // Splitting them is what makes guests discoverable, but it also tears
        // apart band names carrying "&" or a comma – hence the choice.
        let credit_group = adw::PreferencesGroup::builder()
            .title(gettext("Artists"))
            .description(gettext(
                "How artist tags like \"A feat. B\" are listed. The tags themselves are never changed.",
            ))
            .build();
        use crate::core::artist::CreditMode;
        let credit_modes = [CreditMode::Split, CreditMode::Primary, CreditMode::Raw];
        let credit_labels = [
            gettext("List guests separately"),
            gettext("Main artist only"),
            gettext("Exactly as tagged"),
        ];
        let credit_label_refs: Vec<&str> = credit_labels.iter().map(String::as_str).collect();
        let credit_row = adw::ComboRow::builder()
            .title(gettext("Featured artists"))
            .subtitle(gettext(
                "\"List guests separately\" gives every guest their own entry",
            ))
            .model(&gtk::StringList::new(&credit_label_refs))
            .build();
        credit_row.set_selected(combo_index(
            &credit_modes,
            &self.settings.artist_credit_mode,
        ));
        {
            // Connect only after `set_selected`, so the preselection doesn't
            // trigger a rebuild.
            let sender = sender.clone();
            credit_row.connect_selected_notify(move |r| {
                let mode = combo_item(&credit_modes, r.selected(), CreditMode::default());
                sender.input(Msg::Setting(SettingMsg::SetArtistCreditMode(mode)));
            });
        }
        credit_group.add(&credit_row);
        page.add(&credit_group);

        page.add(&self.build_tray_group(sender));

        page.add(&self.build_mcp_group(root, sender));

        page
    }

    /// "System tray" group of the View page: tray icon + window behavior.
    fn build_tray_group(&self, sender: &ComponentSender<Self>) -> adw::PreferencesGroup {
        // System: optional desktop tray icon + window behavior. There is no tray
        // on a phone, so the whole group stays hidden in the narrow layout.
        let tray_group = adw::PreferencesGroup::builder()
            .title(gettext("System tray"))
            .visible(!self.nav.narrow.get())
            .build();
        let tray_enabled_row = adw::SwitchRow::builder()
            .title(gettext("Show tray icon"))
            .active(self.tray.enabled)
            .build();
        tray_group.add(&tray_enabled_row);
        let tray_close_row = adw::SwitchRow::builder()
            .title(gettext("Close to tray"))
            .subtitle(gettext("Closing the window keeps it running in the tray"))
            .active(self.tray.close_hides)
            .build();
        {
            let sender = sender.clone();
            tray_close_row.connect_active_notify(move |r| {
                sender.input(Msg::Tray(TrayMsg::SetCloseHides(r.is_active())));
            });
        }
        tray_group.add(&tray_close_row);
        let tray_hidden_row = adw::SwitchRow::builder()
            .title(gettext("Start hidden"))
            .subtitle(gettext("Start in the tray without showing the window"))
            .active(self.tray.start_hidden)
            .build();
        {
            let sender = sender.clone();
            tray_hidden_row.connect_active_notify(move |r| {
                sender.input(Msg::Tray(TrayMsg::SetStartHidden(r.is_active())));
            });
        }
        tray_group.add(&tray_hidden_row);
        let tray_skip_row = adw::SwitchRow::builder()
            .title(gettext("No taskbar entry"))
            .subtitle(gettext("Hide from the taskbar even when visible (X11)"))
            .active(self.tray.skip_taskbar)
            .build();
        {
            let sender = sender.clone();
            tray_skip_row.connect_active_notify(move |r| {
                sender.input(Msg::Tray(TrayMsg::SetSkipTaskbar(r.is_active())));
            });
        }
        tray_group.add(&tray_skip_row);
        let tray_gray_row = adw::SwitchRow::builder()
            .title(gettext("Gray tray icon"))
            .subtitle(gettext("Show the tray icon desaturated"))
            .active(self.tray.icon_gray)
            .build();
        {
            let sender = sender.clone();
            tray_gray_row.connect_active_notify(move |r| {
                sender.input(Msg::Tray(TrayMsg::SetIconGray(r.is_active())));
            });
        }
        tray_group.add(&tray_gray_row);

        // The remaining tray options only make sense with the icon on: show them
        // only while "Show tray icon" is active, and toggle them live with it.
        for row in [
            &tray_close_row,
            &tray_hidden_row,
            &tray_skip_row,
            &tray_gray_row,
        ] {
            row.set_visible(self.tray.enabled);
        }
        {
            let sender = sender.clone();
            let dependents = [
                tray_close_row.clone(),
                tray_hidden_row.clone(),
                tray_skip_row.clone(),
                tray_gray_row.clone(),
            ];
            tray_enabled_row.connect_active_notify(move |r| {
                let on = r.is_active();
                sender.input(Msg::Tray(TrayMsg::SetEnabled(on)));
                for row in &dependents {
                    row.set_visible(on);
                }
            });
        }
        tray_group
    }
}
