//! Runtime theming: app scaling + the design options (custom colors and a
//! blurred cover/background).
//!
//! A single dedicated [`gtk::CssProvider`] is registered once at
//! `STYLE_PROVIDER_PRIORITY_USER` (above the base `APPLICATION` provider from
//! `App::install_styles`) and re-loaded with a freshly built stylesheet whenever
//! a scale/color/background setting changes ([`App::reapply_runtime_style`]).
//!
//! App scaling additionally drives `gtk-xft-dpi`: GTK4 cannot geometrically zoom
//! a whole window per app, but the desktop font resolution moves all of
//! Adwaita's em-derived metrics (row heights, paddings, header sizes); the CSS
//! then scales the remaining pinned px literals (icons, the big play button).
//!
//! The blurred background is dependency-free: the cover (or the user's image) is
//! decoded small and shown in a [`gtk::Picture`] that fills the window behind
//! the content. The chosen [`BgFilter`] either relies on GTK's linear upscaling
//! of a tiny decode (soft) or pre-bakes the effect on the CPU (Gaussian/motion/
//! radial/water; see [`render_filtered`]). A scrim plus translucent chrome keep
//! text readable.

use adw::prelude::*;
use relm4::{adw, gtk};
use std::path::PathBuf;

use crate::ui::app::{read_design_settings, set_design, App};
use crate::ui::app_helpers::apply_color_scheme;
use crate::ui::widgets::decode_scaled;
use relm4::ComponentSender;

/// Built-in default backgrounds (a festival/concert photo per light & dark
/// mode), shown when the background feature is on but the user has not chosen an
/// image. Embedded so there's no Flatpak install path to resolve; materialized
/// to the data dir on first use (the background loader needs a file path).
const LIGHT_DEFAULT_BG: &[u8] = include_bytes!("../../data/backgrounds/light_concert.jpg");
const DARK_DEFAULT_BG: &[u8] = include_bytes!("../../data/backgrounds/dark_concert.jpg");

/// Writes the built-in default background for the current light/dark mode into
/// the data dir (once) and returns its path. `None` only if the data dir is
/// unavailable.
pub(crate) fn default_bg_path(dark: bool) -> Option<PathBuf> {
    let (bytes, name) = if dark {
        (DARK_DEFAULT_BG, "default-bg-dark.jpg")
    } else {
        (LIGHT_DEFAULT_BG, "default-bg-light.jpg")
    };
    let mut dir = dirs::data_dir()?;
    dir.push("emilia");
    std::fs::create_dir_all(&dir).ok()?;
    let dest = dir.join(name);
    // (Re)write when missing or when the embedded image changed (size differs),
    // so updating the built-in default actually takes effect.
    let stale = std::fs::metadata(&dest).map_or(true, |m| m.len() != bytes.len() as u64);
    if stale {
        std::fs::write(&dest, bytes).ok()?;
    }
    Some(dest)
}

/// Decode cap for the unfiltered ("Off") background: large enough to look crisp
/// (1:1) when scaled to fill the window, without loading huge originals.
const SHARP_BG_PX: i32 = 2560;
/// Decode size for the CPU/Gaussian filter modes: large enough that the
/// directional/radial/ripple structure survives, still cheap to process.
const FILTER_BASE_PX: i32 = 200;
/// Decode size for the "Soft" blur. Higher than the CPU-effect base so the fine
/// strength steps stay visible: at 200 px the upscale to the window already
/// blurs the image more than a low Soft setting does, which is why the bottom
/// of the old scale looked the same all the way up. Kept at 512 rather than
/// higher because the blur cost grows with the area (~20 ms here) and it runs
/// on the UI thread while the slider moves.
const SOFT_BASE_PX: i32 = 512;
/// Top of the "Soft" strength scale. Soft covers only the gentle range — its
/// blur saturates within a few pixels of radius, so those pixels get 30 steps
/// instead of 10; the heavy end is what the `Gaussian` filter is for.
pub(crate) const SOFT_STRENGTH_MAX: u32 = 30;
/// Fresh-install Soft strength (the same radius the old scale's step 1 gave).
pub(crate) const SOFT_STRENGTH_DEFAULT: u32 = 17;
/// Blur radius of the strongest Soft step, as a fraction of the decode width.
const SOFT_TOP_RADIUS: f64 = 0.0235;

/// Blur radius (in `SOFT_BASE_PX` pixels) for a Soft strength step. Linear, so
/// the 30 steps divide the gentle range evenly — most of them land below one
/// pixel of radius, which the fractional box blur still resolves.
fn soft_radius(strength: u32) -> f64 {
    let s = f64::from(strength.min(SOFT_STRENGTH_MAX)) / f64::from(SOFT_STRENGTH_MAX);
    s * f64::from(SOFT_BASE_PX) * SOFT_TOP_RADIUS
}

/// Convert a Soft strength saved under the old coarse 0..10 scale to the fine
/// 0..[`SOFT_STRENGTH_MAX`] one at the same blur radius. The old steps 4..10 all
/// blurred past what the new top step reaches, so they clamp to it.
pub(crate) fn soft_strength_from_legacy(v: u32) -> u32 {
    if v == 0 {
        return 0;
    }
    let old_radius = (0.10 + f64::from(v.min(10)) / 10.0 * 0.45) * 0.09;
    let steps = old_radius / SOFT_TOP_RADIUS * f64::from(SOFT_STRENGTH_MAX);
    (steps.round() as u32).min(SOFT_STRENGTH_MAX)
}

/// Blur/effect style applied to the background image. The dropdown order is the
/// enum order (see [`BgFilter::from_index`]). `Off` keeps the cover background
/// disabled — only the chosen image is shown, unfiltered (1:1).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum BgFilter {
    /// No cover background and no filter: the custom image is shown sharp (1:1).
    #[default]
    Off,
    /// Soft "box" blur (tiny decode upscaled).
    Soft,
    /// True Gaussian blur (CPU, separable box passes).
    Gaussian,
    /// Directional motion blur.
    Motion,
    /// Radial / zoom blur from the image center.
    Radial,
    /// Static water-ripple displacement.
    Water,
}

impl BgFilter {
    /// Map a ComboRow index to the variant (out-of-range → `Off`).
    pub(crate) fn from_index(i: u32) -> Self {
        match i {
            1 => Self::Soft,
            2 => Self::Gaussian,
            3 => Self::Motion,
            4 => Self::Radial,
            5 => Self::Water,
            _ => Self::Off,
        }
    }
    /// The ComboRow index of this variant.
    pub(crate) fn index(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Soft => 1,
            Self::Gaussian => 2,
            Self::Motion => 3,
            Self::Radial => 4,
            Self::Water => 5,
        }
    }
    /// Stable string used for DB persistence.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Soft => "soft",
            Self::Gaussian => "gaussian",
            Self::Motion => "motion",
            Self::Radial => "radial",
            Self::Water => "water",
        }
    }
    /// Parse a persisted key (unknown → `Off`).
    pub(crate) fn from_key(s: &str) -> Self {
        match s {
            "soft" => Self::Soft,
            "gaussian" => Self::Gaussian,
            "motion" => Self::Motion,
            "radial" => Self::Radial,
            "water" => Self::Water,
            _ => Self::Off,
        }
    }
}

/// The user-configurable design options (besides scaling).
#[derive(Clone, Default)]
pub(crate) struct DesignSettings {
    /// Master switch for the whole background feature (default on). When on and
    /// no `custom_bg` is set, the built-in light/dark concert default is shown.
    pub(crate) background_on: bool,
    /// A user-chosen background image (already copied into the app data dir).
    /// When set it takes the place of the built-in default as the base image.
    pub(crate) custom_bg: Option<PathBuf>,
    /// Use the now-playing track's cover as the background source (default off).
    /// When on it takes priority over `custom_bg`/the built-in default; the
    /// `bg_filter` then applies to it.
    pub(crate) use_cover_bg: bool,
    /// Blur/effect style applied to the background source (base image or cover).
    pub(crate) bg_filter: BgFilter,
    /// Strength (0..=100) of the selected filter.
    pub(crate) bg_filter_strength: u32,
    /// Also let the background show behind the sidebar/navigation.
    pub(crate) bg_nav: bool,
    /// Also let the background show behind the title bar (headerbar).
    pub(crate) bg_titlebar: bool,
    /// Text (foreground) color (hex `#rrggbb`).
    pub(crate) text_color: Option<String>,
    /// Do the list entries sit on a background of their own (default on)? Off
    /// makes the rows/cards see-through so the app background shows straight
    /// through the lists; the chrome (tabs, navigation, headings) keeps its
    /// tint, or the app would lose the surfaces you navigate by.
    pub(crate) entry_bg_on: bool,
    /// Fields (chrome) color (hex `#rrggbb`); `None` = the theme's window bg.
    pub(crate) field_color: Option<String>,
    /// Transparency (0..=100 %) of entries, buttons, tabs & headings over the
    /// background (0 = opaque, 100 = fully see-through). Default 40.
    pub(crate) field_transparency: u32,
}

/// Holds the runtime CssProvider and the live theme parameters. Lives on `App`.
pub(crate) struct ThemeState {
    provider: gtk::CssProvider,
    /// The desktop's base font resolution (`gtk-xft-dpi`) captured at startup,
    /// so the scale factor multiplies *that* (respects a HiDPI desktop).
    base_xft_dpi: i32,
    /// UI scale factor (0.5 ..= 1.5; 1.0 = unscaled).
    pub(crate) ui_scale: f64,
    pub(crate) design: DesignSettings,
    /// Background layer widgets (from the `view!` tree), wired in `finish_init`.
    bg_picture: Option<gtk::Picture>,
    scrim: Option<gtk::Box>,
    /// Whether a background image is currently shown (drives the transparency CSS).
    bg_active: bool,
    /// What the currently shown background was rendered from (source path,
    /// filter, strength), so an unchanged background is not decoded again.
    bg_key: Option<(String, BgFilter, u32)>,
}

impl ThemeState {
    /// Registers the provider once at USER priority and captures the base DPI.
    pub(crate) fn new(ui_scale: f64, design: DesignSettings) -> Self {
        let provider = gtk::CssProvider::new();
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_USER,
            );
        }
        let base_xft_dpi = gtk::Settings::default()
            .map(|s| s.gtk_xft_dpi())
            .filter(|d| *d > 0)
            .unwrap_or(96 * 1024);
        Self {
            provider,
            base_xft_dpi,
            ui_scale: ui_scale.clamp(0.5, 1.5),
            design,
            bg_picture: None,
            scrim: None,
            bg_active: false,
            bg_key: None,
        }
    }

    /// Wire the background layer widgets created by the `view!` tree.
    pub(crate) fn set_bg_widgets(&mut self, picture: gtk::Picture, scrim: gtk::Box) {
        picture.add_css_class("emilia-bg");
        picture.set_can_target(false);
        scrim.set_can_target(false);
        scrim.set_visible(false);
        self.bg_picture = Some(picture);
        self.scrim = Some(scrim);
    }

    /// The paintable currently shown as the main blurred background (for sharing
    /// it with the tray media popup), or `None` when no background is active.
    pub(crate) fn bg_paintable(&self) -> Option<gtk::gdk::Paintable> {
        if self.bg_active {
            self.bg_picture.as_ref().and_then(|p| p.paintable())
        } else {
            None
        }
    }

    /// Push a (pre-blurred) background texture or clear it. `None` hides the layer.
    pub(crate) fn set_background_texture(&mut self, tex: Option<&gtk::gdk::Texture>) {
        if let Some(pic) = &self.bg_picture {
            pic.set_paintable(tex);
        }
        self.bg_active = tex.is_some();
        if let Some(scrim) = &self.scrim {
            scrim.set_visible(self.bg_active);
        }
    }

    /// Apply the scale factor to the desktop font resolution (moves Adwaita's
    /// font-derived metrics). `1.0` restores the captured base.
    pub(crate) fn apply_scale_dpi(&self) {
        if let Some(settings) = gtk::Settings::default() {
            let dpi = (f64::from(self.base_xft_dpi) * self.ui_scale).round() as i32;
            settings.set_gtk_xft_dpi(dpi);
        }
    }

    /// (Re)build the stylesheet from the current scale/design/background and load
    /// it into the provider (re-parsing invalidates styles in place).
    pub(crate) fn reapply(&self) {
        self.provider.load_from_string(&self.build_css());
    }

    fn build_css(&self) -> String {
        let mut css = String::new();

        // ---- Scaling: icons + the px literals pinned in `install_styles` ----
        let f = self.ui_scale;
        if (f - 1.0).abs() > f64::EPSILON {
            let icon = (16.0 * f).round() as i32;
            let big = (46.0 * f).round() as i32;
            let big_icon = (34.0 * f).round() as i32;
            css.push_str(&format!(
                "image {{ -gtk-icon-size: {icon}px; }}\
                 button.emilia-bigplay, button.emilia-record-dot {{ min-width: {big}px; min-height: {big}px; }}\
                 button.emilia-bigplay image, button.emilia-record-dot image {{ -gtk-icon-size: {big_icon}px; }}"
            ));
        }

        // ---- Record (rec) icons: darker red on the light theme ----
        // The record dot uses libadwaita's `@error_color`, which on the *light*
        // theme is a fairly bright red (Adwaita red 4, #c01c28) that the user
        // finds too light. Override it one palette step darker (red 5, #a51d2d)
        // here at PRIORITY_USER. Only on light: the dark theme's lighter error red
        // is the right contrast against a dark background, so leave it. Re-built on
        // every light/dark flip (this CSS is reapplied via `ReloadDesign`), so the
        // override tracks the current scheme.
        if !adw::StyleManager::default().is_dark() {
            css.push_str(
                "button.emilia-record-dot image, image.emilia-record-dot { color: #a51d2d; }",
            );
        }

        // ---- Text color ----
        // Also override `sidebar_fg_color` (the sidebar/main-nav text uses that,
        // not `window_fg_color`) and color the list section headings explicitly.
        if let Some(fg) = self.design.text_color.as_deref().filter(|c| valid_hex(c)) {
            css.push_str(&format!(
                "@define-color window_fg_color {fg};@define-color view_fg_color {fg};\
                 @define-color sidebar_fg_color {fg};\
                 label.emilia-list-section, .emilia-nav-btn label {{ color: {fg}; }}"
            ));
        }

        // ---- Blurred background: make the chrome translucent so it shows through ----
        if self.bg_active {
            // Fields (chrome) color: a custom hex or the theme's window bg.
            let field = self
                .design
                .field_color
                .as_deref()
                .filter(|c| valid_hex(c))
                .unwrap_or("@window_bg_color");
            // Configurable translucency of entries & buttons (0 % = opaque).
            let a = opacity(self.design.field_transparency);
            // Tabs + list headings stay 30 percentage points *less* transparent.
            let a_head = (a + 0.30).min(1.0);
            // The active tab (nav + in-page switcher) gets another 30 points of
            // opacity on top, so the current section stands out from the rest.
            let a_check = (a_head + 0.30).min(1.0);
            // Note: `window` itself is NOT made transparent — the background
            // Picture (a child) already covers it, and a transparent `window`
            // would bleed into separate dialogs (e.g. the color chooser).
            css.push_str(
                ".view, viewport, stack, scrolledwindow, list, flowbox, clamp \
                 { background-color: transparent; background-image: none; }",
            );
            css.push_str(&format!(
                "headerbar, .toolbar {{ background-color: transparent; background-image: none; }}\
                 /* The virtualised library overviews (albums/singles/compilations/ \
                    artists, see `crate::ui::card_list`) are a GtkListView, whose \
                    rows the `list > row` selector does NOT reach — and their card \
                    look is painted by our own `@card_bg_color` rule in \
                    `install_styles`. Without the extra selector they stayed \
                    opaque white while every other list turned translucent. */\
                 list > row, .boxed-list > row, listview.emilia-card-list row.emilia-card, \
                 entry, spinbutton \
                 {{ background-color: alpha({field}, {a}); }}\
                 .emilia-tabbar button {{ background-color: alpha({field}, {a_head}); }}\
                 .emilia-tabbar button:checked, button.emilia-nav-btn:checked {{ background-color: alpha({field}, {a_check}); }}\
                 label.emilia-list-section {{ background-color: transparent; }}\
                 windowcontrols button, button.titlebutton {{ background-color: transparent; }}"
            ));
            // In-window modals (AdwDialog / AdwAlertDialog / AdwPreferencesDialog).
            // A fully transparent dialog let the *main view* behind it show through
            // (the background Picture is a child of the window, but so is the whole
            // content area — so a see-through modal exposed the lists/info beneath
            // it, which is confusing). Instead give the modal its own solid,
            // app-tinted surface so the content behind it is covered, while it still
            // matches the app's look (chrome rows/entries/headerbars inside are
            // tinted by the rules above; the field colour is the same one used for
            // entries/rows). The solid surface libadwaita draws lives on the inner
            // `sheet` node (an AdwGizmo wrapped in floating-sheet/bottom-sheet), not
            // on the outer `dialog` node — and `dialog` carries no `.background`
            // class, so we target `sheet` directly. The outer `dialog`/`.dialog`
            // node stays transparent so the rounded sheet isn't backed by a solid
            // rectangle and the dimming/Picture around it shows. Our provider loads
            // at PRIORITY_USER (above the theme), so this wins, and `sheet`'s own
            // shadow/radius/outline are untouched. Real top-level dialog *windows*
            // (file/color choosers) are GtkWindows with no `sheet` node, so
            // `window.dialog` keeps a solid background. (Popovers stay opaque.)
            css.push_str(&format!(
                "dialog, .dialog {{ background-color: transparent; background-image: none; }}\
                 sheet {{ background-color: {field}; background-image: none; }}\
                 window.dialog {{ background-color: @window_bg_color; }}",
            ));
            // The tray media popup carries its own blurred background Picture, so
            // make its window transparent to let it show (the content floats over
            // it like the main window, tinted by the shared scrim).
            css.push_str("window.emilia-tray-popup { background-color: transparent; }");
            // Optionally let the blur show fully behind the title bar
            // (headerbar), overriding the field-alpha tint above.
            if self.design.bg_titlebar {
                css.push_str(
                    "headerbar { background-color: transparent; background-image: none; }",
                );
            }
            // Optionally let the blur show behind the sidebar/navigation, too.
            if self.design.bg_nav {
                css.push_str(
                    ".sidebar-pane { background-color: transparent; background-image: none; }",
                );
            }
        }

        // ---- Entries without a background of their own ----
        // Last, so it wins over both the card look from `install_styles` and the
        // field tint the blur branch above gives the rows. The whole surface
        // goes, not just its fill: an outline with a drop shadow around nothing
        // is a card that lost its face, not a flat list. So this also clears the
        // frame libadwaita draws on the *list* (`boxed-list`), the per-row
        // borders our sectioned/card lists paint themselves, and the hairlines
        // between rows — those are borders too.
        //
        // Only the entries are cleared: the chrome (tabs, navigation, list
        // headings) keeps its tint, since those surfaces are what the lists are
        // read against. Text fields (`entry`, `spinbutton`) keep theirs too — a
        // see-through input box reads as decoration, not as somewhere to type.
        if !self.design.entry_bg_on {
            css.push_str(
                "list > row, .boxed-list > row, list.emilia-sectioned > row, \
                 listview.emilia-card-list row.emilia-card, \
                 list.boxed-list, .boxed-list, list.emilia-sectioned, \
                 listview.emilia-card-list \
                 { background-color: transparent; background-image: none; \
                   border: none; box-shadow: none; }",
            );
        }

        css
    }
}

/// Convert a transparency percentage (0 = opaque, 100 = fully transparent) into
/// a CSS alpha value (1.0 = opaque).
fn opacity(transparency: u32) -> f64 {
    f64::from(100u32.saturating_sub(transparency.min(100))) / 100.0
}

/// Copy a chosen background image into the app data dir and return the stored
/// path. Keeping our own copy means the picture survives the original being
/// moved/deleted and needs no extra Flatpak filesystem permission (the file
/// dialog's portal grants one-shot read access, used here immediately). A fixed
/// name (no extension; the loader sniffs the format) avoids leftover files.
pub(crate) fn import_custom_bg(src: &std::path::Path) -> Option<PathBuf> {
    let mut dir = dirs::data_dir()?;
    dir.push("emilia");
    std::fs::create_dir_all(&dir).ok()?;
    let dest = dir.join("custom-background");
    std::fs::copy(src, &dest).ok()?;
    Some(dest)
}

/// `#rgb` / `#rrggbb` / `#rrggbbaa` hex validation (defends `@define-color`/CSS
/// against a malformed value breaking the whole stylesheet).
fn valid_hex(s: &str) -> bool {
    let s = s.trim();
    s.starts_with('#')
        && matches!(s.len(), 4 | 7 | 9)
        && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Appearance / design messages, dispatched by [`App::update_design`]. Grouped
/// out of the flat `Msg` enum (see `app.rs`): whole-app scaling, the colour
/// scheme, custom/cover background + its filter, and the chrome colours.
#[derive(Debug)]
pub(crate) enum DesignMsg {
    /// Change the color scheme ("system"/"dark"/"light"); takes effect immediately.
    ColorScheme(String),
    /// Whole-app scale factor (0.5 ..= 1.5); persisted + applied live.
    UiScale(f64),
    /// Mobile menu-icon size offset (-50 ..= 50 %); persisted + applied live.
    MenuIconScale(i32),
    /// Master switch for the background feature (off → no background at all;
    /// on with no custom image → the built-in light/dark default).
    BackgroundOn(bool),
    /// Reload the per-theme appearance (background + colours) from the DB and
    /// re-apply it — fired when the light/dark theme flips.
    Reload,
    /// Set/clear the custom background image (already copied into the data dir).
    CustomBg(Option<PathBuf>),
    /// Toggle using the now-playing cover as the background source.
    UseCoverBg(bool),
    /// Select the background blur/effect filter (ComboRow index).
    BgFilter(u32),
    /// Strength (0..=100 %) of the selected background filter.
    BgFilterStrength(u32),
    /// Set/clear the text color (hex).
    TextColor(Option<String>),
    /// Set/clear the fields (chrome) color (hex).
    FieldColor(Option<String>),
    /// Toggle whether the list entries carry a background of their own.
    EntryBackground(bool),
    /// Toggle whether the background shows behind the sidebar/navigation.
    BgNav(bool),
    /// Toggle whether the background shows behind the title bar (headerbar).
    BgTitlebar(bool),
    /// Transparency (0..=100 %) of entries & buttons over the background.
    FieldTransparency(u32),
}

impl App {
    /// Dispatch a [`DesignMsg`] (appearance: scaling, colours, background). Split
    /// out of the monolithic `App::update` match.
    pub(crate) fn update_design(
        &mut self,
        msg: DesignMsg,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        match msg {
            DesignMsg::ColorScheme(scheme) => {
                apply_color_scheme(&scheme);
                let _ = self.library.set_setting("color_scheme", &scheme);
                // The built-in default background differs per light/dark, so a
                // scheme change may need a different image.
                self.refresh_background();
            }
            DesignMsg::UiScale(factor) => {
                self.apply_ui_scale(factor);
                let _ = self
                    .library
                    .set_setting("ui_scale", &self.theme.ui_scale.to_string());
            }
            DesignMsg::MenuIconScale(offset) => {
                let offset = offset.clamp(-50, 50);
                crate::ui::app_init::apply_menu_icon_scale(&self.nav.top_nav, offset);
                let _ = self
                    .library
                    .set_setting("menu_icon_scale", &offset.to_string());
            }
            DesignMsg::BackgroundOn(on) => {
                self.theme.design.background_on = on;
                set_design(
                    &self.library,
                    "design_background_on",
                    if on { "1" } else { "0" },
                );
                self.refresh_background();
            }
            DesignMsg::Reload => {
                // Light/dark flipped: load the new theme's appearance and apply.
                self.theme.design = read_design_settings(&self.library);
                self.refresh_background();
                self.reapply_runtime_style();
                // If the settings subpage is on screen, rebuild it so its
                // appearance controls show the new theme's values (it reopens on
                // the same category via `settings_last_page`).
                let on_settings = self
                    .nav
                    .nav_view
                    .visible_page()
                    .and_then(|p| p.tag())
                    .is_some_and(|t| t == crate::ui::app_settings::SETTINGS_TAG);
                if on_settings {
                    self.nav.nav_view.pop();
                    self.open_settings(root, sender);
                }
            }
            DesignMsg::CustomBg(src) => {
                // Copy the chosen image into our data dir; `None` clears it.
                let stored = src.as_deref().and_then(import_custom_bg);
                self.theme.design.custom_bg = stored.clone();
                set_design(
                    &self.library,
                    "design_bg_path",
                    &stored
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                );
                self.refresh_background();
            }
            DesignMsg::UseCoverBg(on) => {
                self.theme.design.use_cover_bg = on;
                set_design(
                    &self.library,
                    "design_use_cover_bg",
                    if on { "1" } else { "0" },
                );
                self.refresh_background();
            }
            DesignMsg::BgFilter(idx) => {
                let filter = BgFilter::from_index(idx);
                self.theme.design.bg_filter = filter;
                set_design(&self.library, "design_bg_filter", filter.key());
                self.refresh_background();
            }
            DesignMsg::BgFilterStrength(pct) => {
                self.theme.design.bg_filter_strength = pct.min(100);
                set_design(
                    &self.library,
                    "design_bg_filter_strength",
                    &self.theme.design.bg_filter_strength.to_string(),
                );
                self.refresh_background();
            }
            DesignMsg::TextColor(color) => {
                self.theme.design.text_color = color.clone();
                set_design(
                    &self.library,
                    "design_text_color",
                    color.as_deref().unwrap_or(""),
                );
                self.reapply_runtime_style();
            }
            DesignMsg::FieldColor(color) => {
                self.theme.design.field_color = color.clone();
                set_design(
                    &self.library,
                    "design_field_color",
                    color.as_deref().unwrap_or(""),
                );
                self.reapply_runtime_style();
            }
            DesignMsg::EntryBackground(on) => {
                self.theme.design.entry_bg_on = on;
                set_design(&self.library, "design_entry_bg", if on { "1" } else { "0" });
                self.reapply_runtime_style();
            }
            DesignMsg::BgNav(on) => {
                self.theme.design.bg_nav = on;
                set_design(&self.library, "design_bg_nav", if on { "1" } else { "0" });
                self.reapply_runtime_style();
            }
            DesignMsg::BgTitlebar(on) => {
                self.theme.design.bg_titlebar = on;
                set_design(
                    &self.library,
                    "design_bg_titlebar",
                    if on { "1" } else { "0" },
                );
                self.reapply_runtime_style();
            }
            DesignMsg::FieldTransparency(pct) => {
                self.theme.design.field_transparency = pct.min(100);
                set_design(
                    &self.library,
                    "design_chrome_transparency",
                    &self.theme.design.field_transparency.to_string(),
                );
                self.reapply_runtime_style();
            }
        }
    }

    /// Single choke point: rebuild + reload the runtime stylesheet from the
    /// current scale, colors and background state.
    pub(crate) fn reapply_runtime_style(&self) {
        self.theme.reapply();
    }

    /// Change the UI scale factor: persist-free apply (the caller persists).
    /// Drives `gtk-xft-dpi` and the px-scaling CSS.
    pub(crate) fn apply_ui_scale(&mut self, factor: f64) {
        self.theme.ui_scale = factor.clamp(0.5, 1.5);
        self.theme.apply_scale_dpi();
        self.reapply_runtime_style();
    }

    /// Cheap track-change hook: only rebuild the background when the now-playing
    /// cover is actually used as the source (a filter other than `Off`, and a
    /// background image configured at all).
    pub(crate) fn refresh_cover_background(&mut self) {
        let d = &self.theme.design;
        if d.background_on && d.use_cover_bg {
            self.refresh_background();
        }
    }

    /// Recompute the blurred background layer for the current design settings +
    /// now-playing cover, then reapply the stylesheet (transparency depends on
    /// whether a background is shown).
    pub(crate) fn refresh_background(&mut self) {
        // Rendering means decoding *and* filtering an image on the UI thread,
        // and `refresh_cover_background` lands here on every single track
        // change — where the source has usually not moved at all (the next
        // track of the same album carries the same cover, and with the cover
        // background off the base image never changes). So re-render only when
        // something the render actually depends on differs. The stylesheet is
        // reapplied either way: callers also use this to pick up design changes
        // that never reach the texture (`bg_nav`, `bg_titlebar`, colors).
        let key = self.bg_render_key();
        if self.theme.bg_key != key {
            let tex = key
                .as_ref()
                .and_then(|(src, filter, strength)| render_filtered(src, *filter, *strength));
            self.theme.set_background_texture(tex.as_ref());
            self.theme.bg_key = key;
        }
        self.reapply_runtime_style();
    }

    /// Everything the background render depends on: the source image path plus
    /// the filter and its strength — or `None` for the neutral look. The master
    /// switch turns the whole feature off; otherwise the base image is the
    /// user's `custom_bg` or, when none is set, the built-in light/dark default.
    /// With "cover as background" on, the now-playing cover takes priority as
    /// the source and the base is the fallback.
    fn bg_render_key(&self) -> Option<(String, BgFilter, u32)> {
        let d = &self.theme.design;
        if !d.background_on {
            return None;
        }
        let base = match d.custom_bg.as_ref() {
            Some(p) => p.to_string_lossy().into_owned(),
            None => {
                let dark = adw::StyleManager::default().is_dark();
                default_bg_path(dark)?.to_string_lossy().into_owned()
            }
        };
        let src = if d.use_cover_bg {
            self.now_playing_cover_path().unwrap_or(base)
        } else {
            base
        };
        Some((src, d.bg_filter, d.bg_filter_strength))
    }

    /// Cover file of the currently playing local track (via its album), if any.
    fn now_playing_cover_path(&self) -> Option<String> {
        let path = self.transport.playing_path.as_ref()?;
        let track = self
            .library
            .track_by_path(&path.to_string_lossy())
            .ok()
            .flatten()?;
        let album = track.album.filter(|a| !a.trim().is_empty())?;
        self.library.album_cover(&album).ok().flatten()
    }
}

// ---------------------------------------------------------------------------
// Background filters. The source image is decoded small (covers are tiny, a
// custom wallpaper is downscaled to `FILTER_BASE_PX`), so the CPU effects below
// touch only a few 10k pixels — cheap enough to run on every track change. The
// soft/Gaussian modes don't need pixel work: soft is a tiny decode upscaled by
// GTK, Gaussian is blurred by a CSS `filter` on the Picture (see `build_css`).
// ---------------------------------------------------------------------------

use gtk::gdk_pixbuf::Pixbuf;

/// Decode `path` and turn it into the background texture for `filter`/`strength`.
fn render_filtered(path: &str, filter: BgFilter, strength: u32) -> Option<gtk::gdk::Texture> {
    let s = f64::from(strength.min(100)) / 100.0;
    // Strength 0 means "no effect": decode the image sharp, whichever filter is
    // selected. Without this the small filter-resolution decode (≈200 px for the
    // CPU effects, 64 px for soft) gets upscaled to the window and looks
    // blocky/pixelated even though no blur was applied.
    if strength == 0 {
        return decode_scaled(path, SHARP_BG_PX);
    }
    match filter {
        BgFilter::Off => decode_scaled(path, SHARP_BG_PX),
        // Soft = a gentle *real* blur (a light Gaussian on a higher-res decode),
        // not the old tiny-decode upscale that looked blocky. It runs on its own
        // fine 0..30 strength scale (see `app_settings`): the visible effect
        // saturates within a few pixels of radius, so the slider spreads exactly
        // those over its whole travel — sub-pixel radii included, which the
        // fractional box blur below resolves.
        BgFilter::Soft => {
            let r = soft_radius(strength);
            cpu_filter_at(path, SOFT_BASE_PX, |pb| gaussian_blur(pb, r))
        }
        BgFilter::Gaussian => cpu_filter(path, |pb| {
            gaussian_blur(pb, s * f64::from(FILTER_BASE_PX) * 0.09)
        }),
        BgFilter::Motion => cpu_filter(path, |pb| motion_blur(pb, s)),
        BgFilter::Radial => cpu_filter(path, |pb| radial_blur(pb, s)),
        BgFilter::Water => cpu_filter(path, |pb| water_ripple(pb, s)),
    }
}

/// Decode a moderate-resolution pixbuf, run a per-pixel effect, return a texture.
fn cpu_filter(path: &str, f: impl FnOnce(&Pixbuf) -> Option<Pixbuf>) -> Option<gtk::gdk::Texture> {
    cpu_filter_at(path, FILTER_BASE_PX, f)
}

/// Same, at an explicit decode size (Soft needs more pixels to stay gradual).
fn cpu_filter_at(
    path: &str,
    base_px: i32,
    f: impl FnOnce(&Pixbuf) -> Option<Pixbuf>,
) -> Option<gtk::gdk::Texture> {
    let src = Pixbuf::from_file_at_scale(path, base_px, base_px, true).ok()?;
    f(&src).map(|pb| gtk::gdk::Texture::for_pixbuf(&pb))
}

/// Build the output pixbuf from a freshly computed byte buffer in `src`'s layout.
fn finish(out: Vec<u8>, src: &Pixbuf) -> Option<Pixbuf> {
    let bytes = gtk::glib::Bytes::from_owned(out);
    Some(Pixbuf::from_bytes(
        &bytes,
        src.colorspace(),
        src.has_alpha(),
        8,
        src.width(),
        src.height(),
        src.rowstride(),
    ))
}

/// Gaussian blur, approximated by three separable box-blur passes (the classic
/// box≈Gaussian trick). `radius` is in source pixels and may be fractional.
fn gaussian_blur(src: &Pixbuf, radius: f64) -> Option<Pixbuf> {
    let (w, h, nch, stride) = (src.width(), src.height(), src.n_channels(), src.rowstride());
    if radius < 0.02 {
        return Some(src.clone());
    }
    let bytes = src.read_pixel_bytes();
    let mut buf = bytes.as_ref().to_vec();
    for _ in 0..3 {
        buf = box_blur(&buf, w, h, nch, stride, radius);
    }
    finish(buf, src)
}

/// One separable box blur (horizontal then vertical) of radius `r`.
fn box_blur(data: &[u8], w: i32, h: i32, nch: i32, stride: i32, r: f64) -> Vec<u8> {
    let horizontal = blur_axis(data, w, h, nch, stride, r, false);
    blur_axis(&horizontal, w, h, nch, stride, r, true)
}

/// One 1-D pass of the box blur, along x (`vertical` false) or y.
///
/// `r` may be fractional: the two pixels just outside the whole-pixel window are
/// mixed in with the leftover weight, so radii less than a pixel apart still
/// give different images — that is what lets the Soft slider resolve 30 steps
/// inside a range of a few pixels. Edges clamp to the border pixel, and the
/// window sum slides, so the cost does not grow with the radius.
fn blur_axis(
    data: &[u8],
    w: i32,
    h: i32,
    nch: i32,
    stride: i32,
    r: f64,
    vertical: bool,
) -> Vec<u8> {
    // `u` runs along the blurred axis, `v` across it.
    let at = |u: i32, v: i32| -> usize {
        if vertical {
            (u * stride + v * nch) as usize
        } else {
            (v * stride + u * nch) as usize
        }
    };
    let (len, lines) = if vertical { (h, w) } else { (w, h) };
    let ri = (r.floor() as i32).min(len - 1);
    let frac = r - r.floor();
    let norm = f64::from(2 * ri + 1) + 2.0 * frac;
    let nc = nch as usize;

    let mut out = vec![0u8; (stride * h) as usize];
    for v in 0..lines {
        // Window sum for u = 0, with both ends clamped to the border pixel.
        let mut acc = [0u32; 4];
        for u in -ri..=ri {
            let i = at(u.clamp(0, len - 1), v);
            for c in 0..nc {
                acc[c] += u32::from(data[i + c]);
            }
        }
        for u in 0..len {
            let lo = at((u - ri - 1).clamp(0, len - 1), v);
            let hi = at((u + ri + 1).clamp(0, len - 1), v);
            let o = at(u, v);
            for c in 0..nc {
                let edges = f64::from(u32::from(data[lo + c]) + u32::from(data[hi + c]));
                out[o + c] = ((f64::from(acc[c]) + frac * edges) / norm).round() as u8;
            }
            // Slide the window one step: add the entering pixel, drop the leaving
            // one (in that order, so the running sum never goes negative).
            let enter = at((u + 1 + ri).clamp(0, len - 1), v);
            let leave = at((u - ri).clamp(0, len - 1), v);
            for c in 0..nc {
                acc[c] = acc[c] + u32::from(data[enter + c]) - u32::from(data[leave + c]);
            }
        }
    }
    out
}

/// Horizontal directional blur — averages a window of pixels along x.
fn motion_blur(src: &Pixbuf, s: f64) -> Option<Pixbuf> {
    let (w, h, nch, stride) = (src.width(), src.height(), src.n_channels(), src.rowstride());
    let bytes = src.read_pixel_bytes();
    let data = bytes.as_ref();
    let mut out = vec![0u8; (stride * h) as usize];
    let k = 1 + (s * (w as f64 / 6.0)).round() as i32;
    let at = |x: i32, y: i32| -> usize { (y * stride + x * nch) as usize };
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0u32; 4];
            let mut n = 0u32;
            for sx in (x - k).max(0)..=(x + k).min(w - 1) {
                let i = at(sx, y);
                for c in 0..nch as usize {
                    acc[c] += u32::from(data[i + c]);
                }
                n += 1;
            }
            let o = at(x, y);
            for c in 0..nch as usize {
                out[o + c] = (acc[c] / n) as u8;
            }
        }
    }
    finish(out, src)
}

/// Radial / zoom blur — averages samples on the line toward the image center.
fn radial_blur(src: &Pixbuf, s: f64) -> Option<Pixbuf> {
    let (w, h, nch, stride) = (src.width(), src.height(), src.n_channels(), src.rowstride());
    let bytes = src.read_pixel_bytes();
    let data = bytes.as_ref();
    let mut out = vec![0u8; (stride * h) as usize];
    let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
    let samples = 10i32;
    let amount = s * 0.55;
    let at = |x: i32, y: i32| -> usize { (y * stride + x * nch) as usize };
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0u32; 4];
            for i in 0..samples {
                let t = 1.0 - amount * (f64::from(i) / f64::from(samples - 1));
                let sx = (cx + (x as f64 - cx) * t).round() as i32;
                let sy = (cy + (y as f64 - cy) * t).round() as i32;
                let j = at(sx.clamp(0, w - 1), sy.clamp(0, h - 1));
                for c in 0..nch as usize {
                    acc[c] += u32::from(data[j + c]);
                }
            }
            let o = at(x, y);
            for c in 0..nch as usize {
                out[o + c] = (acc[c] / samples as u32) as u8;
            }
        }
    }
    finish(out, src)
}

/// Static water effect — displaces each pixel by a sinusoidal ripple.
fn water_ripple(src: &Pixbuf, s: f64) -> Option<Pixbuf> {
    let (w, h, nch, stride) = (src.width(), src.height(), src.n_channels(), src.rowstride());
    let bytes = src.read_pixel_bytes();
    let data = bytes.as_ref();
    let mut out = vec![0u8; (stride * h) as usize];
    let amp = s * (w.min(h) as f64 * 0.06);
    let waves = 6.0;
    let (fx, fy) = (
        2.0 * std::f64::consts::PI * waves / w as f64,
        2.0 * std::f64::consts::PI * waves / h as f64,
    );
    let at = |x: i32, y: i32| -> usize { (y * stride + x * nch) as usize };
    for y in 0..h {
        for x in 0..w {
            let dx = amp * (y as f64 * fy).sin();
            let dy = amp * (x as f64 * fx).sin();
            let sx = ((x as f64 + dx).round() as i32).clamp(0, w - 1);
            let sy = ((y as f64 + dy).round() as i32).clamp(0, h - 1);
            let (i, o) = (at(sx, sy), at(x, y));
            let n = nch as usize;
            out[o..o + n].copy_from_slice(&data[i..i + n]);
        }
    }
    finish(out, src)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::gdk_pixbuf::Colorspace;

    /// A hard black/white edge: blurring widens it, so the profile flattens.
    fn edge(w: i32) -> Pixbuf {
        let pb = Pixbuf::new(Colorspace::Rgb, false, 8, w, 8).unwrap();
        pb.fill(0x0000_00ff);
        let (stride, nch) = (pb.rowstride(), pb.n_channels());
        // SAFETY: no other reference to the pixel data exists here.
        let px = unsafe { pb.pixels() };
        for y in 0..8 {
            for x in w / 2..w {
                let i = (y * stride + x * nch) as usize;
                px[i..i + nch as usize].fill(255);
            }
        }
        pb
    }

    /// How much contrast is left: flat (fully blurred) profiles score lower.
    fn contrast(pb: &Pixbuf) -> u64 {
        let (w, nch) = (pb.width(), pb.n_channels());
        let bytes = pb.read_pixel_bytes();
        (0..w)
            .map(|x| {
                let v = i64::from(bytes[(x * nch) as usize]);
                (v - 128).unsigned_abs()
            })
            .sum()
    }

    /// Every step of the fine Soft scale must actually change the image — that
    /// is the point of the fractional radius (most steps are sub-pixel).
    #[test]
    fn soft_steps_all_differ() {
        let src = edge(SOFT_BASE_PX);
        let mut prev = contrast(&src);
        for v in 1..=SOFT_STRENGTH_MAX {
            let out = gaussian_blur(&src, soft_radius(v)).unwrap();
            let c = contrast(&out);
            assert!(c < prev, "step {v} did not soften the edge ({c} vs {prev})");
            prev = c;
        }
    }

    /// Old 0..10 values keep their radius; anything past the new top clamps.
    #[test]
    fn legacy_soft_strengths_convert() {
        assert_eq!(soft_strength_from_legacy(0), 0);
        assert_eq!(soft_strength_from_legacy(1), SOFT_STRENGTH_DEFAULT);
        assert_eq!(soft_strength_from_legacy(10), SOFT_STRENGTH_MAX);
        assert!(soft_strength_from_legacy(3) < SOFT_STRENGTH_MAX);
    }
}
