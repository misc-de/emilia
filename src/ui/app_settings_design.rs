//! Settings "Design" category: theme, background (image, filter,
//! transparency) and the entry colors. Split out of
//! [`crate::ui::app_settings`] – pure reorganization, no functional change.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::gettext;
use crate::ui::app::{App, Msg};
use crate::ui::app_settings::{combo_index, combo_item};
use crate::ui::theme::{DesignMsg, SOFT_STRENGTH_MAX};

/// `#rrggbb` hex string of a color (alpha dropped, channels clamped to 0..1).
fn rgba_to_hex(c: &gtk::gdk::RGBA) -> String {
    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        to_u8(c.red()),
        to_u8(c.green()),
        to_u8(c.blue())
    )
}

/// The swatch next to each color row: the chosen color as a rounded chip,
/// or – when no color is set – a neutral outline with a centered X, so an
/// empty color reads as "none" instead of looking like a real (red) color.
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

/// Shared builder for the snapped 0–100 % sliders below.
fn percent_scale(initial: u32) -> gtk::Scale {
    let s = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 5.0);
    s.set_value(f64::from(initial));
    s.set_size_request(170, -1);
    s.set_valign(gtk::Align::Center);
    s.set_draw_value(true);
    s.set_value_pos(gtk::PositionType::Left);
    s.set_round_digits(0);
    s
}

/// A slider value snapped to the nearest 5 % step (negative values end at 0).
fn snap_percent(value: f64) -> u32 {
    ((value / 5.0).round() as u32) * 5
}

/// Emit only when the snapped (5 %) value changes, to avoid a DB write +
/// CSS reload on every drag pixel. `make` is a tuple-variant constructor.
fn wire_percent_scale(
    scale: &gtk::Scale,
    initial: u32,
    make: fn(u32) -> Msg,
    sender: &ComponentSender<App>,
) {
    let sender = sender.clone();
    let last = std::cell::Cell::new(initial);
    scale.connect_value_changed(move |s| {
        let v = snap_percent(s.value());
        if v != last.get() {
            last.set(v);
            sender.input(make(v));
        }
    });
}

/// Build a color row (color button + reset). `set` is the tuple-variant
/// constructor that persists the picked/cleared color.
fn color_row(
    title: String,
    subtitle: Option<String>,
    initial: &Option<String>,
    set: fn(Option<String>) -> Msg,
    sender: &ComponentSender<App>,
) -> adw::ActionRow {
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
}

impl App {
    /// Builds the "Design" settings page.
    pub(super) fn build_design_page(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) -> adw::PreferencesPage {
        let page = adw::PreferencesPage::builder()
            .title(gettext("Design"))
            .icon_name("applications-graphics-symbolic")
            .name("design")
            .build();

        // Appearance (light/dark theme) on the Design page, so the visual
        // options live together. ("List display" lives on the View page, after
        // "Scaling".)
        page.add(&self.build_theme_group(sender));
        page.add(&self.build_background_group(root, sender));
        page.add(&self.build_entries_group(sender));
        page
    }

    /// "Appearance" group (color scheme automatic/dark/light).
    fn build_theme_group(&self, sender: &ComponentSender<Self>) -> adw::PreferencesGroup {
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
        theme_row.set_selected(combo_index(&theme_codes, &cur_scheme.as_str()));
        {
            // Connect the handler only after `set_selected`, so the preselection
            // doesn't trigger a change.
            let sender = sender.clone();
            theme_row.connect_selected_notify(move |r| {
                let code = combo_item(&theme_codes, r.selected(), "system");
                sender.input(Msg::Design(DesignMsg::ColorScheme(code.to_string())));
            });
        }
        theme_group.add(&theme_row);
        theme_group
    }

    /// "Background" group: master switch, custom image, cover source, filter,
    /// strength and the two transparency switches.
    fn build_background_group(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) -> adw::PreferencesGroup {
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
                sender.input(Msg::Design(DesignMsg::UseCoverBg(r.is_active())));
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
        let strength_scale = percent_scale(self.theme.design.bg_filter_strength);
        // Soft saturates after a small radius, so it uses a finer 0..30 scale
        // (step 1) that spreads just that gentle range over the whole slider;
        // the other filters keep 0..100 (step 5). The filter dropdown retunes
        // this on change (see below).
        if self.theme.design.bg_filter.index() == 1 {
            strength_scale.set_range(0.0, f64::from(SOFT_STRENGTH_MAX));
            strength_scale.set_increments(1.0, 1.0);
        }
        strength_scale.set_value(f64::from(self.theme.design.bg_filter_strength));
        {
            let sender = sender.clone();
            let last = std::cell::Cell::new(self.theme.design.bg_filter_strength);
            strength_scale.connect_value_changed(move |s| {
                let v = s.value().round() as u32;
                if v != last.get() {
                    last.set(v);
                    sender.input(Msg::Design(DesignMsg::BgFilterStrength(v)));
                }
            });
        }
        strength_row.add_suffix(&strength_scale);

        // 4) Make the navigation transparent so the background shows through.
        // Both transparency switches only reach desktop chrome (the sidebar
        // pane, the wide title bar); in the narrow layout there is no sidebar
        // and the header is already see-through, so they change nothing there
        // — hide them on a phone rather than offer dead switches.
        let chrome_rows = !self.nav.narrow.get();
        let bg_nav_row = adw::SwitchRow::builder()
            .title(gettext("Transparency - Navigation"))
            .subtitle(gettext(
                "Also show the blurred background behind the sidebar",
            ))
            .active(self.theme.design.bg_nav)
            .visible(bg_on && chrome_rows)
            .build();
        {
            let sender = sender.clone();
            bg_nav_row.connect_active_notify(move |r| {
                sender.input(Msg::Design(DesignMsg::BgNav(r.is_active())));
            });
        }

        // 5) Make the title bar transparent so the background shows through.
        let bg_titlebar_row = adw::SwitchRow::builder()
            .title(gettext("Transparency - Title bar"))
            .subtitle(gettext(
                "Also show the blurred background behind the title bar",
            ))
            .active(self.theme.design.bg_titlebar)
            .visible(bg_on && chrome_rows)
            .build();
        {
            let sender = sender.clone();
            bg_titlebar_row.connect_active_notify(move |r| {
                sender.input(Msg::Design(DesignMsg::BgTitlebar(r.is_active())));
            });
        }

        // Filter change: a strength only applies to an active filter. Soft uses
        // a finer 0..30 scale, the others 0..100 — retune the slider on change
        // (a clamp of the old value re-emits via the handler above).
        {
            let sender = sender.clone();
            let strength_row = strength_row.clone();
            let strength_scale = strength_scale.clone();
            filter_row.connect_selected_notify(move |r| {
                if r.selected() == 1 {
                    strength_scale.set_range(0.0, f64::from(SOFT_STRENGTH_MAX));
                    strength_scale.set_increments(1.0, 1.0);
                } else {
                    strength_scale.set_range(0.0, 100.0);
                    strength_scale.set_increments(5.0, 5.0);
                }
                strength_row.set_sensitive(r.selected() != 0);
                sender.input(Msg::Design(DesignMsg::BgFilter(r.selected())));
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
                    if let Ok(file) = res
                        && let Some(path) = file.path()
                    {
                        row.set_subtitle(&gettext("Image selected"));
                        clear.set_visible(true);
                        filter_row.set_visible(true);
                        strength_row.set_visible(true);
                        strength_row.set_sensitive(filter_row.selected() != 0);
                        nav_row.set_visible(chrome_rows);
                        titlebar_row.set_visible(chrome_rows);
                        sender.input(Msg::Design(DesignMsg::CustomBg(Some(path))));
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
                sender.input(Msg::Design(DesignMsg::CustomBg(None)));
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
                nav_row.set_visible(on && chrome_rows);
                titlebar_row.set_visible(on && chrome_rows);
                sender.input(Msg::Design(DesignMsg::BackgroundOn(on)));
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
        bg_group
    }

    /// "Entries" group: text color, entry background switch, color and
    /// transparency.
    fn build_entries_group(&self, sender: &ComponentSender<Self>) -> adw::PreferencesGroup {
        // Entries: text and entry rows, each with its own color (with reset)
        // and a transparency over the background.
        let colors_group = adw::PreferencesGroup::builder()
            .title(gettext("Entries"))
            .build();
        // First in the group: do the entries get a background at all? Off makes
        // the rows see-through and hides the two rows that shape that
        // background — a colour and a transparency for something that is no
        // longer drawn would be dead controls.
        let entry_bg_row = adw::SwitchRow::builder()
            .title(gettext("Show background"))
            .subtitle(gettext("Entries sit on a background of their own"))
            .active(self.theme.design.entry_bg_on)
            .build();
        colors_group.add(&entry_bg_row);

        // Text color.
        let text_color_row = color_row(
            gettext("Text color"),
            None,
            &self.theme.design.text_color,
            |c| Msg::Design(DesignMsg::TextColor(c)),
            sender,
        );
        colors_group.add(&text_color_row);

        // Entry color + its transparency (tabs, navigation, list headings …).
        let field_color_row = color_row(
            gettext("Color"),
            Some(gettext("Background of tabs, navigation and list headings")),
            &self.theme.design.field_color,
            |c| Msg::Design(DesignMsg::FieldColor(c)),
            sender,
        );
        colors_group.add(&field_color_row);
        let field_trans_row = adw::ActionRow::builder()
            .title(gettext("Transparency"))
            .subtitle(gettext("0 % opaque, 100 % fully transparent"))
            .build();
        let field_trans_scale = percent_scale(self.theme.design.field_transparency);
        wire_percent_scale(
            &field_trans_scale,
            self.theme.design.field_transparency,
            |v| Msg::Design(DesignMsg::FieldTransparency(v)),
            sender,
        );
        field_trans_row.add_suffix(&field_trans_scale);
        colors_group.add(&field_trans_row);
        // The colour and transparency only shape the entry background, so they
        // follow the switch above.
        let bg_rows = [field_color_row.clone(), field_trans_row.clone()];
        for row in &bg_rows {
            row.set_visible(self.theme.design.entry_bg_on);
        }
        {
            let sender = sender.clone();
            entry_bg_row.connect_active_notify(move |r| {
                let on = r.is_active();
                for row in &bg_rows {
                    row.set_visible(on);
                }
                sender.input(Msg::Design(DesignMsg::EntryBackground(on)));
            });
        }
        colors_group
    }
}

#[cfg(test)]
mod tests {
    use super::{rgba_to_hex, snap_percent};
    use relm4::gtk::gdk::RGBA;

    #[test]
    fn rgba_to_hex_rounds_and_drops_alpha() {
        assert_eq!(rgba_to_hex(&RGBA::new(1.0, 0.5, 0.0, 0.3)), "#ff8000");
        assert_eq!(rgba_to_hex(&RGBA::BLACK), "#000000");
        assert_eq!(rgba_to_hex(&RGBA::WHITE), "#ffffff");
    }

    #[test]
    fn rgba_to_hex_clamps_out_of_range_channels() {
        assert_eq!(rgba_to_hex(&RGBA::new(1.5, -0.2, 0.2, 1.0)), "#ff0033");
    }

    #[test]
    fn snap_percent_rounds_to_five() {
        assert_eq!(snap_percent(0.0), 0);
        assert_eq!(snap_percent(2.4), 0);
        assert_eq!(snap_percent(2.5), 5);
        assert_eq!(snap_percent(47.6), 50);
        assert_eq!(snap_percent(100.0), 100);
        // Negative values saturate to 0 (the slider never goes below 0).
        assert_eq!(snap_percent(-12.0), 0);
    }
}
