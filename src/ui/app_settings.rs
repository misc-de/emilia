//! Settings dialog: preferences, extra sources, section order & hidden items.
//! Split out of app_dialogs.rs – pure reordering, no functional change.

use crate::i18n::{gettext, gettext_f};
use crate::ui::app::{cover_widget, App, Msg};
use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

impl App {
    pub(crate) fn open_settings(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::PreferencesDialog::new();

        // The former "Library" page (music folder, extra sources, Nextcloud
        // connect) was removed: all of that is now managed directly on the Files
        // page – the "+" adds a folder/Nextcloud, and a tab's context menu
        // renames/edits/removes a source (the "Music" tab changes the primary
        // folder).

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
                sender.input(Msg::SetGapless(r.is_active()));
            });
        }
        playback_group.add(&gapless_row);
        let xfade_row = adw::SpinRow::with_range(0.0, 12.0, 1.0);
        xfade_row.set_title(&gettext("Crossfade"));
        xfade_row.set_subtitle(&gettext("Seconds to overlap tracks (0 = off)"));
        xfade_row.set_value(self.settings.crossfade_secs);
        {
            let sender = sender.clone();
            xfade_row.connect_value_notify(move |r| {
                sender.input(Msg::SetCrossfade(r.value()));
            });
        }
        playback_group.add(&xfade_row);
        page.add(&playback_group);

        let sound_page = page;

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
                sender.input(Msg::SetAutoEnrich(r.is_active()));
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
                sender.input(Msg::SetAcoustidKey(r.text().to_string()));
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
                sender.input(Msg::SetFanartKey(r.text().to_string()));
            });
        }
        fanart_group.add(&fanart_row);
        page.add(&fanart_group);

        // --- Device synchronization: hidden in the settings
        //     (the feature stays reachable via the share button). ---

        let search_page = page;

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
        let current_idx = lang_codes
            .iter()
            .position(|c| *c == self.settings.ui_language)
            .unwrap_or(0);
        lang_row.set_selected(current_idx as u32);
        {
            // Connect the handler only after `set_selected`, so the preselection
            // doesn't trigger a language change.
            let sender = sender.clone();
            lang_row.connect_selected_notify(move |r| {
                let code = lang_codes
                    .get(r.selected() as usize)
                    .copied()
                    .unwrap_or("system");
                sender.input(Msg::SetLanguage(code.to_string()));
            });
        }
        lang_group.add(&lang_row);
        page.add(&lang_group);

        // Appearance: color scheme automatic/dark/light (takes effect immediately).
        let theme_group = adw::PreferencesGroup::builder()
            .title(gettext("Appearance"))
            .build();
        let theme_codes = ["system", "dark", "light"];
        let theme_labels = [gettext("Automatic"), gettext("Dark"), gettext("Light")];
        let theme_label_refs: Vec<&str> = theme_labels.iter().map(String::as_str).collect();
        let theme_row = adw::ComboRow::builder()
            .title(gettext("Theme"))
            .model(&gtk::StringList::new(&theme_label_refs))
            .build();
        let cur_scheme = self
            .library
            .get_setting("color_scheme")
            .ok()
            .flatten()
            .unwrap_or_else(|| "system".to_string());
        let cur_theme_idx = theme_codes
            .iter()
            .position(|c| *c == cur_scheme)
            .unwrap_or(0);
        theme_row.set_selected(cur_theme_idx as u32);
        {
            // Connect the handler only after `set_selected`, so the preselection
            // doesn't trigger a change.
            let sender = sender.clone();
            theme_row.connect_selected_notify(move |r| {
                let code = theme_codes
                    .get(r.selected() as usize)
                    .copied()
                    .unwrap_or("system");
                sender.input(Msg::SetColorScheme(code.to_string()));
            });
        }
        theme_group.add(&theme_row);
        // Shown on the "Design" page (added further down), not here.

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
                sender.input(Msg::SetGalleryView(r.is_active()));
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
                sender.input(Msg::SetGalleryColumns(r.value() as u32));
            });
        }
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
                sender.input(Msg::SetUiScale(r.value() / 100.0));
            });
        }
        scale_group.add(&scale_row);
        page.add(&scale_group);
        // "List display" sits right after "Scaling" on the View page.
        page.add(&gallery_group);

        // System: optional desktop tray icon + window behavior.
        let tray_group = adw::PreferencesGroup::builder()
            .title(gettext("System tray"))
            .build();
        let tray_enabled_row = adw::SwitchRow::builder()
            .title(gettext("Show tray icon"))
            .active(self.tray.enabled)
            .build();
        {
            let sender = sender.clone();
            tray_enabled_row.connect_active_notify(move |r| {
                sender.input(Msg::SetTrayEnabled(r.is_active()));
            });
        }
        tray_group.add(&tray_enabled_row);
        let tray_close_row = adw::SwitchRow::builder()
            .title(gettext("Close to tray"))
            .subtitle(gettext("Closing the window keeps it running in the tray"))
            .active(self.tray.close_hides)
            .build();
        {
            let sender = sender.clone();
            tray_close_row.connect_active_notify(move |r| {
                sender.input(Msg::SetTrayCloseHides(r.is_active()));
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
                sender.input(Msg::SetTrayStartHidden(r.is_active()));
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
                sender.input(Msg::SetTraySkipTaskbar(r.is_active()));
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
                sender.input(Msg::SetTrayIconGray(r.is_active()));
            });
        }
        tray_group.add(&tray_gray_row);
        page.add(&tray_group);

        let view_page = page;

        // --- Category: Design (colors, blurred background) ---
        fn rgba_to_hex(c: &gtk::gdk::RGBA) -> String {
            let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            format!(
                "#{:02x}{:02x}{:02x}",
                to_u8(c.red()),
                to_u8(c.green()),
                to_u8(c.blue())
            )
        }
        // The swatch next to each color row: the chosen color as a rounded chip,
        // or – when no color is set – a neutral outline with a centered X, so an
        // empty color reads as "none" instead of looking like a real (red) color.
        fn draw_swatch(cr: &gtk::cairo::Context, w: i32, h: i32, color: Option<gtk::gdk::RGBA>) {
            use std::f64::consts::PI;
            let (w, h) = (w as f64, h as f64);
            let inset = 2.0;
            let r = 5.0;
            let (x0, y0, x1, y1) = (inset, inset, w - inset, h - inset);
            cr.new_sub_path();
            cr.arc(x1 - r, y0 + r, r, -0.5 * PI, 0.0);
            cr.arc(x1 - r, y1 - r, r, 0.0, 0.5 * PI);
            cr.arc(x0 + r, y1 - r, r, 0.5 * PI, PI);
            cr.arc(x0 + r, y0 + r, r, PI, 1.5 * PI);
            cr.close_path();
            match color {
                Some(c) => {
                    cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 1.0);
                    let _ = cr.fill_preserve();
                    cr.set_source_rgba(0.0, 0.0, 0.0, 0.25);
                    cr.set_line_width(1.0);
                    let _ = cr.stroke();
                }
                None => {
                    cr.set_source_rgba(0.55, 0.55, 0.55, 0.6);
                    cr.set_line_width(1.0);
                    let _ = cr.stroke();
                    let pad = w.min(h) * 0.30;
                    cr.set_source_rgba(0.55, 0.55, 0.55, 0.9);
                    cr.set_line_width(1.6);
                    cr.move_to(pad, pad);
                    cr.line_to(w - pad, h - pad);
                    cr.move_to(w - pad, pad);
                    cr.line_to(pad, h - pad);
                    let _ = cr.stroke();
                }
            }
        }

        let page = adw::PreferencesPage::builder()
            .title(gettext("Design"))
            .icon_name("applications-graphics-symbolic")
            .name("design")
            .build();

        // Appearance (light/dark theme), built up in the "View" section above
        // but shown here so the visual options live together on the Design page.
        // ("List display" now lives on the View page, after "Scaling".)
        page.add(&theme_group);

        // Shared builders for the snapped 0–100 % sliders below.
        let mk_scale = |initial: u32| {
            let s = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 5.0);
            s.set_value(f64::from(initial));
            s.set_size_request(170, -1);
            s.set_valign(gtk::Align::Center);
            s.set_draw_value(true);
            s.set_value_pos(gtk::PositionType::Left);
            s.set_round_digits(0);
            s
        };
        // Emit only when the snapped (5 %) value changes, to avoid a DB write +
        // CSS reload on every drag pixel. `make` is a tuple-variant constructor.
        let wire_scale = |scale: &gtk::Scale, initial: u32, make: fn(u32) -> Msg| {
            let sender = sender.clone();
            let last = std::cell::Cell::new(initial);
            scale.connect_value_changed(move |s| {
                let v = ((s.value() / 5.0).round() as u32) * 5;
                if v != last.get() {
                    last.set(v);
                    sender.input(make(v));
                }
            });
        };

        // Background: a master switch turns the whole feature on/off (default on);
        // the image/filter/transparency options below apply while it is on. With
        // it on and no custom image chosen, the built-in light/dark default shows.
        let bg_group = adw::PreferencesGroup::builder()
            .title(gettext("Background"))
            .build();
        let has_bg = self.theme.design.custom_bg.is_some();
        let bg_on = self.theme.design.background_on;

        // 0) Master switch for the whole background feature.
        let bg_on_row = adw::SwitchRow::builder()
            .title(gettext("Show a background"))
            .subtitle(gettext(
                "On without a chosen image uses the built-in default",
            ))
            .active(bg_on)
            .build();

        // 1) Custom background image (shown while the feature is on).
        let bg_subtitle = if has_bg {
            gettext("Image selected")
        } else {
            gettext("None (built-in default)")
        };
        let bg_row = adw::ActionRow::builder()
            .title(gettext("Custom background"))
            .subtitle(&bg_subtitle)
            .visible(bg_on)
            .build();
        let bg_choose = gtk::Button::builder()
            .icon_name("document-open-symbolic")
            .tooltip_text(gettext("Choose image"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let bg_clear = gtk::Button::builder()
            .icon_name("edit-clear-symbolic")
            .tooltip_text(gettext("Remove"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .visible(has_bg)
            .build();

        // 1b) Use the now-playing cover as the background source (default off).
        let cover_row = adw::SwitchRow::builder()
            .title(gettext("Cover as background"))
            .subtitle(gettext("Use the current track's cover as the background"))
            .active(self.theme.design.use_cover_bg)
            .visible(bg_on)
            .build();
        {
            let sender = sender.clone();
            cover_row.connect_active_notify(move |r| {
                sender.input(Msg::SetUseCoverBg(r.is_active()));
            });
        }

        // 2) Blur/effect filter for the cover background (revealed with an image).
        let filter_names = gtk::StringList::new(&[]);
        for s in [
            gettext("Off"),
            gettext("Soft blur"),
            gettext("Gaussian blur"),
            gettext("Motion blur"),
            gettext("Radial blur"),
            gettext("Water"),
        ] {
            filter_names.append(&s);
        }
        let filter_row = adw::ComboRow::builder()
            .title(gettext("Background filter"))
            .subtitle(gettext(
                "Apply a filter to the current cover shown behind the app",
            ))
            .model(&filter_names)
            .selected(self.theme.design.bg_filter.index())
            .visible(bg_on)
            .build();

        // 3) Strength of the selected filter.
        let strength_row = adw::ActionRow::builder()
            .title(gettext("Strength"))
            .visible(bg_on)
            .sensitive(self.theme.design.bg_filter.index() != 0)
            .build();
        let strength_scale = mk_scale(self.theme.design.bg_filter_strength);
        wire_scale(
            &strength_scale,
            self.theme.design.bg_filter_strength,
            Msg::SetBgFilterStrength,
        );
        strength_row.add_suffix(&strength_scale);

        // 4) Make the navigation transparent so the background shows through.
        let bg_nav_row = adw::SwitchRow::builder()
            .title(gettext("Transparency - Navigation"))
            .subtitle(gettext(
                "Also show the blurred background behind the sidebar",
            ))
            .active(self.theme.design.bg_nav)
            .visible(bg_on)
            .build();
        {
            let sender = sender.clone();
            bg_nav_row.connect_active_notify(move |r| {
                sender.input(Msg::SetBgNav(r.is_active()));
            });
        }

        // 5) Make the title bar transparent so the background shows through.
        let bg_titlebar_row = adw::SwitchRow::builder()
            .title(gettext("Transparency - Title bar"))
            .subtitle(gettext(
                "Also show the blurred background behind the title bar",
            ))
            .active(self.theme.design.bg_titlebar)
            .visible(bg_on)
            .build();
        {
            let sender = sender.clone();
            bg_titlebar_row.connect_active_notify(move |r| {
                sender.input(Msg::SetBgTitlebar(r.is_active()));
            });
        }

        // Filter change: a strength only applies to an active filter.
        {
            let sender = sender.clone();
            let strength_row = strength_row.clone();
            filter_row.connect_selected_notify(move |r| {
                strength_row.set_sensitive(r.selected() != 0);
                sender.input(Msg::SetBgFilter(r.selected()));
            });
        }

        // Choosing/removing the image reveals or hides the options above.
        {
            let sender = sender.clone();
            let win = root.clone();
            let row = bg_row.clone();
            let clear = bg_clear.clone();
            let filter_row = filter_row.clone();
            let strength_row = strength_row.clone();
            let nav_row = bg_nav_row.clone();
            let titlebar_row = bg_titlebar_row.clone();
            bg_choose.connect_clicked(move |_| {
                let filter = gtk::FileFilter::new();
                filter.add_pixbuf_formats();
                filter.set_name(Some(&gettext("Images")));
                let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);
                let chooser = gtk::FileDialog::builder()
                    .title(gettext("Choose background image"))
                    .filters(&filters)
                    .build();
                let sender = sender.clone();
                let row = row.clone();
                let clear = clear.clone();
                let filter_row = filter_row.clone();
                let strength_row = strength_row.clone();
                let nav_row = nav_row.clone();
                let titlebar_row = titlebar_row.clone();
                chooser.open(Some(&win), gtk::gio::Cancellable::NONE, move |res| {
                    if let Ok(file) = res {
                        if let Some(path) = file.path() {
                            row.set_subtitle(&gettext("Image selected"));
                            clear.set_visible(true);
                            filter_row.set_visible(true);
                            strength_row.set_visible(true);
                            strength_row.set_sensitive(filter_row.selected() != 0);
                            nav_row.set_visible(true);
                            titlebar_row.set_visible(true);
                            sender.input(Msg::SetCustomBg(Some(path)));
                        }
                    }
                });
            });
        }
        {
            let sender = sender.clone();
            let row = bg_row.clone();
            // Clearing the image falls back to the built-in default (the feature
            // stays on), so the filter/transparency options remain visible.
            bg_clear.connect_clicked(move |b| {
                row.set_subtitle(&gettext("None (built-in default)"));
                b.set_visible(false);
                sender.input(Msg::SetCustomBg(None));
            });
        }

        // Master switch: reveal/hide all background options and toggle the feature.
        {
            let sender = sender.clone();
            let row = bg_row.clone();
            let cover_row = cover_row.clone();
            let filter_row = filter_row.clone();
            let strength_row = strength_row.clone();
            let nav_row = bg_nav_row.clone();
            let titlebar_row = bg_titlebar_row.clone();
            bg_on_row.connect_active_notify(move |r| {
                let on = r.is_active();
                row.set_visible(on);
                cover_row.set_visible(on);
                filter_row.set_visible(on);
                strength_row.set_visible(on);
                nav_row.set_visible(on);
                titlebar_row.set_visible(on);
                sender.input(Msg::SetBackgroundOn(on));
            });
        }
        bg_row.add_suffix(&bg_clear);
        bg_row.add_suffix(&bg_choose);
        bg_group.add(&bg_on_row);
        bg_group.add(&bg_row);
        bg_group.add(&cover_row);
        bg_group.add(&filter_row);
        bg_group.add(&strength_row);
        bg_group.add(&bg_nav_row);
        bg_group.add(&bg_titlebar_row);
        page.add(&bg_group);

        // Colors: text and fields, each with its own color (with reset) and a
        // transparency over the background.
        let colors_group = adw::PreferencesGroup::builder()
            .title(gettext("Colors"))
            .build();
        // Build a color row (color button + reset). `set` is the tuple-variant
        // constructor that persists the picked/cleared color.
        let mk_color_row = |title: String,
                            subtitle: Option<String>,
                            initial: &Option<String>,
                            set: fn(Option<String>) -> Msg| {
            use std::cell::Cell;
            use std::rc::Rc;

            let row = adw::ActionRow::builder().title(title).build();
            if let Some(sub) = subtitle {
                row.set_subtitle(&sub);
            }

            // Current color (`None` = no color set), shared by the swatch's draw
            // func and the picker/reset callbacks.
            let color: Rc<Cell<Option<gtk::gdk::RGBA>>> = Rc::new(Cell::new(
                initial
                    .as_deref()
                    .and_then(|h| gtk::gdk::RGBA::parse(h).ok()),
            ));

            let swatch = gtk::DrawingArea::builder()
                .content_width(24)
                .content_height(24)
                .valign(gtk::Align::Center)
                .build();
            {
                let color = color.clone();
                swatch.set_draw_func(move |_, cr, w, h| draw_swatch(cr, w, h, color.get()));
            }

            // With no color set, the button shows an edit icon (inviting a pick);
            // the color swatch replaces it once a color exists.
            let edit_icon = gtk::Image::from_icon_name("document-edit-symbolic");
            let has_color = color.get().is_some();
            swatch.set_visible(has_color);
            edit_icon.set_visible(!has_color);
            let btn_content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            btn_content.set_valign(gtk::Align::Center);
            btn_content.append(&edit_icon);
            btn_content.append(&swatch);

            // The clear button only makes sense once a color is actually set.
            let reset = gtk::Button::builder()
                .icon_name("edit-clear-symbolic")
                .tooltip_text(gettext("Reset"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .visible(has_color)
                .build();

            // The swatch button opens a color dialog; picking persists the color.
            let btn = gtk::Button::builder()
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .tooltip_text(gettext("Choose color"))
                .child(&btn_content)
                .build();
            {
                let sender = sender.clone();
                let color = color.clone();
                let swatch = swatch.clone();
                let reset = reset.clone();
                let edit_icon = edit_icon.clone();
                btn.connect_clicked(move |b| {
                    let dialog = gtk::ColorDialog::new();
                    dialog.set_with_alpha(false);
                    let parent = b.root().and_downcast::<gtk::Window>();
                    let start = color.get().unwrap_or(gtk::gdk::RGBA::WHITE);
                    let sender = sender.clone();
                    let color = color.clone();
                    let swatch = swatch.clone();
                    let reset = reset.clone();
                    let edit_icon = edit_icon.clone();
                    dialog.choose_rgba(
                        parent.as_ref(),
                        Some(&start),
                        gtk::gio::Cancellable::NONE,
                        move |res| {
                            if let Ok(rgba) = res {
                                color.set(Some(rgba));
                                swatch.set_visible(true);
                                edit_icon.set_visible(false);
                                swatch.queue_draw();
                                reset.set_visible(true);
                                sender.input(set(Some(rgba_to_hex(&rgba))));
                            }
                        },
                    );
                });
            }
            {
                let sender = sender.clone();
                let color = color.clone();
                let swatch = swatch.clone();
                let reset_btn = reset.clone();
                let edit_icon = edit_icon.clone();
                reset.connect_clicked(move |_| {
                    color.set(None);
                    swatch.set_visible(false);
                    edit_icon.set_visible(true);
                    reset_btn.set_visible(false);
                    sender.input(set(None));
                });
            }
            row.add_suffix(&reset);
            row.add_suffix(&btn);
            row
        };

        // Text color.
        let text_color_row = mk_color_row(
            gettext("Text color"),
            None,
            &self.theme.design.text_color,
            Msg::SetTextColor,
        );
        colors_group.add(&text_color_row);

        // Fields color + its transparency (tabs, navigation, list headings …).
        let field_color_row = mk_color_row(
            gettext("Fields color"),
            Some(gettext("Background of tabs, navigation and list headings")),
            &self.theme.design.field_color,
            Msg::SetFieldColor,
        );
        colors_group.add(&field_color_row);
        let field_trans_row = adw::ActionRow::builder()
            .title(gettext("Fields transparency"))
            .subtitle(gettext("0 % opaque, 100 % fully transparent"))
            .build();
        let field_trans_scale = mk_scale(self.theme.design.field_transparency);
        wire_scale(
            &field_trans_scale,
            self.theme.design.field_transparency,
            Msg::SetFieldTransparency,
        );
        field_trans_row.add_suffix(&field_trans_scale);
        colors_group.add(&field_trans_row);
        page.add(&colors_group);

        let design_page = page;

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
        let menu_page = page;

        // (The former "Cache" page with the stream recording timeshift buffer
        // was removed from the settings.)

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
                    sender.input(Msg::UnhideEntry {
                        scope: scope.clone(),
                        key: key.clone(),
                    });
                    group.remove(&row);
                });
            }
            row.add_suffix(&reveal);
            hidden_group.add(&row);
        }
        page.add(&hidden_group);
        let hidden_page = page;

        // YouTube (optional feature; the extractor yt-dlp is downloaded at
        // runtime, never bundled, and the feature is off by default). Lives on
        // the "Meta" page (added to `search_page` below).
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
            // (see `Msg::FetchYtDlp`), so the button is correct even mid-probe.
            dl_btn.connect_clicked(move |_| sender.input(Msg::FetchYtDlp));
        }
        ytdlp_row.add_suffix(&dl_btn);
        yt_group.add(&ytdlp_row);
        // The YouTube group lives at the bottom of the "Meta" page.
        search_page.add(&yt_group);
        // Remember the status row + button so a finished probe/download/update
        // refreshes them (see `refresh_ytdlp_status_label`).
        *self.youtube.settings_status.borrow_mut() = Some(ytdlp_row.clone());
        *self.youtube.settings_dl_btn.borrow_mut() = Some(dl_btn);
        {
            let status_slot = self.youtube.settings_status.clone();
            let btn_slot = self.youtube.settings_dl_btn.clone();
            dialog.connect_closed(move |_| {
                *status_slot.borrow_mut() = None;
                *btn_slot.borrow_mut() = None;
            });
        }
        // Resolve the real version in the background unless it is already cached.
        if cached.is_none() {
            sender.spawn_command(|out| {
                let _ = out.send(crate::ui::app::Cmd::YtDlpChecked(
                    crate::core::youtube::version(),
                ));
            });
        }

        // Order of the settings pages: "View" first.
        dialog.add(&view_page);
        dialog.add(&design_page);
        dialog.add(&sound_page);
        dialog.add(&search_page);
        dialog.add(&menu_page);
        dialog.add(&hidden_page);

        // Reopen on the category last viewed, and remember it on every switch.
        if let Some(name) = self
            .library
            .get_setting("settings_last_page")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
        {
            dialog.set_visible_page_name(&name);
        }
        {
            let sender = sender.clone();
            dialog.connect_visible_page_name_notify(move |d| {
                if let Some(name) = d.visible_page_name() {
                    sender.input(Msg::SetLastSettingsPage(name.to_string()));
                }
            });
        }

        dialog.present(Some(root));
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
            .subtitle(gettext(crate::ui::app::section_description(name)))
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
                sender.input(Msg::MoveSection { from, to });
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
                sender.input(Msg::SetSectionVisible {
                    section: name,
                    visible: s.is_active(),
                });
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
