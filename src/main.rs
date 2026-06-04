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
    // precedence over the system locale; "system"/no entry follows the locale.
    let lang = core::db::Library::open()
        .ok()
        .and_then(|lib| lib.get_setting("ui_language").ok().flatten())
        .filter(|code| code == "de" || code == "en");
    i18n::init(lang.as_deref());

    let gtk_app = adw::Application::builder()
        .application_id(APP_ID)
        .build();

    let app = RelmApp::from_app(gtk_app);
    app.run::<ui::app::App>(());
}
