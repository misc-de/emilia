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
//! decoded **tiny** ([`COVER_BLUR_PX`]) and shown in a [`gtk::Picture`] that
//! fills the window behind the content; GTK's linear upscaling turns it into a
//! smooth blur. A scrim plus translucent chrome keep text readable.

use gtk::prelude::*;
use relm4::gtk;
use std::path::PathBuf;

use crate::ui::app::App;
use crate::ui::widgets::decode_scaled;

/// Longer-edge pixel size the now-playing cover is decoded to before it is
/// upscaled to fill the window — small = heavily blurred.
const COVER_BLUR_PX: i32 = 32;
/// Same for a user-chosen background image ("extremely blurred").
const CUSTOM_BG_PX: i32 = 48;

/// The four user-configurable design options (besides scaling).
#[derive(Clone, Default)]
pub(crate) struct DesignSettings {
    /// Blur the current cover behind the app content.
    pub(crate) cover_blur: bool,
    /// A user-chosen background image (already copied into the app data dir).
    /// Fallback when cover-blur is on but no cover is available; shown fixed
    /// everywhere when cover-blur is off.
    pub(crate) custom_bg: Option<PathBuf>,
    /// Background color for buttons / list rows / entries (hex `#rrggbb`).
    pub(crate) button_bg: Option<String>,
    /// Text (foreground) color (hex `#rrggbb`).
    pub(crate) text_color: Option<String>,
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
        }
    }

    /// Wire the background layer widgets created by the `view!` tree.
    pub(crate) fn set_bg_widgets(&mut self, picture: gtk::Picture, scrim: gtk::Box) {
        picture.set_can_target(false);
        scrim.set_can_target(false);
        scrim.set_visible(false);
        self.bg_picture = Some(picture);
        self.scrim = Some(scrim);
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

        // ---- Colors ----
        if let Some(fg) = self.design.text_color.as_deref().filter(|c| valid_hex(c)) {
            css.push_str(&format!(
                "@define-color window_fg_color {fg};@define-color view_fg_color {fg};"
            ));
        }
        if let Some(bg) = self.design.button_bg.as_deref().filter(|c| valid_hex(c)) {
            css.push_str(&format!(
                "button:not(.flat):not(.suggested-action):not(.destructive-action):not(.image-button),\
                 entry, spinbutton, .boxed-list > row {{ background-color: {bg}; }}"
            ));
        }

        // ---- Blurred background: make the chrome translucent so it shows through ----
        if self.bg_active {
            css.push_str(
                "window, window > box, window > overlay, .view, viewport, stack, \
                 scrolledwindow, list, flowbox, clamp { background-color: transparent; background-image: none; }\
                 headerbar, .toolbar { background-color: alpha(@window_bg_color, 0.55); }\
                 list > row, .boxed-list > row { background-color: alpha(@window_bg_color, 0.6); }",
            );
        }

        css
    }
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

impl App {
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

    /// Cheap track-change hook: only rebuild the background when cover-blur is
    /// actually on (otherwise nothing about the background depends on the track).
    pub(crate) fn refresh_cover_background(&mut self) {
        if self.theme.design.cover_blur {
            self.refresh_background();
        }
    }

    /// Recompute the blurred background layer for the current design settings +
    /// now-playing cover, then reapply the stylesheet (transparency depends on
    /// whether a background is shown).
    pub(crate) fn refresh_background(&mut self) {
        let tex = self.resolve_bg_texture();
        self.theme.set_background_texture(tex.as_ref());
        self.reapply_runtime_style();
    }

    /// The texture to show as the blurred background (already downscaled), or
    /// `None` for the neutral look. Cover-blur uses the now-playing cover and
    /// falls back to the custom image; without cover-blur the custom image is
    /// the fixed background.
    fn resolve_bg_texture(&self) -> Option<gtk::gdk::Texture> {
        let d = &self.theme.design;
        if d.cover_blur {
            if let Some(cover) = self.now_playing_cover_path() {
                if let Some(t) = decode_scaled(&cover, COVER_BLUR_PX) {
                    return Some(t);
                }
            }
        }
        d.custom_bg
            .as_ref()
            .and_then(|p| decode_scaled(&p.to_string_lossy(), CUSTOM_BG_PX))
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
