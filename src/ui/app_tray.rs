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
            icon_px: if self.tray.icon_gray {
                gray_tray_icon()
            } else {
                Vec::new()
            },
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
                    TrayCmd::Popup(x, y) => Msg::TrayMediaPopup(x, y),
                    TrayCmd::ShowHide => Msg::TrayToggleWindow,
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

    /// Push the current play/track state to the tray menu and the open media
    /// popup (if any).
    pub(crate) fn refresh_tray_state(&self) {
        if let Some(handle) = &self.tray.handle {
            let playing = self.mini.playing;
            let has_track = !self.transport.queue.is_empty();
            handle.update(move |t| {
                t.playing = playing;
                t.has_track = has_track;
            });
        }
        self.refresh_media_popup();
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

/// Build a **grayscale** ARGB32 pixmap of the app icon for the tray. Must run on
/// the GTK thread (icon-theme lookup + pixbuf decode). Empty vec on any failure
/// → caller falls back to the themed colored icon name. SNI wants ARGB32 in
/// network byte order, i.e. `[A, R, G, B]` per pixel.
pub(crate) fn gray_tray_icon() -> Vec<ksni::Icon> {
    let Some(display) = gtk::gdk::Display::default() else {
        return Vec::new();
    };
    let paintable = gtk::IconTheme::for_display(&display).lookup_icon(
        "de.cais.Emilia",
        &[],
        48,
        1,
        gtk::TextDirection::None,
        gtk::IconLookupFlags::empty(),
    );
    let Some(path) = paintable.file().and_then(|f| f.path()) else {
        return Vec::new();
    };
    let Ok(pixbuf) = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(&path, 48, 48, true) else {
        return Vec::new();
    };
    let (w, h) = (pixbuf.width(), pixbuf.height());
    let rowstride = pixbuf.rowstride() as usize;
    let nch = pixbuf.n_channels() as usize;
    let bytes = pixbuf.read_pixel_bytes();
    let src = bytes.as_ref();
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let i = y * rowstride + x * nch;
            if i + 2 >= src.len() {
                return Vec::new();
            }
            let (r, g, b) = (
                u32::from(src[i]),
                u32::from(src[i + 1]),
                u32::from(src[i + 2]),
            );
            let a = if nch == 4 { src[i + 3] } else { 255 };
            let gray = ((r * 299 + g * 587 + b * 114) / 1000) as u8;
            data.extend_from_slice(&[a, gray, gray, gray]);
        }
    }
    vec![ksni::Icon {
        width: w,
        height: h,
        data,
    }]
}

/// Best-effort move of a top-level window to screen position (x, y). X11 only;
/// GTK4 dropped window positioning, so the media popup is placed via x11rb.
pub(crate) fn move_window_x11(window: &gtk::Window, x: i32, y: i32) {
    let Some(surface) = window.surface() else {
        return;
    };
    let Ok(x11) = surface.downcast::<gdk4_x11::X11Surface>() else {
        return;
    };
    let xid = x11.xid() as u32;
    if let Err(e) = configure_window_pos(xid, x, y) {
        tracing::debug!("media popup positioning failed: {e}");
    }
}

fn configure_window_pos(xid: u32, x: i32, y: i32) -> Result<(), Box<dyn std::error::Error>> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt};
    let (conn, _) = x11rb::connect(None)?;
    conn.configure_window(xid, &ConfigureWindowAux::new().x(x).y(y))?;
    conn.flush()?;
    Ok(())
}

/// Read the live `tray_skip_taskbar` setting and apply it to the window. Wired
/// to both `realize` and `map` so the hint takes effect from the first show.
pub(crate) fn apply_skip_taskbar_from_db(win: &adw::ApplicationWindow) {
    if let Ok(lib) = crate::core::db::Library::open() {
        let enable = matches!(
            lib.get_setting("tray_skip_taskbar")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        apply_skip_taskbar(win, enable);
    }
}

fn set_skip_taskbar_x11(xid: u32, enable: bool) -> Result<(), Box<dyn std::error::Error>> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        AtomEnum, ClientMessageEvent, ConnectionExt, EventMask, PropMode,
    };
    use x11rb::wrapper::ConnectionExt as _; // for change_property32

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

    // 1) Update the _NET_WM_STATE *property* directly (read-modify-write, keeping
    // any other states). The WM reads this when it maps the window, so the hint
    // is honored from the very first show — including "start hidden → first
    // reveal" — not only after a later settings toggle.
    let cur = conn
        .get_property(false, xid, net_wm_state, AtomEnum::ATOM, 0, 1024)?
        .reply()?;
    let mut atoms: Vec<u32> = cur.value32().map(|it| it.collect()).unwrap_or_default();
    atoms.retain(|a| *a != skip_taskbar && *a != skip_pager);
    if enable {
        atoms.push(skip_taskbar);
        atoms.push(skip_pager);
    }
    conn.change_property32(PropMode::REPLACE, xid, net_wm_state, AtomEnum::ATOM, &atoms)?;

    // 2) Live ClientMessage for the already-mapped case (runtime toggle).
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
