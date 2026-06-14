//! App-side glue for the optional desktop tray icon: starting/stopping the
//! [`ksni`](crate::core::tray) service, bridging its background-thread commands
//! back onto the glib main loop, close-to-tray, and the best-effort X11
//! "hide from taskbar" hint.

use adw::prelude::*;
use relm4::gtk;
use relm4::ComponentSender;

use gtk::glib;

use crate::core::tray::{self, EmiliaTray, TrayCmd};
use crate::ui::app::{App, Msg};

impl App {
    /// Start the tray service (idempotent) and bridge its commands → `Msg`.
    pub(crate) fn start_tray(
        &mut self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        if self.tray.handle.is_some() {
            return;
        }
        let (tx, rx) = async_channel::unbounded::<TrayCmd>();
        let tray = EmiliaTray {
            tx,
            playing: self.mini.playing,
            has_track: !self.transport.queue.is_empty(),
        };
        self.tray.handle = Some(tray::spawn(tray));
        // Keep the GApplication alive while only the tray is left — hiding the
        // last window would otherwise let `run()` return and quit the process.
        if self.tray.hold.is_none() {
            self.tray.hold = root.application().map(|a| a.hold());
        }
        // Tray commands arrive on ksni's thread; translate them into `Msg` on the
        // main loop (where the non-`Send` relm4 sender is valid to use).
        let sender = sender.clone();
        glib::spawn_future_local(async move {
            while let Ok(cmd) = rx.recv().await {
                sender.input(match cmd {
                    TrayCmd::Toggle => Msg::TrayToggleWindow,
                    TrayCmd::PlayPause => Msg::TogglePlay,
                    TrayCmd::Next => Msg::Next,
                    TrayCmd::Prev => Msg::Prev,
                    TrayCmd::Quit => Msg::TrayQuit,
                });
            }
        });
    }

    /// Tear down the tray service and release the app-hold.
    pub(crate) fn stop_tray(&mut self) {
        if let Some(handle) = self.tray.handle.take() {
            handle.shutdown();
        }
        self.tray.hold = None;
    }

    /// Push the current play/track state to the tray menu (if running).
    pub(crate) fn refresh_tray_state(&self) {
        if let Some(handle) = &self.tray.handle {
            let playing = self.mini.playing;
            let has_track = !self.transport.queue.is_empty();
            handle.update(move |t| {
                t.playing = playing;
                t.has_track = has_track;
            });
        }
    }

    /// Tray click / menu "Show / Hide": toggle the main window's visibility.
    pub(crate) fn tray_toggle_window(&self, root: &adw::ApplicationWindow) {
        if root.is_visible() {
            root.set_visible(false);
        } else {
            root.set_visible(true);
            root.present();
        }
    }

    /// (Re)apply the EWMH skip-taskbar hint for the current setting.
    pub(crate) fn refresh_skip_taskbar(&self, root: &adw::ApplicationWindow) {
        apply_skip_taskbar(root, self.tray.skip_taskbar);
    }
}

/// Set/clear `_NET_WM_STATE_SKIP_TASKBAR` (+ pager) on the window. Best-effort
/// and X11 only; silently does nothing on Wayland or on any X error.
pub(crate) fn apply_skip_taskbar(root: &adw::ApplicationWindow, enable: bool) {
    let Some(surface) = root.surface() else {
        return;
    };
    let Ok(x11) = surface.downcast::<gdk4_x11::X11Surface>() else {
        return;
    };
    let xid = x11.xid() as u32;
    if let Err(e) = set_skip_taskbar_x11(xid, enable) {
        tracing::debug!("skip-taskbar hint failed: {e}");
    }
}

fn set_skip_taskbar_x11(xid: u32, enable: bool) -> Result<(), Box<dyn std::error::Error>> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ClientMessageEvent, ConnectionExt, EventMask};

    let (conn, screen_num) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let net_wm_state = conn.intern_atom(false, b"_NET_WM_STATE")?.reply()?.atom;
    let skip_taskbar = conn
        .intern_atom(false, b"_NET_WM_STATE_SKIP_TASKBAR")?
        .reply()?
        .atom;
    let skip_pager = conn
        .intern_atom(false, b"_NET_WM_STATE_SKIP_PAGER")?
        .reply()?
        .atom;
    // _NET_WM_STATE: action (1 = ADD, 0 = REMOVE), two properties, source = app.
    let data = [u32::from(enable), skip_taskbar, skip_pager, 1, 0];
    let event = ClientMessageEvent::new(32, xid, net_wm_state, data);
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )?;
    conn.flush()?;
    Ok(())
}
