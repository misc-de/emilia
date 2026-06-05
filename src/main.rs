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
    // precedence; an explicit "system" follows the locale, while no entry at all
    // (first run) defaults to English – the source language.
    let saved = core::db::Library::open()
        .ok()
        .and_then(|lib| lib.get_setting("ui_language").ok().flatten());
    let lang: Option<&str> = match saved.as_deref() {
        None => Some("en"),       // first run → default to English
        Some("system") => None,   // explicitly follow the system locale
        Some(code) => Some(code), // a chosen language (any of the 24 EU languages)
    };
    i18n::init(lang);

    let gtk_app = adw::Application::builder().application_id(APP_ID).build();

    let app = RelmApp::from_app(gtk_app);
    app.run::<ui::app::App>(());
}
