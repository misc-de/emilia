// The MCP `tool_list()` builds one large nested `serde_json::json!` literal;
// with ~40 tools it exceeds the default macro recursion limit (128).
#![recursion_limit = "512"]

mod core;
mod i18n;
mod model;
mod ui;

use relm4::{adw, RelmApp};

const APP_ID: &str = "de.cais.Emilia";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "emilia=info".into()),
        )
        .init();

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

    let gtk_app = adw::Application::builder().application_id(APP_ID).build();

    let app = RelmApp::from_app(gtk_app);
    app.run::<ui::app::App>(());
}
