//! Optional desktop tray icon (StatusNotifierItem) via `ksni`.
//!
//! `ksni` runs the SNI/D-Bus service on its **own** thread. To stay clear of the
//! relm4 `Msg` (which is not `Send`, because the whole app — including MPRIS —
//! lives on the glib main loop), the tray only carries `Send` data: a tiny
//! [`TrayCmd`] channel back to the app. A `glib::spawn_future_local` receiver on
//! the GTK thread (see `src/ui/app_tray.rs`) translates each command into a
//! `Msg`. State *to* the tray (play/pause label, enabled) is pushed via the
//! returned [`ksni::Handle`], whose `update` is synchronous and main-thread safe.

use crate::i18n::gettext;
use ksni::menu::StandardItem;
use ksni::{MenuItem, Tray};

/// A command from the tray (background thread) to the app (main loop). Kept
/// tiny, `Copy` and `Send` so it can cross the thread boundary through an
/// `async-channel`.
#[derive(Clone, Copy, Debug)]
pub enum TrayCmd {
    /// Left click at screen position (x, y): open the media popup near the icon.
    Popup(i32, i32),
    /// Menu "Show / Hide": toggle the main window's visibility.
    ShowHide,
    PlayPause,
    Next,
    Prev,
    Quit,
}

/// The StatusNotifierItem model handed to `ksni`. Lives on ksni's thread, hence
/// only `Send` data: the channel to the app plus the two bits of state shown in
/// the context menu.
pub struct EmiliaTray {
    pub tx: async_channel::Sender<TrayCmd>,
    pub playing: bool,
    pub has_track: bool,
    /// Optional ARGB32 pixmap (grayscale variant). When non-empty it takes
    /// precedence over the themed `icon_name` (built on the GTK thread in
    /// `src/ui/app_tray.rs`).
    pub icon_px: Vec<ksni::Icon>,
}

impl Tray for EmiliaTray {
    fn id(&self) -> String {
        "de.cais.Emilia".into()
    }
    fn title(&self) -> String {
        "Emilia".into()
    }
    /// Themed icon name = the app id (installed under hicolor). On a bare `cargo
    /// run` (icon not installed) the host shows a placeholder, but the menu and
    /// clicks still work. Empty when a pixmap (grayscale) is provided, so the
    /// host uses that instead.
    fn icon_name(&self) -> String {
        if self.icon_px.is_empty() {
            "de.cais.Emilia".into()
        } else {
            String::new()
        }
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon_px.clone()
    }
    /// Left click: toggle the main window (show/hide).
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.try_send(TrayCmd::ShowHide);
    }
    /// Middle click: the styled media popup (right click is the native menu).
    fn secondary_activate(&mut self, x: i32, y: i32) {
        let _ = self.tx.try_send(TrayCmd::Popup(x, y));
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: gettext("Show / Hide"),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.try_send(TrayCmd::ShowHide);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: if self.playing {
                    gettext("Pause")
                } else {
                    gettext("Play")
                },
                enabled: self.has_track,
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.try_send(TrayCmd::PlayPause);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: gettext("Next"),
                enabled: self.has_track,
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.try_send(TrayCmd::Next);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: gettext("Previous"),
                enabled: self.has_track,
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.try_send(TrayCmd::Prev);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: gettext("Quit Emilia"),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.try_send(TrayCmd::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Spawn the tray on its own thread and return a [`ksni::Handle`] for live menu
/// updates. Inside a Flatpak sandbox, requesting an own bus name is denied, so
/// the dbus-name-less variant (reusing the connection's unique name) is used.
pub fn spawn(tray: EmiliaTray) -> ksni::Handle<EmiliaTray> {
    let service = ksni::TrayService::new(tray);
    let handle = service.handle();
    if std::path::Path::new("/.flatpak-info").exists() {
        service.spawn_without_dbus_name();
    } else {
        service.spawn();
    }
    handle
}
