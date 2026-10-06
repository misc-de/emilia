//! Settings "Meta/Lib" category: online metadata fetch, the AcoustID and
//! fanart.tv keys and the yt-dlp tool management. Split out of
//! [`crate::ui::app_settings`] – pure reorganization, no functional change.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, gettext_f};
use crate::ui::app::{App, Msg};
use crate::ui::app_settings::SettingMsg;
use crate::ui::app_yt_glue::YtMsg;

impl App {
    /// Builds the "Meta/Lib" settings page. Also registers the yt-dlp status
    /// row + button in `self.youtube` (cleared again when the settings page is
    /// hidden, see [`App::open_settings`]) and starts the version probe.
    pub(super) fn build_meta_page(&self, sender: &ComponentSender<Self>) -> adw::PreferencesPage {
        // --- Category: Meta/Lib (read online metadata + the YouTube tool) ---
        let page = adw::PreferencesPage::builder()
            .title(gettext("Meta/Lib"))
            .icon_name("system-search-symbolic")
            .name("meta")
            .build();

        // 1. Automatic fetch (first option).
        let auto_group = adw::PreferencesGroup::builder()
            .title(gettext("Read music data"))
            .description(gettext(
                "Complete missing cover art, photos and tracks from open online sources.",
            ))
            .build();
        let auto_row = adw::SwitchRow::builder()
            .title(gettext("Fetch automatically"))
            .subtitle(gettext(
                "Loads missing data in the background at startup – on any connection.",
            ))
            .active(self.enrich_state.auto_enrich)
            .build();
        {
            let sender = sender.clone();
            auto_row.connect_active_notify(move |r| {
                sender.input(Msg::Setting(SettingMsg::SetAutoEnrich(r.is_active())));
            });
        }
        auto_group.add(&auto_row);
        page.add(&auto_group);

        // 2. AcoustID.
        let acoustid_group = adw::PreferencesGroup::builder()
            .title(gettext("AcoustID"))
            .description(gettext(
                "Optional key for fingerprint-based track detection (free at acoustid.org/new-application).",
            ))
            .build();
        let key_row = adw::EntryRow::builder()
            .title(gettext("AcoustID API key"))
            .build();
        key_row.set_text(self.enrich_state.acoustid_key.as_deref().unwrap_or(""));
        key_row.set_show_apply_button(true);
        crate::ui::widgets::no_autofocus(&key_row);
        {
            let sender = sender.clone();
            key_row.connect_apply(move |r| {
                sender.input(Msg::Setting(SettingMsg::SetAcoustidKey(
                    r.text().to_string(),
                )));
            });
        }
        acoustid_group.add(&key_row);
        page.add(&acoustid_group);

        // 3. fanart.tv.
        let fanart_group = adw::PreferencesGroup::builder()
            .title(gettext("fanart.tv"))
            .description(gettext("Optional key for showing several artist photos."))
            .build();
        let fanart_row = adw::EntryRow::builder()
            .title(gettext("fanart.tv API key"))
            .build();
        fanart_row.set_text(self.enrich_state.fanart_key.as_deref().unwrap_or(""));
        fanart_row.set_show_apply_button(true);
        crate::ui::widgets::no_autofocus(&fanart_row);
        {
            let sender = sender.clone();
            fanart_row.connect_apply(move |r| {
                sender.input(Msg::Setting(SettingMsg::SetFanartKey(r.text().to_string())));
            });
        }
        fanart_group.add(&fanart_row);
        page.add(&fanart_group);

        // --- Device synchronization: hidden in the settings
        //     (the feature stays reachable via the share button). ---

        // YouTube (optional feature; the extractor yt-dlp is downloaded at
        // runtime, never bundled, and the feature is off by default). Lives on
        // the "Meta" page (added at the bottom of it).
        // Enabling/disabling the YouTube *section* is done via the menu settings
        // (the "youtube" menu switch doubles as the feature toggle), so there is no
        // separate "Enable YouTube" switch here – only the yt-dlp tool management.
        let yt_group = adw::PreferencesGroup::builder()
            .title(gettext("YouTube"))
            .description(gettext(
                "YouTube uses the bundled yt-dlp tool. Since YouTube frequently breaks older versions, you can update it to a newer one here. Turn the YouTube section itself on under Menu. May be restricted in some countries.",
            ))
            .build();

        // The status (version / progress) goes into the row **subtitle** – a
        // second line below the "yt-dlp" title – instead of a suffix label next to
        // the button. On narrow (mobile) screens a suffix label crowded the button;
        // a subtitle wraps cleanly under the title.
        // Probing the installed version spawns `yt-dlp --version` (a Python zipapp
        // whose import takes a second or more on a phone). NEVER do that on the UI
        // thread while building the dialog – it would freeze the settings open for
        // seconds. Show the cached value (or the busy text) and run the probe in the
        // background; `Cmd::YtDlpChecked` updates the row when it finishes. (Reuses
        // the already-translated "Working …" string rather than a new one.)
        let cached = self.youtube.ytdlp_version.clone();
        let ytdlp_row = adw::ActionRow::builder()
            .title("yt-dlp")
            .subtitle(match &cached {
                Some(v) => gettext_f("Installed (version {v})", &[("v", v)]),
                None => gettext("Working …"),
            })
            .build();
        let dl_label = if cached.is_some() {
            gettext("Update")
        } else {
            gettext("Download")
        };
        let dl_btn = gtk::Button::builder()
            .label(&dl_label)
            .valign(gtk::Align::Center)
            .build();
        dl_btn.add_css_class("flat");
        {
            let sender = sender.clone();
            // Download vs. update is decided from the cached version at click time
            // (see `Msg::Yt(YtMsg::FetchYtDlp)`), so the button is correct even mid-probe.
            dl_btn.connect_clicked(move |_| sender.input(Msg::Yt(YtMsg::FetchYtDlp)));
        }
        ytdlp_row.add_suffix(&dl_btn);
        yt_group.add(&ytdlp_row);
        // The YouTube group lives at the bottom of the "Meta" page.
        page.add(&yt_group);
        // Remember the status row + button so a finished probe/download/update
        // refreshes them (see `refresh_ytdlp_status_label`).
        *self.youtube.settings_status.borrow_mut() = Some(ytdlp_row.clone());
        *self.youtube.settings_dl_btn.borrow_mut() = Some(dl_btn);
        // Resolve the real version in the background unless it is already cached.
        if cached.is_none() {
            sender.spawn_command(|out| {
                let _ = out.send(crate::ui::app::Cmd::YtDlpChecked(
                    crate::core::youtube::version(),
                ));
            });
        }

        page
    }
}
