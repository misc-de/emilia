//! MPRIS-style media popup, opened by a **left click** on the tray icon (see
//! `src/core/tray.rs`). ksni/StatusNotifierItem can only offer a D-Bus *menu*,
//! not a rich popover, so we render our own small borderless window (cover +
//! title/artist + transport controls) and position it near the icon via x11rb.

use adw::prelude::*;
use relm4::gtk;
use relm4::ComponentSender;

use crate::i18n::gettext;
use crate::ui::app::{App, Msg};
use crate::ui::widgets::decode_scaled;

/// Widgets of the tray media popup that need live updates.
pub(crate) struct MediaPopup {
    window: gtk::Window,
    cover: gtk::Image,
    title: gtk::Label,
    artist: gtk::Label,
    play_btn: gtk::Button,
    /// Blurred-background layer (mirrors the main window) so the popup shows the
    /// same Design; `bg_picture` carries the shared texture, `scrim` tints it.
    bg_picture: gtk::Picture,
    scrim: gtk::Box,
}

impl MediaPopup {
    fn build(root: &adw::ApplicationWindow, sender: &ComponentSender<App>) -> Self {
        let window = gtk::Window::builder()
            .decorated(false)
            .resizable(false)
            .modal(false)
            .css_classes(["emilia-tray-popup"])
            .build();
        window.set_transient_for(Some(root));
        // Popover-like: hide as soon as it loses focus.
        window.connect_is_active_notify(|w| {
            if !w.is_active() {
                w.set_visible(false);
            }
        });

        let cover = gtk::Image::builder()
            .pixel_size(56)
            .css_classes(["card"])
            .build();
        let title = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        let artist = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build();

        let info = gtk::Box::new(gtk::Orientation::Vertical, 0);
        info.set_valign(gtk::Align::Center);
        info.set_hexpand(true);
        info.append(&title);
        info.append(&artist);

        let top = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        top.append(&cover);
        top.append(&info);

        let prev = gtk::Button::builder()
            .icon_name("media-skip-backward-symbolic")
            .css_classes(["flat", "circular"])
            .build();
        let play_btn = gtk::Button::builder()
            .icon_name("media-playback-start-symbolic")
            .css_classes(["circular", "suggested-action"])
            .build();
        let next = gtk::Button::builder()
            .icon_name("media-skip-forward-symbolic")
            .css_classes(["flat", "circular"])
            .build();
        {
            let s = sender.clone();
            prev.connect_clicked(move |_| s.input(Msg::Prev));
        }
        {
            let s = sender.clone();
            play_btn.connect_clicked(move |_| s.input(Msg::TogglePlay));
        }
        {
            let s = sender.clone();
            next.connect_clicked(move |_| s.input(Msg::Next));
        }
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        controls.set_halign(gtk::Align::Center);
        controls.append(&prev);
        controls.append(&play_btn);
        controls.append(&next);

        let root_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
        root_box.set_margin_top(12);
        root_box.set_margin_bottom(12);
        root_box.set_margin_start(12);
        root_box.set_margin_end(12);
        root_box.set_size_request(280, -1);
        root_box.append(&top);
        root_box.append(&controls);

        // Same Design as the app: a blurred background Picture + scrim behind the
        // content. The texture is shared from the main window's theme state and
        // pushed in `refresh_media_popup`; the chrome tint/text color come from
        // the runtime stylesheet (`.emilia-tray-popup`, see `build_css`).
        let bg_picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Cover)
            .can_target(false)
            .css_classes(["emilia-bg"])
            .build();
        let scrim = gtk::Box::builder()
            .css_classes(["emilia-bg-scrim"])
            .can_target(false)
            .visible(false)
            .build();
        let overlay = gtk::Overlay::new();
        overlay.add_css_class("emilia-tray-popup-clip");
        overlay.set_overflow(gtk::Overflow::Hidden);
        overlay.set_child(Some(&bg_picture));
        overlay.add_overlay(&scrim);
        overlay.add_overlay(&root_box);
        window.set_child(Some(&overlay));

        Self {
            window,
            cover,
            title,
            artist,
            play_btn,
            bg_picture,
            scrim,
        }
    }
}

impl App {
    /// Toggle the tray media popup: hide if open, otherwise refresh + show it
    /// near the click position (x, y).
    pub(crate) fn toggle_media_popup(
        &mut self,
        x: i32,
        y: i32,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let popup = self
            .media_popup
            .get_or_insert_with(|| MediaPopup::build(root, sender));
        let window = popup.window.clone();
        if window.is_visible() {
            window.set_visible(false);
            return;
        }
        self.refresh_media_popup();
        window.present();
        // Position up-left of the click (best-effort, X11) so it doesn't cover a
        // bottom/right tray. (0, 0) = host gave no coordinates → let the WM place it.
        if x != 0 || y != 0 {
            let w = window.clone();
            gtk::glib::idle_add_local_once(move || {
                crate::ui::app_tray::move_window_x11(&w, (x - 290).max(0), (y - 150).max(0));
            });
        }
    }

    /// Push the current track + play state into the (possibly open) media popup.
    pub(crate) fn refresh_media_popup(&self) {
        let Some(p) = &self.media_popup else {
            return;
        };
        let title = self
            .mini
            .now_playing
            .clone()
            .unwrap_or_else(|| gettext("Nothing playing"));
        p.title.set_text(&title);

        let (artist, cover) = match self.transport.playing_path.as_ref() {
            Some(path) => {
                let track = self
                    .library
                    .track_by_path(&path.to_string_lossy())
                    .ok()
                    .flatten();
                let artist = track
                    .as_ref()
                    .and_then(|t| t.artist.clone())
                    .unwrap_or_default();
                let cover = track
                    .and_then(|t| t.album)
                    .and_then(|a| self.library.album_cover(&a).ok().flatten());
                (artist, cover)
            }
            None => (String::new(), None),
        };
        p.artist.set_text(&artist);
        p.artist.set_visible(!artist.is_empty());

        match cover.as_deref().and_then(|c| decode_scaled(c, 64)) {
            Some(tex) => p.cover.set_paintable(Some(&tex)),
            None => p.cover.set_icon_name(Some("audio-x-generic-symbolic")),
        }

        p.play_btn.set_icon_name(if self.mini.playing {
            "media-playback-pause-symbolic"
        } else {
            "media-playback-start-symbolic"
        });

        // Mirror the app's blurred background into the popup (shared texture).
        let paintable = self.theme.bg_paintable();
        p.bg_picture.set_paintable(paintable.as_ref());
        p.scrim.set_visible(paintable.is_some());
    }
}
