// The MCP `tool_list()` builds one large nested `serde_json::json!` literal;
// with ~40 tools it exceeds the default macro recursion limit (128).
#![recursion_limit = "512"]

mod core;
mod i18n;
mod model;
mod ui;

use std::time::Duration;

use relm4::gtk::gio::prelude::ApplicationExt;
use relm4::gtk::{gio, glib};
use relm4::{RelmApp, adw};

const APP_ID: &str = "de.cais.Emilia";

/// Inside the Flatpak, a device without a DRM render node has no GPU the
/// runtime's Mesa can drive — e.g. libhybris phones, whose Android GPU driver
/// only the host can load. GTK would then pick Vulkan on lavapipe, which loads
/// LLVM and compiles shaders on every start; the Cairo renderer starts about
/// 1 s sooner there (measured on a MediaTek/Mali phone: 3.4 s → 2.2 s cold,
/// 2.35 s → 1.45 s warm to the first frame). Outside the Flatpak the host may
/// render through libhybris without a render node, so nothing changes there,
/// and an explicit `GSK_RENDERER` always wins.
fn software_only_renderer() -> bool {
    if std::env::var_os("GSK_RENDERER").is_some()
        || !std::path::Path::new("/.flatpak-info").exists()
    {
        return false;
    }
    let has_render_node = std::fs::read_dir("/dev/dri").is_ok_and(|dir| {
        dir.flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
    });
    !has_render_node
}

fn main() {
    // Decided before anything else runs: setting the variable is only sound
    // while the process is still single-threaded, and GTK reads it on startup.
    let cairo_fallback = software_only_renderer();
    if cairo_fallback {
        // SAFETY: no other thread exists yet (first statement of `main`).
        unsafe { std::env::set_var("GSK_RENDERER", "cairo") };
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "emilia=info".into()),
        )
        .init();
    if cairo_fallback {
        tracing::info!("no GPU render node in the sandbox, using the Cairo renderer");
    }

    // Route panics through tracing as well. The default hook only writes to
    // stderr, which is nowhere to be seen when the app was started from a
    // desktop launcher or inside the Flatpak sandbox — so a panic on the UI
    // thread would take the window down leaving no trace in the log. Chained
    // after the default hook, so the usual stderr output (and backtrace, when
    // `RUST_BACKTRACE` is set) is kept.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let where_ = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown location".to_string());
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("unnamed").to_string();
        tracing::error!("panic in thread '{thread}' at {where_}: {}", info);
        default_hook(info);
    }));

    // Initialize i18n before any UI construction. The saved language takes
    // precedence; an explicit "system" follows the locale. With no entry at all
    // (first run) we also follow the system locale, so the first-run setup
    // appears in the user's language – if its catalog is missing, gettext falls
    // back to the English source strings anyway.
    let saved = core::db::Library::open()
        .ok()
        .and_then(|lib| lib.get_setting("ui_language").ok().flatten());
    let lang: Option<&str> = match saved.as_deref() {
        None => None,             // first run → follow the system locale
        Some("system") => None,   // explicitly follow the system locale
        Some(code) => Some(code), // a chosen language (any of the supported ones)
    };
    i18n::init(lang);

    // HANDLES_OPEN: files on the command line (`%U` in the desktop entry) arrive
    // through the `open` signal, see `App::init`.
    let gtk_app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    // A later start only hands over to the running instance and has to end then.
    // `RelmApp::run` closes with one blocking main-loop iteration, and in such a
    // process nothing would ever wake it, so it would stay around for good.
    gtk_app.connect_handle_local_options(|app, _| {
        if app.register(gio::Cancellable::NONE).is_ok() && app.is_remote() {
            glib::timeout_add_local(Duration::from_millis(100), || glib::ControlFlow::Continue);
        }
        -1 // Carry on as usual
    });

    let app = RelmApp::from_app(gtk_app);
    app.run::<ui::app::App>(());
}
