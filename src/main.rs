mod core;
mod i18n;
mod model;
mod ui;

use relm4::{adw, gtk, RelmApp};

const APP_ID: &str = "de.cais.Emilia";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "emilia=info".into()),
        )
        .init();

    // Initialize i18n before any UI construction. The saved language takes
    // precedence over the system locale; "system"/no entry follows the locale.
    let lang = core::db::Library::open()
        .ok()
        .and_then(|lib| lib.get_setting("ui_language").ok().flatten())
        .filter(|code| code == "de" || code == "en");
    i18n::init(lang.as_deref());

    // NON_UNIQUE: each execution opens its own window (no "reuse" of an already
    // running instance). Reliable during development; for a release this can be
    // removed again.
    let gtk_app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let app = RelmApp::from_app(gtk_app);
    app.run::<ui::app::App>(());
}
