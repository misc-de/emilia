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
