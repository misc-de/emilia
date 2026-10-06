//! Settings "Sound" category: global equalizer entry and the track
//! transitions (gapless / crossfade). Split out of
//! [`crate::ui::app_settings`] – pure reorganization, no functional change.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::gettext;
use crate::ui::app::{App, Msg};
use crate::ui::app_settings::{SettingMsg, narrow_spin_value};

impl App {
    /// Builds the "Sound" settings page.
    pub(super) fn build_sound_page(&self, sender: &ComponentSender<Self>) -> adw::PreferencesPage {
        // --- Category: Sound ---
        let page = adw::PreferencesPage::builder()
            .title(gettext("Sound"))
            .icon_name("audio-speakers-symbolic")
            .name("sound")
            .build();
        // Global equalizer (basis for everything without a custom artist/album/track EQ).
        let eq_group = adw::PreferencesGroup::builder()
            .title(gettext("Equalizer"))
            .description(gettext(
                "Global sound control. It applies everywhere unless a custom \
                 setting is set for an artist, an album or a track.",
            ))
            .build();
        let eq_row = adw::ActionRow::builder()
            .title(gettext("Global equalizer"))
            .subtitle(gettext("Ten bands, per output"))
            .activatable(true)
            .build();
        eq_row.add_prefix(&gtk::Image::from_icon_name("multimedia-equalizer-symbolic"));
        eq_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        {
            let sender = sender.clone();
            eq_row.connect_activated(move |_| sender.input(Msg::OpenGlobalEq));
        }
        eq_group.add(&eq_row);
        page.add(&eq_group);

        // Track transitions (gapless / crossfade). Only for sequential local
        // queues (albums, concerts, audiobooks); streams keep a hard cut.
        let playback_group = adw::PreferencesGroup::builder()
            .title(gettext("Playback"))
            .description(gettext(
                "Transitions between tracks of local albums, concerts and audiobooks.",
            ))
            .build();
        let gapless_row = adw::SwitchRow::builder()
            .title(gettext("Gapless playback"))
            .subtitle(gettext("No gap between consecutive tracks"))
            .active(self.settings.gapless)
            .build();
        {
            let sender = sender.clone();
            gapless_row.connect_active_notify(move |r| {
                sender.input(Msg::Setting(SettingMsg::SetGapless(r.is_active())));
            });
        }
        playback_group.add(&gapless_row);
        let xfade_row = adw::SpinRow::with_range(0.0, 12.0, 1.0);
        xfade_row.set_title(&gettext("Crossfade"));
        xfade_row.set_subtitle(&gettext("Seconds to overlap tracks (0 = off)"));
        xfade_row.set_value(self.settings.crossfade_secs);
        narrow_spin_value(&xfade_row);
        {
            let sender = sender.clone();
            xfade_row.connect_value_notify(move |r| {
                sender.input(Msg::Setting(SettingMsg::SetCrossfade(r.value())));
            });
        }
        playback_group.add(&xfade_row);
        page.add(&playback_group);

        page
    }
}
